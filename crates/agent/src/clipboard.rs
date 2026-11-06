//! The X side of the clipboard: what the agent offers when a program
//! pastes text copied on Windows, and how it reads text a program copied.
//! The helpers here are pure, so they are tested without an X server.

/// Atoms the clipboard uses.
#[derive(Debug, Clone, Copy)]
pub struct Atoms {
    pub clipboard: u32,
    pub primary: u32,
    pub targets: u32,
    pub utf8_string: u32,
    pub string: u32,
    pub text: u32,
    pub text_plain_utf8: u32,
    pub incr: u32,
    /// Where a program puts the text the agent asked it for.
    pub property: u32,
}

/// Names for [`Atoms`] except PRIMARY and STRING (predefined), in order:
/// clipboard, targets, utf8_string, text, text_plain_utf8, incr, property.
pub const ATOM_NAMES: [&[u8]; 7] = [
    b"CLIPBOARD",
    b"TARGETS",
    b"UTF8_STRING",
    b"TEXT",
    b"text/plain;charset=utf-8",
    b"INCR",
    b"_FEROSTREAM_SELECTION",
];

/// What goes into the property a pasting program reads.
#[derive(Debug, PartialEq, Eq)]
pub enum Data {
    /// A list of atoms (format 32).
    Atoms(Vec<u32>),
    /// Text bytes (format 8).
    Text(Vec<u8>),
}

/// What the agent hands a program that asks for the clipboard in `target`:
/// the property type and its data, or `None` for a target it cannot give.
pub fn answer(a: &Atoms, target: u32, text: &str) -> Option<(u32, Data)> {
    if target == a.targets {
        let list = vec![
            a.targets,
            a.utf8_string,
            a.text_plain_utf8,
            a.string,
            a.text,
        ];
        return Some((xproto_atom(), Data::Atoms(list)));
    }
    if target == a.utf8_string || target == a.text_plain_utf8 {
        return Some((target, Data::Text(text.as_bytes().to_vec())));
    }
    if target == a.string || target == a.text {
        // STRING is Latin-1; characters beyond it become '?'.
        let latin1 = text
            .chars()
            .map(|c| u8::try_from(u32::from(c)).unwrap_or(b'?'))
            .collect();
        return Some((a.string, Data::Text(latin1)));
    }
    None
}

fn xproto_atom() -> u32 {
    x11rb::protocol::xproto::AtomEnum::ATOM.into()
}

/// A point on a `width` x `height` screen that none of `windows` (x, y,
/// width, height) covers: where a click closes an open menu without
/// landing in a program.
pub fn free_point(width: u16, height: u16, windows: &[(i32, i32, u16, u16)]) -> Option<(i16, i16)> {
    let (w, h) = (i32::from(width) - 1, i32::from(height) - 1);
    let candidates = [
        (w, h),
        (0, h),
        (w, 0),
        (0, 0),
        (w / 2, h),
        (w, h / 2),
        (0, h / 2),
        (w / 2, 0),
    ];
    candidates
        .into_iter()
        .find(|&(px, py)| {
            windows.iter().all(|&(x, y, ww, wh)| {
                px < x || py < y || px >= x + i32::from(ww) || py >= y + i32::from(wh)
            })
        })
        .map(|(x, y)| (x as i16, y as i16))
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

    fn atoms() -> Atoms {
        Atoms {
            clipboard: 100,
            primary: 1,
            targets: 101,
            utf8_string: 102,
            string: 31,
            text: 103,
            text_plain_utf8: 104,
            incr: 105,
            property: 106,
        }
    }

    #[test]
    fn answers() {
        let a = atoms();
        let (ty, d) = answer(&a, a.utf8_string, "é€").unwrap();
        assert_eq!(ty, a.utf8_string);
        assert_eq!(d, Data::Text("é€".as_bytes().to_vec()));
        let (ty, d) = answer(&a, a.string, "é€").unwrap();
        assert_eq!(ty, a.string);
        assert_eq!(d, Data::Text(vec![0xe9, b'?']));
        let (ty, d) = answer(&a, a.targets, "").unwrap();
        assert_eq!(ty, 4);
        assert!(matches!(d, Data::Atoms(l) if l.len() == 5));
        assert!(answer(&a, 999, "x").is_none());
    }

    #[test]
    fn free_points() {
        assert_eq!(free_point(100, 50, &[]), Some((99, 49)));
        // A window over the bottom-right corner: the next corner is used.
        assert_eq!(free_point(100, 50, &[(50, 25, 50, 25)]), Some((0, 49)));
        // Covered everywhere.
        assert_eq!(free_point(100, 50, &[(0, 0, 100, 50)]), None);
    }
}
