//! The FeroStream wire protocol.
//!
//! An agent on a Linux host runs apps on a private X display and sends each
//! top-level window's pixels; a client draws each one as a native window and
//! sends input back. This crate is the whole contract between the two, so
//! other clients and agents can be written against it.
//!
//! # Framing
//!
//! The agent first writes [`PREAMBLE`]; a client skips anything before it
//! (a login script that prints on a non-interactive shell, for example).
//! After that, and from the client from its first byte, the stream is a
//! sequence of frames:
//!
//! ```text
//! u32 LE length of what follows (1 ..= MAX_FRAME) | u8 tag | body
//! ```
//!
//! Integers are little-endian. A string is a `u32` byte count and UTF-8; a
//! byte string is a `u32` count and the bytes. Every count is checked
//! against a limit before anything is allocated: the other side is never
//! trusted, whichever side that is.
//!
//! The protocol does not depend on the transport: OpenSSH's stdin and stdout,
//! an SSH exec channel, or (later) QUIC all carry the same frames.

use std::fmt;

pub mod image;
pub mod tile;

/// Protocol version this crate speaks. Both sides send theirs in
/// [`Message::Hello`]; a side refuses a version it does not know.
pub const VERSION: u16 = 2;

/// What the agent writes before its first frame.
pub const PREAMBLE: &[u8] = b"\0FEROSTREAM\n";

/// How many bytes a client skips looking for [`PREAMBLE`] before it gives up.
pub const MAX_PREAMBLE_SKIP: usize = 64 * 1024;

/// Largest frame (tag and body), in bytes.
pub const MAX_FRAME: usize = 8 * 1024 * 1024;

/// Largest window, in pixels, either way.
pub const MAX_WINDOW_SIDE: u16 = 8192;

/// Largest string (a title, a program argument, an error), in bytes.
pub const MAX_STRING: usize = 16 * 1024;

/// Lowest and highest screen DPI in [`Message::Start`] (100 % to 500 %).
pub const MIN_DPI: u16 = 96;
pub const MAX_DPI: u16 = 480;

/// Most program arguments in [`Message::Start`].
pub const MAX_ARGS: usize = 256;

/// What kind of window [`Message::WindowNew`] announces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowKind {
    /// An ordinary top-level window: frame, title, taskbar button.
    Normal,
    /// A menu, tooltip or drop-down (X11 override-redirect): no frame, no
    /// taskbar button, never takes the focus.
    Popup,
}

/// One message, either way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    // ----- Both ways -----
    /// First frame from each side.
    Hello { version: u16, software: String },
    /// Asks for a [`Message::Pong`] with the same token.
    Ping { token: u32 },
    /// Answers a [`Message::Ping`].
    Pong { token: u32 },

    // ----- Client to agent -----
    /// Starts a program. `width` x `height` is the size of the virtual
    /// screen the windows live on (the client's desktop, usually); `dpi` is
    /// the client screen's resolution (96 = 100 %, 144 = 150 %), so programs
    /// draw text and widgets at the size the user expects.
    Start {
        width: u16,
        height: u16,
        dpi: u16,
        argv: Vec<String>,
    },
    /// The pointer is at `x`,`y` in `window`'s coordinates.
    PointerMove { window: u32, x: i32, y: i32 },
    /// A mouse button went down or up. Buttons as in X11: 1 left, 2 middle,
    /// 3 right, 4/5 wheel up/down, 6/7 wheel left/right, 8/9 back/forward.
    PointerButton {
        window: u32,
        x: i32,
        y: i32,
        button: u8,
        down: bool,
    },
    /// A key with no text of its own (an arrow, F1, Shift) or a key pressed
    /// with Ctrl or Alt, as an X11 keysym.
    Key { keysym: u32, down: bool },
    /// One typed character (press and release). The agent picks the key
    /// and the Shift state that produce it on the remote keyboard map.
    Text { ch: char },
    /// The client has drawn everything up to [`Message::FrameEnd`] `seq`.
    FrameAck { window: u32, seq: u32 },
    /// The user closed the window (the close button, Alt+F4).
    WindowClose { window: u32 },
    /// The user moved or resized the window; position on the virtual screen.
    WindowMove {
        window: u32,
        x: i32,
        y: i32,
        width: u16,
        height: u16,
    },
    /// The window got the keyboard focus. Window 0: the client lost the
    /// focus to another program; the agent lets go of held keys.
    Focus { window: u32 },

    // ----- Agent to client -----
    /// A window appeared.
    WindowNew {
        window: u32,
        x: i32,
        y: i32,
        width: u16,
        height: u16,
        kind: WindowKind,
        /// The window this one belongs to (a dialog's main window), or 0.
        transient_for: u32,
        title: String,
    },
    /// A window moved or changed size. Its pixels are undefined until the
    /// tiles that follow arrive.
    WindowConfigure {
        window: u32,
        x: i32,
        y: i32,
        width: u16,
        height: u16,
    },
    /// A window's title changed.
    WindowTitle { window: u32, title: String },
    /// A window went away (closed or hidden).
    WindowGone { window: u32 },
    /// The pointer shape changed: show `image` over every window from now
    /// on, with its hot spot (the pixel that points) at `hot_x`,`hot_y`.
    Cursor {
        hot_x: u16,
        hot_y: u16,
        image: image::Image,
    },
    /// New pixels for a rectangle of a window; see [`tile`].
    Tile(tile::Tile),
    /// The tiles sent so far for `window` make a whole picture; draw it and
    /// answer with [`Message::FrameAck`].
    FrameEnd { window: u32, seq: u32 },
    /// The program ended and every window is gone.
    Exited { code: i32 },
    /// Something went wrong; the agent stops after sending it.
    Error { message: String },
}

