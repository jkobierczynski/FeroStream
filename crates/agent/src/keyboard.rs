//! Keys and typed characters, injected with XTEST.
//!
//! The client sends keysyms (layout-independent names such as `Return` or
//! `a`) and characters. Here they become key codes on the X server's
//! keyboard map; a character the map does not have gets a spare key code
//! remapped to it, as xdotool does.

use std::collections::HashMap;

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{self, ConnectionExt as _};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

/// Keysyms of keys that modify others; held ones are tracked so a typed
/// character can be produced with exactly the modifiers it needs.
const MODIFIERS: [u32; 12] = [
    0xffe1, 0xffe2, // Shift_L, Shift_R
    0xffe3, 0xffe4, // Control_L, Control_R
    0xffe9, 0xffea, // Alt_L, Alt_R
    0xffe7, 0xffe8, // Meta_L, Meta_R
    0xffeb, 0xffec, // Super_L, Super_R
    0xfe03, // ISO_Level3_Shift (AltGr)
    0xff7e, // Mode_switch
];
const SHIFT_L: u32 = 0xffe1;
const SHIFT_R: u32 = 0xffe2;

#[derive(Debug)]
pub struct Keyboard {
    min: u8,
    per: usize,
    map: Vec<u32>,
    /// Key codes with nothing on them, free to remap.
    spare: Vec<u8>,
    /// Keysyms put on spare key codes so far, oldest first.
    remapped: Vec<(u32, u8)>,
    /// Modifier keysyms the client holds down, with their key codes.
    held: HashMap<u32, u8>,
}

impl Keyboard {
    pub fn load(conn: &RustConnection) -> Result<Keyboard, String> {
        let setup = conn.setup();
        let (min, max) = (setup.min_keycode, setup.max_keycode);
        let count = max.saturating_sub(min).saturating_add(1);
        let reply = conn
            .get_keyboard_mapping(min, count)
            .map_err(|e| e.to_string())?
            .reply()
            .map_err(|e| e.to_string())?;
        let per = usize::from(reply.keysyms_per_keycode).max(1);
        let map = reply.keysyms;
        let mut spare: Vec<u8> = map
            .chunks(per)
            .enumerate()
            .filter(|(_, syms)| syms.iter().all(|&s| s == 0))
            .filter_map(|(i, _)| u8::try_from(usize::from(min) + i).ok())
            .collect();
        // Use the highest free codes first; low ones are where real keys go.
        spare.reverse();
        Ok(Keyboard {
            min,
            per,
            map,
            spare,
            remapped: Vec::new(),
            held: HashMap::new(),
        })
    }

    /// The key code and column (0 plain, 1 with Shift) that produce `keysym`.
    fn find(&self, keysym: u32) -> Option<(u8, usize)> {
        for col in 0..self.per.min(2) {
            for (i, syms) in self.map.chunks(self.per).enumerate() {
                if syms.get(col) == Some(&keysym) {
                    return Some((u8::try_from(usize::from(self.min) + i).ok()?, col));
                }
            }
        }
        // Further columns (AltGr levels): the key exists, but we cannot be
        // sure which modifiers reach it, so remapping is the safer choice.
        None
    }

    /// Puts `keysym` on a spare key code (both columns) and returns it.
    fn remap(&mut self, conn: &RustConnection, keysym: u32) -> Option<u8> {
        if let Some(&(_, code)) = self.remapped.iter().find(|(s, _)| *s == keysym) {
            return Some(code);
        }
        let code = if let Some(code) = self.spare.pop() {
            code
        } else {
            // All spares used: recycle the oldest remapped one.
            if self.remapped.is_empty() {
                return None;
            }
            self.remapped.remove(0).1
        };
        let mut syms = vec![0u32; self.per];
        for s in syms.iter_mut().take(2) {
            *s = keysym;
        }
        conn.change_keyboard_mapping(1, code, self.per as u8, &syms)
            .ok()?;
        let at = (usize::from(code) - usize::from(self.min)) * self.per;
        if let Some(slot) = self.map.get_mut(at..at + self.per) {
            slot.copy_from_slice(&syms);
        }
        self.remapped.push((keysym, code));
        Some(code)
    }

