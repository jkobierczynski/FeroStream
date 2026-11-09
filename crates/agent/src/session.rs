//! Watching the private display: which windows exist, which pixels changed,
//! and injecting the client's input.
//!
//! Every top-level window is redirected off-screen (Composite), so each one
//! keeps all its pixels even where others overlap it. Damage reports what
//! changed; GetImage reads it back. The agent keeps a copy of what the
//! client has ([`crate::shadow`]): only pixels that really changed are sent,
//! scrolls go as copies, and tiles are packed as hard as the link calls for.
//! There is no real window manager: windows sit where the client puts them.
//! The agent does answer the questions programs ask one (EWMH): window
//! types, maximise and full-screen requests, icons, size limits.
//! XFixes reports pointer-shape changes, which go to the client as images,
//! and clipboard changes, whose text goes too. Text copied on Windows is
//! offered to programs with the agent as the selection owner.
//!
//! A client may go away and another come (a reconnect): without one, frames
//! are dropped and changes pile up as dirty rectangles; a new client gets
//! every window announced again and its pixels in full ([`Session::attach`]).

use std::collections::{BTreeMap, VecDeque};
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use ferostream_proto::image::{self, Image, MAX_IMAGE_SIDE};
use ferostream_proto::tile::{Effort, MAX_TILE_SIDE, Tile};
use ferostream_proto::{MAX_WINDOW_SIDE, Message, Packing, WindowKind, WindowState};
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::composite::{self, ConnectionExt as _};
use x11rb::protocol::damage::{self, ConnectionExt as _};
use x11rb::protocol::randr::{self, ConnectionExt as _};
use x11rb::protocol::xfixes::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{self, ConnectionExt as _};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use crate::clipboard;
use crate::keyboard::Keyboard;
use crate::shadow::{self, Rect, Shadow};
use crate::wm;

/// Frames a window may have unacknowledged before the agent waits for the
/// client (keeps a slow link from queueing stale pictures).
const MAX_IN_FLIGHT: u32 = 2;

/// More damaged rectangles than this in one window are sent as their
/// bounding box.
const MAX_RECTS: usize = 32;

/// More damaged rectangles than this waiting in one window (no client to
/// send them to) become the whole window.
const MAX_DIRTY: usize = 256;

/// Drawing must pause this long before pixels are read...
const SETTLE_QUIET: Duration = Duration::from_millis(4);
/// ...but a program that never pauses (an animation) is read at least this
/// often.
const SETTLE_MAX: Duration = Duration::from_millis(30);

/// Rows read from X in one GetImage, so one reply stays a few MB.
const BAND_ROWS: u16 = 128;

/// In [`Packing::Auto`], a link slower than this (bytes a second) gets
/// Deflate...
const SLOW_LINK: f64 = 3_000_000.0;
/// ...and one faster than this LZ4. In between, the choice stays.
const FAST_LINK: f64 = 8_000_000.0;

/// Frames smaller than this say too little about the link's speed.
const MIN_SAMPLE: usize = 16 * 1024;

/// How fast the link is, from how long frames take to be acknowledged.
#[derive(Debug)]
struct Link {
    /// Bytes a second, smoothed.
    rate: Option<f64>,
    /// Whether Auto packs with Deflate now. Until the link is measured it
    /// does: the first frames are the biggest, and a slow link is where they
    /// hurt.
    slow: bool,
}

impl Default for Link {
    fn default() -> Self {
        Link {
            rate: None,
            slow: true,
        }
    }
}

impl Link {
    fn sample(&mut self, bytes: usize, took: Duration) {
        if bytes < MIN_SAMPLE {
            return;
        }
        let r = bytes as f64 / took.as_secs_f64().max(0.000_1);
        let rate = match self.rate {
            Some(old) => old * 0.7 + r * 0.3,
            None => r,
        };
        self.rate = Some(rate);
        if rate < SLOW_LINK {
            self.slow = true;
        } else if rate > FAST_LINK {
            self.slow = false;
        }
    }
}

#[derive(Debug)]
struct Win {
    x: i16,
    y: i16,
    w: u16,
    h: u16,
    popup: bool,
    state: WindowState,
    damage: damage::Damage,
    dirty: Vec<Rect>,
    sent: u32,
    acked: u32,
    /// What the client has on screen for this window.
    shadow: Shadow,
    /// Frames not acknowledged yet: sequence number, when, how many bytes.
    in_flight: VecDeque<(u32, Instant, usize)>,
}

impl Win {
    fn full(&self) -> Rect {
        Rect {
            x: 0,
            y: 0,
            w: self.w,
            h: self.h,
        }
    }
}

#[derive(Debug, Default)]
struct State {
    windows: BTreeMap<u32, Win>,
    exited: Option<i32>,
    /// XFixes serial of the pointer shape last sent.
    cursor: Option<u32>,
    /// Text copied on Windows that the agent offers as the clipboard.
    offered: Option<String>,
    /// Clipboard text last sent to the client (not sent again).
    sent_clipboard: Option<String>,
    /// What the client lets the agent do with text copied on the host
    /// ([`Message::ClipboardRules`]); nothing until it says.
    clip_send: bool,
    clip_selection: bool,
    /// Copied text arriving in pieces (INCR), while it does, and the
    /// property it comes through.
    incr: Option<(xproto::Atom, clipboard::Incr)>,
    /// Monitors the agent set up on the display (RandR names).
    monitors: Vec<xproto::Atom>,
    /// How hard the client wants tiles packed.
    packing: Packing,
    link: Link,
}

impl State {
    fn effort(&self) -> Effort {
        match self.packing {
            Packing::Fast => Effort::Fast,
            Packing::Strong => Effort::Strong,
            Packing::Auto if self.link.slow => Effort::Strong,
            Packing::Auto => Effort::Fast,
        }
    }
}

/// Where frames go: the attached client, or nowhere while the session is
/// detached.
struct Out {
    sink: Option<Box<dyn Sink>>,
    /// Counts attaches, so a client that went away cannot detach the one
    /// that took its place.
    generation: u64,
}

/// Writes frames to the client.
pub trait Sink: Send {
    fn send(&mut self, frames: &[Message]) -> std::io::Result<()>;
}

impl<W: Write + Send> Sink for W {
    fn send(&mut self, frames: &[Message]) -> std::io::Result<()> {
        for m in frames {
            self.write_all(&m.encode())?;
        }
        self.flush()
    }
}

#[derive(Debug, Clone, Copy)]
struct Atoms {
    net_wm_name: xproto::Atom,
    utf8_string: xproto::Atom,
    wm_protocols: xproto::Atom,
    wm_delete_window: xproto::Atom,
    wake: xproto::Atom,
    wm_normal_hints: xproto::Atom,
}