impl Message {
    fn tag(&self) -> u8 {
        match self {
            Message::Hello { .. } => 1,
            Message::Ping { .. } => 2,
            Message::Pong { .. } => 3,
            Message::Start { .. } => 16,
            Message::PointerMove { .. } => 17,
            Message::PointerButton { .. } => 18,
            Message::Key { .. } => 19,
            Message::Text { .. } => 20,
            Message::FrameAck { .. } => 21,
            Message::WindowClose { .. } => 22,
            Message::WindowMove { .. } => 23,
            Message::Focus { .. } => 24,
            Message::WindowNew { .. } => 64,
            Message::WindowConfigure { .. } => 65,
            Message::WindowTitle { .. } => 66,
            Message::WindowGone { .. } => 67,
            Message::Tile(_) => 68,
            Message::FrameEnd { .. } => 69,
            Message::Exited { .. } => 70,
            Message::Error { .. } => 71,
            Message::Cursor { .. } => 72,
        }
    }

    /// The message as one frame, length prefix included.
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer(Vec::with_capacity(32));
        w.u32(0);
        w.u8(self.tag());
        match self {
            Message::Hello { version, software } => {
                w.u16(*version);
                w.str(software);
            }
            Message::Ping { token } | Message::Pong { token } => w.u32(*token),
            Message::Start {
                width,
                height,
                dpi,
                argv,
            } => {
                w.u16(*width);
                w.u16(*height);
                w.u16(*dpi);
                w.u32(argv.len() as u32);
                for a in argv {
                    w.str(a);
                }
            }
            Message::PointerMove { window, x, y } => {
                w.u32(*window);
                w.i32(*x);
                w.i32(*y);
            }
            Message::PointerButton {
                window,
                x,
                y,
                button,
                down,
            } => {
                w.u32(*window);
                w.i32(*x);
                w.i32(*y);
                w.u8(*button);
                w.u8(u8::from(*down));
            }
            Message::Key { keysym, down } => {
                w.u32(*keysym);
                w.u8(u8::from(*down));
            }
            Message::Text { ch } => w.u32(u32::from(*ch)),
            Message::FrameAck { window, seq } | Message::FrameEnd { window, seq } => {
                w.u32(*window);
                w.u32(*seq);
            }
            Message::WindowClose { window }
            | Message::Focus { window }
            | Message::WindowGone { window } => w.u32(*window),
            Message::WindowMove {
                window,
                x,
                y,
                width,
                height,
            }
            | Message::WindowConfigure {
                window,
                x,
                y,
                width,
                height,
            } => {
                w.u32(*window);
                w.i32(*x);
                w.i32(*y);
                w.u16(*width);
                w.u16(*height);
            }
            Message::WindowNew {
                window,
                x,
                y,
                width,
                height,
                kind,
                transient_for,
                title,
            } => {
                w.u32(*window);
                w.i32(*x);
                w.i32(*y);
                w.u16(*width);
                w.u16(*height);
                w.u8(match kind {
                    WindowKind::Normal => 0,
                    WindowKind::Popup => 1,
                });
                w.u32(*transient_for);
                w.str(title);
            }
            Message::WindowTitle { window, title } => {
                w.u32(*window);
                w.str(title);
            }
            Message::Tile(t) => {
                w.u32(t.window);
                w.u16(t.x);
                w.u16(t.y);
                w.u16(t.width);
                w.u16(t.height);
                w.u8(t.encoding as u8);
                w.bytes(&t.data);
            }
            Message::Exited { code } => w.i32(*code),
            Message::Error { message } => w.str(message),
            Message::Cursor {
                hot_x,
                hot_y,
                image,
            } => {
                w.u16(*hot_x);
                w.u16(*hot_y);
                w.image(image);
            }
        }
        let mut out = w.0;
        let len = (out.len() - 4) as u32;
        if let Some(head) = out.get_mut(..4) {
            head.copy_from_slice(&len.to_le_bytes());
        }
        out
    }

    /// Decodes one frame's tag and body (the length prefix already taken
    /// off). Every field is checked; trailing bytes are an error.
    pub fn decode(frame: &[u8]) -> Result<Message, Error> {
        if frame.len() > MAX_FRAME {
            return Err(Error::TooBig);
        }
        let mut r = Reader(frame);
        let tag = r.u8()?;
        let m = match tag {
            1 => Message::Hello {
                version: r.u16()?,
                software: r.str()?,
            },
            2 => Message::Ping { token: r.u32()? },
            3 => Message::Pong { token: r.u32()? },
            16 => {
                let width = r.side()?;
                let height = r.side()?;
                let dpi = r.u16()?;
                if !(MIN_DPI..=MAX_DPI).contains(&dpi) {
                    return Err(Error::Invalid("DPI"));
                }
                let n = r.u32()? as usize;
                if n == 0 || n > MAX_ARGS {
                    return Err(Error::Invalid("argument count"));
                }
                let mut argv = Vec::with_capacity(n);
                for _ in 0..n {
                    argv.push(r.str()?);
                }
                Message::Start {
                    width,
                    height,
                    dpi,
                    argv,
                }
            }
            17 => Message::PointerMove {
                window: r.u32()?,
                x: r.i32()?,
                y: r.i32()?,
            },
            18 => Message::PointerButton {
                window: r.u32()?,
                x: r.i32()?,
                y: r.i32()?,
                button: r.u8()?,
                down: r.bool()?,
            },
            19 => Message::Key {
                keysym: r.u32()?,
                down: r.bool()?,
            },
            20 => Message::Text {
                ch: char::from_u32(r.u32()?).ok_or(Error::Invalid("character"))?,
            },
            21 => Message::FrameAck {
                window: r.u32()?,
                seq: r.u32()?,
            },
            22 => Message::WindowClose { window: r.u32()? },
            23 => Message::WindowMove {
                window: r.u32()?,
                x: r.i32()?,
                y: r.i32()?,
                width: r.side()?,
                height: r.side()?,
            },
            24 => Message::Focus { window: r.u32()? },
            64 => Message::WindowNew {
                window: r.u32()?,
                x: r.i32()?,
                y: r.i32()?,
                width: r.side()?,
                height: r.side()?,
                kind: match r.u8()? {
                    0 => WindowKind::Normal,
                    1 => WindowKind::Popup,
                    _ => return Err(Error::Invalid("window kind")),
                },
                transient_for: r.u32()?,
                title: r.str()?,
            },
            65 => Message::WindowConfigure {
                window: r.u32()?,
                x: r.i32()?,
                y: r.i32()?,
                width: r.side()?,
                height: r.side()?,
            },
            66 => Message::WindowTitle {
                window: r.u32()?,
                title: r.str()?,
            },
            67 => Message::WindowGone { window: r.u32()? },
            68 => {
                let window = r.u32()?;
                let x = r.u16()?;
                let y = r.u16()?;
                let width = r.side()?;
                let height = r.side()?;
                let encoding = tile::Encoding::from_u8(r.u8()?)?;
                let data = r.bytes()?.to_vec();
                let t = tile::Tile {
                    window,
                    x,
                    y,
                    width,
                    height,
                    encoding,
                    data,
                };
                t.check()?;
                Message::Tile(t)
            }
            69 => Message::FrameEnd {
                window: r.u32()?,
                seq: r.u32()?,
            },
            70 => Message::Exited { code: r.i32()? },
            71 => Message::Error { message: r.str()? },
            72 => {
                let hot_x = r.u16()?;
                let hot_y = r.u16()?;
                let image = r.image()?;
                if hot_x >= image.width || hot_y >= image.height {
                    return Err(Error::Invalid("cursor hot spot"));
                }
                Message::Cursor {
                    hot_x,
                    hot_y,
                    image,
                }
            }
            _ => return Err(Error::UnknownTag(tag)),
        };
        if !r.0.is_empty() {
            return Err(Error::Invalid("trailing bytes"));
        }
        Ok(m)
    }
}

