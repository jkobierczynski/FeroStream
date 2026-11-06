//! Clipboard text between Windows (UTF-16, CRLF line ends) and the host
//! (UTF-8, LF line ends).

use ferostream_proto::MAX_CLIPBOARD;

/// Windows clipboard text (up to its NUL) for the host.
pub fn from_windows(units: &[u16]) -> String {
    let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
    let text = String::from_utf16_lossy(units.get(..end).unwrap_or_default());
    let mut text = text.replace("\r\n", "\n");
    if text.len() > MAX_CLIPBOARD {
        let mut cut = MAX_CLIPBOARD;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
    }
    text
}

/// Host text for the Windows clipboard, NUL-terminated.
pub fn to_windows(text: &str) -> Vec<u16> {
    let mut out = Vec::with_capacity(text.len() + 1);
    let mut prev = '\0';
    for c in text.chars() {
        if c == '\n' && prev != '\r' {
            out.push(u16::from(b'\r'));
        }
        let mut buf = [0u16; 2];
        out.extend_from_slice(c.encode_utf16(&mut buf));
        prev = c;
    }
    out.push(0);
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

    use super::*;

    #[test]
    fn line_ends_and_nul() {
        let w = to_windows("a\nb\r\nc ✓");
        assert_eq!(
            String::from_utf16(&w[..w.len() - 1]).unwrap(),
            "a\r\nb\r\nc ✓"
        );
        assert_eq!(*w.last().unwrap(), 0);
        assert_eq!(from_windows(&w), "a\nb\nc ✓");
        assert_eq!(from_windows(&[u16::from(b'x')]), "x");
    }
}