/// The display, shared by the X event thread and the input thread.
pub struct Session {
    conn: RustConnection,
    root: xproto::Window,
    wake_window: xproto::Window,
    atoms: Atoms,
    wm: wm::Atoms,
    clip: clipboard::Atoms,
    /// Screen size, for clicks that must miss every window.
    screen: (u16, u16),
    /// The client screen's DPI: icons are picked for its size.
    dpi: u16,
    state: Mutex<State>,
    keyboard: Mutex<Keyboard>,
    out: Mutex<Out>,
    /// Damage arrived since the event loop last looked.
    fresh_damage: AtomicBool,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session").field("root", &self.root).finish()
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

impl Session {
    /// Sets up redirection and event selection on a fresh display.
    /// No client is attached yet: see [`Session::attach`].
    pub fn new(conn: RustConnection, screen: usize, dpi: u16) -> Result<Arc<Session>, String> {
        let root = conn
            .setup()
            .roots
            .get(screen)
            .ok_or("the X server has no screen 0")?
            .root;
        for (name, ok) in [
            (
                "Composite",
                conn.composite_query_version(0, 4)
                    .map_err(err)?
                    .reply()
                    .is_ok(),
            ),
            (
                "XFIXES",
                conn.xfixes_query_version(5, 0)
                    .map_err(err)?
                    .reply()
                    .is_ok(),
            ),
            (
                "DAMAGE",
                conn.damage_query_version(1, 1)
                    .map_err(err)?
                    .reply()
                    .is_ok(),
            ),
            (
                "XTEST",
                conn.xtest_get_version(2, 2).map_err(err)?.reply().is_ok(),
            ),
        ] {
            if !ok {
                return Err(format!("the X server lacks the {name} extension"));
            }
        }
        let intern = |name: &[u8]| -> Result<xproto::Atom, String> {
            Ok(conn
                .intern_atom(false, name)
                .map_err(err)?
                .reply()
                .map_err(err)?
                .atom)
        };
        let atoms = Atoms {
            net_wm_name: intern(b"_NET_WM_NAME")?,
            utf8_string: intern(b"UTF8_STRING")?,
            wm_protocols: intern(b"WM_PROTOCOLS")?,
            wm_delete_window: intern(b"WM_DELETE_WINDOW")?,
            wake: intern(b"_FEROSTREAM_WAKE")?,
            wm_normal_hints: xproto::AtomEnum::WM_NORMAL_HINTS.into(),
        };
        let mut list = [0u32; wm::ATOM_NAMES.len()];
        for (slot, name) in list.iter_mut().zip(wm::ATOM_NAMES) {
            *slot = intern(name)?;
        }
        let wm = wm::Atoms::from_list(&list);
        let mut c = [0u32; clipboard::ATOM_NAMES.len()];
        for (slot, name) in c.iter_mut().zip(clipboard::ATOM_NAMES) {
            *slot = intern(name)?;
        }
        let [
            clipboard,
            targets,
            utf8_string,
            text,
            text_plain_utf8,
            incr,
            property,
            primary_property,
        ] = c;
        let clip = clipboard::Atoms {
            clipboard,
            primary: xproto::AtomEnum::PRIMARY.into(),
            targets,
            utf8_string,
            string: xproto::AtomEnum::STRING.into(),
            text,
            text_plain_utf8,
            incr,
            property,
            primary_property,
        };
        let screen_size = conn
            .setup()
            .roots
            .get(screen)
            .map(|s| (s.width_in_pixels, s.height_in_pixels))
            .unwrap_or((1, 1));
        conn.change_window_attributes(
            root,
            &xproto::ChangeWindowAttributesAux::new()
                .event_mask(xproto::EventMask::SUBSTRUCTURE_NOTIFY),
        )
        .map_err(err)?;
        conn.composite_redirect_subwindows(root, composite::Redirect::MANUAL)
            .map_err(err)?;
        conn.xfixes_select_cursor_input(root, xfixes::CursorNotifyMask::DISPLAY_CURSOR)
            .map_err(err)?;
        // An unmapped InputOnly window the input thread sends ClientMessages
        // to, to wake the event thread (an acknowledged frame, a stop).
        let wake_window = conn.generate_id().map_err(err)?;
        conn.create_window(
            0,
            wake_window,
            root,
            -10,
            -10,
            1,
            1,
            0,
            xproto::WindowClass::INPUT_ONLY,
            0,
            &xproto::CreateWindowAux::new()
                .override_redirect(1)
                // Copied text over ~256 KiB arrives in pieces (INCR), each
                // announced by a property change.
                .event_mask(xproto::EventMask::PROPERTY_CHANGE),
        )
        .map_err(err)?;
        // Programs look for a window manager before they ask one to
        // maximise a window or go full screen (EWMH): the wake window
        // stands in as the "supporting WM check" window.
        conn.change_property32(
            xproto::PropMode::REPLACE,
            root,
            wm.net_supported,
            xproto::AtomEnum::ATOM,
            &wm.supported(),
        )
        .map_err(err)?;
        for w in [root, wake_window] {
            conn.change_property32(
                xproto::PropMode::REPLACE,
                w,
                wm.net_supporting_wm_check,
                xproto::AtomEnum::WINDOW,
                &[wake_window],
            )
            .map_err(err)?;
        }
        conn.change_property8(
            xproto::PropMode::REPLACE,
            wake_window,
            atoms.net_wm_name,
            atoms.utf8_string,
            b"FeroStream",
        )
        .map_err(err)?;
        for sel in [clip.clipboard, clip.primary] {
            conn.xfixes_select_selection_input(
                root,
                sel,
                xfixes::SelectionEventMask::SET_SELECTION_OWNER,
            )
            .map_err(err)?;
        }
        let keyboard = Keyboard::load(&conn)?;
        conn.sync().map_err(err)?;
        Ok(Arc::new(Session {
            conn,
            root,
            wake_window,
            atoms,
            wm,
            clip,
            screen: screen_size,
            dpi,
            state: Mutex::new(State::default()),
            keyboard: Mutex::new(keyboard),
            out: Mutex::new(Out {
                sink: None,
                generation: 0,
            }),
            fresh_damage: AtomicBool::new(false),
        }))
    }

    /// Sends to the attached client. A client that cannot take frames any
    /// more is let go: the session goes on without one (detached).
    fn send(&self, frames: &[Message]) -> Result<(), String> {
        let mut out = lock(&self.out);
        if let Some(sink) = out.sink.as_mut()
            && sink.send(frames).is_err()
        {
            out.sink = None;
        }
        Ok(())
    }

