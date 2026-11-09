//! What the client has on screen for one window, kept by the agent.
//!
//! The agent only sends pixels that differ from this copy, and finds
//! scrolls in it: rows that moved up or down go as a
//! [`ferostream_proto::Message::TileCopy`] instead of new pixels. It must
//! match the client's picture exactly, so it changes the way the client's
//! does: starts mid grey, keeps the top-left corner on a resize, takes every
//! tile and copy sent, in order.
//!
//! Pixels from X have their unused fourth byte set to 0 before they get
//! here, and so does every tile the client decodes. The grey of a window not
//! drawn yet has 0x80 there, so it never matches real pixels.

use std::collections::HashMap;

/// A rectangle in window pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

/// Unchanged rows shorter than this between changed ones do not split a
/// changed area in two (each piece costs a tile header and a FrameEnd
/// round of work; a few unchanged rows pack to almost nothing).
const JOIN_GAP: usize = 8;

/// A scroll is only worth a copy over this many rows.
const MIN_SCROLL_ROWS: usize = 16;

/// Areas narrower or lower than this are not searched for scrolls.
const MIN_SCROLL_SIDE: u16 = 32;

/// A rows-moved-by-`dy` copy found by [`Shadow::find_scroll`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Scroll {
    pub x: u16,
    pub w: u16,
    /// First row of the source.
    pub from_y: u16,
    /// First row of the destination.
    pub to_y: u16,
    pub rows: u16,
}

/// The client's picture of one window; see the module text.
#[derive(Debug, Clone, Default)]
pub struct Shadow {
    w: u16,
    h: u16,
    px: Vec<u8>,
}

