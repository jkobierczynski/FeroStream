//! Small pictures with transparency: pointer shapes and window icons.
//!
//! Pixels are 32 bits, bytes in the order blue, green, red, alpha, with
//! straight (not premultiplied) alpha, rows top to bottom with no padding.
//! That is what a Windows 32-bit icon or cursor bitmap takes. The bytes go
//! LZ4-packed after a `u32` LE decoded size, as tiles do.

use crate::Error;

/// Largest side of an image, in pixels (a 256 x 256 icon or cursor).
pub const MAX_IMAGE_SIDE: u16 = 256;

/// A small BGRA picture, LZ4-packed.
#[derive(Clone, PartialEq, Eq)]
pub struct Image {
    pub width: u16,
    pub height: u16,
    pub data: Vec<u8>,
}

impl std::fmt::Debug for Image {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Image")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("bytes", &self.data.len())
            .finish()
    }
}

impl Image {
    /// Packs `pixels` (`width` x `height`, 4 bytes each).
    pub fn new(width: u16, height: u16, pixels: &[u8]) -> Image {
        Image {
            width,
            height,
            data: lz4_flex::block::compress_prepend_size(pixels),
        }
    }

    /// Decoded size in bytes.
    pub fn pixel_bytes(&self) -> usize {
        usize::from(self.width) * usize::from(self.height) * 4
    }

    /// Checks the size and the stated decoded size, without decoding.
    pub fn check(&self) -> Result<(), Error> {
        if self.width == 0
            || self.height == 0
            || self.width > MAX_IMAGE_SIDE
            || self.height > MAX_IMAGE_SIDE
        {
            return Err(Error::Invalid("image size"));
        }
        let Some(head) = self.data.get(..4) else {
            return Err(Error::Short);
        };
        let mut n = [0; 4];
        n.copy_from_slice(head);
        if u32::from_le_bytes(n) as usize != self.pixel_bytes() {
            return Err(Error::Invalid("LZ4 image size"));
        }
        Ok(())
    }

    /// The pixels, `width * height * 4` bytes.
    pub fn decode(&self) -> Result<Vec<u8>, Error> {
        self.check()?;
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

/// Turns X11 ARGB pixels (one `u32` each, alpha premultiplied, as XFixes
/// gives a cursor) into [`Image`] bytes with straight alpha.
pub fn from_premultiplied_argb(pixels: &[u32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(pixels.len() * 4);
    for &p in pixels {
        let a = (p >> 24) as u8;
        let [b, g, r] = [p as u8, (p >> 8) as u8, (p >> 16) as u8];
        let un = |c: u8| -> u8 {
            if a == 0 {
                0
            } else {
                ((u32::from(c) * 255 + u32::from(a) / 2) / u32::from(a)).min(255) as u8
            }
        };
        out.extend_from_slice(&[un(b), un(g), un(r), a]);
    }
    out
}

/// Turns X11 ARGB pixels with straight alpha (`_NET_WM_ICON`) into
/// [`Image`] bytes.
pub fn from_argb(pixels: &[u32]) -> Vec<u8> {
    pixels.iter().flat_map(|p| p.to_le_bytes()).collect()
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
    fn round_trip_and_limits() {
        let px = vec![7u8; 32 * 32 * 4];
        let i = Image::new(32, 32, &px);
        assert_eq!(i.decode(), Ok(px));
        let mut big = Image::new(32, 32, &[0; 32 * 32 * 4]);
        big.width = 300;
        assert!(big.check().is_err());
        let mut lie = Image::new(2, 2, &[0; 16]);
        lie.height = 3;
        assert!(lie.decode().is_err());
    }

    #[test]
    fn premultiplied_alpha_is_undone() {
        // Half-transparent pure red, premultiplied: 0x80 alpha, 0x80 red.
        assert_eq!(
            from_premultiplied_argb(&[0x8080_0000]),
            vec![0, 0, 255, 0x80]
        );
        assert_eq!(from_premultiplied_argb(&[0x0012_3456]), vec![0, 0, 0, 0]);
        assert_eq!(
            from_premultiplied_argb(&[0xff11_2233]),
            vec![0x33, 0x22, 0x11, 0xff]
        );
        assert_eq!(from_argb(&[0x8011_2233]), vec![0x33, 0x22, 0x11, 0x80]);
    }
}
