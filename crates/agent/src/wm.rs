//! The little a window manager must tell programs, and read from them:
//! window types, size hints, icons and window states (EWMH and ICCCM).
//! The parsing here is pure, so it is tested without an X server.

use ferostream_proto::WindowKind;
use ferostream_proto::image::{self, Image, MAX_IMAGE_SIDE};

/// Atoms named in this module, interned once.
#[derive(Debug, Clone, Copy)]
pub struct Atoms {
    pub net_supported: u32,
    pub net_supporting_wm_check: u32,
    pub net_active_window: u32,
    pub net_close_window: u32,
    pub net_wm_icon: u32,
    pub net_wm_state: u32,
    pub net_wm_state_maximized_vert: u32,
    pub net_wm_state_maximized_horz: u32,
    pub net_wm_state_fullscreen: u32,
    pub net_wm_state_hidden: u32,
    pub net_wm_window_type: u32,
    pub type_normal: u32,
    pub type_dialog: u32,
    pub type_utility: u32,
    pub type_toolbar: u32,
    pub type_splash: u32,
    pub type_menu: u32,
    pub type_dropdown_menu: u32,
    pub type_popup_menu: u32,
    pub type_tooltip: u32,
    pub type_notification: u32,
    pub type_combo: u32,
    pub type_dnd: u32,
    pub wm_state: u32,
    pub wm_change_state: u32,
}

/// Names for [`Atoms`], in field order.
pub const ATOM_NAMES: [&[u8]; 25] = [
    b"_NET_SUPPORTED",
    b"_NET_SUPPORTING_WM_CHECK",
    b"_NET_ACTIVE_WINDOW",
    b"_NET_CLOSE_WINDOW",
    b"_NET_WM_ICON",
    b"_NET_WM_STATE",
    b"_NET_WM_STATE_MAXIMIZED_VERT",
    b"_NET_WM_STATE_MAXIMIZED_HORZ",
    b"_NET_WM_STATE_FULLSCREEN",
    b"_NET_WM_STATE_HIDDEN",
    b"_NET_WM_WINDOW_TYPE",
    b"_NET_WM_WINDOW_TYPE_NORMAL",
    b"_NET_WM_WINDOW_TYPE_DIALOG",
    b"_NET_WM_WINDOW_TYPE_UTILITY",
    b"_NET_WM_WINDOW_TYPE_TOOLBAR",
    b"_NET_WM_WINDOW_TYPE_SPLASH",
    b"_NET_WM_WINDOW_TYPE_MENU",
    b"_NET_WM_WINDOW_TYPE_DROPDOWN_MENU",
    b"_NET_WM_WINDOW_TYPE_POPUP_MENU",
    b"_NET_WM_WINDOW_TYPE_TOOLTIP",
    b"_NET_WM_WINDOW_TYPE_NOTIFICATION",
    b"_NET_WM_WINDOW_TYPE_COMBO",
    b"_NET_WM_WINDOW_TYPE_DND",
    b"WM_STATE",
    b"WM_CHANGE_STATE",
];

impl Atoms {
    pub fn from_list(a: &[u32; 25]) -> Atoms {
        let [
            net_supported,
            net_supporting_wm_check,
            net_active_window,
            net_close_window,
            net_wm_icon,
            net_wm_state,
            net_wm_state_maximized_vert,
            net_wm_state_maximized_horz,
            net_wm_state_fullscreen,
            net_wm_state_hidden,
            net_wm_window_type,
            type_normal,
            type_dialog,
            type_utility,
            type_toolbar,
            type_splash,
            type_menu,
            type_dropdown_menu,
            type_popup_menu,
            type_tooltip,
            type_notification,
            type_combo,
            type_dnd,
            wm_state,
            wm_change_state,
        ] = *a;
        Atoms {
            net_supported,
            net_supporting_wm_check,
            net_active_window,
            net_close_window,
            net_wm_icon,
            net_wm_state,
            net_wm_state_maximized_vert,
            net_wm_state_maximized_horz,
            net_wm_state_fullscreen,
            net_wm_state_hidden,
            net_wm_window_type,
            type_normal,
            type_dialog,
            type_utility,
            type_toolbar,
            type_splash,
            type_menu,
            type_dropdown_menu,
            type_popup_menu,
            type_tooltip,
            type_notification,
            type_combo,
            type_dnd,
            wm_state,
            wm_change_state,
        }
    }