/// Why a frame was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The frame ended before a field did.
    Short,
    /// A length is over its limit.
    TooBig,
    /// A tag this version does not know.
    UnknownTag(u8),
    /// A field has a value it may not have.
    Invalid(&'static str),
    /// The stream did not start with [`PREAMBLE`].
    NoPreamble,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Short => f.write_str("frame ends early"),
            Error::TooBig => f.write_str("length over the limit"),
            Error::UnknownTag(t) => write!(f, "unknown message tag {t}"),
            Error::Invalid(what) => write!(f, "invalid {what}"),
            Error::NoPreamble => f.write_str("the agent did not answer (no FeroStream preamble)"),
        }
    }
}

impl std::error::Error for Error {}

struct Writer(Vec<u8>);

impl Writer {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u16(&mut self, v: u16) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn i32(&mut self, v: i32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn str(&mut self, s: &str) {
        // Over-long strings are cut on a character boundary rather than
        // producing a frame the other side must refuse.
        let mut end = s.len().min(MAX_STRING);
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        self.bytes(s.as_bytes().get(..end).unwrap_or_default());
    }
    fn bytes(&mut self, b: &[u8]) {
        self.u32(b.len() as u32);
        self.0.extend_from_slice(b);
    }
    fn image(&mut self, i: &image::Image) {
        self.u16(i.width);
        self.u16(i.height);
        self.bytes(&i.data);
    }
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], Error> {
        if self.0.len() < n {
            return Err(Error::Short);
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Ok(head)
    }
    fn array<const N: usize>(&mut self) -> Result<[u8; N], Error> {
        let mut a = [0; N];
        a.copy_from_slice(self.take(N)?);
        Ok(a)
    }
    fn u8(&mut self) -> Result<u8, Error> {
        Ok(self.array::<1>()?[0])
    }
    fn bool(&mut self) -> Result<bool, Error> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(Error::Invalid("flag")),
        }
    }
    fn u16(&mut self) -> Result<u16, Error> {
        Ok(u16::from_le_bytes(self.array()?))
    }
    fn side(&mut self) -> Result<u16, Error> {
        let v = self.u16()?;
        if v == 0 || v > MAX_WINDOW_SIDE {
            return Err(Error::Invalid("size"));
        }
        Ok(v)
    }
    fn u32(&mut self) -> Result<u32, Error> {
        Ok(u32::from_le_bytes(self.array()?))
    }
    fn i32(&mut self) -> Result<i32, Error> {
        Ok(i32::from_le_bytes(self.array()?))
    }
    fn bytes(&mut self) -> Result<&'a [u8], Error> {
        let n = self.u32()? as usize;
        if n > self.0.len() {
            return Err(Error::Short);
        }
        self.take(n)
    }
    fn str(&mut self) -> Result<String, Error> {
        let b = self.bytes()?;
        if b.len() > MAX_STRING {
            return Err(Error::TooBig);
        }
        String::from_utf8(b.to_vec()).map_err(|_| Error::Invalid("UTF-8"))
    }
    fn image(&mut self) -> Result<image::Image, Error> {
        let i = image::Image {
            width: self.u16()?,
            height: self.u16()?,
            data: self.bytes()?.to_vec(),
        };
        i.check()?;
        Ok(i)
    }
}

