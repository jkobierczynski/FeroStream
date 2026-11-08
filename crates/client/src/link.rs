//! The running transport: a child process (ssh) whose stdout carries the
//! agent's frames and whose stdin carries ours. Reading and writing happen
//! on their own threads so a slow network never blocks the windows.
//!
//! Before the agent's preamble, the remote command may ask for the agent
//! binary ([`UPLOAD_MARKER`]); the link sends the file for the host's
//! architecture, untouched, and nothing else until the agent answers.

use std::io::{BufWriter, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use ferostream_proto::{FrameReader, Message, PREAMBLE};

use crate::transport::{AgentFile, UPLOAD_MARKER, is_arch};

/// Most of ssh's error output kept for the user, in bytes (the end of it).
const STDERR_KEEP: usize = 4096;

/// What the reader thread reports.
#[derive(Debug)]
pub enum Event {
    Message(Message),
    /// The stream ended. `error` is set when it ended badly (a protocol
    /// error); `stderr` is the end of ssh's error output.
    Closed {
        error: Option<String>,
        stderr: String,
    },
}

/// What the writer thread sends.
#[derive(Debug)]
enum Out {
    Message(Message),
    /// The agent binary, as the remote command asked.
    Raw(Vec<u8>),
}

/// Watches what comes before the agent's preamble for the remote command
/// asking for the agent.
#[derive(Debug, Default)]
pub struct UploadScan {
    seen: Vec<u8>,
    done: bool,
}

impl UploadScan {
    /// Takes the next bytes from the host; returns the architecture whose
    /// agent the host asks for, once.
    pub fn push(&mut self, data: &[u8]) -> Option<String> {
        if self.done {
            return None;
        }
        self.seen.extend_from_slice(data);
        let preamble = find(&self.seen, PREAMBLE);
        let marker = find(&self.seen, UPLOAD_MARKER);
        match (marker, preamble) {
            (Some(m), p) if p.is_none_or(|p| m < p) => {
                let rest = self.seen.get(m + UPLOAD_MARKER.len()..).unwrap_or_default();
                let end = rest.iter().position(|&b| b == b'\n')?;
                let arch =
                    String::from_utf8_lossy(rest.get(..end).unwrap_or_default()).into_owned();
                self.done = true;
                self.seen = Vec::new();
                is_arch(&arch).then_some(arch)
            }
            (_, Some(_)) => {
                self.done = true;
                self.seen = Vec::new();
                None
            }
            _ => {
                // Keep only what could start a marker or the preamble.
                let keep = UPLOAD_MARKER.len().max(PREAMBLE.len()) + 64;
                if self.seen.len() > ferostream_proto::MAX_PREAMBLE_SKIP + keep {
                    self.done = true;
                    self.seen = Vec::new();
                }
                None
            }
        }
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// A started transport.
#[derive(Debug)]
pub struct Link {
    tx: mpsc::Sender<Out>,
    child: Arc<Mutex<Child>>,
}

impl Link {
    /// Starts `cmd` with piped stdio. `notify` is called on the reader
    /// thread for every message and once at the end. `files` are the agents
    /// the remote command may ask for.
    pub fn start(
        mut cmd: Command,
        files: Vec<AgentFile>,
        notify: impl Fn(Event) + Send + 'static,
    ) -> std::io::Result<Link> {
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let (Some(stdin), Some(mut stdout), Some(mut stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            let _ = child.kill();
            return Err(std::io::Error::other("no pipes to the child process"));
        };

        let errors = Arc::new(Mutex::new(Vec::<u8>::new()));
        let err_thread = {
            let errors = Arc::clone(&errors);
            std::thread::spawn(move || {
                let mut buf = [0u8; 1024];
                while let Ok(n) = stderr.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    let mut e = errors.lock().unwrap_or_else(|p| p.into_inner());
                    e.extend_from_slice(buf.get(..n).unwrap_or_default());
                    if e.len() > STDERR_KEEP * 2 {
                        let cut = e.len() - STDERR_KEEP;
                        e.drain(..cut);
                    }
                }
            })
        };

        let (tx, rx) = mpsc::channel::<Out>();
        let upload_tx = tx.clone();
        std::thread::spawn(move || {
            let mut reader = FrameReader::after_preamble();
            let mut scan = UploadScan::default();
            let mut buf = vec![0u8; 256 * 1024];
            let error = 'read: loop {
                let n = match stdout.read(&mut buf) {
                    Ok(0) => break None,
                    Ok(n) => n,
                    Err(e) => break Some(e.to_string()),
                };
                let data = buf.get(..n).unwrap_or_default();
                if let Some(arch) = scan.push(data) {
                    // The remote command checks size and CRC again, so a
                    // file that changed meanwhile is refused there.
                    match files.iter().find(|f| f.arch == arch) {
                        Some(f) => match std::fs::read(&f.path) {
                            Ok(bytes) => {
                                let _ = upload_tx.send(Out::Raw(bytes));
                            }
                            Err(e) => {
                                break 'read Some(format!(
                                    "could not read {}: {e}",
                                    f.path.display()
                                ));
                            }
                        },
                        None => {
                            break 'read Some(format!("the host asked for an agent for {arch}"));
                        }
                    }
                }
                reader.push(data);
                loop {
                    match reader.next_message() {
                        Ok(Some(m)) => notify(Event::Message(m)),
                        Ok(None) => break,
                        Err(e) => break 'read Some(e.to_string()),
                    }
                }
            };
            let _ = err_thread.join();
            let e = errors.lock().unwrap_or_else(|p| p.into_inner());
            let tail = e
                .get(e.len().saturating_sub(STDERR_KEEP)..)
                .unwrap_or_default();
            notify(Event::Closed {
                error,
                stderr: String::from_utf8_lossy(tail).trim().to_string(),
            });
        });

        std::thread::spawn(move || {
            let mut w = BufWriter::new(stdin);
            let bytes = |o: Out| match o {
                Out::Message(m) => m.encode(),
                Out::Raw(b) => b,
            };
            while let Ok(o) = rx.recv() {
                if w.write_all(&bytes(o)).is_err() {
                    return;
                }
                // Send what has queued up meanwhile in the same write.
                while let Ok(o) = rx.try_recv() {
                    if w.write_all(&bytes(o)).is_err() {
                        return;
                    }
                }
                if w.flush().is_err() {
                    return;
                }
            }
        });

        Ok(Link {
            tx,
            child: Arc::new(Mutex::new(child)),
        })
    }

    /// Queues a message for the agent.
    pub fn send(&self, m: Message) {
        let _ = self.tx.send(Out::Message(m));
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        let mut c = self.child.lock().unwrap_or_else(|p| p.into_inner());
        let _ = c.kill();
        let _ = c.wait();
    }
}

