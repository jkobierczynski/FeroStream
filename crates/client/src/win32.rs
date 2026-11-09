//! The Win32 side: one native window per remote window.
//!
//! All windows live on the UI thread. A session's transport delivers what
//! arrives through [`Events`], which queues it and posts `WM_APP` to a
//! message-only window; the UI thread takes it from there. Pixels are kept
//! per window ([`Framebuffer`]) and painted with `StretchDIBits` at 1:1 (the
//! process should be per-monitor DPI aware, so a remote pixel is a screen
//! pixel).
//!
//! The standalone client ([`run`]) has one session and its own message
//! loop. A program that embeds the client (FeroSSH) calls [`start`] for
//! each session, on its UI thread, and its own message loop drives the
//! windows; a [`Host`] gives each session its transport and hears when it
//! ends.
//!
//! Re-entrancy: Win32 calls such as `SetWindowPos` send messages straight
//! back into the window procedure. The sessions' state is borrowed with
//! `try_borrow_mut`; a message that arrives while it is borrowed (one our
//! own call caused) gets default handling. That is also what stops a move
//! we make on the agent's behalf from being echoed back to it.

#![allow(unsafe_code)]

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::os::windows::process::CommandExt as _;
use std::process::ExitCode;
use std::sync::Mutex;
use std::sync::atomic::{AtomicIsize, AtomicU32, Ordering};

use ferostream_proto::tile::Encoding;
use ferostream_proto::{Message, VERSION, WindowKind, WindowState};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BeginPaint, ClientToScreen, CreateSolidBrush,
    DIB_RGB_COLORS, EndPaint, EnumDisplayMonitors, FillRect, GetMonitorInfoW, HBRUSH, HDC,
    HMONITOR, InvalidateRect, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow,
    PAINTSTRUCT, SRCCOPY, ScreenToClient, StretchDIBits,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    AdjustWindowRectExForDpi, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForSystem,
    GetDpiForWindow, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, MAPVK_VK_TO_CHAR, MapVirtualKeyW, ReleaseCapture, SetCapture,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{HSTRING, PCWSTR, w};

use crate::clip;
use crate::fb::Framebuffer;
use crate::keys;
use crate::link::{Event, Link};
use crate::reconnect;
use crate::transport::{AgentFile, AttachTo, Clipboard, Options, find_agent_files, ssh_command};

const WM_LINK: u32 = WM_APP + 1;
/// The window class of remote windows.
const CLASS: PCWSTR = w!("FeroStreamWindow");
const MSG_CLASS: PCWSTR = w!("FeroStreamLink");
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const SOFTWARE: &str = concat!("ferostream ", env!("CARGO_PKG_VERSION"));

/// What transports have delivered and the UI thread not yet handled: the
/// session, the link number it came from, the event.
static QUEUE: Mutex<VecDeque<(u32, u32, Event)>> = Mutex::new(VecDeque::new());
/// Numbers for sessions (from 1) and for links (each connect gets one;
/// events from a session's earlier links are dropped).
static NEXT_SESSION: AtomicU32 = AtomicU32::new(0);
static NEXT_LINK: AtomicU32 = AtomicU32::new(0);
/// The message-only window `WM_LINK` and the reconnect timers go to.
static LINK_HWND: AtomicIsize = AtomicIsize::new(0);

thread_local! {
    static APPS: RefCell<HashMap<u32, App>> = RefCell::new(HashMap::new());
}

/// Where a session's messages for the agent go.
pub trait Transport: std::fmt::Debug {
    fn send(&self, m: Message);
}

impl Transport for Link {
    fn send(&self, m: Message) {
        Link::send(self, m);
    }
}

/// Hands one transport's events to its session, from any thread.
#[derive(Debug, Clone)]
pub struct Events {
    session: u32,
    link: u32,
}

impl Events {
    pub fn send(&self, event: Event) {
        let mut q = QUEUE.lock().unwrap_or_else(|p| p.into_inner());
        let was_empty = q.is_empty();
        q.push_back((self.session, self.link, event));
        drop(q);
        if was_empty {
            let hwnd = HWND(LINK_HWND.load(Ordering::SeqCst) as *mut _);
            unsafe {
                let _ = PostMessageW(Some(hwnd), WM_LINK, WPARAM(0), LPARAM(0));
            }
        }
    }

    /// The session these events are for.
    pub fn session(&self) -> u32 {
        self.session
    }
}

/// The program a session runs in: it connects the session to the agent and
/// hears when the session is over.
pub trait Host: std::fmt::Debug {
    /// Starts a transport to the agent (`ferostream-agent --stdio` with
    /// the arguments for `attach`). Everything it receives goes to
    /// `events`; an [`Event::Closed`] when it ends.
    fn connect(&mut self, attach: &AttachTo, events: Events) -> Result<Box<dyn Transport>, String>;
    /// The session ended; its windows are gone. Called on the UI thread,
    /// outside any of the session's own state.
    fn ended(&mut self, end: End);
}

/// How a session ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct End {
    pub session: u32,
    /// The program's exit code (1 when the session failed).
    pub exit_code: i32,
    /// What went wrong, for the user.
    pub problem: Option<String>,
    /// The user closed the windows while the connection was down: the
    /// program still runs on the host as this session id.
    pub left_running: Option<String>,
}

/// What a session runs and how.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// The program and its arguments (ignored when attaching).
    pub program: Vec<String>,
    pub clipboard: Clipboard,
    pub attach: AttachTo,
    /// How hard the agent packs pixels (sent to it at every connect).
    pub packing: ferostream_proto::Packing,
    /// Accept only raw tiles: the transport's bytes went through a
    /// [`crate::decode::Decoder`] elsewhere (a sandboxed process) that
    /// unpacked them, so a compressed tile here means that process is not
    /// doing its job, and the session ends.
    pub raw_only: bool,
}

#[derive(Debug)]
struct Remote {
    hwnd: HWND,
    fb: Framebuffer,
    kind: WindowKind,
    /// Last pointer position in this window, for wheel clicks.
    pointer: (i32, i32),
    /// Tiles and copies of the frame still arriving, in order: drawn
    /// together at FrameEnd, so a half-received frame is never on screen.
    pending: Vec<Message>,
    /// Smallest and biggest client size the program allows; 0 is no limit.
    min: (u16, u16),
    max: (u16, u16),
    state: WindowState,
    /// Big (taskbar) and small (title bar) icons the window shows.
    icons: [Option<HICON>; 2],
    /// While full screen: the style and frame to go back to.
    restore: Option<(i32, RECT)>,
    /// The program's title for the window (shown with a note while the
    /// connection is down).
    title: String,
}

