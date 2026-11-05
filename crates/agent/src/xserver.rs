//! The private X display: Xvfb on a free display number, with a random
//! cookie in an Xauthority file only this user can read.

use std::fs;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use x11rb::rust_connection::{DefaultStream, RustConnection};

/// A running Xvfb and what a program needs to reach it.
#[derive(Debug)]
pub struct XServer {
    pub display: u32,
    pub child: Child,
    pub dir: PathBuf,
    pub auth_file: PathBuf,
    pub cookie: [u8; 16],
}

const FIRST_DISPLAY: u32 = 100;
const LAST_DISPLAY: u32 = 399;

impl XServer {
    /// Starts Xvfb with a `width` x `height` screen.
    pub fn start(width: u16, height: u16) -> Result<XServer, String> {
        let mut cookie = [0u8; 16];
        getrandom::fill(&mut cookie).map_err(|e| format!("no random numbers: {e}"))?;
        let dir = private_dir()?;
        let auth_file = dir.join("Xauthority");
        write_xauthority(&auth_file, &cookie)?;

        let mut last_err = String::from("no free X display number");
        for display in FIRST_DISPLAY..=LAST_DISPLAY {
            if Path::new(&format!("/tmp/.X{display}-lock")).exists()
                || Path::new(&format!("/tmp/.X11-unix/X{display}")).exists()
            {
                continue;
            }
            let child = Command::new("Xvfb")
                .arg(format!(":{display}"))
                .args(["-screen", "0"])
                .arg(format!("{width}x{height}x24"))
                .args(["-nolisten", "tcp", "-noreset", "-auth"])
                .arg(&auth_file)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
            let mut child = match child {
                Ok(c) => c,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return Err(
                        "Xvfb is not installed on this host (Debian/Ubuntu: sudo apt install xvfb; \
                         Fedora: sudo dnf install xorg-x11-server-Xvfb)."
                            .into(),
                    );
                }
                Err(e) => return Err(format!("could not start Xvfb: {e}")),
            };
            match wait_for_socket(display, &mut child) {
                Ok(()) => {
                    return Ok(XServer {
                        display,
                        child,
                        dir,
                        auth_file,
                        cookie,
                    });
                }
                Err(e) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    last_err = e;
                }
            }
        }
        let _ = fs::remove_dir_all(&dir);
        Err(last_err)
    }

    /// Opens a connection to the display.
    pub fn connect(&self) -> Result<(RustConnection, usize), String> {
        // Xvfb accepts connections a moment before it has read the cookie,
        // so a refusal right after start is tried again.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match self.connect_once() {
                Ok(c) => return Ok(c),
                Err(e) if Instant::now() > deadline => return Err(e),
                Err(_) => std::thread::sleep(Duration::from_millis(50)),
            }
        }
    }

    fn connect_once(&self) -> Result<(RustConnection, usize), String> {
        let path = socket_path(self.display);
        let stream = UnixStream::connect(&path)
            .map_err(|e| format!("could not reach Xvfb at {}: {e}", path.display()))?;
        let (stream, _) =
            DefaultStream::from_unix_stream(stream).map_err(|e| format!("X connection: {e}"))?;
        let conn = RustConnection::connect_to_stream_with_auth_info(
            stream,
            0,
            b"MIT-MAGIC-COOKIE-1".to_vec(),
            self.cookie.to_vec(),
        )
        .map_err(|e| format!("X connection setup: {e}"))?;
        Ok((conn, 0))
    }

    /// Stops Xvfb and removes the cookie.
    pub fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn socket_path(display: u32) -> PathBuf {
    PathBuf::from(format!("/tmp/.X11-unix/X{display}"))
}

fn wait_for_socket(display: u32, child: &mut Child) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(15);
    let path = socket_path(display);
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            return Err(format!("Xvfb on :{display} stopped at start ({status})"));
        }
        if UnixStream::connect(&path).is_ok() {
            return Ok(());
        }
        if Instant::now() > deadline {
            return Err(format!("Xvfb on :{display} did not start within 15 s"));
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// A fresh directory only this user can enter, under `$XDG_RUNTIME_DIR` when
/// there is one, else `/tmp`.
fn private_dir() -> Result<PathBuf, String> {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .unwrap_or_else(std::env::temp_dir);
    let mut tag = [0u8; 8];
    getrandom::fill(&mut tag).map_err(|e| format!("no random numbers: {e}"))?;
    let name: String = tag.iter().map(|b| format!("{b:02x}")).collect();
    let dir = base.join(format!("ferostream-{name}"));
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&dir)
        .map_err(|e| format!("could not make {}: {e}", dir.display()))?;
    Ok(dir)
}

/// An Xauthority file with one entry that matches any display (family
/// "wild"), so both Xvfb (`-auth`) and programs (`XAUTHORITY`) can use it.
fn write_xauthority(path: &Path, cookie: &[u8; 16]) -> Result<(), String> {
    let mut entry = Vec::new();
    entry.extend_from_slice(&0xffffu16.to_be_bytes());
    for field in [&b""[..], &b""[..], &b"MIT-MAGIC-COOKIE-1"[..], &cookie[..]] {
        entry.extend_from_slice(&(field.len() as u16).to_be_bytes());
        entry.extend_from_slice(field);
    }
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| format!("could not write {}: {e}", path.display()))?;
    f.write_all(&entry)
        .map_err(|e| format!("could not write {}: {e}", path.display()))
}