/// Splits a byte stream into frames. Feed it what the transport delivers;
/// take frames out with [`FrameReader::next_message`].
#[derive(Debug, Default)]
pub struct FrameReader {
    buf: Vec<u8>,
    start: usize,
    preamble: bool,
}

impl FrameReader {
    /// A reader for a stream that starts with frames (the agent's side).
    pub fn new() -> Self {
        Self {
            preamble: true,
            ..Self::default()
        }
    }

    /// A reader that first skips to [`PREAMBLE`] (the client's side).
    pub fn after_preamble() -> Self {
        Self::default()
    }

    /// Adds bytes from the transport.
    pub fn push(&mut self, data: &[u8]) {
        if self.start > 0 && self.start == self.buf.len() {
            self.buf.clear();
            self.start = 0;
        } else if self.start > 64 * 1024 {
            self.buf.drain(..self.start);
            self.start = 0;
        }
        self.buf.extend_from_slice(data);
    }

    /// The next whole message, `Ok(None)` when more bytes are needed.
    pub fn next_message(&mut self) -> Result<Option<Message>, Error> {
        let pending = self.buf.get(self.start..).unwrap_or_default();
        if !self.preamble {
            match find(pending, PREAMBLE) {
                Some(at) => {
                    self.start += at + PREAMBLE.len();
                    self.preamble = true;
                    return self.next_message();
                }
                None if self.buf.len() > MAX_PREAMBLE_SKIP => return Err(Error::NoPreamble),
                None => return Ok(None),
            }
        }
        let Some(head) = pending.get(..4) else {
            return Ok(None);
        };
        let mut len = [0; 4];
        len.copy_from_slice(head);
        let len = u32::from_le_bytes(len) as usize;
        if len == 0 {
            return Err(Error::Short);
        }
        if len > MAX_FRAME {
            return Err(Error::TooBig);
        }
        let Some(frame) = pending.get(4..4 + len) else {
            return Ok(None);
        };
        let m = Message::decode(frame)?;
        self.start += 4 + len;
        Ok(Some(m))
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
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

    fn all() -> Vec<Message> {
        vec![
            Message::Hello {
                version: VERSION,
                software: "ferostream-agent 0.1.0".into(),
            },
            Message::Ping { token: 7 },
            Message::Pong { token: 7 },
            Message::Start {
                width: 1920,
                height: 1080,
                dpi: 144,
                argv: vec!["xterm".into(), "-e".into(), "top".into()],
            },
            Message::PointerMove {
                window: 3,
                x: -4,
                y: 10,
            },
            Message::PointerButton {
                window: 3,
                x: 1,
                y: 2,
                button: 1,
                down: true,
            },
            Message::Key {
                keysym: 0xff0d,
                down: false,
            },
            Message::Text { ch: 'é' },
            Message::FrameAck { window: 3, seq: 9 },
            Message::WindowClose { window: 3 },
            Message::WindowMove {
                window: 3,
                x: 10,
                y: 20,
                width: 640,
                height: 480,
            },
            Message::Focus { window: 3 },
            Message::WindowNew {
                window: 3,
                x: 0,
                y: 0,
                width: 484,
                height: 316,
                kind: WindowKind::Popup,
                transient_for: 2,
                title: "xterm".into(),
            },
            Message::WindowConfigure {
                window: 3,
                x: 5,
                y: 6,
                width: 7,
                height: 8,
            },
            Message::WindowTitle {
                window: 3,
                title: "vim — notes.txt".into(),
            },
            Message::WindowGone { window: 3 },
            Message::Tile(tile::Tile::lz4(3, 0, 0, 2, 2, &[9; 16])),
            Message::FrameEnd { window: 3, seq: 9 },
            Message::Cursor {
                hot_x: 3,
                hot_y: 4,
                image: image::Image::new(16, 16, &[1; 16 * 16 * 4]),
            },
            Message::Exited { code: -1 },
            Message::Error {
                message: "Xvfb is not installed".into(),
            },
        ]
    }

    #[test]
    fn round_trip() {
        for m in all() {
            let f = m.encode();
            let back = Message::decode(f.get(4..).unwrap_or_default());
            assert_eq!(back, Ok(m));
        }
    }

    #[test]
    fn reader_skips_junk_before_preamble_and_splits_frames() {
        let mut stream = b"Welcome to host\n".to_vec();
        stream.extend_from_slice(PREAMBLE);
        for m in all() {
            stream.extend_from_slice(&m.encode());
        }
        let mut r = FrameReader::after_preamble();
        let mut got = Vec::new();
        // One byte at a time: every split point is exercised.
        for b in &stream {
            r.push(std::slice::from_ref(b));
            while let Ok(Some(m)) = r.next_message() {
                got.push(m);
            }
        }
        assert_eq!(got, all());
    }

    #[test]
    fn refuses_bad_frames() {
        assert_eq!(Message::decode(&[]), Err(Error::Short));
        assert_eq!(Message::decode(&[200]), Err(Error::UnknownTag(200)));
        // Hello with a string length past the end.
        assert_eq!(
            Message::decode(&[1, 1, 0, 255, 255, 255, 255]),
            Err(Error::Short)
        );
        // Trailing byte.
        let mut f = Message::Ping { token: 1 }.encode();
        f.push(0);
        assert!(Message::decode(f.get(4..).unwrap_or_default()).is_err());
        // Window of size 0.
        let f = Message::WindowGone { window: 1 }.encode();
        assert!(Message::decode(f.get(4..).unwrap_or_default()).is_ok());
        let mut r = FrameReader::new();
        r.push(&u32::MAX.to_le_bytes());
        assert_eq!(r.next_message(), Err(Error::TooBig));
    }

    #[test]
    fn no_preamble_gives_up() {
        let mut r = FrameReader::after_preamble();
        r.push(&vec![b'x'; MAX_PREAMBLE_SKIP + 1]);
        assert_eq!(r.next_message(), Err(Error::NoPreamble));
    }
}