impl Remote {
    /// Draws the tiles received so far.
    fn apply_pending(&mut self) {
        for m in std::mem::take(&mut self.pending) {
            let changed = match m {
                Message::Tile(t) => self.fb.apply(&t),
                Message::TileCopy {
                    x,
                    y,
                    width,
                    height,
                    to_x,
                    to_y,
                    ..
                } => self.fb.copy(x, y, width, height, to_x, to_y),
                _ => None,
            };
            if let Some(c) = changed {
                let rect = RECT {
                    left: i32::from(c.x),
                    top: i32::from(c.y),
                    right: i32::from(c.x) + i32::from(c.width),
                    bottom: i32::from(c.y) + i32::from(c.height),
                };
                unsafe {
                    let _ = InvalidateRect(Some(self.hwnd), Some(&rect), false);
                }
            }
        }
    }
}

/// One session: a program on a host and its windows here.
#[derive(Debug)]
struct App {
    /// This session's number.
    id: u32,
    link: Box<dyn Transport>,
    /// The link number events must carry.
    link_number: u32,
    host: Box<dyn Host>,
    settings: Settings,
    /// The session on the host, once it has said its id.
    session: Option<String>,
    /// The connection is down: reconnect attempts made so far.
    away: Option<u32>,
    /// The user closed the last window while the connection was down.
    left_running: bool,
    windows: HashMap<u32, Remote>,
    by_hwnd: HashMap<isize, u32>,
    /// Top-left of the Windows virtual screen; remote (0,0) maps here.
    origin: (i32, i32),
    grey: HBRUSH,
    wheel: i32,
    hwheel: i32,
    /// Keysyms the client holds down at the agent (released on focus loss).
    held: Vec<u32>,
    /// Chord keys (pressed with Ctrl or Alt) that are down: virtual key to keysym.
    chords: HashMap<u16, u32>,
    /// Right Alt is AltGr on this layout and is down: neither it nor the
    /// left Ctrl Windows adds goes to the agent.
    altgr: bool,
    high_surrogate: Option<u16>,
    /// The remote pointer shape, shown over every remote window; `None`
    /// until the agent sends one (the arrow until then).
    cursor: Option<HCURSOR>,
    /// Which ways copied text may travel.
    clipboard: Clipboard,
    /// Hello went over the current link (so other messages may follow).
    greeted: bool,
    /// Windows clipboard sequence number when we last looked at it or
    /// wrote it ourselves.
    clip_seq: u32,
    /// Clipboard text last sent or received, never sent back.
    last_clip: Option<String>,
    /// The screens, as last sent to the agent.
    monitors: Vec<ferostream_proto::Monitor>,
    /// One of our windows is the active window. Every top-level window
    /// gets WM_ACTIVATEAPP; this makes the agent hear it once.
    app_active: bool,
    ended: bool,
    /// The session is over (set where it cannot end at once).
    done: bool,
    exit_code: i32,
    problem: Option<String>,
}

fn utf16(s: &str) -> HSTRING {
    HSTRING::from(s)
}

/// Shows a message box (the usage text, an error).
pub fn message_box(title: &str, text: &str, error: bool) {
    let style = if error {
        MB_ICONERROR
    } else {
        MB_ICONINFORMATION
    };
    unsafe {
        MessageBoxW(None, &utf16(text), &utf16(title), style | MB_OK);
    }
}

/// Registers the window classes and makes the message-only window, once.
fn init() -> Result<(), String> {
    if LINK_HWND.load(Ordering::SeqCst) != 0 {
        return Ok(());
    }
    let module = unsafe { GetModuleHandleW(None) }.map_err(|e| e.to_string())?;
    let instance = module.into();
    let cursor = unsafe { LoadCursorW(None, IDC_ARROW) }.unwrap_or_default();
    let class = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(window_proc),
        hInstance: instance,
        hCursor: cursor,
        lpszClassName: CLASS,
        ..Default::default()
    };
    let msg_class = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(link_proc),
        hInstance: instance,
        lpszClassName: MSG_CLASS,
        ..Default::default()
    };
    unsafe {
        RegisterClassExW(&class);
        RegisterClassExW(&msg_class);
    }
    let link_hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            MSG_CLASS,
            w!(""),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(instance),
            None,
        )
    }
    .map_err(|e| format!("Could not create a window: {e}"))?;
    LINK_HWND.store(link_hwnd.0 as isize, Ordering::SeqCst);
    Ok(())
}

/// Starts a session on the calling (UI) thread: `host` connects it, and
/// its windows appear as the agent announces them. Returns the session's
/// number. The thread needs a message loop that dispatches to all its
/// windows (and should leave key messages for [`is_remote_window`]
/// windows to them, untranslated by accelerators).
pub fn start(settings: Settings, mut host: Box<dyn Host>) -> Result<u32, String> {
    init()?;
    let id = NEXT_SESSION.fetch_add(1, Ordering::SeqCst).wrapping_add(1);
    let link_number = next_link();
    let link = host.connect(
        &settings.attach,
        Events {
            session: id,
            link: link_number,
        },
    )?;
    let (ox, oy) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
        )
    };
    // Hello, Start and the screens go once the agent has answered: before
    // that, the host may still be taking the agent binary.
    let screens = monitors((ox, oy));
    let clipboard = settings.clipboard;
    let grey = unsafe { CreateSolidBrush(windows::Win32::Foundation::COLORREF(0x0080_8080)) };
    let app = App {
        id,
        link,
        link_number,
        host,
        settings,
        session: None,
        away: None,
        left_running: false,
        windows: HashMap::new(),
        by_hwnd: HashMap::new(),
        origin: (ox, oy),
        grey,
        wheel: 0,
        hwheel: 0,
        held: Vec::new(),
        chords: HashMap::new(),
        altgr: false,
        high_surrogate: None,
        cursor: None,
        clipboard,
        greeted: false,
        clip_seq: 0,
        last_clip: None,
        monitors: screens,
        app_active: false,
        ended: false,
        done: false,
        exit_code: 0,
        problem: None,
    };
    APPS.with(|a| {
        a.try_borrow_mut()
            .map(|mut a| {
                a.insert(id, app);
            })
            .map_err(|_| "FeroStream is busy; try again.".to_string())
    })?;
    Ok(id)
}

/// Ends session `session` from the host's side: its windows close and its
/// transport is dropped. The program goes on running on the host (it can
/// be attached again). [`Host::ended`] is not called.
pub fn stop(session: u32) {
    let app = APPS.with(|a| a.try_borrow_mut().ok()?.remove(&session));
    if let Some(app) = app {
        app.close_windows();
    }
}

/// Changes which ways copied text travels for session `session`, at once:
/// the agent hears it, and the client stops (or starts) passing text on.
pub fn set_clipboard(session: u32, clipboard: Clipboard) {
    APPS.with(|a| {
        if let Ok(mut a) = a.try_borrow_mut()
            && let Some(app) = a.get_mut(&session)
        {
            app.clipboard = clipboard;
            // Text copied on Windows while sharing was off is not sent now
            // it is on: only what is copied from here on.
            app.clip_seq = clip::sequence();
            if app.greeted {
                app.link.send(clipboard.rules());
            }
        }
    });
}