    /// Sends the last message of the session.
    pub fn finish(&self, last: &Message) {
        if let Some(sink) = lock(&self.out).sink.as_mut() {
            let _ = sink.send(std::slice::from_ref(last));
        }
    }

    /// Whether a client is attached.
    pub fn attached(&self) -> bool {
        lock(&self.out).sink.is_some()
    }

    /// Makes `sink` the session's client, in place of any other, and tells
    /// it everything a new client needs: [`Message::Session`], every window
    /// as if it were new, the pointer shape; the pixels follow. Returns the
    /// attach's number for [`Session::detach`].
    pub fn attach(&self, mut sink: Box<dyn Sink>, id: &str) -> u64 {
        // Holding `out` throughout: whatever the event thread sends meanwhile
        // (a window that just appeared) waits and comes after this.
        let mut out = lock(&self.out);
        let windows: Vec<(u32, i16, i16, u16, u16, bool, WindowState)> = {
            let mut st = lock(&self.state);
            st.cursor = None;
            // A new client, maybe over another link, with its own setting.
            st.packing = Packing::Auto;
            st.clip_send = false;
            st.clip_selection = false;
            st.incr = None;
            st.link = Link::default();
            for w in st.windows.values_mut() {
                w.dirty = vec![w.full()];
                w.sent = 0;
                w.acked = 0;
                // The client starts each window grey again.
                w.shadow = Shadow::new(w.w, w.h);
                w.in_flight.clear();
            }
            st.windows
                .iter()
                .map(|(&id, w)| (id, w.x, w.y, w.w, w.h, w.popup, w.state))
                .collect()
        };
        let mut frames = vec![Message::Session {
            id: id.into(),
            windows: windows.iter().map(|w| w.0).collect(),
        }];
        for (window, x, y, width, height, popup, state) in windows {
            let (kind, transient_for) = self.kind(window, popup);
            frames.push(Message::WindowNew {
                window,
                x: i32::from(x),
                y: i32::from(y),
                width,
                height,
                kind,
                transient_for,
                title: self.title(window),
            });
            if !popup {
                frames.push(self.hints(window));
                frames.extend(self.icons(window));
                if state != WindowState::Normal {
                    frames.push(Message::WindowState { window, state });
                }
            }
        }
        if let Some((serial, m)) = self.cursor_message() {
            lock(&self.state).cursor = Some(serial);
            frames.push(m);
        }
        let ok = sink.send(&frames).is_ok();
        out.generation += 1;
        out.sink = ok.then_some(sink);
        let generation = out.generation;
        drop(out);
        self.wake();
        generation
    }

    /// The client of attach number `generation` went away. Returns whether
    /// the session is now detached (`false`: another client has taken over).
    pub fn detach(&self, generation: u64) -> bool {
        let mut out = lock(&self.out);
        if out.generation != generation {
            return false;
        }
        out.sink = None;
        drop(out);
        // Keys the client held when it went away would stay down.
        lock(&self.keyboard).release_all(&self.conn, self.root);
        let _ = self.conn.flush();
        true
    }

    /// Wakes the event thread.
    pub fn wake(&self) {
        let ev = xproto::ClientMessageEvent::new(32, self.wake_window, self.atoms.wake, [0u32; 5]);
        let _ = self
            .conn
            .send_event(false, self.wake_window, xproto::EventMask::NO_EVENT, ev);
        let _ = self.conn.flush();
    }

    /// The program ended with `code`; the session ends when its windows
    /// are gone too.
    pub fn program_exited(&self, code: i32) {
        lock(&self.state).exited = Some(code);
        self.wake();
    }

    /// Runs the event loop until the program and all its windows are gone.
    /// Returns the program's exit code.
    pub fn run(&self) -> Result<i32, String> {
        loop {
            let ev = self.conn.wait_for_event().map_err(err)?;
            self.handle(ev)?;
            // A program often draws one picture in several steps (clear,
            // background, layers). Reading pixels between steps would send a
            // half-drawn frame, so wait until drawing pauses briefly.
            let started = Instant::now();
            loop {
                while let Some(ev) = self.conn.poll_for_event().map_err(err)? {
                    self.handle(ev)?;
                }
                if !self.fresh_damage.swap(false, Ordering::Relaxed)
                    || started.elapsed() >= SETTLE_MAX
                {
                    break;
                }
                std::thread::sleep(SETTLE_QUIET);
            }
            self.flush_damage()?;
            let st = lock(&self.state);
            if let Some(code) = st.exited
                && st.windows.is_empty()
            {
                return Ok(code);
            }
        }
    }

