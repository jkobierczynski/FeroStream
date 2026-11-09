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

    /// Copies the `width` x `height` pixels at `x`,`y` to `to_x`,`to_y` (a
    /// scroll), as if read whole first. A copy that does not fit in the
    /// window changes nothing.
    pub fn copy(
        &mut self,
        x: u16,
        y: u16,
        width: u16,
        height: u16,
        to_x: u16,
        to_y: u16,
    ) -> Option<Changed> {
        let fits = |a: u16, n: u16, limit: u16| u32::from(a) + u32::from(n) <= u32::from(limit);
        if width == 0
            || height == 0
            || !fits(x, width, self.width)
            || !fits(to_x, width, self.width)
            || !fits(y, height, self.height)
            || !fits(to_y, height, self.height)
        {
            return None;
        }
        let stride = usize::from(self.width) * 4;
        let len = usize::from(width) * 4;
        let row = |i: u16, base: u16, col: u16| {
            (usize::from(base) + usize::from(i)) * stride + usize::from(col) * 4
        };
        // Moving down, the bottom row goes first, so no row is overwritten
        // before it is read; within a row copy_within handles overlap.
        let mut each = |i: u16| {
            let (src, dst) = (row(i, y, x), row(i, to_y, to_x));
            if src + len <= self.pixels.len() && dst + len <= self.pixels.len() {
                self.pixels.copy_within(src..src + len, dst);
            }
        };
        if to_y > y {
            (0..height).rev().for_each(&mut each);
        } else {
            (0..height).for_each(&mut each);
        }
        Some(Changed {
            x: to_x,
            y: to_y,
            width,
            height,
        })
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
    fn copy_scrolls_both_ways() {
        let mut fb = Framebuffer::new(2, 4);
        fb.pixels = (0..32).collect();
        // Up by one row: rows 1..4 to 0..3.
        assert!(fb.copy(0, 1, 2, 3, 0, 0).is_some());
        assert_eq!(&fb.pixels[..24], &(8..32).collect::<Vec<u8>>()[..]);
        fb.pixels = (0..32).collect();
        // Down by two: rows 0..2 to 2..4.
        assert!(fb.copy(0, 0, 2, 2, 0, 2).is_some());
        assert_eq!(&fb.pixels[16..], &(0..16).collect::<Vec<u8>>()[..]);
        assert_eq!(&fb.pixels[..16], &(0..16).collect::<Vec<u8>>()[..]);
        // Past the edge: nothing.
        assert_eq!(fb.copy(0, 3, 2, 2, 0, 0), None);
        assert_eq!(fb.copy(1, 0, 2, 1, 0, 0), None);
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