/// The sessions running on this thread.
pub fn sessions() -> Vec<u32> {
    APPS.with(|a| {
        a.try_borrow()
            .map(|a| a.keys().copied().collect())
            .unwrap_or_default()
    })
}

/// Whether `hwnd` is one of the remote windows: an embedding program sends
/// its key messages straight to it (no accelerators, no dialog keys).
pub fn is_remote_window(hwnd: HWND) -> bool {
    let mut name = [0u16; 32];
    let n = unsafe { GetClassNameW(hwnd, &mut name) };
    let want = b"FeroStreamWindow".map(u16::from);
    usize::try_from(n).is_ok_and(|n| name.get(..n) == Some(&want[..]))
}

fn next_link() -> u32 {
    NEXT_LINK.fetch_add(1, Ordering::SeqCst).wrapping_add(1)
}

/// The standalone client's host: `ssh.exe` starts the agent.
#[derive(Debug)]
struct SshHost {
    options: Options,
    /// Agents the client can copy to the host.
    files: Vec<AgentFile>,
}

thread_local! {
    /// How the standalone client's session ended.
    static OUTCOME: RefCell<Option<End>> = const { RefCell::new(None) };
}

impl Host for SshHost {
    fn connect(&mut self, attach: &AttachTo, events: Events) -> Result<Box<dyn Transport>, String> {
        let mut cmd = ssh_command(&self.options, &self.files, attach);
        cmd.creation_flags(CREATE_NO_WINDOW);
        // ssh has no console to ask for a password or a host-key answer in,
        // so it asks us: this exe in askpass mode shows a small dialog.
        if let Ok(me) = std::env::current_exe() {
            cmd.env("SSH_ASKPASS", me)
                .env("SSH_ASKPASS_REQUIRE", "force")
                .env(crate::askpass::ENV, "1");
        }
        Link::start(cmd, self.files.clone(), move |event| events.send(event))
            .map(|l| Box::new(l) as Box<dyn Transport>)
            .map_err(|e| format!("Could not start {}: {e}", self.options.ssh.display()))
    }

    fn ended(&mut self, end: End) {
        OUTCOME.with(|o| *o.borrow_mut() = Some(end));
        unsafe { PostQuitMessage(0) };
    }
}