    fn handle(&self, ev: Event) -> Result<(), String> {
        match ev {
            Event::MapNotify(e) if e.event == self.root && e.window != self.wake_window => {
                self.add_window(e.window, e.override_redirect)?;
            }
            Event::UnmapNotify(e) if e.event == self.root => self.remove_window(e.window, true)?,
            Event::DestroyNotify(e) if e.event == self.root => {
                self.remove_window(e.window, false)?
            }
            Event::ConfigureNotify(e) if e.event == self.root => {
                let mut st = lock(&self.state);
                if let Some(w) = st.windows.get_mut(&e.window) {
                    let (nw, nh) = (clamp_side(e.width), clamp_side(e.height));
                    let resized = (nw, nh) != (w.w, w.h);
                    let moved = (e.x, e.y) != (w.x, w.y);
                    w.x = e.x;
                    w.y = e.y;
                    w.w = nw;
                    w.h = nh;
                    if resized {
                        // The client resizes its picture the same way.
                        w.shadow.resize(nw, nh);
                        w.dirty.clear();
                        let full = w.full();
                        w.dirty.push(full);
                    }
                    if resized || moved {
                        let m = Message::WindowConfigure {
                            window: e.window,
                            x: i32::from(e.x),
                            y: i32::from(e.y),
                            width: nw,
                            height: nh,
                        };
                        drop(st);
                        self.send(&[m])?;
                    }
                }
            }
            Event::DamageNotify(e) => {
                self.fresh_damage.store(true, Ordering::Relaxed);
                let mut st = lock(&self.state);
                if let Some(w) = st.windows.get_mut(&e.drawable) {
                    if w.dirty.len() >= MAX_DIRTY {
                        w.dirty = vec![w.full()];
                    } else {
                        w.dirty.push(Rect {
                            x: e.area.x.max(0) as u16,
                            y: e.area.y.max(0) as u16,
                            w: e.area.width,
                            h: e.area.height,
                        });
                    }
                }
            }
            Event::XfixesCursorNotify(e) if lock(&self.state).cursor != Some(e.cursor_serial) => {
                self.send_cursor()?;
            }
            Event::PropertyNotify(e)
                if e.window == self.wake_window
                    && e.state == xproto::Property::NEW_VALUE
                    && lock(&self.state)
                        .incr
                        .as_ref()
                        .is_some_and(|i| i.0 == e.atom) =>
            {
                self.read_piece(e.atom)?;
            }
            Event::PropertyNotify(e) if lock(&self.state).windows.contains_key(&e.window) => {
                if e.atom == self.atoms.net_wm_name
                    || e.atom == u32::from(xproto::AtomEnum::WM_NAME)
                {
                    let title = self.title(e.window);
                    self.send(&[Message::WindowTitle {
                        window: e.window,
                        title,
                    }])?;
                } else if e.atom == self.wm.net_wm_icon {
                    self.send(&self.icons(e.window))?;
                } else if e.atom == self.atoms.wm_normal_hints {
                    self.send(&[self.hints(e.window)])?;
                }
            }
            Event::ClientMessage(e) if e.format == 32 => self.client_message(&e)?,
            Event::XfixesSelectionNotify(e)
                if e.owner != self.wake_window
                    && e.owner != x11rb::NONE
                    && self.wants(e.selection) =>
            {
                // A program copied (or, for PRIMARY, selected): ask it for
                // the text as UTF-8. A transfer still in pieces is dropped.
                let property = self.clip.property_for(e.selection);
                {
                    let mut st = lock(&self.state);
                    if st.incr.as_ref().is_some_and(|i| i.0 == property) {
                        st.incr = None;
                    }
                }
                let _ = self.conn.convert_selection(
                    self.wake_window,
                    e.selection,
                    self.clip.utf8_string,
                    property,
                    e.selection_timestamp,
                );
                let _ = self.conn.flush();
            }
            Event::SelectionNotify(e)
                if e.requestor == self.wake_window
                    && (e.property == self.clip.property
                        || e.property == self.clip.primary_property) =>
            {
                self.read_copied(e.property)?;
            }
            Event::SelectionRequest(e) if e.owner == self.wake_window => self.give_clipboard(&e),
            Event::SelectionClear(e) if e.selection == self.clip.clipboard => {
                lock(&self.state).offered = None;
            }
            _ => {}
        }
        Ok(())
    }

    fn add_window(&self, window: xproto::Window, override_redirect: bool) -> Result<(), String> {
        if lock(&self.state).windows.contains_key(&window) {
            return Ok(());
        }
        let Ok(Ok(attrs)) = self.conn.get_window_attributes(window).map(|c| c.reply()) else {
            return Ok(());
        };
        if attrs.class == xproto::WindowClass::INPUT_ONLY {
            return Ok(());
        }
        let Ok(Ok(geo)) = self.conn.get_geometry(window).map(|c| c.reply()) else {
            return Ok(());
        };
        let _ = self.conn.change_window_attributes(
            window,
            &xproto::ChangeWindowAttributesAux::new()
                .event_mask(xproto::EventMask::PROPERTY_CHANGE),
        );
        let damage = self.conn.generate_id().map_err(err)?;
        self.conn
            .damage_create(damage, window, damage::ReportLevel::DELTA_RECTANGLES)
            .map_err(err)?;
        let title = self.title(window);
        let (kind, transient_for) = self.kind(window, override_redirect);
        let state = if kind == WindowKind::Popup {
            WindowState::Normal
        } else {
            self.initial_state(window)
        };
        let (w, h) = (clamp_side(geo.width), clamp_side(geo.height));
        let win = Win {
            x: geo.x,
            y: geo.y,
            w,
            h,
            popup: kind == WindowKind::Popup,
            state,
            damage,
            dirty: vec![Rect { x: 0, y: 0, w, h }],
            sent: 0,
            acked: 0,
            shadow: Shadow::new(w, h),
            in_flight: VecDeque::new(),
        };
        lock(&self.state).windows.insert(window, win);
        let mut frames = vec![Message::WindowNew {
            window,
            x: i32::from(geo.x),
            y: i32::from(geo.y),
            width: w,
            height: h,
            kind,
            transient_for,
            title,
        }];
        if kind != WindowKind::Popup {
            self.write_state(window, state);
            frames.push(self.hints(window));
            frames.extend(self.icons(window));
            if state != WindowState::Normal {
                frames.push(Message::WindowState { window, state });
            }
        }
        self.send(&frames)
    }

    /// What kind of window it is, and the window it belongs to (or 0).
    fn kind(&self, window: xproto::Window, override_redirect: bool) -> (WindowKind, u32) {
        let transient_for = self.transient_for(window);
        let kind = if override_redirect {
            WindowKind::Popup
        } else {
            let types = self.atom_list(window, self.wm.net_wm_window_type);
            self.wm.kind(&types, transient_for != 0)
        };
        (kind, transient_for)
    }

    /// A list of atoms in a window property (a window type, a state).
    fn atom_list(&self, window: xproto::Window, prop: xproto::Atom) -> Vec<u32> {
        self.conn
            .get_property(false, window, prop, xproto::AtomEnum::ATOM, 0, 64)
            .ok()
            .and_then(|c| c.reply().ok())
            .and_then(|r| r.value32().map(Iterator::collect))
            .unwrap_or_default()
    }

    /// The state a program asked for before showing its window.
    fn initial_state(&self, window: xproto::Window) -> WindowState {
        let set = self.atom_list(window, self.wm.net_wm_state);
        if set.contains(&self.wm.net_wm_state_fullscreen) {
            WindowState::Fullscreen
        } else if set.contains(&self.wm.net_wm_state_maximized_vert)
            && set.contains(&self.wm.net_wm_state_maximized_horz)
        {
            WindowState::Maximized
        } else {
            WindowState::Normal
        }
    }

    /// Records a window's state where programs read it: `_NET_WM_STATE`
    /// and ICCCM `WM_STATE` (normal 1, iconic 3).
    fn write_state(&self, window: xproto::Window, state: WindowState) {
        let _ = self.conn.change_property32(
            xproto::PropMode::REPLACE,
            window,
            self.wm.net_wm_state,
            xproto::AtomEnum::ATOM,
            &self.wm.state_atoms(state),
        );
        let iccm = if state == WindowState::Minimized {
            3
        } else {
            1
        };
        let _ = self.conn.change_property32(
            xproto::PropMode::REPLACE,
            window,
            self.wm.wm_state,
            self.wm.wm_state,
            &[iccm, x11rb::NONE],
        );
    }

