//! The agent proper (Linux): runs a Linux program on a private X display and
//! streams its windows to a FeroStream client.
//!
//! Started by the client through SSH as `ferostream-agent --stdio`. That
//! process ([`relay`]) only passes bytes along: the session itself runs in
//! a process of its own (`--serve`), which keeps the display and the program
//! when the connection drops, until a client attaches again
//! (`--stdio --attach ID`) or the program ends. It needs Xvfb; nothing else
//! is installed or started, and nothing listens on the network: the session
//! listens on a Unix socket in a directory only this user can enter
//! ([`runtime`]).

use std::io::{BufWriter, Read, Write};
use std::os::fd::AsFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ferostream_proto::{FrameReader, Message, PREAMBLE, VERSION, is_session_id};

use crate::relay::{self, Target};
use crate::runtime;
use crate::scale;
use crate::session::{Session, Sink};
use crate::xserver::XServer;

const SOFTWARE: &str = concat!("ferostream-agent ", env!("CARGO_PKG_VERSION"));

const USAGE: &str = "usage: ferostream-agent --stdio [--attach [ID]] [--remove-when-done]
       ferostream-agent --list | --stop ID

Speaks the FeroStream protocol on stdin and stdout. Start it through
ferostream.exe (or another FeroStream client), not by hand.

  --stdio            start a new session
  --stdio --attach   take over the session whose client left last
  --stdio --attach ID  take over session ID
  --list             the sessions running for this user
  --stop ID          end session ID (its program is stopped)
  --remove-when-done delete this agent's file once its session ends and
                     no other session of this user runs";

/// How long the first client may take to connect to a new session.
const FIRST_CLIENT_WAIT: Duration = Duration::from_secs(10);
/// How long a client may take to say who it is.
const HANDSHAKE_WAIT: Duration = Duration::from_secs(30);

pub fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let remove = args.iter().any(|a| a == "--remove-when-done");
    let args: Vec<&str> = args
        .iter()
        .map(String::as_str)
        .filter(|a| *a != "--remove-when-done")
        .collect();
    match args.as_slice() {
        [] | ["--stdio"] => relay::run(&Target::New, remove),
        ["--stdio", "--attach"] => relay::run(&Target::Newest, remove),
        ["--stdio", "--attach", id] if is_session_id(id) => {
            relay::run(&Target::Id((*id).into()), remove)
        }
        ["--serve", dir] => match serve(Path::new(dir)) {
            Ok(code) => ExitCode::from(code.clamp(0, 255) as u8),
            Err(e) => {
                eprintln!("ferostream-agent: {e}");
                ExitCode::FAILURE
            }
        },
        ["--list"] => list(),
        ["--stop", id] if is_session_id(id) => stop(id),
        ["--version" | "-V"] => {
            println!("{SOFTWARE} (protocol {VERSION})");
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
    }
}

fn list() -> ExitCode {
    let base = match runtime::base() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("ferostream-agent: {e}");
            return ExitCode::FAILURE;
        }
    };
    for s in runtime::list(&base) {
        let state = match s.detached.and_then(|t| t.elapsed().ok()) {
            None => "attached".to_string(),
            Some(d) => format!("detached {} min ago", d.as_secs() / 60),
        };
        println!("{}  {state:<22}  {}", s.id, s.program);
    }
    ExitCode::SUCCESS
}

