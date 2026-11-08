//! `ferostream-agent --stdio`: what SSH starts. It finds the session (a new
//! one, or the one to attach to) and passes bytes between SSH's stdin and
//! stdout and the session's socket, untouched. When SSH goes away, so does
//! this process; the session goes on.

use std::fs;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt as _;
use std::path::Path;
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use ferostream_proto::{Message, PREAMBLE};

use crate::runtime;

/// Which session the client wants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// Start a new one.
    New,
    /// The one whose client left most recently.
    Newest,
    /// This one.
    Id(String),
}

/// How long a new session may take to start listening.
const START_WAIT: Duration = Duration::from_secs(10);

pub fn run(target: &Target) -> ExitCode {
    match connect(target).and_then(pass_through) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // In the protocol, so the client can show it.
            let mut out = std::io::stdout().lock();
            let _ = out.write_all(PREAMBLE);
            let _ = out.write_all(&Message::Error { message: e.clone() }.encode());
            let _ = out.flush();
            eprintln!("ferostream-agent: {e}");
            ExitCode::FAILURE
        }
    }
}

fn connect(target: &Target) -> Result<UnixStream, String> {
    let base = runtime::base()?;
    match target {
        Target::New => start(&base),
        Target::Id(id) => {
            let known = runtime::list(&base).into_iter().any(|s| &s.id == id);
            if !known {
                return Err(format!(
                    "there is no FeroStream session {id} on this host (any more)"
                ));
            }
            UnixStream::connect(base.join(id).join("sock"))
                .map_err(|e| format!("could not reach session {id}: {e}"))
        }
        Target::Newest => {
            let sessions = runtime::list(&base);
            let s = runtime::newest_detached(&sessions)
                .ok_or("there is no FeroStream session on this host to attach to")?;
            UnixStream::connect(s.dir.join("sock"))
                .map_err(|e| format!("could not reach session {}: {e}", s.id))
        }
    }
}

/// Starts a session process, detached from SSH, and connects to it.
fn start(base: &Path) -> Result<UnixStream, String> {
    let id = runtime::new_id()?;
    let dir = runtime::create(base, &id)?;
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("log"))
        .map_err(|e| format!("could not make the session log: {e}"))?;
    let me = std::env::current_exe().map_err(|e| format!("where is this program? {e}"))?;
    let child = Command::new(me)
        .arg("--serve")
        .arg(&dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log)
        // Its own process group: a signal for SSH's (Ctrl+C, a hang-up)
        // does not reach it.
        .process_group(0)
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            let _ = fs::remove_dir_all(&dir);
            return Err(format!("could not start the session: {e}"));
        }
    };
    let sock = dir.join("sock");
    let deadline = Instant::now() + START_WAIT;
    loop {
        if let Ok(s) = UnixStream::connect(&sock) {
            return Ok(s);
        }
        if let Ok(Some(status)) = child.try_wait() {
            let log = fs::read_to_string(dir.join("log")).unwrap_or_default();
            let _ = fs::remove_dir_all(&dir);
            return Err(format!(
                "the session stopped at start ({status}): {}",
                log.trim()
            ));
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = fs::remove_dir_all(&dir);
            return Err("the session did not start within 10 s".into());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Copies stdin to the session and the session to stdout until either ends.
fn pass_through(sock: UnixStream) -> Result<(), String> {
    let mut to_session = sock.try_clone().map_err(|e| format!("socket: {e}"))?;
    std::thread::spawn(move || {
        let mut input = std::io::stdin().lock();
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match input.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if to_session
                        .write_all(buf.get(..n).unwrap_or_default())
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
        // The client went away: the session hears the end of its input and
        // carries on without a client.
        let _ = to_session.shutdown(std::net::Shutdown::Both);
    });
    let mut from_session = sock;
    let mut out = std::io::stdout().lock();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        match from_session.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if out.write_all(buf.get(..n).unwrap_or_default()).is_err() || out.flush().is_err()
                {
                    break;
                }
            }
        }
    }
    Ok(())
}
