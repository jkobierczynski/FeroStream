//! Everything a client decodes from an agent it does not trust: the
//! preamble scan, frame splitting, every message, and every tile's and
//! image's pixels.

#![no_main]

use ferostream_proto::{FrameReader, Message};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Split at a point chosen by the input, so partial frames are covered.
    let cut = data.first().map_or(0, |&b| usize::from(b)).min(data.len());
    let (a, b) = data.split_at(cut);
    for mut reader in [FrameReader::after_preamble(), FrameReader::new()] {
        reader.push(a);
        drain(&mut reader);
        reader.push(b);
        drain(&mut reader);
    }
    if let Ok(Message::Tile(t)) = Message::decode(data) {
        if let Ok(px) = t.decode() {
            assert_eq!(px.len(), t.pixel_bytes());
        }
    }
});

fn drain(reader: &mut FrameReader) {
    while let Ok(Some(m)) = reader.next_message() {
        if let Message::Tile(t) = &m {
            if let Ok(px) = t.decode() {
                assert_eq!(px.len(), t.pixel_bytes());
            }
        }
        if let Message::Cursor { image, .. } | Message::WindowIcon { image, .. } = &m {
            if let Ok(px) = image.decode() {
                assert_eq!(px.len(), image.pixel_bytes());
            }
        }
        // What decodes must encode back to the same message.
        let again = m.encode();
        assert_eq!(Message::decode(again.get(4..).unwrap_or_default()).as_ref(), Ok(&m));
    }
}
