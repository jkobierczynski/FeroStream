//! The agent proper (Linux): runs a Linux program on a private X display and
//! streams its windows to a FeroStream client over stdin and stdout.
//!
//! Normally started by the client through SSH:
//! `ssh host ferostream-agent --stdio`. It needs Xvfb; nothing else is
//! installed or started, and nothing listens on the network.

use std::io::{BufWriter, Read, Write};
use std::os::fd::AsFd;
use std::process::{Child, Command, ExitCode, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ferostream_proto::{FrameReader, Message, PREAMBLE, VERSION};

use crate::session::Session;
use crate::xserver::XServer;

const SOFTWARE: &str = concat!("ferostream-agent ", env!("CARGO_PKG_VERSION"));

pub fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None | Some("--stdio") => {}
        Some("--version" | "-V") => {
            println!("{SOFTWARE} (protocol {VERSION})");
            return ExitCode::SUCCESS;
        }
        Some(_) => {
            eprintln!(
                "usage: ferostream-agent [--stdio]\n\n\
                 Speaks the FeroStream protocol on stdin and stdout. Start it through\n\
                 ferostream.exe (or another FeroStream client), not by hand."
            );
            return ExitCode::from(2);
        }
    }
    match run() {
        Ok(code) => ExitCode::from(code.clamp(0, 255) as u8),
        Err(e) => {
            eprintln!("ferostream-agent: {e}");
            ExitCode::FAILURE
        }
    }
}

/// What must be stopped however the session ends.
#[derive(Debug, Default)]
struct Running {
    program: Option<Child>,
    xserver: Option<XServer>,
}

impl Running {
    fn stop(&mut self) {
        if let Some(mut p) = self.program.take() {
            let _ = p.kill();
            let _ = p.wait();
        }
        if let Some(mut x) = self.xserver.take() {
            x.stop();
        }
    }
}

fn read_message(
    input: &mut impl Read,
    reader: &mut FrameReader,
) -> Result<Option<Message>, String> {
    let mut buf = [0u8; 64 * 1024];
    loop {
        if let Some(m) = reader.next_message().map_err(|e| e.to_string())? {
            return Ok(Some(m));
        }
        let n = input.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            return Ok(None);
        }
        reader.push(buf.get(..n).unwrap_or_default());
    }
}

fn run() -> Result<i32, String> {
    let mut out = BufWriter::new(std::io::stdout());
    out.write_all(PREAMBLE).map_err(|e| e.to_string())?;
    out.write_all(
        &Message::Hello {
            version: VERSION,
            software: SOFTWARE.into(),
        }
        .encode(),
    )
    .map_err(|e| e.to_string())?;
    out.flush().map_err(|e| e.to_string())?;

    let mut reader = FrameReader::new();
    let (width, height, argv) = {
        let mut input = std::io::stdin().lock();
        match read_message(&mut input, &mut reader)? {
            Some(Message::Hello { version, .. }) if version == VERSION => {}
            Some(Message::Hello { version, .. }) => {
                return fail(
                    &mut out,
                    &format!("the client speaks protocol {version}, this agent {VERSION}"),
                );
            }
            Some(_) => return fail(&mut out, "the client did not start with Hello"),
            None => return Ok(0),
        }
        match read_message(&mut input, &mut reader)? {
            Some(Message::Start {
                width,
                height,
                argv,
            }) => (width, height, argv),
            Some(_) => return fail(&mut out, "expected Start"),
            None => return Ok(0),
        }
    };

    let running = Arc::new(Mutex::new(Running::default()));
    let mut xs = match XServer::start(width, height) {
        Ok(x) => x,
        Err(e) => return fail(&mut out, &e),
    };
    let (conn, screen) = match xs.connect() {
        Ok(c) => c,
        Err(e) => {
            xs.stop();
            return fail(&mut out, &e);
        }
    };
    let display = format!(":{}", xs.display);
    let auth = xs.auth_file.clone();
    lock(&running).xserver = Some(xs);

    let session = match Session::new(conn, screen, Box::new(out)) {
        Ok(s) => s,
        Err(e) => {
            lock(&running).stop();
            return fail(&mut std::io::stdout(), &e);
        }
    };

    let Some((program, rest)) = argv.split_first() else {
        lock(&running).stop();
        return fail(&mut std::io::stdout(), "no program to run");
    };
    // The program's own output must never reach stdout, which carries the
    // protocol: it goes to stderr, which SSH keeps separate.
    let to_stderr = || {
        std::io::stderr()
            .as_fd()
            .try_clone_to_owned()
            .map(Stdio::from)
            .unwrap_or_else(|_| Stdio::null())
    };
    let child = Command::new(program)
        .args(rest)
        .env("DISPLAY", &display)
        .env("XAUTHORITY", &auth)
        .env("GDK_BACKEND", "x11")
        .env("QT_QPA_PLATFORM", "xcb")
        .env_remove("WAYLAND_DISPLAY")
        .stdin(Stdio::null())
        .stdout(to_stderr())
        .stderr(to_stderr())
        .spawn();
    match child {
        Ok(c) => lock(&running).program = Some(c),
        Err(e) => {
            lock(&running).stop();
            return fail(
                &mut std::io::stdout(),
                &format!("could not start {program}: {e}"),
            );
        }
    }

    // Watches the program; the session ends once it has exited and its
    // windows are gone (a program can leave a window to a child).
    {
        let running = Arc::clone(&running);
        let session = Arc::clone(&session);
        std::thread::spawn(move || {
            loop {
                let status = {
                    let mut r = lock(&running);
                    match r.program.as_mut().map(Child::try_wait) {
                        Some(Ok(Some(s))) => Some(s.code().unwrap_or(-1)),
                        Some(Ok(None)) => None,
                        _ => Some(-1),
                    }
                };
                if let Some(code) = status {
                    session.program_exited(code);
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        });
    }

    // Input from the client on its own thread; the end of input (the client
    // or the SSH connection went away) ends everything.
    {
        let running = Arc::clone(&running);
        let session = Arc::clone(&session);
        std::thread::spawn(move || {
            let mut input = std::io::stdin().lock();
            loop {
                match read_message(&mut input, &mut reader) {
                    Ok(Some(m)) => {
                        if session.input(m).is_err() {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        eprintln!("ferostream-agent: bad input from the client: {e}");
                        break;
                    }
                }
            }
            lock(&running).stop();
            std::process::exit(0);
        });
    }

    let result = session.run();
    let code = *result.as_ref().unwrap_or(&1);
    let last = match result {
        Ok(code) => Message::Exited { code },
        Err(message) => Message::Error { message },
    };
    session.finish(&last);
    lock(&running).stop();
    Ok(code)
}

fn fail(out: &mut impl Write, message: &str) -> Result<i32, String> {
    let _ = out.write_all(
        &Message::Error {
            message: message.into(),
        }
        .encode(),
    );
    let _ = out.flush();
    Err(message.into())
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}
