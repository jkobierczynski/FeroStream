//! What the agent does with clipboard text from the programs it runs (any
//! X client can copy anything): text handed over in INCR pieces, and the
//! answers it gives a program that pastes text copied on Windows.

#![no_main]

#[allow(dead_code, clippy::all)]
#[path = "../../crates/agent/src/clipboard.rs"]
mod clipboard;

use clipboard::{Atoms, Data, Incr, Piece};
use ferostream_proto::MAX_CLIPBOARD;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Pieces: a header byte (format, whether bytes were left unread, a
    // scaled length) and that many bytes; an empty piece ends the text.
    let mut incr = Incr::default();
    let mut rest = data;
    let mut total = 0usize;
    while let Some((&head, tail)) = rest.split_first() {
        let format = [8, 8, 8, 16, 32, 8, 8, 8][usize::from(head & 7)];
        let after = u32::from(head & 8 != 0);
        // Up to ~1.9 MiB per piece, from a few input bytes: over the limit
        // is reached quickly.
        let len = usize::from(head >> 4) * (128 * 1024 - 1) % (tail.len().max(1) * 4096 + 1);
        let take = len.min(tail.len());
        let (piece, more) = tail.split_at(take);
        let big;
        let piece = if len > take {
            big = piece.iter().copied().cycle().take(len).collect::<Vec<u8>>();
            &big[..]
        } else {
            piece
        };
        total += piece.len();
        match incr.piece(format, after, piece) {
            Piece::More => {}
            Piece::Done(text) => {
                if let Some(t) = text {
                    // Nothing over the limit is kept (lossy UTF-8 may
                    // still grow it; encoding the message cuts it).
                    assert!(total <= MAX_CLIPBOARD);
                    assert!(t.len() <= 3 * MAX_CLIPBOARD);
                }
                incr = Incr::default();
                total = 0;
            }
        }
        rest = more;
    }

    // Answers to a pasting program, for any text and target.
    let a = Atoms {
        clipboard: 100,
        primary: 1,
        targets: 101,
        utf8_string: 102,
        string: 31,
        text: 103,
        text_plain_utf8: 104,
        incr: 105,
        property: 106,
        primary_property: 107,
    };
    let text = String::from_utf8_lossy(data);
    let target = data.first().map_or(0, |&b| 99 + u32::from(b % 8));
    match clipboard::answer(&a, target, &text) {
        Some((_, Data::Text(bytes))) => assert!(bytes.len() <= text.len()),
        Some((_, Data::Atoms(list))) => assert!(list.len() <= 8),
        None => {}
    }
    let _ = a.property_for(target);
});
