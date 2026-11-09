//! Window pixels.
//!
//! A [`Tile`] carries a rectangle of one window. Pixels are 32 bits, bytes
//! in the order blue, green, red, unused (what X11 ZPixmap depth 24 and a
//! Windows top-down DIB both use), rows top to bottom with no padding.
//!
//! # Encodings
//!
//! - [`Encoding::Raw`]: the pixels as they are.
//! - [`Encoding::Lz4`]: an LZ4 block of the raw pixels after a `u32` LE
//!   decoded size (protocol 5 and older agents).
//! - [`Encoding::Solid`]: one colour, 3 bytes (blue, green, red).
//! - [`Encoding::Packed`]: a small header, then a body that is optionally
//!   compressed:
//!
//! ```text
//! u8 layout       0 palette, 1 colour rows
//! u8 compressor   0 none, 1 LZ4 block, 2 raw Deflate
//! palette only:   u8 count - 1 (2 ..= 256 colours), count x (blue, green, red)
//! body            (compressed) bytes, see below
//! ```
//!
//! A palette body is one index per pixel, 1, 2, 4 or 8 bits (the fewest that
//! hold `count`), most significant bits first, each row starting on a new
//! byte. A colour-rows body is, per row, a filter byte and 3 bytes per
//! pixel (blue, green, red): filter 0 stores them as they are, 1 as the
//! difference to the pixel on the left, 2 to the pixel above (wrapping
//! byte arithmetic; nothing left of or above the tile counts as 0).
//!
//! The decoded body size follows from the header and the tile size, so a
//! decoder knows it before inflating anything and never makes more. The
//! unused fourth byte of every decoded pixel is 0.

use crate::{Error, MAX_WINDOW_SIDE};

/// Largest tile, in decoded bytes (512 x 512 pixels). Bigger damage is sent
/// as several tiles.
pub const MAX_TILE_BYTES: usize = 512 * 512 * 4;

/// Largest side of a tile, in pixels.
pub const MAX_TILE_SIDE: u16 = 512;

/// How a tile's bytes are packed; see the module text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Encoding {
    /// Pixels as they are.
    Raw = 0,
    /// An LZ4 block of the raw pixels, after a `u32` LE decoded size.
    Lz4 = 1,
    /// One colour.
    Solid = 2,
    /// A palette or filtered colour rows, optionally compressed.
    Packed = 3,
}

impl Encoding {
    pub(crate) fn from_u8(v: u8) -> Result<Self, Error> {
        match v {
            0 => Ok(Encoding::Raw),
            1 => Ok(Encoding::Lz4),
            2 => Ok(Encoding::Solid),
            3 => Ok(Encoding::Packed),
            _ => Err(Error::Invalid("tile encoding")),
        }
    }
}

/// How hard the sender packs tiles: what the link is worth in CPU time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effort {
    /// LZ4: little CPU, for fast links.
    Fast,
    /// Deflate: about half the bytes of LZ4 on most screens, several times
    /// the CPU, for slow links.
    Strong,
}

const LAYOUT_PALETTE: u8 = 0;
const LAYOUT_ROWS: u8 = 1;
const PACK_NONE: u8 = 0;
const PACK_LZ4: u8 = 1;
const PACK_DEFLATE: u8 = 2;

/// Bodies smaller than this are not worth compressing.
const MIN_COMPRESS: usize = 64;

/// Deflate level for [`Effort::Strong`] (of 0 ..= 10): most of the gain of
/// the higher levels at a fraction of their time.
const DEFLATE_LEVEL: u8 = 5;

/// New pixels for a rectangle of a window.
#[derive(Clone, PartialEq, Eq)]
pub struct Tile {
    pub window: u32,
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
    pub encoding: Encoding,
    pub data: Vec<u8>,
}

impl std::fmt::Debug for Tile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tile")
            .field("window", &self.window)
            .field("x", &self.x)
            .field("y", &self.y)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("encoding", &self.encoding)
            .field("bytes", &self.data.len())
            .finish()
    }
}

/// Bits per palette index for `count` colours.
fn index_bits(count: usize) -> usize {
    match count {
        0..=2 => 1,
        3..=4 => 2,
        5..=16 => 4,
        _ => 8,
    }
}