    /// What `_NET_SUPPORTED` lists: what programs may ask of us.
    pub fn supported(&self) -> Vec<u32> {
        vec![
            self.net_supported,
            self.net_supporting_wm_check,
            self.net_active_window,
            self.net_close_window,
            self.net_wm_icon,
            self.net_wm_state,
            self.net_wm_state_maximized_vert,
            self.net_wm_state_maximized_horz,
            self.net_wm_state_fullscreen,
            self.net_wm_state_hidden,
            self.net_wm_window_type,
            self.type_normal,
            self.type_dialog,
            self.type_utility,
            self.type_toolbar,
            self.type_splash,
            self.type_menu,
            self.type_dropdown_menu,
            self.type_popup_menu,
            self.type_tooltip,
            self.type_notification,
            self.type_combo,
            self.type_dnd,
        ]
    }

    /// The kind of window from its `_NET_WM_WINDOW_TYPE` list (first known
    /// type wins, as EWMH says) and whether it belongs to another window.
    pub fn kind(&self, types: &[u32], transient: bool) -> WindowKind {
        for &t in types {
            if t == self.type_normal {
                return WindowKind::Normal;
            }
            if t == self.type_dialog {
                return WindowKind::Dialog;
            }
            if t == self.type_utility || t == self.type_toolbar {
                return WindowKind::Utility;
            }
            if t == self.type_splash {
                return WindowKind::Splash;
            }
            if [
                self.type_menu,
                self.type_dropdown_menu,
                self.type_popup_menu,
                self.type_tooltip,
                self.type_notification,
                self.type_combo,
                self.type_dnd,
            ]
            .contains(&t)
            {
                return WindowKind::Popup;
            }
        }
        // ICCCM: a window that names an owner and no type is a dialog.
        if transient {
            WindowKind::Dialog
        } else {
            WindowKind::Normal
        }
    }

    /// The atoms `_NET_WM_STATE` holds for a state.
    pub fn state_atoms(&self, state: ferostream_proto::WindowState) -> Vec<u32> {
        use ferostream_proto::WindowState as S;
        match state {
            S::Normal => vec![],
            S::Minimized => vec![self.net_wm_state_hidden],
            S::Maximized => vec![
                self.net_wm_state_maximized_vert,
                self.net_wm_state_maximized_horz,
            ],
            S::Fullscreen => vec![self.net_wm_state_fullscreen],
        }
    }

    /// The state a `_NET_WM_STATE` request (action 0 remove, 1 add,
    /// 2 toggle, with up to two state atoms) leaves a window in.
    pub fn apply_state_request(
        &self,
        current: ferostream_proto::WindowState,
        action: u32,
        a: u32,
        b: u32,
    ) -> ferostream_proto::WindowState {
        use ferostream_proto::WindowState as S;
        let max = [
            self.net_wm_state_maximized_vert,
            self.net_wm_state_maximized_horz,
        ];
        let (target, is_on) =
            if a == self.net_wm_state_fullscreen || b == self.net_wm_state_fullscreen {
                (S::Fullscreen, current == S::Fullscreen)
            } else if max.contains(&a) || max.contains(&b) {
                (S::Maximized, current == S::Maximized)
            } else if a == self.net_wm_state_hidden || b == self.net_wm_state_hidden {
                (S::Minimized, current == S::Minimized)
            } else {
                return current;
            };
        let on = match action {
            0 => false,
            1 => true,
            2 => !is_on,
            _ => return current,
        };
        if on {
            target
        } else if is_on {
            S::Normal
        } else {
            current
        }
    }
}

/// Minimum and maximum size from `WM_NORMAL_HINTS` (18 CARDINALs: flags,
/// then x, y, width, height, min w/h, max w/h, ...); 0 is no limit.
pub fn size_limits(hints: &[u32]) -> (u16, u16, u16, u16) {
    const P_MIN_SIZE: u32 = 16;
    const P_MAX_SIZE: u32 = 32;
    let get = |i: usize| -> u16 {
        hints.get(i).map_or(0, |&v| {
            v.min(u32::from(ferostream_proto::MAX_WINDOW_SIDE)) as u16
        })
    };
    let flags = hints.first().copied().unwrap_or(0);
    let (min_w, min_h) = if flags & P_MIN_SIZE != 0 {
        (get(5), get(6))
    } else {
        (0, 0)
    };
    let (max_w, max_h) = if flags & P_MAX_SIZE != 0 {
        (get(7), get(8))
    } else {
        (0, 0)
    };
    // A maximum below the minimum is nonsense; drop it.
    let max_w = if max_w != 0 && max_w < min_w {
        0
    } else {
        max_w
    };
    let max_h = if max_h != 0 && max_h < min_h {
        0
    } else {
        max_h
    };
    (min_w, min_h, max_w, max_h)
}