#[cfg(all(test, unix))]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic
    )]

    use super::*;
    use std::time::Duration;

    #[test]
    fn frames_both_ways_and_stderr_on_close() {
        // A fake agent: the preamble, then our own frames echoed back.
        let mut cmd = Command::new("sh");
        cmd.args([
            "-c",
            "printf 'motd\\n\\000FEROSTREAM\\n'; echo oops >&2; head -c 9",
        ]);
        let (tx, rx) = mpsc::channel();
        let link = Link::start(cmd, Vec::new(), move |e| {
            let _ = tx.send(e);
        })
        .unwrap();
        link.send(Message::Ping { token: 42 });
        let first = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(
            matches!(first, Event::Message(Message::Ping { token: 42 })),
            "{first:?}"
        );
        match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
            Event::Closed { error, stderr } => {
                assert_eq!(error, None);
                assert_eq!(stderr, "oops");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn scan_finds_the_upload_request_once() {
        let mut s = UploadScan::default();
        assert_eq!(s.push(b"Welcome!\n\0FEROSTREAM-UP"), None);
        assert_eq!(s.push(b"LOAD x86_"), None);
        assert_eq!(s.push(b"64\nmore"), Some("x86_64".into()));
        assert_eq!(s.push(b"\0FEROSTREAM-UPLOAD aarch64\n"), None);
        // After the preamble nothing counts (it could be a program's output).
        let mut s = UploadScan::default();
        assert_eq!(s.push(b"\0FEROSTREAM\n\0FEROSTREAM-UPLOAD x86_64\n"), None);
        // A name that is no architecture is not followed.
        let mut s = UploadScan::default();
        assert_eq!(s.push(b"\0FEROSTREAM-UPLOAD ../x\n"), None);
    }

    #[test]
    fn sends_the_agent_when_asked() {
        let dir = std::env::temp_dir().join(format!("ferostream-link-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("ferostream-agent");
        std::fs::write(&path, b"AGENT-BYTES").unwrap();
        let file = AgentFile {
            arch: "x86_64".into(),
            path,
            crc: 0,
            size: 11,
        };
        // A fake remote command: asks, reads 11 bytes, then (as the agent)
        // greets with a Pong, echoes one frame and reports what it got.
        let mut cmd = Command::new("sh");
        cmd.args([
            "-c",
            "printf '\\000FEROSTREAM-UPLOAD x86_64\\n'; head -c 11 > got; \
             printf '\\000FEROSTREAM\\n\\005\\000\\000\\000\\003\\001\\000\\000\\000'; \
             head -c 9; cat got >&2",
        ])
        .current_dir(&dir);
        let (tx, rx) = mpsc::channel();
        let link = Link::start(cmd, vec![file], move |e| {
            let _ = tx.send(e);
        })
        .unwrap();
        // Like the client: nothing is sent before the agent speaks.
        let first = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(
            matches!(first, Event::Message(Message::Pong { token: 1 })),
            "{first:?}"
        );
        link.send(Message::Ping { token: 5 });
        let echo = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(
            matches!(echo, Event::Message(Message::Ping { token: 5 })),
            "{echo:?}"
        );
        match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
            Event::Closed { error, stderr } => {
                assert_eq!(error, None);
                assert_eq!(stderr, "AGENT-BYTES");
            }
            other => panic!("{other:?}"),
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
