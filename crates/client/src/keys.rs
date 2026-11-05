//! Windows virtual-key codes to X11 keysyms.
//!
//! Keys that type text go as characters ([`ferostream_proto::Message::Text`])
//! so the Windows keyboard layout decides what was typed. Keys without text,
//! modifiers, and keys pressed with Ctrl or Alt go as keysyms
//! ([`ferostream_proto::Message::Key`]).

/// The keysym of a key that has no text of its own, or `None` for a key
/// that types text. `extended` is bit 24 of the key message's `lParam`
/// (right-hand Ctrl and Alt, the arrow block, keypad Enter).
pub fn special_keysym(vk: u16, extended: bool) -> Option<u32> {
    let ks = match vk {
        0x08 => 0xff08,                               // BackSpace
        0x09 => 0xff09,                               // Tab
        0x0d if extended => 0xff8d,                   // KP_Enter
        0x0d => 0xff0d,                               // Return
        0x13 => 0xff13,                               // Pause
        0x14 => 0xffe5,                               // Caps_Lock
        0x1b => 0xff1b,                               // Escape
        0x21 => 0xff55,                               // Prior (Page Up)
        0x22 => 0xff56,                               // Next (Page Down)
        0x23 => 0xff57,                               // End
        0x24 => 0xff50,                               // Home
        0x25 => 0xff51,                               // Left
        0x26 => 0xff52,                               // Up
        0x27 => 0xff53,                               // Right
        0x28 => 0xff54,                               // Down
        0x2c => 0xff61,                               // Print
        0x2d => 0xff63,                               // Insert
        0x2e => 0xffff,                               // Delete
        0x5b => 0xffeb,                               // Super_L
        0x5c => 0xffec,                               // Super_R
        0x5d => 0xff67,                               // Menu
        0x70..=0x87 => 0xffbe + u32::from(vk - 0x70), // F1 .. F24
        0x90 => 0xff7f,                               // Num_Lock
        0x91 => 0xff14,                               // Scroll_Lock
        0xa0 => 0xffe1,                               // Shift_L
        0xa1 => 0xffe2,                               // Shift_R
        0xa2 => 0xffe3,                               // Control_L
        0xa3 => 0xffe4,                               // Control_R
        0xa4 => 0xffe9,                               // Alt_L
        0xa5 => 0xffea,                               // Alt_R
        // Generic Shift/Ctrl/Alt: the side comes from the scan code or the
        // extended bit, which the caller resolves to 0xa0..0xa5 first.
        0x10 => 0xffe1,
        0x11 if extended => 0xffe4,
        0x11 => 0xffe3,
        0x12 if extended => 0xffea,
        0x12 => 0xffe9,
        _ => return None,
    };
    Some(ks)
}

/// The keysym for a letter, digit or punctuation key pressed with Ctrl or
/// Alt (Ctrl+C, Alt+F as a menu key): the unshifted character the key
/// carries. `ch` is what `MapVirtualKeyW(vk, MAPVK_VK_TO_CHAR)` returns.
pub fn chord_keysym(vk: u16, ch: u32) -> Option<u32> {
    match vk {
        0x30..=0x39 => Some(u32::from(vk)),                 // 0 .. 9
        0x41..=0x5a => Some(u32::from(vk) + 0x20),          // a .. z
        0x60..=0x69 => Some(0xffb0 + u32::from(vk - 0x60)), // KP_0 .. KP_9
        _ => {
            // Dead keys come back with the top bit set.
            let c = ch & 0x7fff_ffff;
            let c = char::from_u32(c)?.to_lowercase().next()?;
            let cp = u32::from(c);
            match cp {
                0x20..=0x7e | 0xa0..=0xff => Some(cp),
                0 => None,
                _ => Some(0x0100_0000 + cp),
            }
        }
    }
}

/// Characters a `WM_CHAR` carries that a key already sent as a keysym: the
/// client drops these to avoid typing them twice.
pub fn char_sent_as_key(ch: u32) -> bool {
    matches!(ch, 0x08 | 0x09 | 0x0a | 0x0d | 0x1b | 0x7f) || ch < 0x20
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
    fn specials() {
        assert_eq!(special_keysym(0x0d, false), Some(0xff0d));
        assert_eq!(special_keysym(0x0d, true), Some(0xff8d));
        assert_eq!(special_keysym(0x70, false), Some(0xffbe));
        assert_eq!(special_keysym(0x87, false), Some(0xffbe + 23));
        assert_eq!(special_keysym(0x41, false), None);
    }

    #[test]
    fn chords() {
        assert_eq!(chord_keysym(0x43, u32::from('C')), Some(u32::from('c')));
        assert_eq!(chord_keysym(0x31, u32::from('1')), Some(u32::from('1')));
        assert_eq!(chord_keysym(0xbd, u32::from('-')), Some(u32::from('-')));
        assert_eq!(chord_keysym(0xff, 0), None);
    }
}