/// What a [`Encoding::Packed`] header says, checked against the tile size.
struct Header<'a> {
    layout: u8,
    compressor: u8,
    palette: &'a [u8],
    body: &'a [u8],
    /// Decoded body size.
    body_len: usize,
}

fn header(data: &[u8], width: u16, height: u16) -> Result<Header<'_>, Error> {
    let (w, h) = (usize::from(width), usize::from(height));
    let (&layout, rest) = data.split_first().ok_or(Error::Short)?;
    let (&compressor, rest) = rest.split_first().ok_or(Error::Short)?;
    if compressor > PACK_DEFLATE {
        return Err(Error::Invalid("tile compressor"));
    }
    let (palette, body, body_len) = match layout {
        LAYOUT_PALETTE => {
            let (&n, rest) = rest.split_first().ok_or(Error::Short)?;
            let count = usize::from(n) + 1;
            if count < 2 {
                return Err(Error::Invalid("tile palette"));
            }
            if rest.len() < count * 3 {
                return Err(Error::Short);
            }
            let (palette, body) = rest.split_at(count * 3);
            (palette, body, h * (w * index_bits(count)).div_ceil(8))
        }
        LAYOUT_ROWS => (&[][..], rest, h * (1 + w * 3)),
        _ => return Err(Error::Invalid("tile layout")),
    };
    if compressor == PACK_NONE && body.len() != body_len {
        return Err(Error::Invalid("tile body size"));
    }
    Ok(Header {
        layout,
        compressor,
        palette,
        body,
        body_len,
    })
}

/// Undoes the compressor: exactly `len` bytes, or an error.
fn inflate(compressor: u8, body: &[u8], len: usize) -> Result<Vec<u8>, Error> {
    match compressor {
        PACK_NONE => Ok(body.to_vec()),
        PACK_LZ4 => {
            let mut out = vec![0; len];
            let n = lz4_flex::block::decompress_into(body, &mut out)
                .map_err(|_| Error::Invalid("LZ4 data"))?;
            if n != len {
                return Err(Error::Invalid("LZ4 data"));
            }
            Ok(out)
        }
        _ => {
            let out = miniz_oxide::inflate::decompress_to_vec_with_limit(body, len)
                .map_err(|_| Error::Invalid("Deflate data"))?;
            if out.len() != len {
                return Err(Error::Invalid("Deflate data"));
            }
            Ok(out)
        }
    }
}

/// Packs `body` with `effort`, or leaves it as it is when that is smaller.
fn deflate(body: Vec<u8>, effort: Effort) -> (u8, Vec<u8>) {
    if body.len() < MIN_COMPRESS {
        return (PACK_NONE, body);
    }
    let (kind, packed) = match effort {
        Effort::Fast => (PACK_LZ4, lz4_flex::block::compress(&body)),
        Effort::Strong => (
            PACK_DEFLATE,
            miniz_oxide::deflate::compress_to_vec(&body, DEFLATE_LEVEL),
        ),
    };
    if packed.len() < body.len() {
        (kind, packed)
    } else {
        (PACK_NONE, body)
    }
}

/// The pixel at `i` (4 bytes each) as `0x00RRGGBB`.
fn colour_at(pixels: &[u8], i: usize) -> u32 {
    match pixels.get(i * 4..i * 4 + 3) {
        Some(&[b, g, r]) => u32::from_le_bytes([b, g, r, 0]),
        _ => 0,
    }
}

/// The distinct colours of `pixels`, in order of first use, or `None` past
/// 256 of them.
fn palette_of(pixels: &[u8]) -> Option<(Vec<u32>, Vec<u8>)> {
    let n = pixels.len() / 4;
    let mut colours: Vec<u32> = Vec::new();
    let mut lookup: std::collections::HashMap<u32, u8> = std::collections::HashMap::new();
    let mut indices = Vec::with_capacity(n);
    let mut last = (u32::MAX, 0u8);
    for i in 0..n {
        let c = colour_at(pixels, i);
        if c == last.0 {
            indices.push(last.1);
            continue;
        }
        let idx = if colours.len() <= 16 {
            colours.iter().position(|&k| k == c)
        } else {
            lookup.get(&c).map(|&k| usize::from(k))
        };
        let idx = match idx {
            Some(k) => k,
            None => {
                if colours.len() == 256 {
                    return None;
                }
                colours.push(c);
                let k = colours.len() - 1;
                if colours.len() == 17 {
                    // From here a map beats a scan.
                    lookup = colours
                        .iter()
                        .enumerate()
                        .map(|(k, &c)| (c, k as u8))
                        .collect();
                } else if colours.len() > 17 {
                    lookup.insert(c, k as u8);
                }
                k
            }
        };
        let idx = idx as u8;
        last = (c, idx);
        indices.push(idx);
    }
    Some((colours, indices))
}