/// Ends a session by stopping its display: the session sees its X
/// connection end and stops the program and cleans up, as when the program
/// ends.
fn stop(id: &str) -> ExitCode {
    let result = runtime::base().and_then(|base| {
        let s = runtime::list(&base)
            .into_iter()
            .find(|s| s.id == id)
            .ok_or_else(|| format!("there is no session {id}"))?;
        let pid = std::fs::read_to_string(s.dir.join("xvfb-pid"))
            .ok()
            .and_then(|p| p.trim().parse::<u32>().ok())
            .ok_or("the session has no display yet")?;
        let killed = Command::new("kill")
            .arg(pid.to_string())
            .status()
            .is_ok_and(|s| s.success());
        if !killed {
            return Err(format!("could not stop the display (process {pid})"));
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        while s.dir.exists() {
            if Instant::now() > deadline {
                return Err("the session did not end within 10 s".into());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Ok(())
    });
    match result {
        Ok(()) => ExitCode::SUCCESS,
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

/// A client connection as the session writes to it. A failed write shuts
/// the socket, so the connection's reader sees the end too.
struct Client {
    w: BufWriter<UnixStream>,
}

impl Sink for Client {
    fn send(&mut self, frames: &[Message]) -> std::io::Result<()> {
        let r = frames
            .iter()
            .try_for_each(|m| self.w.write_all(&m.encode()))
            .and_then(|()| self.w.flush());
        if r.is_err() {
            let _ = self.w.get_ref().shutdown(std::net::Shutdown::Both);
        }
        r
    }
}

/// What a client asked for after Hello.
enum Wants {
    Start {
        width: u16,
        height: u16,
        dpi: u16,
        argv: Vec<String>,
    },
    Attach,
}

/// Greets a new connection and reads what it wants. `Ok(None)`: it left,
/// or was refused (and told why).
fn handshake(stream: &UnixStream, reader: &mut FrameReader) -> Result<Option<Wants>, String> {
    let mut w = stream.try_clone().map_err(|e| e.to_string())?;
    let mut hello = PREAMBLE.to_vec();
    hello.extend(
        Message::Hello {
            version: VERSION,
            software: SOFTWARE.into(),
        }
        .encode(),
    );
    w.write_all(&hello).map_err(|e| e.to_string())?;
    let _ = stream.set_read_timeout(Some(HANDSHAKE_WAIT));
    let mut input = stream;
    let refuse = |mut w: &UnixStream, message: String| {
        let _ = w.write_all(&Message::Error { message }.encode());
        Ok(None)
    };
    match read_message(&mut input, reader) {
        Ok(Some(Message::Hello { version, .. })) if version == VERSION => {}
        Ok(Some(Message::Hello { version, .. })) => {
            return refuse(
                &w,
                format!("the client speaks protocol {version}, this agent {VERSION}"),
            );
        }
        Ok(Some(_)) => return refuse(&w, "the client did not start with Hello".into()),
        Ok(None) | Err(_) => return Ok(None),
    }
    let wants = match read_message(&mut input, reader) {
        Ok(Some(Message::Start {
            width,
            height,
            dpi,
            argv,
        })) => Wants::Start {
            width,
            height,
            dpi,
            argv,
        },
        Ok(Some(Message::Attach)) => Wants::Attach,
        Ok(Some(_)) => return refuse(&w, "expected Start or Attach".into()),
        Ok(None) | Err(_) => return Ok(None),
    };
    let _ = stream.set_read_timeout(None);
    Ok(Some(wants))
}

/// The session process: `dir` is its directory, made by the relay.
fn serve(dir: &Path) -> Result<i32, String> {
    let id = dir
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| is_session_id(n))
        .ok_or("not a session directory")?
        .to_string();
    let result = serve_in(dir, &id);
    let remove = dir.join(runtime::REMOVE_MARK).exists();
    let _ = std::fs::remove_dir_all(dir);
    if remove && let Some(base) = dir.parent() {
        runtime::remove_agent_if_idle(base);
    }
    result
}

fn serve_in(dir: &Path, id: &str) -> Result<i32, String> {
    let listener =
        UnixListener::bind(dir.join("sock")).map_err(|e| format!("could not listen: {e}"))?;
    std::fs::write(dir.join("pid"), std::process::id().to_string())
        .map_err(|e| format!("could not write the pid file: {e}"))?;

    // The first client is the one that started us.
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    let deadline = Instant::now() + FIRST_CLIENT_WAIT;
    let stream = loop {
        match listener.accept() {
            Ok((s, _)) => break s,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() > deadline {
                    return Err("no client came".into());
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => return Err(e.to_string()),
        }
    };
    listener.set_nonblocking(false).map_err(|e| e.to_string())?;
    stream.set_nonblocking(false).map_err(|e| e.to_string())?;
    let mut reader = FrameReader::new();
    let (width, height, dpi, argv) = match handshake(&stream, &mut reader)? {
        Some(Wants::Start {
            width,
            height,
            dpi,
            argv,
        }) => (width, height, dpi, argv),
        Some(Wants::Attach) => {
            return fail(&stream, "there is no session to attach to");
        }
        None => return Ok(0),
    };
    let _ = std::fs::write(dir.join("program"), argv.join(" "));

    let running = Arc::new(Mutex::new(Running::default()));
    let mut xs = match XServer::start(width, height, dpi, dir) {
        Ok(x) => x,
        Err(e) => return fail(&stream, &e),
    };
    let (conn, screen) = match xs.connect() {
        Ok(c) => c,
        Err(e) => {
            xs.stop();
            return fail(&stream, &e);
        }
    };
    if let Err(e) = scale::apply_resources(&conn, screen, dpi) {
        xs.stop();
        return fail(&stream, &e);
    }
    let _ = std::fs::write(dir.join("xvfb-pid"), xs.child.id().to_string());
    let display = format!(":{}", xs.display);
    let auth = xs.auth_file.clone();
    lock(&running).xserver = Some(xs);

    let session = match Session::new(conn, screen, dpi) {
        Ok(s) => s,
        Err(e) => {
            lock(&running).stop();
            return fail(&stream, &e);
        }
    };

    let Some((program, rest)) = argv.split_first() else {
        lock(&running).stop();
        return fail(&stream, "no program to run");
    };
    // The program's own output goes to the session's log (our stderr).
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
        .envs(scale::environment(dpi))
        .envs(desktop_environment(
            std::env::var_os("XDG_CURRENT_DESKTOP").is_some(),
        ))
        .stdin(Stdio::null())
        .stdout(to_stderr())
        .stderr(to_stderr())
        .spawn();
    match child {
        Ok(c) => lock(&running).program = Some(c),
        Err(e) => {
            lock(&running).stop();
            return fail(&stream, &format!("could not start {program}: {e}"));
        }
    }

    let clients = Arc::new(Clients {
        session: Arc::clone(&session),
        id: id.to_string(),
        dir: dir.to_path_buf(),
        current: Mutex::new(None),
    });
    clients.take_over(stream, reader);

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

    // Clients that come back.
    {
        let clients = Arc::clone(&clients);
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let clients = Arc::clone(&clients);
                std::thread::spawn(move || clients.welcome(stream));
            }
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

/// The session's clients: at most one at a time.
struct Clients {
    session: Arc<Session>,
    id: String,
    dir: PathBuf,
    /// The attached client's socket, to shut when another takes over.
    current: Mutex<Option<UnixStream>>,
}

impl Clients {
    /// A client connected to a running session: it may only attach.
    fn welcome(self: &Arc<Self>, stream: UnixStream) {
        let mut reader = FrameReader::new();
        match handshake(&stream, &mut reader) {
            Ok(Some(Wants::Attach)) => self.take_over(stream, reader),
            Ok(Some(Wants::Start { .. })) => {
                let _ = fail(&stream, "this session is already running; attach to it");
            }
            Ok(None) | Err(_) => {}
        }
    }

    /// Makes `stream` the session's client, sending the one before it away,
    /// and reads its input on a thread of its own.
    fn take_over(self: &Arc<Self>, stream: UnixStream, mut reader: FrameReader) {
        let (Ok(writer), Ok(keep)) = (stream.try_clone(), stream.try_clone()) else {
            return;
        };
        if let Some(old) = lock(&self.current).replace(keep) {
            let _ = old.shutdown(std::net::Shutdown::Both);
        }
        let generation = self.session.attach(
            Box::new(Client {
                w: BufWriter::with_capacity(256 * 1024, writer),
            }),
            &self.id,
        );
        runtime::mark_detached(&self.dir, false);
        let clients = Arc::clone(self);
        std::thread::spawn(move || {
            let mut input = &stream;
            loop {
                match read_message(&mut input, &mut reader) {
                    Ok(Some(m)) => {
                        if clients.session.input(m).is_err() {
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
            let _ = stream.shutdown(std::net::Shutdown::Both);
            if clients.session.detach(generation) {
                runtime::mark_detached(&clients.dir, true);
            }
        });
    }
}

fn fail(mut out: &UnixStream, message: &str) -> Result<i32, String> {
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

/// What a program expects from a desktop session that the private display
/// does not have. Without `XDG_CURRENT_DESKTOP`, Qt picks a bare theme with
/// almost no icons (KStars showed blank buttons); `KDE` selects Qt's KDE
/// theme, which reads `kdeglobals` and uses the Breeze icons when they are
/// installed. GTK programs are not affected by it. A value the host already
/// sets is kept.
fn desktop_environment(already_set: bool) -> Vec<(&'static str, &'static str)> {
    let mut env = vec![("XDG_SESSION_TYPE", "x11")];
    if !already_set {
        env.push(("XDG_CURRENT_DESKTOP", "KDE"));
    }
    env
}

#[cfg(test)]
mod tests {
    use super::desktop_environment;

    #[test]
    fn desktop_is_kde_unless_the_host_says_otherwise() {
        assert!(desktop_environment(false).contains(&("XDG_CURRENT_DESKTOP", "KDE")));
        assert!(
            desktop_environment(true)
                .iter()
                .all(|(k, _)| *k != "XDG_CURRENT_DESKTOP")
        );
    }
}
