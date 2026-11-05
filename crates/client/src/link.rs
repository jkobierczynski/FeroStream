//! The running transport: a child process (ssh) whose stdout carries the
//! agent's frames and whose stdin carries ours. Reading and writing happen
//! on their own threads so a slow network never blocks the windows.

use std::io::{BufWriter, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use ferostream_proto::{FrameReader, Message};

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

/// A started transport.
#[derive(Debug)]
pub struct Link {
    tx: mpsc::Sender<Message>,
    child: Arc<Mutex<Child>>,
}

impl Link {
    /// Starts `cmd` with piped stdio. `notify` is called on the reader
    /// thread for every message and once at the end.
    pub fn start(
        mut cmd: Command,
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

        std::thread::spawn(move || {
            let mut reader = FrameReader::after_preamble();
            let mut buf = vec![0u8; 256 * 1024];
            let error = 'read: loop {
                let n = match stdout.read(&mut buf) {
                    Ok(0) => break None,
                    Ok(n) => n,
                    Err(e) => break Some(e.to_string()),
                };
                reader.push(buf.get(..n).unwrap_or_default());
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

        let (tx, rx) = mpsc::channel::<Message>();
        std::thread::spawn(move || {
            let mut w = BufWriter::new(stdin);
            while let Ok(m) = rx.recv() {
                if w.write_all(&m.encode()).is_err() {
                    return;
                }
                // Send what has queued up meanwhile in the same write.
                while let Ok(m) = rx.try_recv() {
                    if w.write_all(&m.encode()).is_err() {
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
        let _ = self.tx.send(m);
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
        let link = Link::start(cmd, move |e| {
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
}
