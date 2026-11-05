//! Window pixels.
//!
//! A [`Tile`] carries a rectangle of one window. Pixels are 32 bits, bytes
//! in the order blue, green, red, unused (what X11 ZPixmap depth 24 and a
//! Windows top-down DIB both use), rows top to bottom with no padding.

use crate::{Error, MAX_WINDOW_SIDE};

/// Largest tile, in decoded bytes (512 x 512 pixels). Bigger damage is sent
/// as several tiles.
pub const MAX_TILE_BYTES: usize = 512 * 512 * 4;

/// Largest side of a tile, in pixels.
pub const MAX_TILE_SIDE: u16 = 512;

/// How a tile's bytes are packed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Encoding {
    /// Pixels as they are.
    Raw = 0,
    /// An LZ4 block of the raw pixels, after a `u32` LE decoded size.
    Lz4 = 1,
}

impl Encoding {
    pub(crate) fn from_u8(v: u8) -> Result<Self, Error> {
        match v {
            0 => Ok(Encoding::Raw),
            1 => Ok(Encoding::Lz4),
            _ => Err(Error::Invalid("tile encoding")),
        }
    }
}

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

impl Tile {
    /// Decoded size in bytes.
    pub fn pixel_bytes(&self) -> usize {
        usize::from(self.width) * usize::from(self.height) * 4
    }

    /// A tile of `pixels` (`width` x `height`, 4 bytes each), LZ4-packed.
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
        }
    }

    /// The tile's pixels, `width * height * 4` bytes.
    pub fn decode(&self) -> Result<Vec<u8>, Error> {
        self.check()?;
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

    #[test]
    fn lz4_round_trip() {
        let px: Vec<u8> = (0..64 * 32 * 4).map(|i| (i % 7) as u8).collect();
        let t = Tile::lz4(1, 10, 20, 64, 32, &px);
        assert!(t.data.len() < px.len());
        assert_eq!(t.decode(), Ok(px));
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
    }
}
