//! The new tile encodings both ways. Arbitrary bytes as a packed or solid
//! tile must decode to exactly the tile's size or fail, never panic or
//! allocate past it; arbitrary pixels packed by the agent must come back
//! unchanged (with the unused fourth byte 0) at either effort.

#![no_main]

use ferostream_proto::tile::{Effort, Encoding, Tile};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let [w, h, enc, rest @ ..] = data else {
        return;
    };
    let (width, height) = (u16::from(*w % 64) + 1, u16::from(*h % 64) + 1);

    // As received: any bytes.
    let encoding = match enc % 3 {
        0 => Encoding::Packed,
        1 => Encoding::Solid,
        _ => Encoding::Lz4,
    };
    let t = Tile {
        window: 1,
        x: 0,
        y: 0,
        width,
        height,
        encoding,
        data: rest.to_vec(),
    };
    if let Ok(px) = t.decode() {
        assert_eq!(px.len(), t.pixel_bytes());
    }

    // As sent: pixels from the input, few colours when `enc` says so, so
    // palettes of every size get packed too.
    let n = usize::from(width) * usize::from(height);
    let mut px = Vec::with_capacity(n * 4);
    for i in 0..n {
        let b = rest.get(i % rest.len().max(1)).copied().unwrap_or(0);
        let c = if enc & 0x80 != 0 {
            u32::from(b % (enc & 0x7f).max(1)) * 0x0103_07
        } else {
            u32::from(b) * 0x0101_01 ^ (i as u32).wrapping_mul(2_654_435_761)
        };
        px.extend_from_slice(&(c & 0xff_ffff).to_le_bytes());
    }
    for effort in [Effort::Fast, Effort::Strong] {
        let t = Tile::pack(1, 0, 0, width, height, &px, effort);
        assert_eq!(t.decode().as_deref(), Ok(&px[..]));
    }
});