    fn code_for(&mut self, conn: &RustConnection, keysym: u32) -> Option<(u8, usize)> {
        match self.find(keysym) {
            Some(found) => Some(found),
            None => self.remap(conn, keysym).map(|c| (c, 0)),
        }
    }

    /// A key went down or up (Key message).
    pub fn key(&mut self, conn: &RustConnection, root: xproto::Window, keysym: u32, down: bool) {
        let Some((code, _)) = self.code_for(conn, keysym) else {
            return;
        };
        if MODIFIERS.contains(&keysym) {
            if down {
                self.held.insert(keysym, code);
            } else {
                self.held.remove(&keysym);
            }
        }
        fake_key(conn, root, code, down);
    }

    /// Releases every modifier the client holds (it lost the focus).
    pub fn release_all(&mut self, conn: &RustConnection, root: xproto::Window) {
        for (_, code) in self.held.drain() {
            fake_key(conn, root, code, false);
        }
    }

    /// Types one character (Text message) with exactly the Shift state it
    /// needs; other held modifiers (Ctrl and Alt that Windows reports with
    /// AltGr) are let go for the moment.
    pub fn text(&mut self, conn: &RustConnection, root: xproto::Window, ch: char) {
        let keysym = char_keysym(ch);
        let Some((code, col)) = self.code_for(conn, keysym) else {
            return;
        };
        let shift_held = self.held.contains_key(&SHIFT_L) || self.held.contains_key(&SHIFT_R);
        let want_shift = col == 1;
        let lift: Vec<u8> = self
            .held
            .iter()
            .filter(|(s, _)| {
                let is_shift = **s == SHIFT_L || **s == SHIFT_R;
                !is_shift || !want_shift
            })
            .map(|(_, c)| *c)
            .collect();
        for &c in &lift {
            fake_key(conn, root, c, false);
        }
        let mut added_shift = None;
        if want_shift
            && !shift_held
            && let Some((shift, _)) = self.find(SHIFT_L)
        {
            fake_key(conn, root, shift, true);
            added_shift = Some(shift);
        }
        fake_key(conn, root, code, true);
        fake_key(conn, root, code, false);
        if let Some(shift) = added_shift {
            fake_key(conn, root, shift, false);
        }
        for &c in &lift {
            fake_key(conn, root, c, true);
        }
    }
}

fn fake_key(conn: &RustConnection, root: xproto::Window, code: u8, down: bool) {
    let kind = if down {
        xproto::KEY_PRESS_EVENT
    } else {
        xproto::KEY_RELEASE_EVENT
    };
    let _ = conn.xtest_fake_input(kind, code, x11rb::CURRENT_TIME, root, 0, 0, 0);
}

/// The keysym for a character: Latin-1 keysyms are the code point, control
/// characters map to their keys, anything else is `0x0100_0000 + code point`.
pub fn char_keysym(ch: char) -> u32 {
    let cp = u32::from(ch);
    match cp {
        0x08 => 0xff08,        // BackSpace
        0x09 => 0xff09,        // Tab
        0x0a | 0x0d => 0xff0d, // Return
        0x1b => 0xff1b,        // Escape
        0x7f => 0xffff,        // Delete
        0x20..=0x7e | 0xa0..=0xff => cp,
        _ => 0x0100_0000 + cp,
    }
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

    #[test]
    fn keysyms_for_characters() {
        assert_eq!(char_keysym('a'), 0x61);
        assert_eq!(char_keysym('é'), 0xe9);
        assert_eq!(char_keysym('\r'), 0xff0d);
        assert_eq!(char_keysym('€'), 0x0100_20ac);
    }
}
