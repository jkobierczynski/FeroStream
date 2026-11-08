//! What a sandboxed decoder (FeroSSH's `--stream` process) reads from an
//! agent it does not trust: the upload request, every frame, and LZ4
//! tiles unpacked. Whatever comes out must be a whole, raw message.

#![no_main]

use ferostream_client::decode::{Decoder, Output};
use ferostream_proto::Message;
use ferostream_proto::tile::Encoding;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // The first byte picks the chunk size, so frames arrive cut anywhere.
    let step = data.first().map_or(1, |&b| usize::from(b).max(1));
    for unpack in [true, false] {
        let mut decoder = Decoder::new(unpack);
        let mut out = Vec::new();
        for chunk in data.chunks(step) {
            let result = decoder.push(chunk, &mut out);
            for o in out.drain(..) {
                if let Output::Message(m) = o {
                    if unpack && let Message::Tile(t) = &m {
                        assert_eq!(t.encoding, Encoding::Raw);
                        assert_eq!(t.data.len(), t.pixel_bytes());
                    }
                    let again = m.encode();
                    assert_eq!(
                        Message::decode(again.get(4..).unwrap_or_default()).as_ref(),
                        Ok(&m)
                    );
                }
            }
            if result.is_err() {
                break;
            }
        }
    }
});