    /// The window's size limits.
    fn hints(&self, window: xproto::Window) -> Message {
        let raw: Vec<u32> = self
            .conn
            .get_property(
                false,
                window,
                self.atoms.wm_normal_hints,
                xproto::AtomEnum::WM_SIZE_HINTS,
                0,
                18,
            )
            .ok()
            .and_then(|c| c.reply().ok())
            .and_then(|r| r.value32().map(Iterator::collect))
            .unwrap_or_default();
        let (min_width, min_height, max_width, max_height) = wm::size_limits(&raw);
        Message::WindowHints {
            window,
            min_width,
            min_height,
            max_width,
            max_height,
        }
    }

    /// The window's icons for the taskbar and the title bar, sized for the
    /// client screen.
    fn icons(&self, window: xproto::Window) -> Vec<Message> {
        // At most 4 MiB of icons (a few 256 x 256 ones).
        let value: Vec<u32> = self
            .conn
            .get_property(
                false,
                window,
                self.wm.net_wm_icon,
                xproto::AtomEnum::CARDINAL,
                0,
                1 << 20,
            )
            .ok()
            .and_then(|c| c.reply().ok())
            .and_then(|r| r.value32().map(Iterator::collect))
            .unwrap_or_default();
        let scale = |side: u32| (side * u32::from(self.dpi) / 96).min(256) as u16;
        let mut out = Vec::new();
        for (big, side) in [(true, scale(32)), (false, scale(16))] {
            if let Some(image) = wm::pick_icon(&value, side) {
                out.push(Message::WindowIcon { window, big, image });
            }
        }
        out
    }

    /// Whether text that goes into `selection` is passed on to the client.
    fn wants(&self, selection: xproto::Atom) -> bool {
        let st = lock(&self.state);
        st.clip_send
            && (selection == self.clip.clipboard
                || (selection == self.clip.primary && st.clip_selection))
    }

    /// Reads one piece of an INCR transfer; the empty piece ends it.
    fn read_piece(&self, property: xproto::Atom) -> Result<(), String> {
        // Read and delete the piece (deleting asks the program for the next
        // one), at most what still fits under the limit, plus one byte to
        // tell a piece that is too big.
        let Ok(Ok(r)) = self
            .conn
            .get_property(
                true,
                self.wake_window,
                property,
                xproto::AtomEnum::ANY,
                0,
                (ferostream_proto::MAX_CLIPBOARD / 4 + 1) as u32,
            )
            .map(|c| c.reply())
        else {
            lock(&self.state).incr = None;
            return Ok(());
        };
        let done = {
            let mut st = lock(&self.state);
            let Some((_, incr)) = st.incr.as_mut().filter(|i| i.0 == property) else {
                return Ok(());
            };
            match incr.piece(r.format, r.bytes_after, &r.value) {
                clipboard::Piece::More => None,
                clipboard::Piece::Done(text) => {
                    st.incr = None;
                    Some(text)
                }
            }
        };
        let _ = self.conn.flush();
        match done {
            Some(Some(text)) => self.pass_on(text),
            _ => Ok(()),
        }
    }

    /// Reads the text a program handed over after a copy, and passes it on.
    fn read_copied(&self, property: xproto::Atom) -> Result<(), String> {
        let max = (ferostream_proto::MAX_CLIPBOARD / 4) as u32;
        let Ok(Ok(r)) = self
            .conn
            .get_property(
                true,
                self.wake_window,
                property,
                xproto::AtomEnum::ANY,
                0,
                max,
            )
            .map(|c| c.reply())
        else {
            return Ok(());
        };
        // INCR is a transfer in pieces, for text over ~256 KiB: deleting
        // the property (done by reading it) starts it; each piece then
        // arrives as a property change ([`Self::read_piece`]). Anything
        // that is not 8-bit text is not passed on.
        if r.type_ == self.clip.incr {
            let mut st = lock(&self.state);
            if st.clip_send {
                st.incr = Some((property, clipboard::Incr::default()));
            }
            return Ok(());
        }
        if r.format != 8 || r.bytes_after > 0 {
            return Ok(());
        }
        self.pass_on(String::from_utf8_lossy(&r.value).into_owned())
    }

    /// Sends text a program copied to the client, unless it is the text
    /// already sent or the text the client offered.
    fn pass_on(&self, text: String) -> Result<(), String> {
        let mut st = lock(&self.state);
        if !st.clip_send {
            return Ok(());
        }
        if text.is_empty()
            || st.sent_clipboard.as_deref() == Some(&text)
            || st.offered.as_deref() == Some(&text)
        {
            return Ok(());
        }
        st.sent_clipboard = Some(text.clone());
        drop(st);
        self.send(&[Message::Clipboard { text }])
    }

    /// A program pastes text copied on Windows.
    fn give_clipboard(&self, e: &xproto::SelectionRequestEvent) {
        let property = if e.property == x11rb::NONE {
            e.target
        } else {
            e.property
        };
        let offered = lock(&self.state).offered.clone();
        let answer = offered.and_then(|t| clipboard::answer(&self.clip, e.target, &t));
        let ok = match answer {
            Some((ty, clipboard::Data::Atoms(atoms))) => self
                .conn
                .change_property32(xproto::PropMode::REPLACE, e.requestor, property, ty, &atoms)
                .is_ok(),
            Some((ty, clipboard::Data::Text(bytes))) => self
                .conn
                .change_property8(xproto::PropMode::REPLACE, e.requestor, property, ty, &bytes)
                .is_ok(),
            None => false,
        };
        let notify = xproto::SelectionNotifyEvent {
            response_type: xproto::SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: e.time,
            requestor: e.requestor,
            selection: e.selection,
            target: e.target,
            property: if ok { property } else { x11rb::NONE },
        };
        let _ = self
            .conn
            .send_event(false, e.requestor, xproto::EventMask::NO_EVENT, notify);
        let _ = self.conn.flush();
    }

    /// Clicks where no window is, so a program with an open menu (which
    /// holds the pointer) closes it, as clicking elsewhere would.
    fn close_menus(&self) {
        let rects: Vec<(i32, i32, u16, u16)> = {
            let st = lock(&self.state);
            if !st.windows.values().any(|w| w.popup) {
                return;
            }
            st.windows
                .values()
                .map(|w| (i32::from(w.x), i32::from(w.y), w.w, w.h))
                .collect()
        };
        let Some((x, y)) = clipboard::free_point(self.screen.0, self.screen.1, &rects) else {
            return;
        };
        let at = x11rb::CURRENT_TIME;
        let _ = self
            .conn
            .xtest_fake_input(xproto::MOTION_NOTIFY_EVENT, 0, at, self.root, x, y, 0);
        for kind in [xproto::BUTTON_PRESS_EVENT, xproto::BUTTON_RELEASE_EVENT] {
            let _ = self.conn.xtest_fake_input(kind, 1, at, self.root, 0, 0, 0);
        }
    }