/// A row's hash: equal rows hash equal; unequal ones almost never do.
fn row_hash(row: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut chunks = row.chunks_exact(8);
    for c in &mut chunks {
        let mut b = [0u8; 8];
        b.copy_from_slice(c);
        h = (h ^ u64::from_le_bytes(b))
            .wrapping_mul(0x0100_0000_01b3)
            .rotate_left(29);
    }
    for &b in chunks.remainder() {
        h = (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// Whether every pixel of `row` is the same.
fn flat(row: &[u8]) -> bool {
    match row.get(..4) {
        Some(first) => row.chunks_exact(4).all(|p| p == first),
        None => true,
    }
}

impl Shadow {
    /// A window the client has just created: all mid grey.
    pub fn new(w: u16, h: u16) -> Shadow {
        Shadow {
            w,
            h,
            px: vec![0x80; usize::from(w) * usize::from(h) * 4],
        }
    }

    /// Size change, as the client makes it: keeps the top-left corner and
    /// fills the rest with grey.
    pub fn resize(&mut self, w: u16, h: u16) {
        if (w, h) == (self.w, self.h) {
            return;
        }
        let mut next = Shadow::new(w, h);
        let copy = usize::from(w.min(self.w)) * 4;
        for y in 0..usize::from(h.min(self.h)) {
            let from = y * usize::from(self.w) * 4;
            let to = y * usize::from(w) * 4;
            if let (Some(d), Some(s)) = (
                next.px.get_mut(to..to + copy),
                self.px.get(from..from + copy),
            ) {
                d.copy_from_slice(s);
            }
        }
        *self = next;
    }

    /// Row `y` of the window, columns `x .. x + w`.
    fn row(&self, x: u16, y: usize, w: u16) -> &[u8] {
        let start = (y * usize::from(self.w) + usize::from(x)) * 4;
        self.px
            .get(start..start + usize::from(w) * 4)
            .unwrap_or_default()
    }

    fn inside(&self, r: Rect) -> bool {
        r.w > 0
            && r.h > 0
            && u32::from(r.x) + u32::from(r.w) <= u32::from(self.w)
            && u32::from(r.y) + u32::from(r.h) <= u32::from(self.h)
    }

    /// Looks for a vertical scroll inside `r`, whose new pixels are `new`
    /// (`r.w * r.h * 4` bytes): rows of `new` that the client already has,
    /// higher or lower. The longest such run of rows, if it is long enough.
    pub fn find_scroll(&self, r: Rect, new: &[u8]) -> Option<Scroll> {
        if r.w < MIN_SCROLL_SIDE || r.h < MIN_SCROLL_SIDE || !self.inside(r) {
            return None;
        }
        let stride = usize::from(r.w) * 4;
        let rows = usize::from(r.h);
        if new.len() != stride * rows {
            return None;
        }
        let new_row = |i: usize| new.get(i * stride..(i + 1) * stride).unwrap_or_default();
        let old_row = |i: usize| self.row(r.x, usize::from(r.y) + i, r.w);

        // Old rows by hash; a hash seen twice says nothing about where a
        // row came from.
        let mut old: HashMap<u64, Option<usize>> = HashMap::with_capacity(rows);
        for j in 0..rows {
            let row = old_row(j);
            if flat(row) {
                continue;
            }
            old.entry(row_hash(row))
                .and_modify(|e| *e = None)
                .or_insert(Some(j));
        }
        let mut votes: HashMap<isize, usize> = HashMap::new();
        let mut telling = 0;
        for i in 0..rows {
            let row = new_row(i);
            if flat(row) {
                continue;
            }
            telling += 1;
            if let Some(Some(j)) = old.get(&row_hash(row))
                && *j != i
            {
                *votes.entry(i as isize - *j as isize).or_default() += 1;
            }
        }
        let (dy, n) = votes.into_iter().max_by_key(|&(dy, n)| (n, -dy.abs()))?;
        if n < 4 || n * 4 < telling {
            return None;
        }
        // The longest run of rows that really are the old ones moved by dy.
        let mut best = (0usize, 0usize);
        let mut run = (0usize, 0usize);
        for i in 0..rows {
            let j = i as isize - dy;
            let same = j >= 0 && (j as usize) < rows && new_row(i) == old_row(j as usize);
            if same {
                if run.1 == 0 {
                    run.0 = i;
                }
                run.1 += 1;
                if run.1 > best.1 {
                    best = run;
                }
            } else {
                run.1 = 0;
            }
        }
        if best.1 < MIN_SCROLL_ROWS {
            return None;
        }
        let to = usize::from(r.y) + best.0;
        let from = (to as isize - dy) as usize;
        Some(Scroll {
            x: r.x,
            w: r.w,
            from_y: from as u16,
            to_y: to as u16,
            rows: best.1 as u16,
        })
    }

    /// Applies a copy the client is told to make.
    pub fn copy(&mut self, s: Scroll) {
        let from = Rect {
            x: s.x,
            y: s.from_y,
            w: s.w,
            h: s.rows,
        };
        let to = Rect { y: s.to_y, ..from };
        if !self.inside(from) || !self.inside(to) {
            return;
        }
        let stride = usize::from(self.w) * 4;
        let len = usize::from(s.w) * 4;
        let x = usize::from(s.x) * 4;
        let rows: Vec<usize> = (0..usize::from(s.rows)).collect();
        // Moving down: copy the bottom row first, so no source row is
        // overwritten before it is read.
        let order: Box<dyn Iterator<Item = &usize>> = if s.to_y > s.from_y {
            Box::new(rows.iter().rev())
        } else {
            Box::new(rows.iter())
        };
        for &i in order {
            let src = (usize::from(s.from_y) + i) * stride + x;
            let dst = (usize::from(s.to_y) + i) * stride + x;
            if src + len <= self.px.len() && dst + len <= self.px.len() {
                self.px.copy_within(src..src + len, dst);
            }
        }
    }

    /// The parts of `r` where `new` (`r.w * r.h * 4` bytes) differs from
    /// what the client has: bands of changed rows, each as narrow as its
    /// changes.
    pub fn changed(&self, r: Rect, new: &[u8]) -> Vec<Rect> {
        if !self.inside(r) || new.len() != usize::from(r.w) * usize::from(r.h) * 4 {
            // Outside the picture we know: all of it.
            return vec![r];
        }
        let stride = usize::from(r.w) * 4;
        let mut out = Vec::new();
        // Current band: first row, last changed row, leftmost and rightmost
        // changed column (exclusive).
        let mut band: Option<(usize, usize, usize, usize)> = None;
        let close = |b: (usize, usize, usize, usize), out: &mut Vec<Rect>| {
            out.push(Rect {
                x: r.x + b.2 as u16,
                y: r.y + b.0 as u16,
                w: (b.3 - b.2) as u16,
                h: (b.1 - b.0 + 1) as u16,
            });
        };
        for i in 0..usize::from(r.h) {
            let a = new.get(i * stride..(i + 1) * stride).unwrap_or_default();
            let b = self.row(r.x, usize::from(r.y) + i, r.w);
            if a == b {
                if let Some(bd) = band
                    && i - bd.1 > JOIN_GAP
                {
                    close(bd, &mut out);
                    band = None;
                }
                continue;
            }
            let pa = a.chunks_exact(4);
            let pb = b.chunks_exact(4);
            let first = pa
                .clone()
                .zip(pb.clone())
                .position(|(p, q)| p != q)
                .unwrap_or(0);
            let last = usize::from(r.w)
                - pa.rev()
                    .zip(pb.rev())
                    .position(|(p, q)| p != q)
                    .unwrap_or(0);
            band = Some(match band {
                Some((y0, _, x0, x1)) => (y0, i, x0.min(first), x1.max(last)),
                None => (i, i, first, last),
            });
        }
        if let Some(bd) = band {
            close(bd, &mut out);
        }
        out
    }

    /// Takes `px` (`r.w * r.h * 4` bytes) as the client's pixels for `r`.
    pub fn store(&mut self, r: Rect, px: &[u8]) {
        if !self.inside(r) {
            return;
        }
        let len = usize::from(r.w) * 4;
        for i in 0..usize::from(r.h) {
            let dst = ((usize::from(r.y) + i) * usize::from(self.w) + usize::from(r.x)) * 4;
            if let (Some(d), Some(s)) = (
                self.px.get_mut(dst..dst + len),
                px.get(i * len..(i + 1) * len),
            ) {
                d.copy_from_slice(s);
            }
        }
    }
}

/// The `sub` part of `px`, which holds the pixels of `outer`.
pub fn crop(px: &[u8], outer: Rect, sub: Rect) -> Vec<u8> {
    let stride = usize::from(outer.w) * 4;
    let len = usize::from(sub.w) * 4;
    let x = usize::from(sub.x - outer.x) * 4;
    let mut out = Vec::with_capacity(len * usize::from(sub.h));
    for i in 0..usize::from(sub.h) {
        let start = (usize::from(sub.y - outer.y) + i) * stride + x;
        out.extend_from_slice(px.get(start..start + len).unwrap_or_default());
    }
    out
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

    /// A window of `w` x `h` whose row `y` is "line" `f(y)`.
    fn page(w: u16, h: u16, f: impl Fn(usize) -> u32) -> Vec<u8> {
        let mut out = Vec::new();
        for y in 0..usize::from(h) {
            for x in 0..usize::from(w) {
                // Text-like rows: the pattern depends on the line number.
                let on = (x * 7 + f(y) as usize * 13) % 11 < 3;
                let c: u32 = if on { 0xd8dee9 ^ f(y) } else { 0x2e3440 };
                out.extend_from_slice(&c.to_le_bytes());
            }
        }
        out
    }

    fn full(w: u16, h: u16) -> Rect {
        Rect { x: 0, y: 0, w, h }
    }

    #[test]
    fn finds_a_terminal_scroll() {
        let (w, h) = (200, 120);
        let r = full(w, h);
        let mut s = Shadow::new(w, h);
        s.store(r, &page(w, h, |y| y as u32));
        // Scrolled up by 16 rows (one text line).
        let next = page(w, h, |y| y as u32 + 16);
        let sc = s.find_scroll(r, &next).unwrap();
        assert_eq!(
            sc,
            Scroll {
                x: 0,
                w,
                from_y: 16,
                to_y: 0,
                rows: 104
            }
        );
        s.copy(sc);
        // Only the new line at the bottom is left to send.
        assert_eq!(
            s.changed(r, &next),
            vec![Rect {
                x: 0,
                y: 104,
                w,
                h: 16
            }]
        );
        s.store(r, &next);
        assert_eq!(s.px, next);

        // And down by 3.
        let back = page(w, h, |y| y as u32 + 13);
        let sc = s.find_scroll(r, &back).unwrap();
        assert_eq!((sc.from_y, sc.to_y, sc.rows), (0, 3, 117));
        s.copy(sc);
        assert_eq!(
            s.changed(r, &back),
            vec![Rect {
                x: 0,
                y: 0,
                w,
                h: 3
            }]
        );
    }

    #[test]
    fn no_scroll_in_blank_or_unrelated_pictures() {
        let (w, h) = (100, 100);
        let r = full(w, h);
        let mut s = Shadow::new(w, h);
        assert_eq!(s.find_scroll(r, &page(w, h, |_| 0)), None);
        s.store(r, &page(w, h, |y| y as u32));
        assert_eq!(s.find_scroll(r, &page(w, h, |y| y as u32 * 3 + 1000)), None);
        // Too small to bother.
        let small = Rect {
            x: 0,
            y: 0,
            w: 10,
            h: 100,
        };
        assert_eq!(s.find_scroll(small, &crop(&s.px.clone(), r, small)), None);
    }

    #[test]
    fn changed_is_tight() {
        let (w, h) = (64, 64);
        let r = full(w, h);
        let mut s = Shadow::new(w, h);
        let mut px = page(w, h, |y| y as u32);
        // Never drawn: everything differs from grey.
        assert_eq!(s.changed(r, &px), vec![r]);
        s.store(r, &px);
        assert!(s.changed(r, &px).is_empty());
        // Two pixels in nearby rows: one band, as wide as the two.
        px[(5 * 64 + 10) * 4] ^= 1;
        px[(9 * 64 + 20) * 4] ^= 1;
        // One far below: a band of its own.
        px[(40 * 64 + 3) * 4] ^= 1;
        assert_eq!(
            s.changed(r, &px),
            vec![
                Rect {
                    x: 10,
                    y: 5,
                    w: 11,
                    h: 5
                },
                Rect {
                    x: 3,
                    y: 40,
                    w: 1,
                    h: 1
                }
            ]
        );
        // A part of the window.
        let part = Rect {
            x: 8,
            y: 4,
            w: 20,
            h: 10,
        };
        assert_eq!(
            s.changed(part, &crop(&px, r, part)),
            vec![Rect {
                x: 10,
                y: 5,
                w: 11,
                h: 5
            }]
        );
    }

    #[test]
    fn resize_matches_the_client() {
        let mut s = Shadow::new(2, 2);
        s.px = (0..16).collect();
        s.resize(3, 1);
        assert_eq!(&s.px[..8], &[0, 1, 2, 3, 4, 5, 6, 7]);
        assert_eq!(&s.px[8..], &[0x80; 4]);
    }
}