/// Filtered colour rows; see the module text.
fn rows_body(pixels: &[u8], w: usize, h: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(h * (1 + w * 3));
    let mut prev = vec![0u8; w * 3];
    let mut cur = vec![0u8; w * 3];
    let mut cand = [vec![0u8; w * 3], vec![0u8; w * 3], vec![0u8; w * 3]];
    for y in 0..h {
        for x in 0..w {
            let s = (y * w + x) * 4;
            if let (Some(d), Some(src)) = (cur.get_mut(x * 3..x * 3 + 3), pixels.get(s..s + 3)) {
                d.copy_from_slice(src);
            }
        }
        // Each filter, scored by the sum of the bytes as signed values
        // (the usual PNG rule: small differences compress best).
        let mut best = (0usize, u64::MAX);
        for (f, c) in cand.iter_mut().enumerate() {
            let mut score = 0u64;
            for (i, out) in c.iter_mut().enumerate() {
                let v = cur.get(i).copied().unwrap_or(0);
                let p = match f {
                    0 => 0,
                    1 => i
                        .checked_sub(3)
                        .and_then(|j| cur.get(j))
                        .copied()
                        .unwrap_or(0),
                    _ => prev.get(i).copied().unwrap_or(0),
                };
                *out = v.wrapping_sub(p);
                score += u64::from((*out as i8).unsigned_abs());
            }
            if score < best.1 {
                best = (f, score);
            }
        }
        out.push(best.0 as u8);
        if let Some(c) = cand.get(best.0) {
            out.extend_from_slice(c);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    out
}

impl Tile {
    /// Decoded size in bytes.
    pub fn pixel_bytes(&self) -> usize {
        usize::from(self.width) * usize::from(self.height) * 4
    }

    /// A tile of `pixels` (`width` x `height`, 4 bytes each), LZ4-packed
    /// the protocol 5 way.
    pub fn lz4(window: u32, x: u16, y: u16, width: u16, height: u16, pixels: &[u8]) -> Tile {
        Tile {
            window,
            x,
            y,
            width,
            height,
            encoding: Encoding::Lz4,
            data: lz4_flex::block::compress_prepend_size(pixels),
        }
    }

    /// A tile of `pixels` (`width` x `height`, 4 bytes each) in the
    /// smallest of [`Encoding::Solid`] and [`Encoding::Packed`] that fits
    /// it, compressed with `effort`. The unused fourth bytes are dropped.
    pub fn pack(
        window: u32,
        x: u16,
        y: u16,
        width: u16,
        height: u16,
        pixels: &[u8],
        effort: Effort,
    ) -> Tile {
        let (w, h) = (usize::from(width), usize::from(height));
        let tile = |encoding, data| Tile {
            window,
            x,
            y,
            width,
            height,
            encoding,
            data,
        };
        let data = match palette_of(pixels) {
            Some((colours, _)) if colours.len() <= 1 => {
                let c = colours.first().copied().unwrap_or(0).to_le_bytes();
                return tile(Encoding::Solid, c.get(..3).unwrap_or_default().to_vec());
            }
            Some((colours, indices)) => {
                let bits = index_bits(colours.len());
                let stride = (w * bits).div_ceil(8);
                let mut body = vec![0u8; h * stride];
                for (row, line) in indices.chunks(w.max(1)).zip(body.chunks_mut(stride.max(1))) {
                    for (x, &k) in row.iter().enumerate() {
                        let bit = x * bits;
                        if let Some(b) = line.get_mut(bit / 8) {
                            *b |= k << (8 - bits - bit % 8);
                        }
                    }
                }
                let (kind, body) = deflate(body, effort);
                let mut data = Vec::with_capacity(3 + colours.len() * 3 + body.len());
                data.extend_from_slice(&[LAYOUT_PALETTE, kind, (colours.len() - 1) as u8]);
                for c in &colours {
                    data.extend_from_slice(c.to_le_bytes().get(..3).unwrap_or_default());
                }
                data.extend_from_slice(&body);
                data
            }
            None => {
                let (kind, body) = deflate(rows_body(pixels, w, h), effort);
                let mut data = Vec::with_capacity(2 + body.len());
                data.extend_from_slice(&[LAYOUT_ROWS, kind]);
                data.extend_from_slice(&body);
                data
            }
        };
        tile(Encoding::Packed, data)
    }

    /// Checks the geometry and the stated sizes, without decoding.
    pub fn check(&self) -> Result<(), Error> {
        if self.width == 0
            || self.height == 0
            || self.width > MAX_TILE_SIDE
            || self.height > MAX_TILE_SIDE
            || u32::from(self.x) + u32::from(self.width) > u32::from(MAX_WINDOW_SIDE)
            || u32::from(self.y) + u32::from(self.height) > u32::from(MAX_WINDOW_SIDE)
        {
            return Err(Error::Invalid("tile geometry"));
        }
        let want = self.pixel_bytes();
        match self.encoding {
            Encoding::Raw if self.data.len() != want => Err(Error::Invalid("raw tile size")),
            Encoding::Raw => Ok(()),
            Encoding::Lz4 => {
                let Some(head) = self.data.get(..4) else {
                    return Err(Error::Short);
                };
                let mut n = [0; 4];
                n.copy_from_slice(head);
                if u32::from_le_bytes(n) as usize != want {
                    return Err(Error::Invalid("LZ4 tile size"));
                }
                Ok(())
            }
            Encoding::Solid if self.data.len() != 3 => Err(Error::Invalid("solid tile size")),
            Encoding::Solid => Ok(()),
            Encoding::Packed => header(&self.data, self.width, self.height).map(|_| ()),
        }
    }

    /// The tile's pixels, `width * height * 4` bytes.
    pub fn decode(&self) -> Result<Vec<u8>, Error> {
        self.check()?;
        let (w, h) = (usize::from(self.width), usize::from(self.height));
        match self.encoding {
            Encoding::Raw => Ok(self.data.clone()),
            Encoding::Lz4 => {
                let mut out = vec![0; self.pixel_bytes()];
                let body = self.data.get(4..).unwrap_or_default();
                let n = lz4_flex::block::decompress_into(body, &mut out)
                    .map_err(|_| Error::Invalid("LZ4 data"))?;
                if n != out.len() {
                    return Err(Error::Invalid("LZ4 data"));
                }
                Ok(out)
            }
            Encoding::Solid => {
                let mut px = [0u8; 4];
                if let Some(d) = px.get_mut(..3) {
                    d.copy_from_slice(&self.data);
                }
                Ok(px.repeat(w * h))
            }
            Encoding::Packed => {
                let hd = header(&self.data, self.width, self.height)?;
                let body = inflate(hd.compressor, hd.body, hd.body_len)?;
                let mut out = vec![0u8; self.pixel_bytes()];
                if hd.layout == LAYOUT_PALETTE {
                    let count = hd.palette.len() / 3;
                    let bits = index_bits(count);
                    let stride = (w * bits).div_ceil(8);
                    let mask = ((1u16 << bits) - 1) as u8;
                    for (y, line) in body.chunks(stride.max(1)).take(h).enumerate() {
                        for x in 0..w {
                            let bit = x * bits;
                            let byte = line.get(bit / 8).copied().unwrap_or(0);
                            let k = usize::from((byte >> (8 - bits - bit % 8)) & mask);
                            let c = hd
                                .palette
                                .get(k * 3..k * 3 + 3)
                                .ok_or(Error::Invalid("palette index"))?;
                            let o = (y * w + x) * 4;
                            if let Some(d) = out.get_mut(o..o + 3) {
                                d.copy_from_slice(c);
                            }
                        }
                    }
                } else {
                    let row = w * 3;
                    let mut prev = vec![0u8; row];
                    let mut cur = vec![0u8; row];
                    for (y, line) in body.chunks(row + 1).take(h).enumerate() {
                        let (&filter, bytes) = line.split_first().ok_or(Error::Short)?;
                        if filter > 2 {
                            return Err(Error::Invalid("row filter"));
                        }
                        for i in 0..row {
                            let v = bytes.get(i).copied().unwrap_or(0);
                            let p = match filter {
                                0 => 0,
                                1 => i
                                    .checked_sub(3)
                                    .and_then(|j| cur.get(j))
                                    .copied()
                                    .unwrap_or(0),
                                _ => prev.get(i).copied().unwrap_or(0),
                            };
                            if let Some(c) = cur.get_mut(i) {
                                *c = v.wrapping_add(p);
                            }
                        }
                        for x in 0..w {
                            let o = (y * w + x) * 4;
                            if let (Some(d), Some(s)) =
                                (out.get_mut(o..o + 3), cur.get(x * 3..x * 3 + 3))
                            {
                                d.copy_from_slice(s);
                            }
                        }
                        std::mem::swap(&mut prev, &mut cur);
                    }
                }
                Ok(out)
            }
        }
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

    /// Pixels with the fourth byte 0, as the agent sends them.
    fn px(w: usize, h: usize, mut f: impl FnMut(usize, usize) -> u32) -> Vec<u8> {
        let mut out = Vec::with_capacity(w * h * 4);
        for y in 0..h {
            for x in 0..w {
                out.extend_from_slice(&(f(x, y) & 0xff_ffff).to_le_bytes());
            }
        }
        out
    }

    fn round_trip(w: u16, h: u16, pixels: &[u8]) -> [Tile; 2] {
        [Effort::Fast, Effort::Strong].map(|e| {
            let t = Tile::pack(1, 0, 0, w, h, pixels, e);
            assert_eq!(t.decode().as_deref(), Ok(pixels), "{t:?}");
            // Survives the wire.
            let wire = crate::Message::Tile(t.clone()).encode();
            assert_eq!(
                crate::Message::decode(&wire[4..]),
                Ok(crate::Message::Tile(t.clone()))
            );
            t
        })
    }

    #[test]
    fn lz4_round_trip() {
        let px: Vec<u8> = (0..64 * 32 * 4).map(|i| (i % 7) as u8).collect();
        let t = Tile::lz4(1, 10, 20, 64, 32, &px);
        assert!(t.data.len() < px.len());
        assert_eq!(t.decode(), Ok(px));
    }

    #[test]
    fn solid() {
        let p = px(100, 7, |_, _| 0x123456);
        for t in round_trip(100, 7, &p) {
            assert_eq!(t.encoding, Encoding::Solid);
            assert_eq!(t.data, [0x56, 0x34, 0x12]);
        }
    }

    #[test]
    fn palettes_of_every_width() {
        for count in [2u32, 3, 4, 5, 16, 17, 200, 256] {
            for (w, h) in [(1, 1), (3, 5), (17, 9), (64, 64), (512, 2)] {
                let p = px(w, h, |x, y| ((x * 7 + y * 3) as u32 % count) * 0x010203);
                let ts = round_trip(w as u16, h as u16, &p);
                let distinct = palette_of(&p).unwrap().0.len();
                if distinct > 1 {
                    assert_eq!(ts[0].encoding, Encoding::Packed);
                    assert_eq!(ts[0].data[0], LAYOUT_PALETTE);
                }
            }
        }
    }

    #[test]
    fn colour_rows() {
        // A gradient: far more than 256 colours.
        let (w, h) = (300usize, 70usize);
        let p = px(w, h, |x, y| {
            (x as u32) << 16 | (y as u32 * 3) << 8 | (x + y) as u32
        });
        let ts = round_trip(w as u16, h as u16, &p);
        for t in &ts {
            assert_eq!(t.data[0], LAYOUT_ROWS);
        }
        // Filtered, a gradient packs far better than LZ4 on the raw pixels.
        let old = Tile::lz4(1, 0, 0, w as u16, h as u16, &p);
        assert!(
            ts[0].data.len() * 4 < old.data.len(),
            "{} vs {}",
            ts[0].data.len(),
            old.data.len()
        );
        assert!(ts[1].data.len() <= ts[0].data.len());
        // Noise does not compress: stored as it is.
        let mut seed = 1u32;
        let noise = px(64, 64, |_, _| {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed
        });
        for t in round_trip(64, 64, &noise) {
            assert_eq!(t.data[1], PACK_NONE);
        }
    }

    #[test]
    fn text_packs_smaller_than_lz4() {
        // Something like a terminal: dark background, two text colours.
        let (w, h) = (512usize, 512usize);
        let p = px(w, h, |x, y| {
            let glyph = (x / 8 * 31 + y / 16 * 17) % 11;
            if (x % 8 + y % 16 + glyph) % 5 == 0 && y % 16 < 13 {
                if glyph < 3 { 0x88_c0_d0 } else { 0xd8_de_e9 }
            } else {
                0x2e_34_40
            }
        });
        let ts = round_trip(w as u16, h as u16, &p);
        let old = Tile::lz4(1, 0, 0, w as u16, h as u16, &p);
        assert!(
            ts[0].data.len() * 2 < old.data.len(),
            "{} vs {}",
            ts[0].data.len(),
            old.data.len()
        );
        assert!(ts[1].data.len() < ts[0].data.len());
    }

    #[test]
    fn refuses_lies() {
        let mut t = Tile::lz4(1, 0, 0, 2, 2, &[1; 16]);
        t.width = 3;
        assert!(t.decode().is_err());
        let t = Tile {
            window: 1,
            x: 0,
            y: 0,
            width: 2,
            height: 2,
            encoding: Encoding::Raw,
            data: vec![0; 15],
        };
        assert!(t.decode().is_err());
        let t = Tile {
            window: 1,
            x: 8000,
            y: 0,
            width: 500,
            height: 2,
            encoding: Encoding::Raw,
            data: vec![0; 4000],
        };
        assert!(t.check().is_err());
        // Short LZ4 data that claims the right size.
        let mut t = Tile::lz4(1, 0, 0, 16, 16, &[0; 1024]);
        t.data.truncate(6);
        assert!(t.decode().is_err());

        let p = px(40, 40, |x, y| ((x + y) % 3) as u32 * 0x111111);
        let good = Tile::pack(1, 0, 0, 40, 40, &p, Effort::Strong);
        // A tile cut anywhere fails, and never panics.
        for n in 0..good.data.len() {
            let mut t = good.clone();
            t.data.truncate(n);
            assert!(t.decode().is_err(), "cut at {n}");
        }
        // A palette index past the palette.
        let mut t = Tile::pack(
            1,
            0,
            0,
            4,
            1,
            &px(4, 1, |x, _| (x % 3) as u32),
            Effort::Fast,
        );
        assert_eq!(&t.data[..3], &[LAYOUT_PALETTE, PACK_NONE, 2]);
        t.data[3 + 9] = 0b1111_0000;
        assert!(t.decode().is_err());
        // Bad layout, compressor, filter.
        for (i, v) in [(0, 9), (1, 9)] {
            let mut t = good.clone();
            t.data[i] = v;
            assert!(t.check().is_err());
        }
        let mut t = Tile::pack(
            1,
            0,
            0,
            2,
            1,
            &px(2, 1, |x, _| x as u32 * 77777),
            Effort::Fast,
        );
        t.data = vec![LAYOUT_ROWS, PACK_NONE, 3, 0, 0, 0, 0, 0, 0];
        assert!(t.check().is_ok());
        assert!(t.decode().is_err());
        // A Deflate bomb stops at the stated size.
        let big = miniz_oxide::deflate::compress_to_vec(&[0; 100_000], 6);
        let mut t = Tile {
            window: 1,
            x: 0,
            y: 0,
            width: 16,
            height: 16,
            encoding: Encoding::Packed,
            data: vec![LAYOUT_ROWS, PACK_DEFLATE],
        };
        t.data.extend_from_slice(&big);
        assert!(t.decode().is_err());
        // A solid tile is 3 bytes.
        let t = Tile {
            window: 1,
            x: 0,
            y: 0,
            width: 1,
            height: 1,
            encoding: Encoding::Solid,
            data: vec![1, 2, 3, 4],
        };
        assert!(t.check().is_err());
    }
}