    /// Mirrors the client's screens as RandR monitors, so programs keep
    /// menus and dialogs on one screen.
    fn set_monitors(&self, monitors: &[ferostream_proto::Monitor]) {
        if !self
            .conn
            .randr_query_version(1, 5)
            .ok()
            .and_then(|c| c.reply().ok())
            .is_some_and(|v| v.major_version > 1 || v.minor_version >= 5)
        {
            return;
        }
        let old = std::mem::take(&mut lock(&self.state).monitors);
        for name in old {
            let _ = self.conn.randr_delete_monitor(self.root, name);
        }
        // Xvfb has an automatic monitor covering the whole screen, made from
        // its one output. A monitor that names that output takes its place;
        // only the first one may, or each would replace the one before.
        let outputs = self
            .conn
            .randr_get_screen_resources_current(self.root)
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|r| r.outputs)
            .unwrap_or_default();
        let mut names = Vec::new();
        for (i, m) in monitors.iter().enumerate() {
            let Ok(Ok(name)) = self
                .conn
                .intern_atom(false, format!("FEROSTREAM-{i}").as_bytes())
                .map(|c| c.reply())
            else {
                continue;
            };
            // Size in millimetres at the session's DPI.
            let mm = |px: u16| (u32::from(px) * 254 / (u32::from(self.dpi) * 10)).max(1);
            let info = randr::MonitorInfo {
                name: name.atom,
                primary: i == 0,
                automatic: false,
                x: m.x.min(i16::MAX as u16) as i16,
                y: m.y.min(i16::MAX as u16) as i16,
                width: m.width,
                height: m.height,
                width_in_millimeters: mm(m.width),
                height_in_millimeters: mm(m.height),
                outputs: if i == 0 { outputs.clone() } else { Vec::new() },
            };
            if self.conn.randr_set_monitor(self.root, info).is_ok() {
                names.push(name.atom);
            }
        }
        lock(&self.state).monitors = names;
    }

    /// Requests programs send to the window manager (to the root window).
    fn client_message(&self, e: &xproto::ClientMessageEvent) -> Result<(), String> {
        let d = e.data.as_data32();
        let window = e.window;
        let current = match lock(&self.state).windows.get(&window) {
            Some(w) if !w.popup => w.state,
            _ => return Ok(()),
        };
        if e.type_ == self.wm.net_active_window {
            return self.send(&[Message::WindowActivate { window }]);
        }
        let new = if e.type_ == self.wm.net_wm_state {
            self.wm.apply_state_request(current, d[0], d[1], d[2])
        } else if e.type_ == self.wm.wm_change_state && d[0] == 3 {
            WindowState::Minimized
        } else if e.type_ == self.wm.net_close_window {
            self.close(window);
            return Ok(());
        } else {
            return Ok(());
        };
        if new == current {
            return Ok(());
        }
        if let Some(w) = lock(&self.state).windows.get_mut(&window) {
            w.state = new;
        }
        self.write_state(window, new);
        self.send(&[Message::WindowState { window, state: new }])
    }

    /// Sends the pointer shape the display shows now.
    fn send_cursor(&self) -> Result<(), String> {
        let Some((serial, m)) = self.cursor_message() else {
            return Ok(());
        };
        lock(&self.state).cursor = Some(serial);
        self.send(&[m])
    }

    /// The pointer shape the display shows now, with its XFixes serial.
    fn cursor_message(&self) -> Option<(u32, Message)> {
        let c = self.conn.xfixes_get_cursor_image().ok()?.reply().ok()?;
        let count = usize::from(c.width) * usize::from(c.height);
        if c.width == 0
            || c.height == 0
            || c.width > MAX_IMAGE_SIDE
            || c.height > MAX_IMAGE_SIDE
            || c.cursor_image.len() != count
        {
            return None;
        }
        let pixels = image::from_premultiplied_argb(&c.cursor_image);
        Some((
            c.cursor_serial,
            Message::Cursor {
                hot_x: c.xhot.min(c.width - 1),
                hot_y: c.yhot.min(c.height - 1),
                image: Image::new(c.width, c.height, &pixels),
            },
        ))
    }

    fn remove_window(&self, window: xproto::Window, alive: bool) -> Result<(), String> {
        let Some(w) = lock(&self.state).windows.remove(&window) else {
            return Ok(());
        };
        if alive {
            let _ = self.conn.damage_destroy(w.damage);
        }
        self.send(&[Message::WindowGone { window }])
    }

    fn title(&self, window: xproto::Window) -> String {
        let get = |prop: xproto::Atom, ty: xproto::Atom| {
            self.conn
                .get_property(false, window, prop, ty, 0, 1024)
                .ok()
                .and_then(|c| c.reply().ok())
                .filter(|r| r.format == 8 && !r.value.is_empty())
                .map(|r| r.value)
        };
        if let Some(v) = get(self.atoms.net_wm_name, self.atoms.utf8_string) {
            return String::from_utf8_lossy(&v).into_owned();
        }
        if let Some(v) = get(
            xproto::AtomEnum::WM_NAME.into(),
            xproto::AtomEnum::ANY.into(),
        ) {
            // STRING is Latin-1; COMPOUND_TEXT mostly is too for titles.
            return v.iter().map(|&b| char::from(b)).collect();
        }
        String::new()
    }

    fn transient_for(&self, window: xproto::Window) -> u32 {
        self.conn
            .get_property(
                false,
                window,
                xproto::AtomEnum::WM_TRANSIENT_FOR,
                xproto::AtomEnum::WINDOW,
                0,
                1,
            )
            .ok()
            .and_then(|c| c.reply().ok())
            .and_then(|r| r.value32().and_then(|mut v| v.next()))
            .unwrap_or(0)
    }

