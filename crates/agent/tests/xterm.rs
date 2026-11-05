//! The agent against a real Xvfb and xterm: a window appears, its pixels
//! arrive, typed text reaches the program, and closing ends the session.
//! Skipped when Xvfb or xterm is missing, unless FEROSTREAM_REQUIRE_X is set
//! (CI sets it).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use ferostream_proto::{FrameReader, Message, VERSION, WindowKind};

fn have(program: &str) -> bool {
    Command::new("sh")
        .args(["-c", &format!("command -v {program}")])
        .stdout(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

struct Agent {
    child: std::process::Child,
    rx: mpsc::Receiver<Message>,
}

impl Agent {
    fn start(argv: &[&str]) -> Agent {
        Self::start_at(argv, 96)
    }

    fn start_at(argv: &[&str], dpi: u16) -> Agent {
        let mut child = Command::new(env!("CARGO_BIN_EXE_ferostream-agent"))
            .arg("--stdio")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut out = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut r = FrameReader::after_preamble();
            let mut buf = [0u8; 65536];
            loop {
                let n = match out.read(&mut buf) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => n,
                };
                r.push(&buf[..n]);
                while let Some(m) = r.next_message().unwrap() {
                    if tx.send(m).is_err() {
                        return;
                    }
                }
            }
        });
        let mut a = Agent { child, rx };
        a.send(Message::Hello {
            version: VERSION,
            software: "test".into(),
        });
        a.send(Message::Start {
            width: 1024,
            height: 768,
            dpi,
            argv: argv.iter().map(|s| s.to_string()).collect(),
        });
        a
    }

    fn send(&mut self, m: Message) {
        let stdin = self.child.stdin.as_mut().unwrap();
        stdin.write_all(&m.encode()).unwrap();
        stdin.flush().unwrap();
    }

    fn next(&self) -> Message {
        self.rx
            .recv_timeout(Duration::from_secs(20))
            .expect("the agent went quiet")
    }
}

#[test]
fn xterm_window_pixels_typing_and_exit() {
    if !(have("Xvfb") && have("xterm")) {
        assert!(
            std::env::var_os("FEROSTREAM_REQUIRE_X").is_none(),
            "Xvfb and xterm are required"
        );
        eprintln!("skipped: Xvfb or xterm missing");
        return;
    }
    let mut a = Agent::start(&["xterm", "-geometry", "40x10", "-T", "ferostream-test"]);
    assert!(matches!(
        a.next(),
        Message::Hello {
            version: VERSION,
            ..
        }
    ));

    let (window, w, h) = loop {
        match a.next() {
            Message::WindowNew {
                window,
                width,
                height,
                kind: WindowKind::Normal,
                title,
                ..
            } => {
                assert_eq!(title, "ferostream-test");
                break (window, width, height);
            }
            Message::Error { message } => panic!("agent error: {message}"),
            _ => {}
        }
    };
    assert!(w > 100 && h > 50, "xterm is {w}x{h}");

    // The first frame covers the whole window and decodes.
    let mut covered = 0usize;
    let mut colours = std::collections::HashSet::new();
    loop {
        match a.next() {
            Message::Tile(t) if t.window == window => {
                let px = t.decode().unwrap();
                assert_eq!(px.len(), t.pixel_bytes());
                for p in px.chunks(4) {
                    colours.insert([p[0], p[1], p[2]]);
                }
                covered += usize::from(t.width) * usize::from(t.height);
            }
            Message::FrameEnd { window: fw, seq } if fw == window => {
                a.send(Message::FrameAck { window, seq });
                break;
            }
            _ => {}
        }
    }
    assert!(covered >= usize::from(w) * usize::from(h));
    // Black text on white: a blank or garbled picture has one colour.
    assert!(
        colours.len() >= 2,
        "the window has {} colours",
        colours.len()
    );

    // Typing reaches the shell: the window changes, then `exit` ends it.
    a.send(Message::Focus { window });
    a.send(Message::PointerMove {
        window,
        x: 20,
        y: 20,
    });
    for ch in "exit\r".chars() {
        a.send(Message::Text { ch });
    }
    let mut gone = false;
    let code = loop {
        match a.next() {
            Message::FrameEnd { window: fw, seq } => a.send(Message::FrameAck { window: fw, seq }),
            Message::WindowGone { window: g } if g == window => gone = true,
            Message::Exited { code } => break code,
            Message::Error { message } => panic!("agent error: {message}"),
            _ => {}
        }
    };
    assert!(gone);
    assert_eq!(code, 0);
    let status = a.child.wait().unwrap();
    assert!(status.success());
}

#[test]
fn missing_program_is_reported() {
    if !have("Xvfb") {
        return;
    }
    let a = Agent::start(&["ferostream-no-such-program"]);
    assert!(matches!(a.next(), Message::Hello { .. }));
    match a.next() {
        Message::Error { message } => assert!(message.contains("could not start"), "{message}"),
        other => panic!("expected an error, got {other:?}"),
    }
}

/// The first normal window's size, then the session is dropped.
fn first_window(argv: &[&str], dpi: u16) -> (u16, u16) {
    let mut a = Agent::start_at(argv, dpi);
    let size = loop {
        match a.next() {
            Message::WindowNew {
                width,
                height,
                kind: WindowKind::Normal,
                ..
            } => break (width, height),
            Message::Error { message } => panic!("agent error: {message}"),
            _ => {}
        }
    };
    // Ending input is how a session ends (the SSH connection closing); the
    // agent then stops the program and Xvfb.
    drop(a.child.stdin.take());
    let _ = a.child.wait();
    size
}

#[test]
fn text_grows_with_the_client_dpi() {
    if !(have("Xvfb") && have("xterm")) {
        assert!(std::env::var_os("FEROSTREAM_REQUIRE_X").is_none());
        return;
    }
    let xterm = ["xterm", "-geometry", "40x10"];
    let (w1, h1) = first_window(&xterm, 96);
    let (w2, h2) = first_window(&xterm, 192);
    // Same 40x10 characters, twice the DPI: about twice the pixels.
    assert!(
        u32::from(w2) * 10 >= u32::from(w1) * 17 && u32::from(h2) * 10 >= u32::from(h1) * 17,
        "96 dpi: {w1}x{h1}, 192 dpi: {w2}x{h2}"
    );
    // GTK gets whole-number scaling at 200 %.
    let check = "test \"$GDK_SCALE\" = 2 && test \"$GDK_DPI_SCALE\" = 0.5000 && exec xterm";
    let (w, _) = first_window(&["sh", "-c", check], 192);
    assert!(w > 1);
}