/// The icons in a `_NET_WM_ICON` value (width, height, then that many
/// ARGB pixels, repeated): `(width, height, pixels)` of each that fits.
pub fn icons(value: &[u32]) -> Vec<(u16, u16, &[u32])> {
    let mut out = Vec::new();
    let mut rest = value;
    while let [w, h, tail @ ..] = rest {
        let n = (*w as usize).saturating_mul(*h as usize);
        let Some(pixels) = tail.get(..n) else { break };
        rest = tail.get(n..).unwrap_or_default();
        if (1..=u32::from(MAX_IMAGE_SIDE)).contains(w)
            && (1..=u32::from(MAX_IMAGE_SIDE)).contains(h)
        {
            out.push((*w as u16, *h as u16, pixels));
        }
    }
    out
}

/// The icon to show at `side` pixels: the smallest at least that big, or
/// else the biggest there is.
pub fn pick_icon(value: &[u32], side: u16) -> Option<Image> {
    let all = icons(value);
    let best = all
        .iter()
        .filter(|(w, h, _)| (*w).min(*h) >= side)
        .min_by_key(|(w, h, _)| u32::from(*w) * u32::from(*h))
        .or_else(|| {
            all.iter()
                .max_by_key(|(w, h, _)| u32::from(*w) * u32::from(*h))
        })?;
    let (w, h, px) = *best;
    Some(Image::new(w, h, &image::from_argb(px)))
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
    use ferostream_proto::WindowState as S;

    fn atoms() -> Atoms {
        let mut list = [0u32; 25];
        for (i, a) in list.iter_mut().enumerate() {
            *a = 100 + i as u32;
        }
        Atoms::from_list(&list)
    }

    #[test]
    fn kinds() {
        let a = atoms();
        assert_eq!(a.kind(&[], false), WindowKind::Normal);
        assert_eq!(a.kind(&[], true), WindowKind::Dialog);
        assert_eq!(a.kind(&[a.type_dialog], false), WindowKind::Dialog);
        assert_eq!(a.kind(&[999, a.type_toolbar], false), WindowKind::Utility);
        assert_eq!(a.kind(&[a.type_splash], false), WindowKind::Splash);
        assert_eq!(a.kind(&[a.type_tooltip], true), WindowKind::Popup);
        assert_eq!(
            a.kind(&[a.type_normal, a.type_dialog], true),
            WindowKind::Normal
        );
    }

    #[test]
    fn state_requests() {
        let a = atoms();
        let (v, h, f) = (
            a.net_wm_state_maximized_vert,
            a.net_wm_state_maximized_horz,
            a.net_wm_state_fullscreen,
        );
        assert_eq!(a.apply_state_request(S::Normal, 1, v, h), S::Maximized);
        assert_eq!(a.apply_state_request(S::Maximized, 0, v, h), S::Normal);
        assert_eq!(a.apply_state_request(S::Maximized, 2, v, h), S::Normal);
        assert_eq!(a.apply_state_request(S::Normal, 2, f, 0), S::Fullscreen);
        assert_eq!(a.apply_state_request(S::Fullscreen, 2, f, 0), S::Normal);
        // Removing fullscreen from a maximised window leaves it maximised.
        assert_eq!(a.apply_state_request(S::Maximized, 0, f, 0), S::Maximized);
        assert_eq!(a.apply_state_request(S::Normal, 1, 12345, 0), S::Normal);
    }

    #[test]
    fn limits() {
        let mut hints = [0u32; 18];
        assert_eq!(size_limits(&hints), (0, 0, 0, 0));
        hints[0] = 16 | 32;
        hints[5] = 200;
        hints[6] = 100;
        hints[7] = 400;
        hints[8] = 50; // below the minimum: dropped
        assert_eq!(size_limits(&hints), (200, 100, 400, 0));
        assert_eq!(size_limits(&[16]), (0, 0, 0, 0));
    }

    #[test]
    fn icon_choice() {
        let mut v = vec![16, 16];
        v.extend(std::iter::repeat_n(0xff00_00ffu32, 256));
        v.extend([48, 48]);
        v.extend(std::iter::repeat_n(0xff00_ff00u32, 48 * 48));
        v.extend([1000, 1000]); // too big, and cut short
        assert_eq!(icons(&v).len(), 2);
        let small = pick_icon(&v, 16).unwrap();
        assert_eq!((small.width, small.height), (16, 16));
        let big = pick_icon(&v, 32).unwrap();
        assert_eq!((big.width, big.height), (48, 48));
        assert_eq!(&big.decode().unwrap()[..4], &[0, 0xff, 0, 0xff]);
        let huge = pick_icon(&v, 64).unwrap();
        assert_eq!(huge.width, 48);
        assert!(pick_icon(&[5], 16).is_none());
    }
}