    /// Sends what changed in every window that is not waiting on the client.
    fn flush_damage(&self) -> Result<(), String> {
        // Detached: the changes wait for the next client.
        if !self.attached() {
            return Ok(());
        }
        let mut work = Vec::new();
        {
            let mut st = lock(&self.state);
            for (&id, w) in st.windows.iter_mut() {
                if w.dirty.is_empty() || w.sent.wrapping_sub(w.acked) >= MAX_IN_FLIGHT {
                    continue;
                }
                let rects = merge(std::mem::take(&mut w.dirty), w.w, w.h);
                w.sent = w.sent.wrapping_add(1);
                work.push((id, w.damage, rects, w.sent));
            }
        }
        for (id, damage, rects, seq) in work {
            // Re-arm damage reporting before reading, so a change made while
            // we read is reported again rather than lost.
            let _ = self.conn.damage_subtract(damage, x11rb::NONE, x11rb::NONE);
            let mut asked = Vec::new();
            for r in rects {
                let mut bands = Vec::new();
                let mut y = 0;
                while y < r.h {
                    let h = (r.h - y).min(BAND_ROWS);
                    let c = self
                        .conn
                        .get_image(
                            xproto::ImageFormat::Z_PIXMAP,
                            id,
                            r.x as i16,
                            (r.y + y) as i16,
                            r.w,
                            h,
                            !0,
                        )
                        .map_err(err)?;
                    bands.push((usize::from(r.w) * usize::from(h) * 4, c));
                    y += h;
                }
                asked.push((r, bands));
            }
            let mut pictures = Vec::with_capacity(asked.len());
            for (r, bands) in asked {
                let mut px = Vec::with_capacity(usize::from(r.w) * usize::from(r.h) * 4);
                let mut whole = true;
                for (want, c) in bands {
                    // A window that went away or shrank meanwhile answers
                    // with an error or the wrong size.
                    match c.reply() {
                        Ok(img) if img.data.len() == want => px.extend_from_slice(&img.data),
                        _ => whole = false,
                    }
                }
                if whole {
                    // The unused byte: 0, as every tile decodes it.
                    for p in px.chunks_exact_mut(4) {
                        if let Some(b) = p.get_mut(3) {
                            *b = 0;
                        }
                    }
                    pictures.push((r, px));
                }
            }
            // Compare, pack and send with `out` held: an attach in between
            // would give the new client copies of pixels it never had.
            let mut out = lock(&self.out);
            let (mut shadow, effort) = {
                let mut st = lock(&self.state);
                let effort = st.effort();
                let Some(w) = st.windows.get_mut(&id) else {
                    continue;
                };
                (std::mem::take(&mut w.shadow), effort)
            };
            let mut frames = Vec::new();
            let mut bytes = 0;
            for (r, px) in pictures {
                if let Some(sc) = shadow.find_scroll(r, &px) {
                    frames.push(Message::TileCopy {
                        window: id,
                        x: sc.x,
                        y: sc.from_y,
                        width: sc.w,
                        height: sc.rows,
                        to_x: sc.x,
                        to_y: sc.to_y,
                    });
                    bytes += 17;
                    shadow.copy(sc);
                }
                for c in shadow.changed(r, &px) {
                    for t in split(c) {
                        let sub = shadow::crop(&px, r, t);
                        let tile = Tile::pack(id, t.x, t.y, t.w, t.h, &sub, effort);
                        bytes += tile.data.len() + 18;
                        shadow.store(t, &sub);
                        frames.push(Message::Tile(tile));
                    }
                }
            }
            frames.push(Message::FrameEnd { window: id, seq });
            {
                let mut st = lock(&self.state);
                if let Some(w) = st.windows.get_mut(&id) {
                    w.shadow = shadow;
                    w.in_flight.push_back((seq, Instant::now(), bytes));
                    while w.in_flight.len() > MAX_IN_FLIGHT as usize + 1 {
                        w.in_flight.pop_front();
                    }
                }
            }
            if let Some(sink) = out.sink.as_mut()
                && sink.send(&frames).is_err()
            {
                out.sink = None;
            }
        }
        Ok(())
    }

    // ----- Input from the client (called on the input thread) -----

    /// Handles one message from the client.
    pub fn input(&self, m: Message) -> Result<(), String> {
        match m {
            Message::PointerMove { window, x, y } => self.pointer(window, x, y),
            Message::PointerButton {
                window,
                x,
                y,
                button,
                down,
            } => {
                self.pointer(window, x, y);
                let kind = if down {
                    xproto::BUTTON_PRESS_EVENT
                } else {
                    xproto::BUTTON_RELEASE_EVENT
                };
                if (1..=9).contains(&button) {
                    let _ = self.conn.xtest_fake_input(
                        kind,
                        button,
                        x11rb::CURRENT_TIME,
                        self.root,
                        0,
                        0,
                        0,
                    );
                }
            }
            Message::Key { keysym, down } => {
                lock(&self.keyboard).key(&self.conn, self.root, keysym, down);
            }
            Message::Text { ch } => lock(&self.keyboard).text(&self.conn, self.root, ch),
            Message::FrameAck { window, seq } => {
                let mut st = lock(&self.state);
                if let Some(w) = st.windows.get_mut(&window) {
                    // Only a sequence number we sent counts.
                    if w.sent.wrapping_sub(seq) < MAX_IN_FLIGHT + 1 {
                        w.acked = seq;
                    }
                    let mut done = None;
                    while let Some(&(s, at, bytes)) = w.in_flight.front()
                        && seq.wrapping_sub(s) <= MAX_IN_FLIGHT
                    {
                        w.in_flight.pop_front();
                        done = Some((at, bytes));
                    }
                    let dirty = !w.dirty.is_empty();
                    if let Some((at, bytes)) = done {
                        st.link.sample(bytes, at.elapsed());
                    }
                    if dirty {
                        drop(st);
                        self.wake();
                    }
                }
            }
            Message::Packing { mode } => {
                lock(&self.state).packing = mode;
            }
            Message::ClipboardRules { send, selection } => {
                let mut st = lock(&self.state);
                st.clip_send = send;
                st.clip_selection = send && selection;
                if !send {
                    st.incr = None;
                }
            }
            Message::Focus { window } => {
                let normal = lock(&self.state)
                    .windows
                    .get(&window)
                    .is_some_and(|w| !w.popup);
                let active = if normal { window } else { x11rb::NONE };
                if normal || window == 0 {
                    let _ = self.conn.change_property32(
                        xproto::PropMode::REPLACE,
                        self.root,
                        self.wm.net_active_window,
                        xproto::AtomEnum::WINDOW,
                        &[active],
                    );
                }
                if normal {
                    let _ = self.conn.set_input_focus(
                        xproto::InputFocus::PARENT,
                        window,
                        x11rb::CURRENT_TIME,
                    );
                    let _ = self.conn.configure_window(
                        window,
                        &xproto::ConfigureWindowAux::new().stack_mode(xproto::StackMode::ABOVE),
                    );
                } else if window == 0 {
                    lock(&self.keyboard).release_all(&self.conn, self.root);
                    // Like a desktop with the focus in another program: no
                    // window has it, and open menus close.
                    let _ = self.conn.set_input_focus(
                        xproto::InputFocus::NONE,
                        x11rb::NONE,
                        x11rb::CURRENT_TIME,
                    );
                    self.close_menus();
                }
            }
            Message::WindowMove {
                window,
                x,
                y,
                width,
                height,
            } => {
                if lock(&self.state).windows.contains_key(&window) {
                    let _ = self.conn.configure_window(
                        window,
                        &xproto::ConfigureWindowAux::new()
                            .x(x)
                            .y(y)
                            .width(u32::from(width))
                            .height(u32::from(height)),
                    );
                }
            }
            Message::WindowClose { window } => self.close(window),
            Message::WindowState { window, state } => {
                let known = match lock(&self.state).windows.get_mut(&window) {
                    Some(w) if !w.popup && w.state != state => {
                        w.state = state;
                        true
                    }
                    _ => false,
                };
                if known {
                    self.write_state(window, state);
                }
            }
            Message::Clipboard { text } => {
                lock(&self.state).offered = Some(text);
                // CLIPBOARD for Ctrl+V; PRIMARY too, for a middle click or
                // Shift+Insert in xterm.
                for sel in [self.clip.clipboard, self.clip.primary] {
                    let _ =
                        self.conn
                            .set_selection_owner(self.wake_window, sel, x11rb::CURRENT_TIME);
                }
            }
            Message::Monitors { monitors } => self.set_monitors(&monitors),
            Message::Ping { token } => self.send(&[Message::Pong { token }])?,
            _ => {}
        }
        let _ = self.conn.flush();
        Ok(())
    }

