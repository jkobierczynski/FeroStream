//! The agent against a real Xvfb and xterm: a window appears, its pixels
//! arrive, typed text reaches the program, and closing ends the session.
//! Skipped when Xvfb or xterm is missing, unless FEROSTREAM_REQUIRE_X is set
//! (CI sets it).

#![cfg(unix)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use ferostream_proto::{FrameReader, Message, Monitor, VERSION, WindowKind, WindowState};

/// Set for programs the agent starts in these tests: it lets the
/// [`helper_window`] test run when this test binary is that program.
const HELPER: &str = "FEROSTREAM_TEST_HELPER";

fn have(program: &str) -> bool {
    Command::new("sh")
        .args(["-c", &format!("command -v {program}")])
        .stdout(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// A runtime directory of the test's own, so its sessions are its own;
/// sessions still running when it is dropped are stopped.
struct Runtime(PathBuf);

impl Runtime {
    fn new() -> Arc<Runtime> {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "ferostream-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        Arc::new(Runtime(dir))
    }

    fn agent(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_ferostream-agent"));
        c.env("FEROSTREAM_RUNTIME_DIR", &self.0);
        c
    }

    fn sessions(&self) -> Vec<String> {
        std::fs::read_dir(&self.0)
            .map(|d| {
                d.flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn session_dir(&self, id: &str) -> PathBuf {
        self.0.join(id)
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        for id in self.sessions() {
            let _ = self.agent().args(["--stop", &id]).status();
        }
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Agent {
    child: std::process::Child,
    rx: mpsc::Receiver<Message>,
    runtime: Arc<Runtime>,
}

impl Agent {
    fn start(argv: &[&str]) -> Agent {
        Self::start_at(argv, 96)
    }

    fn start_at(argv: &[&str], dpi: u16) -> Agent {
        let mut a = Self::connect(Runtime::new(), &[]);
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

    /// Attaches to a session in `runtime`: `id`, or the newest detached.
    fn attach(runtime: &Arc<Runtime>, id: Option<&str>) -> Agent {
        let mut extra = vec!["--attach"];
        extra.extend(id);
        let mut a = Self::connect(Arc::clone(runtime), &extra);
        // A relay that found nothing to attach to has already answered and
        // gone: these may not get through.
        let stdin = a.child.stdin.as_mut().unwrap();
        for m in [
            Message::Hello {
                version: VERSION,
                software: "test".into(),
            },
            Message::Attach,
        ] {
            let _ = stdin.write_all(&m.encode()).and_then(|()| stdin.flush());
        }
        a
    }

    fn connect(runtime: Arc<Runtime>, extra: &[&str]) -> Agent {
        let mut child = runtime
            .agent()
            .arg("--stdio")
            .args(extra)
            .env(HELPER, "1")
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
        Agent { child, rx, runtime }
    }

    /// Messages until `f` picks one out.
    fn until<T>(&self, mut f: impl FnMut(Message) -> Option<T>) -> T {
        loop {
            if let Some(t) = f(self.next()) {
                return t;
            }
        }
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

#[test]
fn pointer_shape_follows_the_window_under_it() {
    if !(have("Xvfb") && have("xterm")) {
        assert!(std::env::var_os("FEROSTREAM_REQUIRE_X").is_none());
        return;
    }
    let mut a = Agent::start(&["xterm", "-geometry", "40x10"]);
    let window = loop {
        match a.next() {
            Message::WindowNew {
                window,
                kind: WindowKind::Normal,
                ..
            } => break window,
            Message::Error { message } => panic!("agent error: {message}"),
            _ => {}
        }
    };
    a.send(Message::PointerMove {
        window,
        x: 30,
        y: 30,
    });
    // xterm shows a text (I-beam) pointer: a picture with both see-through
    // and solid pixels, its hot spot inside it.
    let (hot_x, hot_y, image) = loop {
        match a.next() {
            Message::Cursor {
                hot_x,
                hot_y,
                image,
            } => break (hot_x, hot_y, image),
            Message::FrameEnd { window: fw, seq } => a.send(Message::FrameAck { window: fw, seq }),
            Message::Error { message } => panic!("agent error: {message}"),
            _ => {}
        }
    };
    let px = image.decode().unwrap();
    assert!(hot_x < image.width && hot_y < image.height);
    assert!(px.chunks(4).any(|p| p[3] == 0));
    assert!(px.chunks(4).any(|p| p[3] == 255));
    drop(a.child.stdin.take());
    let _ = a.child.wait();
}

/// Not a test of its own: the program [`window_manager_requests`] runs on
/// the agent's display. It shows a dialog with size limits and icons, then
/// asks the "window manager" for full screen, as a program would.
#[test]
#[ignore = "runs inside the agent, started by window_manager_requests"]
fn helper_window() {
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{self, ConnectionExt as _};
    use x11rb::wrapper::ConnectionExt as _;

    if std::env::var_os(HELPER).is_none() {
        return;
    }
    let (conn, screen) = x11rb::connect(None).unwrap();
    let root = conn.setup().roots[screen].root;
    let atom = |name: &str| {
        conn.intern_atom(false, name.as_bytes())
            .unwrap()
            .reply()
            .unwrap()
            .atom
    };
    let win = conn.generate_id().unwrap();
    conn.create_window(
        0,
        win,
        root,
        50,
        60,
        200,
        100,
        0,
        xproto::WindowClass::INPUT_OUTPUT,
        0,
        &xproto::CreateWindowAux::new().background_pixel(0x0033_6699),
    )
    .unwrap();
    conn.change_property32(
        xproto::PropMode::REPLACE,
        win,
        atom("_NET_WM_WINDOW_TYPE"),
        xproto::AtomEnum::ATOM,
        &[atom("_NET_WM_WINDOW_TYPE_DIALOG")],
    )
    .unwrap();
    let mut hints = [0u32; 18];
    hints[0] = 16 | 32;
    hints[5] = 150;
    hints[6] = 80;
    hints[7] = 400;
    hints[8] = 300;
    conn.change_property32(
        xproto::PropMode::REPLACE,
        win,
        xproto::AtomEnum::WM_NORMAL_HINTS,
        xproto::AtomEnum::WM_SIZE_HINTS,
        &hints,
    )
    .unwrap();
    let mut icon = vec![16, 16];
    icon.extend(std::iter::repeat_n(0xffff_0000u32, 256));
    icon.extend([32, 32]);
    icon.extend(std::iter::repeat_n(0x8000_ff00u32, 1024));
    conn.change_property32(
        xproto::PropMode::REPLACE,
        win,
        atom("_NET_WM_ICON"),
        xproto::AtomEnum::CARDINAL,
        &icon,
    )
    .unwrap();
    conn.map_window(win).unwrap();
    conn.sync().unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let ev = xproto::ClientMessageEvent::new(
        32,
        win,
        atom("_NET_WM_STATE"),
        [1, atom("_NET_WM_STATE_FULLSCREEN"), 0, 1, 0],
    );
    conn.send_event(
        false,
        root,
        xproto::EventMask::SUBSTRUCTURE_REDIRECT | xproto::EventMask::SUBSTRUCTURE_NOTIFY,
        ev,
    )
    .unwrap();
    conn.flush().unwrap();
    // Stay until the display goes away.
    while conn.wait_for_event().is_ok() {}
}

#[test]
fn window_manager_requests() {
    if !have("Xvfb") {
        assert!(std::env::var_os("FEROSTREAM_REQUIRE_X").is_none());
        return;
    }
    let me = std::env::current_exe().unwrap();
    let me = me.to_str().unwrap();
    let mut a = Agent::start_at(
        &[me, "--exact", "helper_window", "--ignored", "--nocapture"],
        192,
    );
    let mut window = 0;
    let mut hints = None;
    let mut icons = Vec::new();
    let mut full = false;
    while !(full && hints.is_some() && icons.len() == 2) {
        match a.next() {
            Message::WindowNew {
                window: w, kind, ..
            } => {
                assert_eq!(kind, WindowKind::Dialog);
                window = w;
            }
            Message::WindowHints {
                window: w,
                min_width,
                min_height,
                max_width,
                max_height,
            } if w == window => hints = Some((min_width, min_height, max_width, max_height)),
            Message::WindowIcon {
                window: w,
                big,
                image,
            } if w == window => icons.push((big, image.width, image.decode().unwrap())),
            Message::WindowState {
                window: w,
                state: WindowState::Fullscreen,
            } if w == window => full = true,
            Message::FrameEnd { window: fw, seq } => a.send(Message::FrameAck { window: fw, seq }),
            Message::Error { message } => panic!("agent error: {message}"),
            _ => {}
        }
    }
    assert_eq!(hints, Some((150, 80, 400, 300)));
    // At 200 %: a 64-pixel taskbar icon wanted, the 32 one is the biggest;
    // a 32-pixel title-bar icon wanted, the 32 one fits exactly.
    for (_, width, px) in &icons {
        assert_eq!(*width, 32);
        assert_eq!(&px[..4], &[0, 0xff, 0, 0x80]);
    }
    drop(a.child.stdin.take());
    let _ = a.child.wait();
}

/// Not a test of its own: the program [`clipboard_monitors_and_activation`]
/// runs on the agent's display. It pastes until it gets the text copied
/// "on Windows", reports the monitors it sees in its title, asks to be
/// brought to the front, and then selects text (PRIMARY) and copies a
/// large text of its own (CLIPBOARD, handed over in INCR pieces). After
/// that it selects new text every so often.
#[test]
#[ignore = "runs inside the agent, started by clipboard_monitors_and_activation"]
fn helper_clipboard() {
    use x11rb::connection::Connection;
    use x11rb::protocol::Event;
    use x11rb::protocol::randr::ConnectionExt as _;
    use x11rb::protocol::xproto::{self, ConnectionExt as _};
    use x11rb::wrapper::ConnectionExt as _;

    if std::env::var_os(HELPER).is_none() {
        return;
    }
    let (conn, screen) = x11rb::connect(None).unwrap();
    let root = conn.setup().roots[screen].root;
    let atom = |name: &str| {
        conn.intern_atom(false, name.as_bytes())
            .unwrap()
            .reply()
            .unwrap()
            .atom
    };
    let (clipboard, utf8, prop) = (atom("CLIPBOARD"), atom("UTF8_STRING"), atom("HELPER_PASTE"));
    let win = conn.generate_id().unwrap();
    conn.create_window(
        0,
        win,
        root,
        10,
        10,
        120,
        80,
        0,
        xproto::WindowClass::INPUT_OUTPUT,
        0,
        &xproto::CreateWindowAux::new()
            .background_pixel(0x0066_9933)
            .event_mask(xproto::EventMask::PROPERTY_CHANGE),
    )
    .unwrap();
    conn.map_window(win).unwrap();
    conn.flush().unwrap();

    // Paste until the agent offers the text.
    let pasted = loop {
        conn.convert_selection(win, clipboard, utf8, prop, x11rb::CURRENT_TIME)
            .unwrap();
        conn.flush().unwrap();
        let got = loop {
            match conn.wait_for_event().unwrap() {
                Event::SelectionNotify(e) if e.property == x11rb::NONE => break None,
                Event::SelectionNotify(_) => {
                    let r = conn
                        .get_property(true, win, prop, xproto::AtomEnum::ANY, 0, 4096)
                        .unwrap()
                        .reply()
                        .unwrap();
                    break Some(String::from_utf8(r.value).unwrap());
                }
                _ => {}
            }
        };
        if let Some(t) = got.filter(|t| t.starts_with("from windows")) {
            break t;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let monitors = conn
        .randr_get_monitors(root, true)
        .unwrap()
        .reply()
        .map_or(0, |r| r.monitors.len());
    conn.change_property8(
        xproto::PropMode::REPLACE,
        win,
        xproto::AtomEnum::WM_NAME,
        xproto::AtomEnum::STRING,
        format!("pasted {} monitors={monitors}", pasted.len()).as_bytes(),
    )
    .unwrap();
    let ev = xproto::ClientMessageEvent::new(32, win, atom("_NET_ACTIVE_WINDOW"), [1, 0, 0, 0, 0]);
    conn.send_event(
        false,
        root,
        xproto::EventMask::SUBSTRUCTURE_REDIRECT | xproto::EventMask::SUBSTRUCTURE_NOTIFY,
        ev,
    )
    .unwrap();

    // Select, then copy, and hand the text to whoever asks: the copied
    // text in 64 KiB pieces.
    let primary = xproto::Atom::from(xproto::AtomEnum::PRIMARY);
    let incr = atom("INCR");
    let mut selected = String::from("selected 0");
    let copied = helper_copied_text();
    conn.set_selection_owner(win, primary, x11rb::CURRENT_TIME)
        .unwrap();
    conn.set_selection_owner(win, clipboard, x11rb::CURRENT_TIME)
        .unwrap();
    conn.flush().unwrap();
    // The piece transfer under way: requestor, property, bytes sent.
    let mut sending: Option<(u32, u32, usize)> = None;
    let mut copy_done = false;
    let mut n = 0;
    loop {
        let Some(ev) = conn.poll_for_event().unwrap() else {
            std::thread::sleep(Duration::from_millis(20));
            n += 1;
            if copy_done && n % 15 == 0 {
                selected = format!("selected {}", n / 15);
                conn.set_selection_owner(win, primary, x11rb::CURRENT_TIME)
                    .unwrap();
                conn.flush().unwrap();
            }
            continue;
        };
        if let Event::PropertyNotify(e) = &ev {
            if let Some((requestor, property, at)) = sending
                && e.window == requestor
                && e.atom == property
                && e.state == xproto::Property::DELETE
            {
                let end = (at + 64 * 1024).min(copied.len());
                conn.change_property8(
                    xproto::PropMode::REPLACE,
                    requestor,
                    property,
                    utf8,
                    &copied.as_bytes()[at..end],
                )
                .unwrap();
                conn.flush().unwrap();
                if at == end {
                    sending = None;
                    copy_done = true;
                } else {
                    sending = Some((requestor, property, end));
                }
            }
            continue;
        }
        if let Event::SelectionRequest(e) = ev {
            let ok = e.target == utf8;
            if ok && e.selection == clipboard {
                // Changes to the requestor's property drive the pieces.
                conn.change_window_attributes(
                    e.requestor,
                    &xproto::ChangeWindowAttributesAux::new()
                        .event_mask(xproto::EventMask::PROPERTY_CHANGE),
                )
                .unwrap();
                conn.change_property32(
                    xproto::PropMode::REPLACE,
                    e.requestor,
                    e.property,
                    incr,
                    &[copied.len() as u32],
                )
                .unwrap();
                sending = Some((e.requestor, e.property, 0));
            } else if ok {
                conn.change_property8(
                    xproto::PropMode::REPLACE,
                    e.requestor,
                    e.property,
                    utf8,
                    selected.as_bytes(),
                )
                .unwrap();
            }
            let notify = xproto::SelectionNotifyEvent {
                response_type: xproto::SELECTION_NOTIFY_EVENT,
                sequence: 0,
                time: e.time,
                requestor: e.requestor,
                selection: e.selection,
                target: e.target,
                property: if ok { e.property } else { x11rb::NONE },
            };
            conn.send_event(false, e.requestor, xproto::EventMask::NO_EVENT, notify)
                .unwrap();
            conn.flush().unwrap();
        }
    }
}

/// What [`helper_clipboard`] copies: more than one property can hold at
/// once, so it goes over in pieces.
fn helper_copied_text() -> String {
    let mut t = String::from("from linux ✓ ");
    while t.len() < 300 * 1024 {
        t.push_str("0123456789abcdef");
    }
    t
}

#[test]
fn clipboard_monitors_and_activation() {
    if !have("Xvfb") {
        assert!(std::env::var_os("FEROSTREAM_REQUIRE_X").is_none());
        return;
    }
    let me = std::env::current_exe().unwrap();
    let me = me.to_str().unwrap();
    let mut a = Agent::start(&[
        me,
        "--exact",
        "helper_clipboard",
        "--ignored",
        "--nocapture",
    ]);
    let window = loop {
        match a.next() {
            Message::WindowNew { window, .. } => break window,
            Message::Error { message } => panic!("agent error: {message}"),
            _ => {}
        }
    };
    a.send(Message::Monitors {
        monitors: vec![
            Monitor {
                x: 0,
                y: 0,
                width: 512,
                height: 768,
            },
            Monitor {
                x: 512,
                y: 0,
                width: 512,
                height: 768,
            },
        ],
    });
    // Copies only, not selections, at first.
    a.send(Message::ClipboardRules {
        send: true,
        selection: false,
    });
    let copied = "from windows — ✓";
    a.send(Message::Clipboard {
        text: copied.into(),
    });
    let (mut title, mut activated, mut clip) = (None, false, None);
    while title.is_none() || !activated || clip.is_none() {
        match a.next() {
            Message::WindowTitle {
                window: w,
                title: t,
            } if w == window => title = Some(t),
            Message::WindowActivate { window: w } if w == window => activated = true,
            Message::Clipboard { text } => clip = Some(text),
            Message::FrameEnd { window: fw, seq } => a.send(Message::FrameAck { window: fw, seq }),
            Message::Error { message } => panic!("agent error: {message}"),
            _ => {}
        }
    }
    assert_eq!(
        title.unwrap(),
        format!("pasted {} monitors=2", copied.len())
    );
    // The selection made before the copy was not passed on: the large
    // copied text was the first to come.
    assert_eq!(clip.unwrap(), helper_copied_text());

    // Selections too, now.
    a.send(Message::ClipboardRules {
        send: true,
        selection: true,
    });
    a.until(|m| match m {
        Message::Clipboard { text } if text.starts_with("selected ") => Some(()),
        _ => None,
    });
    drop(a.child.stdin.take());
    let _ = a.child.wait();
}

/// Not a test of its own: the program [`menus_close_when_focus_leaves`]
/// runs. It opens a "menu" (an override-redirect window holding the
/// pointer, as menus do) and closes it on a click outside it.
#[test]
#[ignore = "runs inside the agent, started by menus_close_when_focus_leaves"]
fn helper_menu() {
    use x11rb::connection::Connection;
    use x11rb::protocol::Event;
    use x11rb::protocol::xproto::{self, ConnectionExt as _};

    if std::env::var_os(HELPER).is_none() {
        return;
    }
    let (conn, screen) = x11rb::connect(None).unwrap();
    let root = conn.setup().roots[screen].root;
    let make = |x, y, w, h, popup: bool| {
        let id = conn.generate_id().unwrap();
        conn.create_window(
            0,
            id,
            root,
            x,
            y,
            w,
            h,
            0,
            xproto::WindowClass::INPUT_OUTPUT,
            0,
            &xproto::CreateWindowAux::new()
                .background_pixel(0x00cc_cccc)
                .override_redirect(u32::from(popup)),
        )
        .unwrap();
        conn.map_window(id).unwrap();
        id
    };
    let _main = make(100, 100, 300, 200, false);
    let menu = make(120, 130, 100, 80, true);
    conn.flush().unwrap();
    // Grab once the menu is viewable.
    loop {
        let r = conn
            .grab_pointer(
                false,
                menu,
                xproto::EventMask::BUTTON_PRESS,
                xproto::GrabMode::ASYNC,
                xproto::GrabMode::ASYNC,
                x11rb::NONE,
                x11rb::NONE,
                x11rb::CURRENT_TIME,
            )
            .unwrap()
            .reply()
            .unwrap();
        if r.status == xproto::GrabStatus::SUCCESS {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    while let Ok(ev) = conn.wait_for_event() {
        if let Event::ButtonPress(e) = ev {
            let inside =
                e.event == menu && (0..100).contains(&e.event_x) && (0..80).contains(&e.event_y);
            if !inside {
                conn.unmap_window(menu).unwrap();
                conn.ungrab_pointer(x11rb::CURRENT_TIME).unwrap();
                conn.flush().unwrap();
            }
        }
    }
}

#[test]
fn menus_close_when_focus_leaves() {
    if !have("Xvfb") {
        assert!(std::env::var_os("FEROSTREAM_REQUIRE_X").is_none());
        return;
    }
    let me = std::env::current_exe().unwrap();
    let me = me.to_str().unwrap();
    let mut a = Agent::start(&[me, "--exact", "helper_menu", "--ignored", "--nocapture"]);
    let (mut main, mut menu) = (None, None);
    while main.is_none() || menu.is_none() {
        match a.next() {
            Message::WindowNew {
                window,
                kind: WindowKind::Popup,
                ..
            } => menu = Some(window),
            Message::WindowNew { window, .. } => main = Some(window),
            Message::Error { message } => panic!("agent error: {message}"),
            _ => {}
        }
    }
    // Give the helper a moment to take the pointer, then switch away.
    std::thread::sleep(Duration::from_millis(300));
    a.send(Message::Focus {
        window: main.unwrap(),
    });
    a.send(Message::Focus { window: 0 });
    loop {
        match a.next() {
            Message::WindowGone { window } if Some(window) == menu => break,
            Message::WindowGone { window } if Some(window) == main => {
                panic!("the main window went away instead of the menu")
            }
            Message::FrameEnd { window: fw, seq } => a.send(Message::FrameAck { window: fw, seq }),
            Message::Error { message } => panic!("agent error: {message}"),
            _ => {}
        }
    }
    drop(a.child.stdin.take());
    let _ = a.child.wait();
}

fn need_x() -> bool {
    if have("Xvfb") && have("xterm") {
        return true;
    }
    assert!(
        std::env::var_os("FEROSTREAM_REQUIRE_X").is_none(),
        "Xvfb and xterm are required"
    );
    eprintln!("skipped: Xvfb or xterm missing");
    false
}

fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A full picture of `window`: tiles up to its FrameEnd, acknowledged.
fn picture(a: &mut Agent, window: u32) -> usize {
    let mut tiles = 0;
    let seq = a.until(|m| match m {
        Message::Tile(t) if t.window == window => {
            tiles += 1;
            None
        }
        Message::FrameEnd { window: w, seq } if w == window => Some(seq),
        _ => None,
    });
    a.send(Message::FrameAck { window, seq });
    tiles
}

/// Whether process `pid` runs with no new privileges and a seccomp filter.
fn assert_locked(pid: &str, locked: bool) {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
    let field = |name: &str| {
        status
            .lines()
            .find_map(|l| l.strip_prefix(name))
            .map(str::trim)
            .unwrap_or_default()
            .to_string()
    };
    let (nnp, seccomp) = if locked { ("1", "2") } else { ("0", "0") };
    assert_eq!(field("NoNewPrivs:"), nnp, "process {pid}: {status}");
    assert_eq!(field("Seccomp:"), seccomp, "process {pid}: {status}");
}

#[test]
fn check_sandbox_refuses_programs_and_the_network() {
    let runtime = Runtime::new();
    let out = runtime.agent().arg("--check-sandbox").output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{text}");
    assert!(text.contains("start a program: refused"), "{text}");
    assert!(text.contains("open a network socket: refused"), "{text}");
    assert!(
        text.contains("write in the session directory: works"),
        "{text}"
    );
}

fn session_list(runtime: &Runtime) -> String {
    let out = runtime.agent().arg("--list").output().unwrap();
    assert!(out.status.success());
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn session_survives_a_dropped_connection() {
    if !need_x() {
        return;
    }
    let mut a = Agent::start(&["xterm", "-geometry", "40x10", "-T", "come-back"]);
    assert!(matches!(a.next(), Message::Hello { .. }));
    let id = match a.next() {
        Message::Session { id, windows } => {
            assert!(windows.is_empty(), "{windows:?}");
            id
        }
        other => panic!("expected Session, got {other:?}"),
    };
    let window = a.until(|m| match m {
        Message::WindowNew { window, .. } => Some(window),
        _ => None,
    });
    assert!(picture(&mut a, window) > 0);
    let runtime = Arc::clone(&a.runtime);
    let dir = runtime.session_dir(&id);
    assert!(dir.join("sock").exists());
    assert!(session_list(&runtime).contains("attached"));

    // The session process and the relay are locked down; the program the
    // user asked for is not.
    let pid = std::fs::read_to_string(dir.join("pid")).unwrap();
    let pid = pid.trim();
    assert_locked(pid, true);
    assert_locked(&a.child.id().to_string(), true);
    let program = std::fs::read_dir("/proc")
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .find(|p| {
            std::fs::read_to_string(format!("/proc/{p}/status")).is_ok_and(|s| {
                s.lines()
                    .any(|l| l.starts_with("Name:") && l.contains("xterm"))
                    && s.lines().any(|l| l.split_whitespace().eq(["PPid:", pid]))
            })
        })
        .expect("the session's xterm");
    assert_locked(&program, false);
    let log = std::fs::read_to_string(dir.join("log")).unwrap();
    assert!(log.contains("sandbox: seccomp: on"), "{log}");

    // The connection drops (ssh killed): the session stays, detached.
    a.child.kill().unwrap();
    let _ = a.child.wait();
    wait_for("the session to notice", || dir.join("detached").exists());
    let listed = session_list(&runtime);
    assert!(
        listed.contains(&id) && listed.contains("detached"),
        "{listed}"
    );
    assert!(listed.contains("xterm -geometry 40x10"), "{listed}");

    // Attach by id: the same window comes back, with its title and pixels.
    let mut b = Agent::attach(&runtime, Some(&id));
    assert!(matches!(b.next(), Message::Hello { .. }));
    match b.next() {
        Message::Session { id: got, windows } => {
            assert_eq!(got, id);
            assert_eq!(windows, vec![window]);
        }
        other => panic!("expected Session, got {other:?}"),
    }
    match b.next() {
        Message::WindowNew {
            window: w, title, ..
        } => {
            assert_eq!(w, window);
            // The shell has set its own title by now.
            assert!(!title.is_empty());
        }
        other => panic!("expected the window again, got {other:?}"),
    }
    assert!(picture(&mut b, window) > 0);
    assert!(!dir.join("detached").exists());

    // Typing still reaches the program after the reattach.
    b.send(Message::Focus { window });
    for ch in "exit\r".chars() {
        b.send(Message::Text { ch });
    }

    // No session is detached now (b is attached), so attaching to the
    // newest detached one finds none...
    let none = Agent::attach(&runtime, None);
    let message = none.until(|m| match m {
        Message::Error { message } => Some(message),
        _ => None,
    });
    assert!(message.contains("no FeroStream session"), "{message}");
    // ...and an id that never existed is refused.
    let wrong = Agent::attach(&runtime, Some("000000000000"));
    let message = wrong.until(|m| match m {
        Message::Error { message } => Some(message),
        _ => None,
    });
    assert!(
        message.contains("no FeroStream session 000000000000"),
        "{message}"
    );

    // The shell got "exit": xterm ends, and with it the session.
    let code = b.until(|m| match m {
        Message::Exited { code } => Some(code),
        _ => None,
    });
    assert_eq!(code, 0);
    wait_for("the session directory to go", || !dir.exists());
}

#[test]
fn a_new_client_takes_over_from_the_old_one() {
    if !need_x() {
        return;
    }
    let a = Agent::start(&["xterm", "-geometry", "40x10"]);
    let id = a.until(|m| match m {
        Message::Session { id, .. } => Some(id),
        _ => None,
    });
    let window = a.until(|m| match m {
        Message::WindowNew { window, .. } => Some(window),
        _ => None,
    });
    let runtime = Arc::clone(&a.runtime);
    // The old link is still up (a silent network drop looks like this): the
    // new client takes over and the old one is let go.
    let mut b = Agent::attach(&runtime, Some(&id));
    let windows = b.until(|m| match m {
        Message::Session { windows, .. } => Some(windows),
        _ => None,
    });
    assert_eq!(windows, vec![window]);
    let mut a = a;
    wait_for("the old client to be let go", || {
        a.child.try_wait().unwrap().is_some()
    });
    assert!(picture(&mut b, window) > 0);
    stop_and_check(&runtime, &id);
}

/// `--stop` ends a session and its program.
fn stop_and_check(runtime: &Runtime, id: &str) {
    let status = runtime.agent().args(["--stop", id]).status().unwrap();
    assert!(status.success());
    assert!(!runtime.session_dir(id).exists());
    assert!(!session_list(runtime).contains(id));
}

/// `--remove-when-done`: a copied agent deletes its own file once its
/// session ends, but not while another session still runs.
#[test]
fn copied_agent_removes_itself_when_done() {
    if !need_x() {
        return;
    }
    let runtime = Runtime::new();
    let bin = runtime.0.with_extension("bin");
    let _ = std::fs::remove_dir_all(&bin);
    std::fs::create_dir_all(&bin).unwrap();
    let copy = bin.join("ferostream-agent");
    std::fs::copy(env!("CARGO_BIN_EXE_ferostream-agent"), &copy).unwrap();

    // Another session runs (started by the usual agent): the copy stays.
    let other = Agent::start(&["xterm", "-geometry", "20x5"]);
    let other_id = other.until(|m| match m {
        Message::Session { id, .. } => Some(id),
        _ => None,
    });
    let other_runtime = Arc::clone(&other.runtime);

    let run_copy = |runtime: &Path| {
        let mut child = Command::new(&copy)
            .args(["--stdio", "--remove-when-done"])
            .env("FEROSTREAM_RUNTIME_DIR", runtime)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        for m in [
            Message::Hello {
                version: VERSION,
                software: "test".into(),
            },
            Message::Start {
                width: 640,
                height: 480,
                dpi: 96,
                argv: vec!["true".into()],
            },
        ] {
            stdin.write_all(&m.encode()).unwrap();
        }
        stdin.flush().unwrap();
        let mut out = Vec::new();
        child.stdout.take().unwrap().read_to_end(&mut out).unwrap();
        drop(stdin);
        let _ = child.wait();
        out
    };

    // Same runtime as the other session: it still runs, the file stays.
    let out = run_copy(&other_runtime.0);
    assert!(!out.is_empty());
    std::thread::sleep(Duration::from_millis(300));
    assert!(copy.exists(), "removed while another session runs");

    // Its own runtime, no other session: the file goes.
    run_copy(&runtime.0);
    wait_for("the copied agent to remove itself", || !copy.exists());

    stop_and_check(&other_runtime, &other_id);
    drop(other);
    let _ = std::fs::remove_dir_all(&bin);
}

/// Scrolling text: what the client builds from tiles and copies is exactly
/// what a fresh client gets in full, and scrolls go as copies.
#[test]
fn scrolled_picture_matches_a_fresh_one() {
    use ferostream_client::fb::Framebuffer;
    use std::collections::HashMap;

    if !need_x() {
        return;
    }
    // Plain text only: no blinking cursor to change the picture under us.
    let mut a = Agent::start(&["xterm", "-geometry", "60x20", "+bc", "-e", "sh"]);
    let id = a.until(|m| match m {
        Message::Session { id, .. } => Some(id),
        _ => None,
    });
    a.send(Message::Packing {
        mode: ferostream_proto::Packing::Fast,
    });
    let mut fbs: HashMap<u32, Framebuffer> = HashMap::new();
    let mut copies = 0;
    let mut tile_bytes = 0;
    let mut window = None;
    let mut typed = false;
    // Run until the output has stopped for a while.
    loop {
        let Ok(m) =
            a.rx.recv_timeout(Duration::from_millis(if typed { 1500 } else { 20_000 }))
        else {
            assert!(typed, "the agent went quiet");
            break;
        };
        match m {
            Message::WindowNew {
                window: w,
                width,
                height,
                kind: WindowKind::Normal,
                ..
            } => {
                fbs.insert(w, Framebuffer::new(width, height));
                window = Some(w);
            }
            Message::WindowConfigure {
                window: w,
                width,
                height,
                ..
            } => {
                if let Some(fb) = fbs.get_mut(&w) {
                    fb.resize(width, height);
                }
            }
            Message::Tile(t) => {
                tile_bytes += t.data.len();
                if let Some(fb) = fbs.get_mut(&t.window) {
                    assert!(fb.apply(&t).is_some(), "{t:?} does not decode");
                }
            }
            Message::TileCopy {
                window: w,
                x,
                y,
                width,
                height,
                to_x,
                to_y,
            } => {
                copies += 1;
                let fb = fbs.get_mut(&w).unwrap();
                assert!(fb.copy(x, y, width, height, to_x, to_y).is_some());
            }
            Message::FrameEnd { window: w, seq } => {
                a.send(Message::FrameAck { window: w, seq });
                if !typed && Some(w) == window {
                    typed = true;
                    a.send(Message::Focus { window: w });
                    a.send(Message::PointerMove {
                        window: w,
                        x: 20,
                        y: 20,
                    });
                    for ch in "for i in $(seq 1 60); do echo line $i; sleep 0.05; done\r".chars() {
                        a.send(Message::Text { ch });
                    }
                }
            }
            Message::Error { message } => panic!("agent error: {message}"),
            _ => {}
        }
    }
    let window = window.unwrap();
    eprintln!("{copies} copies, {tile_bytes} bytes of tiles");
    assert!(copies > 0, "no scroll was sent as a copy");

    // A new client gets the whole picture fresh.
    let runtime = Arc::clone(&a.runtime);
    let b = Agent::attach(&runtime, Some(&id));
    let mut fresh: Option<Framebuffer> = None;
    b.until(|m| match m {
        Message::WindowNew {
            window: w,
            width,
            height,
            ..
        } if w == window => {
            fresh = Some(Framebuffer::new(width, height));
            None
        }
        Message::Tile(t) if t.window == window => {
            fresh.as_mut().unwrap().apply(&t);
            None
        }
        Message::FrameEnd { window: w, .. } if w == window => Some(()),
        _ => None,
    });
    let fresh = fresh.unwrap();
    let kept = &fbs[&window];
    assert_eq!((kept.width, kept.height), (fresh.width, fresh.height));
    let differ = kept
        .pixels
        .chunks(4)
        .zip(fresh.pixels.chunks(4))
        .filter(|(p, q)| p != q)
        .count();
    assert_eq!(differ, 0, "{differ} pixels differ from a fresh picture");
    drop(a);
    stop_and_check(&runtime, &id);
}
