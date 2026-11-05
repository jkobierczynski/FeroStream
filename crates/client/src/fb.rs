//! A window's pixels on the client side.

use ferostream_proto::tile::Tile;

/// The picture of one remote window: `width * height` pixels, 4 bytes each
/// (blue, green, red, unused), rows top to bottom.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Framebuffer {
    pub width: u16,
    pub height: u16,
    pub pixels: Vec<u8>,
}

/// A rectangle that changed, in window pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Changed {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

impl Framebuffer {
    pub fn new(width: u16, height: u16) -> Self {
        Self {
            width,
            height,
            // Mid grey until the first tiles arrive.
            pixels: vec![0x80; usize::from(width) * usize::from(height) * 4],
        }
    }

    /// Changes the size, keeping what fits in the top-left corner.
    pub fn resize(&mut self, width: u16, height: u16) {
        if (width, height) == (self.width, self.height) {
            return;
        }
        let mut next = Framebuffer::new(width, height);
        let copy_w = usize::from(width.min(self.width)) * 4;
        for y in 0..usize::from(height.min(self.height)) {
            let from = y * usize::from(self.width) * 4;
            let to = y * usize::from(width) * 4;
            if let (Some(dst), Some(src)) = (
                next.pixels.get_mut(to..to + copy_w),
                self.pixels.get(from..from + copy_w),
            ) {
                dst.copy_from_slice(src);
            }
        }
        *self = next;
    }

    /// Decodes a tile and copies the part inside the window. A tile that
    /// does not decode changes nothing.
    pub fn apply(&mut self, tile: &Tile) -> Option<Changed> {
        let px = tile.decode().ok()?;
        if tile.x >= self.width || tile.y >= self.height {
            return None;
        }
        let w = tile.width.min(self.width - tile.x);
        let h = tile.height.min(self.height - tile.y);
        let row = usize::from(w) * 4;
        for y in 0..usize::from(h) {
            let src = y * usize::from(tile.width) * 4;
            let dst =
                ((usize::from(tile.y) + y) * usize::from(self.width) + usize::from(tile.x)) * 4;
            if let (Some(d), Some(s)) =
                (self.pixels.get_mut(dst..dst + row), px.get(src..src + row))
            {
                d.copy_from_slice(s);
            }
        }
        Some(Changed {
            x: tile.x,
            y: tile.y,
            width: w,
            height: h,
        })
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
    fn apply_clips_to_the_window() {
        let mut fb = Framebuffer::new(4, 3);
        let t = Tile::lz4(1, 2, 1, 4, 4, &[7; 64]);
        assert_eq!(
            fb.apply(&t),
            Some(Changed {
                x: 2,
                y: 1,
                width: 2,
                height: 2
            })
        );
        let at = |x: usize, y: usize| fb.pixels[(y * 4 + x) * 4];
        assert_eq!(at(1, 1), 0x80);
        assert_eq!(at(2, 1), 7);
        assert_eq!(at(3, 2), 7);
        assert_eq!(at(3, 0), 0x80);
    }

    #[test]
    fn resize_keeps_the_corner() {
        let mut fb = Framebuffer::new(2, 2);
        fb.pixels = (0..16).collect();
        fb.resize(3, 1);
        assert_eq!(&fb.pixels[..8], &[0, 1, 2, 3, 4, 5, 6, 7]);
        assert_eq!(fb.pixels.len(), 12);
    }
}