    fn pointer(&self, window: u32, x: i32, y: i32) {
        let origin = lock(&self.state)
            .windows
            .get(&window)
            .map(|w| (i32::from(w.x), i32::from(w.y)));
        if let Some((wx, wy)) = origin {
            let rx = (wx + x).clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16;
            let ry = (wy + y).clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16;
            let _ = self.conn.xtest_fake_input(
                xproto::MOTION_NOTIFY_EVENT,
                0,
                x11rb::CURRENT_TIME,
                self.root,
                rx,
                ry,
                0,
            );
        }
    }

    /// Asks a window to close (WM_DELETE_WINDOW), or disconnects its
    /// program when it does not take part in that protocol.
    fn close(&self, window: u32) {
        if !lock(&self.state).windows.contains_key(&window) {
            return;
        }
        let polite = self
            .conn
            .get_property(
                false,
                window,
                self.atoms.wm_protocols,
                xproto::AtomEnum::ATOM,
                0,
                64,
            )
            .ok()
            .and_then(|c| c.reply().ok())
            .and_then(|r| {
                r.value32()
                    .map(|mut v| v.any(|a| a == self.atoms.wm_delete_window))
            })
            .unwrap_or(false);
        if polite {
            let ev = xproto::ClientMessageEvent::new(
                32,
                window,
                self.atoms.wm_protocols,
                [self.atoms.wm_delete_window, 0, 0, 0, 0],
            );
            let _ = self
                .conn
                .send_event(false, window, xproto::EventMask::NO_EVENT, ev);
        } else {
            let _ = self.conn.kill_client(window);
        }
    }
}

fn clamp_side(v: u16) -> u16 {
    v.clamp(1, MAX_WINDOW_SIDE)
}

/// Clips rectangles to the window and merges them when there are many.
fn merge(rects: Vec<Rect>, w: u16, h: u16) -> Vec<Rect> {
    let mut out: Vec<Rect> = Vec::new();
    for r in rects {
        if r.x >= w || r.y >= h {
            continue;
        }
        let r = Rect {
            x: r.x,
            y: r.y,
            w: r.w.min(w - r.x),
            h: r.h.min(h - r.y),
        };
        if r.w == 0 || r.h == 0 {
            continue;
        }
        // Skip rectangles inside one already taken.
        if out
            .iter()
            .any(|o| o.x <= r.x && o.y <= r.y && o.x + o.w >= r.x + r.w && o.y + o.h >= r.y + r.h)
        {
            continue;
        }
        out.retain(|o| {
            !(r.x <= o.x && r.y <= o.y && r.x + r.w >= o.x + o.w && r.y + r.h >= o.y + o.h)
        });
        out.push(r);
    }
    if out.len() > MAX_RECTS {
        let x0 = out.iter().map(|r| r.x).min().unwrap_or(0);
        let y0 = out.iter().map(|r| r.y).min().unwrap_or(0);
        let x1 = out.iter().map(|r| r.x + r.w).max().unwrap_or(0);
        let y1 = out.iter().map(|r| r.y + r.h).max().unwrap_or(0);
        out = vec![Rect {
            x: x0,
            y: y0,
            w: x1 - x0,
            h: y1 - y0,
        }];
    }
    out
}

/// Cuts a rectangle into tiles of at most [`MAX_TILE_SIDE`] a side.
fn split(r: Rect) -> impl Iterator<Item = Rect> {
    let step = MAX_TILE_SIDE;
    (0..r.h.div_ceil(step)).flat_map(move |ty| {
        (0..r.w.div_ceil(step)).map(move |tx| {
            let x = tx * step;
            let y = ty * step;
            Rect {
                x: r.x + x,
                y: r.y + y,
                w: (r.w - x).min(step),
                h: (r.h - y).min(step),
            }
        })
    })
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic
    )]

    use super::*;

    fn r(x: u16, y: u16, w: u16, h: u16) -> Rect {
        Rect { x, y, w, h }
    }

    #[test]
    fn merge_clips_and_drops_contained() {
        let m = merge(
            vec![r(0, 0, 10, 10), r(2, 2, 3, 3), r(5, 5, 100, 100)],
            50,
            40,
        );
        assert_eq!(m, vec![r(0, 0, 10, 10), r(5, 5, 45, 35)]);
        let many: Vec<Rect> = (0..40).map(|i| r(i, i, 1, 1)).collect();
        assert_eq!(merge(many, 100, 100), vec![r(0, 0, 40, 40)]);
    }

    #[test]
    fn split_covers_exactly() {
        let tiles: Vec<Rect> = split(r(3, 4, 1100, 600)).collect();
        assert_eq!(tiles.len(), 6);
        let area: u32 = tiles.iter().map(|t| u32::from(t.w) * u32::from(t.h)).sum();
        assert_eq!(area, 1100 * 600);
        assert!(
            tiles
                .iter()
                .all(|t| t.w <= MAX_TILE_SIDE && t.h <= MAX_TILE_SIDE)
        );
    }
}