/// Connects, shows the remote program's windows until it ends, and returns
/// its exit code.
pub fn run(options: Options) -> ExitCode {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    // The agent for the host ships next to this exe.
    let files = if options.install && options.agent.is_none() {
        std::env::current_exe()
            .ok()
            .and_then(|me| me.parent().map(find_agent_files))
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let settings = Settings {
        program: options.program.clone(),
        clipboard: options.clipboard,
        attach: options.attach.clone(),
        packing: options.packing,
        raw_only: false,
    };
    let destination = options.destination.clone();
    if let Err(e) = start(settings, Box::new(SshHost { options, files })) {
        message_box("FeroStream", &e, true);
        return ExitCode::FAILURE;
    }

    let mut msg = MSG::default();
    while unsafe { GetMessageW(&mut msg, None, 0, 0) }.as_bool() {
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    let Some(end) = OUTCOME.with(|o| o.borrow_mut().take()) else {
        return ExitCode::FAILURE;
    };
    if let Some(p) = end.problem {
        message_box("FeroStream", &p, true);
    } else if let Some(id) = end.left_running {
        message_box(
            "FeroStream",
            &reconnect::left_running(&destination, &id),
            false,
        );
    }
    ExitCode::from(end.exit_code.clamp(0, 255) as u8)
}

/// Runs `f` on session `session`'s state, or returns `None` when the state
/// is already borrowed (a message our own Win32 call sent) or the session
/// is gone.
fn with_app<R>(session: u32, f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APPS.with(|a| {
        let mut b = a.try_borrow_mut().ok()?;
        b.get_mut(&session).map(f)
    })
}

/// Ends session `session` if it is done: its windows go and its host
/// hears how it ended.
fn finish_if_done(session: u32) {
    let app = APPS.with(|a| {
        let mut a = a.try_borrow_mut().ok()?;
        if a.get(&session).is_some_and(|app| app.done) {
            a.remove(&session)
        } else {
            None
        }
    });
    let Some(mut app) = app else { return };
    let end = End {
        session,
        exit_code: app.exit_code,
        problem: app.problem.take(),
        left_running: app.session.clone().filter(|_| app.left_running),
    };
    let link_hwnd = HWND(LINK_HWND.load(Ordering::SeqCst) as *mut _);
    unsafe {
        let _ = KillTimer(Some(link_hwnd), session as usize);
    }
    let mut host = app.close_windows();
    host.ended(end);
}

extern "system" fn link_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_LINK {
        loop {
            let next = QUEUE.lock().unwrap_or_else(|p| p.into_inner()).pop_front();
            let Some((session, number, event)) = next else {
                break;
            };
            let over = with_app(session, |app| {
                if app.link_number != number {
                    return false;
                }
                if app.on_event(event) {
                    app.done = true;
                }
                app.done
            });
            if over == Some(true) {
                finish_if_done(session);
            }
        }
        return LRESULT(0);
    }
    if msg == WM_TIMER {
        let session = wparam.0 as u32;
        unsafe {
            let _ = KillTimer(Some(hwnd), wparam.0);
        }
        with_app(session, App::reconnect);
        return LRESULT(0);
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

impl App {
    /// The connection is down but the session lives on the host: keep the
    /// windows, mark them, and try again after a while.
    fn lost(&mut self) {
        let attempt = match self.away {
            Some(n) => n,
            None => {
                self.release_held();
                for r in self.windows.values() {
                    if r.kind != WindowKind::Popup {
                        unsafe {
                            let _ = SetWindowTextW(
                                r.hwnd,
                                &utf16(&reconnect::title_while_away(&r.title)),
                            );
                        }
                    }
                }
                0
            }
        };
        self.away = Some(attempt + 1);
        let ms = reconnect::delay(attempt).as_millis() as u32;
        let hwnd = HWND(LINK_HWND.load(Ordering::SeqCst) as *mut _);
        unsafe {
            SetTimer(Some(hwnd), self.id as usize, ms, None);
        }
    }

    /// Tries to reach the session again.
    fn reconnect(&mut self) {
        let Some(id) = self.session.clone() else {
            return;
        };
        let number = next_link();
        let events = Events {
            session: self.id,
            link: number,
        };
        match self.host.connect(&AttachTo::Id(id), events) {
            Ok(link) => {
                self.link = link;
                self.link_number = number;
                self.greeted = false;
            }
            Err(_) => self.lost(),
        }
    }

    /// Closes every window and gives back the host.
    fn close_windows(mut self) -> Box<dyn Host> {
        let ids: Vec<u32> = self.windows.keys().copied().collect();
        for id in ids {
            self.destroy(id);
        }
        if let Some(c) = self.cursor.take() {
            unsafe {
                let _ = DestroyCursor(c);
            }
        }
        unsafe {
            let _ = windows::Win32::Graphics::Gdi::DeleteObject(self.grey.into());
        }
        self.host
    }

    /// The agent answered: say who we are and what we want.
    fn greet(&mut self) {
        self.link.send(Message::Hello {
            version: VERSION,
            software: SOFTWARE.into(),
        });
        self.greeted = true;
        if self.session.is_some() || self.settings.attach != AttachTo::New {
            self.link.send(Message::Attach);
        } else {
            let (vw, vh) = unsafe {
                (
                    GetSystemMetrics(SM_CXVIRTUALSCREEN),
                    GetSystemMetrics(SM_CYVIRTUALSCREEN),
                )
            };
            let side = |v: i32| v.clamp(640, i32::from(ferostream_proto::MAX_WINDOW_SIDE)) as u16;
            self.link.send(Message::Start {
                width: side(vw),
                height: side(vh),
                // The primary screen's scaling; the remote display has one DPI.
                dpi: (unsafe { GetDpiForSystem() } as u16)
                    .clamp(ferostream_proto::MIN_DPI, ferostream_proto::MAX_DPI),
                argv: self.settings.program.clone(),
            });
        }
        self.link.send(Message::Packing {
            mode: self.settings.packing,
        });
        self.link.send(self.clipboard.rules());
        if !self.monitors.is_empty() {
            self.link.send(Message::Monitors {
                monitors: self.monitors.clone(),
            });
        }
    }

    /// The session is (back) up. After a reconnect, windows it no longer
    /// has go, and the others lose their note.
    fn session_ready(&mut self, id: String, windows: &[u32]) {
        self.session = Some(id);
        if self.away.take().is_none() {
            return;
        }
        for window in reconnect::stale(self.windows.keys().copied(), windows) {
            self.destroy(window);
        }
        for r in self.windows.values() {
            unsafe {
                let _ = SetWindowTextW(r.hwnd, &utf16(&r.title));
            }
        }
    }

    fn destroy(&mut self, window: u32) {
        if let Some(r) = self.windows.remove(&window) {
            self.by_hwnd.remove(&(r.hwnd.0 as isize));
            unsafe {
                let _ = DestroyWindow(r.hwnd);
                for icon in r.icons.into_iter().flatten() {
                    let _ = DestroyIcon(icon);
                }
            }
        }
    }

    /// Handles one event from the transport; `true` ends the program.
    fn on_event(&mut self, event: Event) -> bool {
        match event {
            Event::Message(m) => self.on_message(m),
            Event::Closed { .. } if !self.ended && self.session.is_some() => {
                // The program goes on on the host; so do we.
                self.lost();
                false
            }
            Event::Closed { error, stderr } => {
                if !self.ended {
                    let mut text = String::from("The connection to the host ended.");
                    if let Some(e) = error {
                        text.push_str(&format!("\n\n{e}"));
                    }
                    if !stderr.is_empty() {
                        text.push_str(&format!("\n\nssh said:\n{stderr}"));
                    }
                    self.problem = Some(text);
                    self.exit_code = 1;
                }
                true
            }
        }
    }

    fn on_message(&mut self, m: Message) -> bool {
        match m {
            Message::Hello { version, .. } if version != VERSION => {
                self.problem = Some(format!(
                    "The agent on the host speaks protocol {version}; this client speaks {VERSION}. \
                     Update ferostream-agent or ferostream.exe so they match."
                ));
                self.exit_code = 1;
                self.ended = true;
                return true;
            }
            Message::Hello { .. } => self.greet(),
            Message::Session { id, windows } => self.session_ready(id, &windows),
            Message::WindowNew {
                window,
                x,
                y,
                width,
                height,
                kind,
                transient_for,
                title,
            } => self.create(window, x, y, width, height, kind, transient_for, &title),
            Message::WindowConfigure {
                window,
                x,
                y,
                width,
                height,
            } => {
                let origin = self.origin;
                if let Some(r) = self.windows.get_mut(&window) {
                    // Tiles before a resize belong to the old size.
                    r.apply_pending();
                    r.fb.resize(width, height);
                    place(r.hwnd, origin.0 + x, origin.1 + y, width, height, true);
                    unsafe {
                        let _ = InvalidateRect(Some(r.hwnd), None, false);
                    }
                }
            }
            Message::WindowTitle { window, title } => {
                if let Some(r) = self.windows.get_mut(&window) {
                    unsafe {
                        let _ = SetWindowTextW(r.hwnd, &utf16(&title));
                    }
                    r.title = title;
                }
            }
            Message::WindowGone { window } => self.destroy(window),
            Message::Tile(t) if self.settings.raw_only && t.encoding != Encoding::Raw => {
                self.ended = true;
                self.exit_code = 1;
                self.problem =
                    Some("The decoder sent a packed tile; the session was ended.".into());
                return true;
            }
            Message::Tile(t) => {
                if let Some(r) = self.windows.get_mut(&t.window) {
                    r.pending.push(Message::Tile(t));
                }
            }
            m @ Message::TileCopy { window, .. } => {
                if let Some(r) = self.windows.get_mut(&window) {
                    r.pending.push(m);
                }
            }
            Message::FrameEnd { window, seq } => {
                if let Some(r) = self.windows.get_mut(&window) {
                    r.apply_pending();
                }
                self.link.send(Message::FrameAck { window, seq });
            }
            Message::Cursor {
                hot_x,
                hot_y,
                image,
            } => self.set_cursor(crate::picture::cursor(hot_x, hot_y, &image)),
            Message::WindowHints {
                window,
                min_width,
                min_height,
                max_width,
                max_height,
            } => {
                if let Some(r) = self.windows.get_mut(&window) {
                    r.min = (min_width, min_height);
                    r.max = (max_width, max_height);
                }
            }
            Message::WindowIcon { window, big, image } => {
                if let Some(r) = self.windows.get_mut(&window)
                    && let Some(icon) = crate::picture::icon(&image)
                {
                    let which = if big { ICON_BIG } else { ICON_SMALL };
                    unsafe {
                        SendMessageW(
                            r.hwnd,
                            WM_SETICON,
                            Some(WPARAM(which as usize)),
                            Some(LPARAM(icon.0 as isize)),
                        );
                    }
                    let [big_slot, small_slot] = &mut r.icons;
                    let slot = if big { big_slot } else { small_slot };
                    if let Some(old) = slot.replace(icon) {
                        unsafe {
                            let _ = DestroyIcon(old);
                        }
                    }
                }
            }
            Message::WindowState { window, state } => self.set_state(window, state),
            Message::Clipboard { text } => {
                if self.clipboard.from_host {
                    let owner = HWND(LINK_HWND.load(Ordering::SeqCst) as *mut _);
                    if clip::write(owner, &text) {
                        // Our own write is not news for the host.
                        self.clip_seq = clip::sequence();
                        self.last_clip = Some(text);
                    }
                }
            }
            Message::WindowActivate { window } => {
                if let Some(r) = self.windows.get(&window) {
                    unsafe {
                        if IsIconic(r.hwnd).as_bool() {
                            let _ = ShowWindow(r.hwnd, SW_RESTORE);
                        }
                        // Windows may refuse to hand over the foreground;
                        // the taskbar button flashes instead.
                        if !SetForegroundWindow(r.hwnd).as_bool() {
                            let _ = FlashWindow(r.hwnd, true);
                        }
                    }
                }
            }
            Message::Ping { token } => self.link.send(Message::Pong { token }),
            Message::Exited { code } => {
                self.ended = true;
                self.exit_code = code;
                return true;
            }
            Message::Error { message } => {
                self.ended = true;
                self.exit_code = 1;
                self.problem = Some(format!("The host reported a problem:\n\n{message}"));
                return true;
            }
            _ => {}
        }
        false
    }

    #[allow(clippy::too_many_arguments)]
    fn create(
        &mut self,
        window: u32,
        x: i32,
        y: i32,
        width: u16,
        height: u16,
        kind: WindowKind,
        transient_for: u32,
        title: &str,
    ) {
        if let Some(r) = self.windows.get_mut(&window) {
            // Announced again after a reconnect: the window stays where the
            // user has it, and the agent hears where that is.
            r.title = title.into();
            unsafe {
                let _ = SetWindowTextW(r.hwnd, &utf16(title));
            }
            if r.kind != WindowKind::Popup {
                let hwnd = r.hwnd;
                let mut client = RECT::default();
                unsafe {
                    let _ = GetClientRect(hwnd, &mut client);
                }
                let (sx, sy) = client_origin(hwnd);
                let minimized = unsafe { IsIconic(hwnd) }.as_bool();
                if !minimized && client.right > 0 && client.bottom > 0 {
                    let side =
                        |v: i32| v.clamp(1, i32::from(ferostream_proto::MAX_WINDOW_SIDE)) as u16;
                    self.link.send(Message::WindowMove {
                        window,
                        x: sx - self.origin.0,
                        y: sy - self.origin.1,
                        width: side(client.right),
                        height: side(client.bottom),
                    });
                }
            }
            return;
        }
        let (style, ex) = styles(kind);
        let owner = self
            .windows
            .get(&transient_for)
            .filter(|_| kind != WindowKind::Normal)
            .map(|r| r.hwnd)
            .or_else(|| {
                // A popup belongs to the window that has the focus.
                (kind == WindowKind::Popup)
                    .then(|| unsafe { GetForegroundWindow() })
                    .filter(|h| self.by_hwnd.contains_key(&(h.0 as isize)))
            });
        // Ordinary windows the program put at the very corner (no window
        // manager placed them) go where Windows puts new windows.
        let at_default =
            matches!(kind, WindowKind::Normal | WindowKind::Dialog) && x == 0 && y == 0;
        let instance = unsafe { GetModuleHandleW(None) }.ok().map(Into::into);
        let hwnd = unsafe {
            CreateWindowExW(
                ex,
                CLASS,
                &utf16(title),
                style,
                if at_default {
                    CW_USEDEFAULT
                } else {
                    self.origin.0 + x
                },
                if at_default {
                    CW_USEDEFAULT
                } else {
                    self.origin.1 + y
                },
                i32::from(width),
                i32::from(height),
                owner,
                None,
                instance,
                None,
            )
        };
        let Ok(hwnd) = hwnd else { return };
        self.windows.insert(
            window,
            Remote {
                hwnd,
                fb: Framebuffer::new(width, height),
                kind,
                pointer: (0, 0),
                pending: Vec::new(),
                min: (0, 0),
                max: (0, 0),
                state: WindowState::Normal,
                icons: [None, None],
                restore: None,
                title: title.into(),
            },
        );
        self.by_hwnd.insert(hwnd.0 as isize, window);
        unsafe {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, self.id as isize);
        }
        // Size the frame so the client area is exactly the remote window.
        let (cx, cy) = if at_default {
            client_origin(hwnd)
        } else {
            (self.origin.0 + x, self.origin.1 + y)
        };
        place(hwnd, cx, cy, width, height, false);
        unsafe {
            let _ = ShowWindow(
                hwnd,
                if kind == WindowKind::Popup {
                    SW_SHOWNA
                } else {
                    SW_SHOW
                },
            );
        }
        if kind != WindowKind::Popup {
            // Tell the agent where the window really is, and that it has
            // the focus (WM_ACTIVATE came while the state was borrowed).
            let (sx, sy) = client_origin(hwnd);
            self.link.send(Message::WindowMove {
                window,
                x: sx - self.origin.0,
                y: sy - self.origin.1,
                width,
                height,
            });
            self.link.send(Message::Focus { window });
        }
    }

    /// Takes a new pointer shape, showing it at once when the pointer is
    /// over one of our windows.
    fn set_cursor(&mut self, new: Option<HCURSOR>) {
        let Some(new) = new else { return };
        let mut p = POINT::default();
        let over_ours = unsafe { GetCursorPos(&mut p) }.is_ok()
            && self
                .by_hwnd
                .contains_key(&(unsafe { WindowFromPoint(p) }.0 as isize));
        if over_ours {
            unsafe {
                SetCursor(Some(new));
            }
        }
        if let Some(old) = self.cursor.replace(new) {
            unsafe {
                let _ = DestroyCursor(old);
            }
        }
    }

    /// Minimises, maximises, restores or makes full screen a window the
    /// program asked for, then tells the agent the size that gave.
    fn set_state(&mut self, window: u32, state: WindowState) {
        let Some(r) = self.windows.get_mut(&window) else {
            return;
        };
        if r.kind == WindowKind::Popup || r.state == state {
            return;
        }
        let hwnd = r.hwnd;
        r.state = state;
        unsafe {
            if state != WindowState::Fullscreen
                && let Some((style, frame)) = r.restore.take()
            {
                SetWindowLongW(hwnd, GWL_STYLE, style);
                let _ = SetWindowPos(
                    hwnd,
                    None,
                    frame.left,
                    frame.top,
                    frame.right - frame.left,
                    frame.bottom - frame.top,
                    SWP_FRAMECHANGED | SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
            match state {
                WindowState::Normal => {
                    let _ = ShowWindow(hwnd, SW_RESTORE);
                }
                WindowState::Minimized => {
                    let _ = ShowWindow(hwnd, SW_MINIMIZE);
                }
                WindowState::Maximized => {
                    let _ = ShowWindow(hwnd, SW_MAXIMIZE);
                }
                WindowState::Fullscreen => {
                    let _ = ShowWindow(hwnd, SW_RESTORE);
                    let mut frame = RECT::default();
                    let _ = GetWindowRect(hwnd, &mut frame);
                    r.restore = Some((GetWindowLongW(hwnd, GWL_STYLE), frame));
                    let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
                    let mut info = MONITORINFO {
                        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                        ..Default::default()
                    };
                    if GetMonitorInfoW(monitor, &mut info).as_bool() {
                        let m = info.rcMonitor;
                        SetWindowLongW(hwnd, GWL_STYLE, (WS_POPUP | WS_VISIBLE).0 as i32);
                        let _ = SetWindowPos(
                            hwnd,
                            Some(HWND_TOP),
                            m.left,
                            m.top,
                            m.right - m.left,
                            m.bottom - m.top,
                            SWP_FRAMECHANGED,
                        );
                    }
                }
            }
        }
        // The moves above came back while the state was borrowed, so the
        // agent has not heard about them.
        if state != WindowState::Minimized {
            self.report_geometry(window);
        }
    }

    /// Sends a window's client-area position and size to the agent.
    fn report_geometry(&self, window: u32) {
        let Some(r) = self.windows.get(&window) else {
            return;
        };
        let mut client = RECT::default();
        unsafe {
            let _ = GetClientRect(r.hwnd, &mut client);
        }
        if client.right <= 0 || client.bottom <= 0 {
            return;
        }
        let (x, y) = client_origin(r.hwnd);
        let side = |v: i32| v.clamp(1, i32::from(ferostream_proto::MAX_WINDOW_SIDE)) as u16;
        self.link.send(Message::WindowMove {
            window,
            x: x - self.origin.0,
            y: y - self.origin.1,
            width: side(client.right),
            height: side(client.bottom),
        });
    }

    /// Sends what was copied on Windows since we last looked, as the user
    /// turns to a remote window.
    fn push_clipboard(&mut self) {
        if !self.clipboard.to_host {
            return;
        }
        let seq = clip::sequence();
        if seq == self.clip_seq {
            return;
        }
        self.clip_seq = seq;
        let Some(text) = clip::read() else { return };
        if text.is_empty() || self.last_clip.as_deref() == Some(&text) {
            return;
        }
        self.last_clip = Some(text.clone());
        self.link.send(Message::Clipboard { text });
    }

    fn release_held(&mut self) {
        for keysym in self.held.drain(..) {
            self.link.send(Message::Key {
                keysym,
                down: false,
            });
        }
        for (_, keysym) in self.chords.drain() {
            self.link.send(Message::Key {
                keysym,
                down: false,
            });
        }
        self.altgr = false;
    }

    fn key(&mut self, keysym: u32, down: bool) {
        if down {
            if !self.held.contains(&keysym) {
                self.held.push(keysym);
            }
        } else {
            self.held.retain(|&k| k != keysym);
        }
        self.link.send(Message::Key { keysym, down });
    }
}

fn styles(kind: WindowKind) -> (WINDOW_STYLE, WINDOW_EX_STYLE) {
    match kind {
        WindowKind::Normal => (WS_OVERLAPPEDWINDOW, WINDOW_EX_STYLE(0)),
        WindowKind::Dialog => (WS_CAPTION | WS_SYSMENU | WS_THICKFRAME, WS_EX_DLGMODALFRAME),
        WindowKind::Utility => (WS_CAPTION | WS_SYSMENU | WS_THICKFRAME, WS_EX_TOOLWINDOW),
        WindowKind::Splash => (WS_POPUP, WINDOW_EX_STYLE(0)),
        WindowKind::Popup => (
            WS_POPUP,
            WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST,
        ),
    }
}

/// Screen position of a window's client area.
/// The screens of the desktop, relative to the virtual screen's top left
/// (`origin`), the primary one first.
fn monitors(origin: (i32, i32)) -> Vec<ferostream_proto::Monitor> {
    extern "system" fn each(
        monitor: HMONITOR,
        _: HDC,
        _: *mut RECT,
        data: LPARAM,
    ) -> windows::core::BOOL {
        // SAFETY: `data` is the Vec below, alive for the whole enumeration.
        let found = unsafe { &mut *(data.0 as *mut Vec<(bool, RECT)>) };
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
            found.push((info.dwFlags & MONITORINFOF_PRIMARY != 0, info.rcMonitor));
        }
        windows::core::BOOL(1)
    }
    let mut found: Vec<(bool, RECT)> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(each), LPARAM((&raw mut found) as isize));
    }
    // The primary screen first (Vec::sort_by_key is stable).
    found.sort_by_key(|(primary, _)| !primary);
    let limit = i32::from(ferostream_proto::MAX_WINDOW_SIDE);
    found
        .into_iter()
        .take(ferostream_proto::MAX_MONITORS)
        .filter_map(|(_, r)| {
            let x = (r.left - origin.0).clamp(0, limit - 1);
            let y = (r.top - origin.1).clamp(0, limit - 1);
            let w = (r.right - r.left).clamp(1, limit - x);
            let h = (r.bottom - r.top).clamp(1, limit - y);
            Some(ferostream_proto::Monitor {
                x: u16::try_from(x).ok()?,
                y: u16::try_from(y).ok()?,
                width: u16::try_from(w).ok()?,
                height: u16::try_from(h).ok()?,
            })
        })
        .collect()
}

fn client_origin(hwnd: HWND) -> (i32, i32) {
    let mut p = POINT { x: 0, y: 0 };
    unsafe {
        let _ = ClientToScreen(hwnd, &mut p);
    }
    (p.x, p.y)
}

/// Moves and sizes `hwnd` so its client area is at `x`,`y` (screen) and
/// `width` x `height`. Skips the call when nothing would change.
fn place(hwnd: HWND, x: i32, y: i32, width: u16, height: u16, skip_same: bool) {
    let mut client = RECT::default();
    unsafe {
        let _ = GetClientRect(hwnd, &mut client);
    }
    let (cx, cy) = client_origin(hwnd);
    if skip_same
        && (cx, cy) == (x, y)
        && client.right == i32::from(width)
        && client.bottom == i32::from(height)
    {
        return;
    }
    let style = WINDOW_STYLE(unsafe { GetWindowLongW(hwnd, GWL_STYLE) } as u32);
    let ex = WINDOW_EX_STYLE(unsafe { GetWindowLongW(hwnd, GWL_EXSTYLE) } as u32);
    let mut r = RECT {
        left: x,
        top: y,
        right: x + i32::from(width),
        bottom: y + i32::from(height),
    };
    unsafe {
        let dpi = GetDpiForWindow(hwnd);
        let _ = AdjustWindowRectExForDpi(&mut r, style, false, ex, dpi);
        let _ = SetWindowPos(
            hwnd,
            None,
            r.left,
            r.top,
            r.right - r.left,
            r.bottom - r.top,
            SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
}

fn lo(l: LPARAM) -> i32 {
    i32::from(l.0 as u16 as i16)
}

fn hi(l: LPARAM) -> i32 {
    i32::from((l.0 >> 16) as u16 as i16)
}

fn down(vk: i32) -> bool {
    (unsafe { GetKeyState(vk) }) < 0
}

extern "system" fn window_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // The session the window belongs to (0 before it is set).
    let session = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as u32;
    let handled = with_app(session, |app| {
        let id = *app.by_hwnd.get(&(hwnd.0 as isize))?;
        let r = on_window(app, id, hwnd, msg, wparam, lparam);
        Some((r, app.done))
    })
    .flatten();
    if let Some((_, true)) = handled {
        finish_if_done(session);
    }
    match handled.and_then(|(r, _)| r) {
        Some(r) => r,
        None => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// One message for a remote window; `None` lets Windows handle it.
fn on_window(
    app: &mut App,
    id: u32,
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> Option<LRESULT> {
    match msg {
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let r = app.windows.get(&id)?;
            unsafe {
                let hdc = BeginPaint(hwnd, &mut ps);
                let fb = &r.fb;
                let bmi = BITMAPINFO {
                    bmiHeader: BITMAPINFOHEADER {
                        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                        biWidth: i32::from(fb.width),
                        biHeight: -i32::from(fb.height),
                        biPlanes: 1,
                        biBitCount: 32,
                        biCompression: BI_RGB.0,
                        ..Default::default()
                    },
                    ..Default::default()
                };
                StretchDIBits(
                    hdc,
                    0,
                    0,
                    i32::from(fb.width),
                    i32::from(fb.height),
                    0,
                    0,
                    i32::from(fb.width),
                    i32::from(fb.height),
                    Some(fb.pixels.as_ptr().cast()),
                    &bmi,
                    DIB_RGB_COLORS,
                    SRCCOPY,
                );
                // Beyond the remote window (while the user drags it bigger).
                let mut client = RECT::default();
                let _ = GetClientRect(hwnd, &mut client);
                let (w, h) = (i32::from(fb.width), i32::from(fb.height));
                if client.right > w {
                    let r = RECT { left: w, ..client };
                    FillRect(hdc, &r, app.grey);
                }
                if client.bottom > h {
                    let r = RECT {
                        top: h,
                        right: w.min(client.right),
                        ..client
                    };
                    FillRect(hdc, &r, app.grey);
                }
                let _ = EndPaint(hwnd, &ps);
            }
            Some(LRESULT(0))
        }
        WM_ERASEBKGND => Some(LRESULT(1)),
        WM_SETCURSOR if (lparam.0 & 0xffff) as u32 == HTCLIENT => {
            let cursor = app.cursor?;
            unsafe {
                SetCursor(Some(cursor));
            }
            Some(LRESULT(1))
        }
        WM_MOUSEMOVE => {
            let (x, y) = (lo(lparam), hi(lparam));
            if let Some(r) = app.windows.get_mut(&id) {
                r.pointer = (x, y);
            }
            app.link.send(Message::PointerMove { window: id, x, y });
            Some(LRESULT(0))
        }
        WM_LBUTTONDOWN | WM_LBUTTONUP | WM_MBUTTONDOWN | WM_MBUTTONUP | WM_RBUTTONDOWN
        | WM_RBUTTONUP | WM_XBUTTONDOWN | WM_XBUTTONUP => {
            let (button, is_down) = match msg {
                WM_LBUTTONDOWN => (1, true),
                WM_LBUTTONUP => (1, false),
                WM_MBUTTONDOWN => (2, true),
                WM_MBUTTONUP => (2, false),
                WM_RBUTTONDOWN => (3, true),
                WM_RBUTTONUP => (3, false),
                // XBUTTON1 = back (8), XBUTTON2 = forward (9).
                _ => (
                    if (wparam.0 >> 16) & 0xffff == 1 { 8 } else { 9 },
                    msg == WM_XBUTTONDOWN,
                ),
            };
            let (x, y) = (lo(lparam), hi(lparam));
            unsafe {
                if is_down {
                    SetCapture(hwnd);
                } else {
                    let _ = ReleaseCapture();
                }
            }
            app.link.send(Message::PointerButton {
                window: id,
                x,
                y,
                button,
                down: is_down,
            });
            Some(LRESULT(if msg == WM_XBUTTONDOWN || msg == WM_XBUTTONUP {
                1
            } else {
                0
            }))
        }
        WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
            let delta = i32::from((wparam.0 >> 16) as u16 as i16);
            let mut p = POINT {
                x: lo(lparam),
                y: hi(lparam),
            };
            unsafe {
                let _ = ScreenToClient(hwnd, &mut p);
            }
            let (acc, up_button, down_button) = if msg == WM_MOUSEWHEEL {
                (&mut app.wheel, 4u8, 5u8)
            } else {
                // Horizontal: positive is right (button 7).
                (&mut app.hwheel, 7u8, 6u8)
            };
            *acc += delta;
            let mut clicks = Vec::new();
            while *acc >= 120 {
                *acc -= 120;
                clicks.push(up_button);
            }
            while *acc <= -120 {
                *acc += 120;
                clicks.push(down_button);
            }
            for button in clicks {
                for d in [true, false] {
                    app.link.send(Message::PointerButton {
                        window: id,
                        x: p.x,
                        y: p.y,
                        button,
                        down: d,
                    });
                }
            }
            Some(LRESULT(0))
        }
        WM_KEYDOWN | WM_SYSKEYDOWN | WM_KEYUP | WM_SYSKEYUP => {
            let is_down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
            let vk = wparam.0 as u16;
            let extended = (lparam.0 >> 24) & 1 == 1;
            let scan = ((lparam.0 >> 16) & 0xff) as u32;
            // Alt+F4 closes the window, as everywhere on Windows.
            if vk == 0x73 && down(0x12) {
                return None;
            }
            if keys::is_lock_key(vk) {
                return Some(LRESULT(0));
            }
            // AltGr: Windows sends a left Ctrl just before the right Alt,
            // with the same time stamp. Neither goes to the agent; the
            // characters AltGr types arrive as WM_CHAR.
            if vk == 0x11 && !extended && is_down && altgr_follows() {
                app.altgr = true;
                return Some(LRESULT(0));
            }
            if app.altgr && (vk == 0x11 && !extended || vk == 0x12 && extended) {
                if vk == 0x12 && !is_down {
                    app.altgr = false;
                }
                return Some(LRESULT(0));
            }
            let vk_side = match vk {
                0x10 if scan == 0x36 => 0xa1,
                0x10 => 0xa0,
                other => other,
            };
            if let Some(keysym) = keys::special_keysym(vk_side, extended) {
                app.key(keysym, is_down);
                return Some(LRESULT(0));
            }
            if !is_down {
                if let Some(keysym) = app.chords.remove(&vk) {
                    app.link.send(Message::Key {
                        keysym,
                        down: false,
                    });
                }
                return Some(LRESULT(0));
            }
            let chord = (down(0x11) || down(0x12)) && !app.altgr;
            if chord {
                let ch = unsafe { MapVirtualKeyW(u32::from(vk), MAPVK_VK_TO_CHAR) };
                if let Some(keysym) = keys::chord_keysym(vk, ch) {
                    app.chords.insert(vk, keysym);
                    app.link.send(Message::Key { keysym, down: true });
                }
                return Some(LRESULT(0));
            }
            // A key that types text: TranslateMessage turns it into WM_CHAR.
            None
        }
        WM_CHAR => {
            let unit = wparam.0 as u16;
            if (0xd800..0xdc00).contains(&unit) {
                app.high_surrogate = Some(unit);
                return Some(LRESULT(0));
            }
            let ch = match app.high_surrogate.take() {
                Some(high) if (0xdc00..0xe000).contains(&unit) => {
                    char::decode_utf16([high, unit]).next().and_then(Result::ok)
                }
                _ => char::from_u32(u32::from(unit)),
            };
            let Some(ch) = ch else {
                return Some(LRESULT(0));
            };
            let chord = (down(0x11) || down(0x12)) && !app.altgr;
            if !keys::char_sent_as_key(u32::from(ch)) && !chord {
                app.link.send(Message::Text { ch });
            }
            Some(LRESULT(0))
        }
        // Alt+letter: already sent as a chord; keep Windows from beeping or
        // opening a (non-existent) menu.
        WM_SYSCHAR => Some(LRESULT(0)),
        WM_ACTIVATE => {
            let active = (wparam.0 & 0xffff) as u32 != WA_INACTIVE;
            if active {
                app.app_active = true;
                app.push_clipboard();
                app.link.send(Message::Focus { window: id });
            } else {
                app.release_held();
            }
            None
        }
        WM_ACTIVATEAPP => {
            // Another program got the focus (not just another of our
            // windows): the agent closes open menus, as a click elsewhere
            // on a Linux desktop would.
            if wparam.0 == 0 && app.app_active {
                app.app_active = false;
                app.release_held();
                app.link.send(Message::Focus { window: 0 });
            }
            None
        }
        WM_DISPLAYCHANGE => {
            let screens = monitors(app.origin);
            if !screens.is_empty() && screens != app.monitors {
                app.monitors = screens.clone();
                app.link.send(Message::Monitors { monitors: screens });
            }
            None
        }
        WM_GETMINMAXINFO => {
            let r = app.windows.get(&id)?;
            if r.kind == WindowKind::Popup || (r.min == (0, 0) && r.max == (0, 0)) {
                return None;
            }
            // The limits are for the client area; Windows wants the frame.
            let frame = |w: u16, h: u16| {
                let mut rect = RECT {
                    left: 0,
                    top: 0,
                    right: i32::from(w),
                    bottom: i32::from(h),
                };
                unsafe {
                    let style = WINDOW_STYLE(GetWindowLongW(hwnd, GWL_STYLE) as u32);
                    let ex = WINDOW_EX_STYLE(GetWindowLongW(hwnd, GWL_EXSTYLE) as u32);
                    let _ = AdjustWindowRectExForDpi(
                        &mut rect,
                        style,
                        false,
                        ex,
                        GetDpiForWindow(hwnd),
                    );
                }
                (rect.right - rect.left, rect.bottom - rect.top)
            };
            // SAFETY: Windows passes a MINMAXINFO for this message.
            let info = unsafe { &mut *(lparam.0 as *mut MINMAXINFO) };
            let (minw, minh) = frame(r.min.0.max(1), r.min.1.max(1));
            if r.min.0 > 0 {
                info.ptMinTrackSize.x = minw;
            }
            if r.min.1 > 0 {
                info.ptMinTrackSize.y = minh;
            }
            let (maxw, maxh) = frame(r.max.0.max(1), r.max.1.max(1));
            if r.max.0 > 0 {
                info.ptMaxTrackSize.x = maxw;
            }
            if r.max.1 > 0 {
                info.ptMaxTrackSize.y = maxh;
            }
            Some(LRESULT(0))
        }
        WM_SIZE => {
            let r = app.windows.get_mut(&id)?;
            if r.kind == WindowKind::Popup {
                return None;
            }
            let state = match wparam.0 as u32 {
                SIZE_MINIMIZED => WindowState::Minimized,
                SIZE_MAXIMIZED => WindowState::Maximized,
                SIZE_RESTORED if r.restore.is_some() => WindowState::Fullscreen,
                SIZE_RESTORED => WindowState::Normal,
                _ => return None,
            };
            if state != r.state {
                r.state = state;
                app.link.send(Message::WindowState { window: id, state });
            }
            None
        }
        WM_CLOSE => {
            if app.away.is_some() {
                // No program to ask: the window goes here, the program
                // stays on the host; the last window closed ends the wait.
                app.destroy(id);
                if !app.windows.values().any(|r| r.kind != WindowKind::Popup) {
                    app.left_running = true;
                    app.done = true;
                }
                return Some(LRESULT(0));
            }
            // The program decides; the window goes when the agent says so.
            app.link.send(Message::WindowClose { window: id });
            Some(LRESULT(0))
        }
        WM_WINDOWPOSCHANGED => {
            let r = app.windows.get(&id)?;
            if r.kind != WindowKind::Popup {
                let mut client = RECT::default();
                unsafe {
                    let _ = GetClientRect(hwnd, &mut client);
                }
                let (x, y) = client_origin(hwnd);
                let side = |v: i32| v.clamp(1, i32::from(ferostream_proto::MAX_WINDOW_SIDE)) as u16;
                let minimized = unsafe { IsIconic(hwnd) }.as_bool();
                if !minimized && client.right > 0 && client.bottom > 0 {
                    app.link.send(Message::WindowMove {
                        window: id,
                        x: x - app.origin.0,
                        y: y - app.origin.1,
                        width: side(client.right),
                        height: side(client.bottom),
                    });
                }
            }
            None
        }
        _ => None,
    }
}

/// Whether the next queued key message is a right Alt with the current
/// message's time stamp (the left Ctrl was AltGr's).
fn altgr_follows() -> bool {
    let mut next = MSG::default();
    unsafe {
        let time = GetMessageTime();
        PeekMessageW(&mut next, None, WM_KEYDOWN, WM_SYSKEYDOWN, PM_NOREMOVE).as_bool()
            && (next.message == WM_KEYDOWN || next.message == WM_SYSKEYDOWN)
            && next.wParam.0 == 0x12
            && (next.lParam.0 >> 24) & 1 == 1
            && next.time as i32 == time
    }
}
