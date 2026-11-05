//! Screen scaling: programs on the private display draw at the client
//! screen's DPI, so text is as big as in native Windows programs.
//!
//! X11 has no one switch for this. Xvfb gets `-dpi`; `Xft.dpi` in the root
//! window's resources reaches GTK, Qt, Electron and anything using Xft for
//! fonts; GTK also gets whole-number widget scaling from 200 %; xterm is
//! pointed at a scalable font, since its default bitmap font cannot grow.

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{AtomEnum, PropMode};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

const BASE_DPI: u32 = 96;

/// The resources set on the root window (what `xrdb` would load).
pub fn resources(dpi: u16) -> String {
    let dpi = u32::from(dpi);
    let cursor = 24 * dpi / BASE_DPI;
    format!(
        "Xft.dpi:\t{dpi}\n\
         Xft.antialias:\t1\n\
         Xft.hinting:\t1\n\
         Xft.hintstyle:\thintslight\n\
         Xft.rgba:\trgb\n\
         Xcursor.size:\t{cursor}\n\
         XTerm*faceName:\tmonospace\n\
         XTerm*faceSize:\t10\n"
    )
}

/// Environment for the program.
pub fn environment(dpi: u16) -> Vec<(&'static str, String)> {
    let whole = (u32::from(dpi) / BASE_DPI).max(1);
    let mut env = Vec::new();
    if whole >= 2 {
        // GTK scales widgets by GDK_SCALE and fonts by Xft.dpi as well;
        // GDK_DPI_SCALE undoes the double scaling of text.
        env.push(("GDK_SCALE", whole.to_string()));
        env.push(("GDK_DPI_SCALE", format!("{:.4}", 1.0 / f64::from(whole))));
    }
    env.push(("XCURSOR_SIZE", (24 * u32::from(dpi) / BASE_DPI).to_string()));
    env
}

/// Loads [`resources`] into the display, before any program starts.
pub fn apply_resources(conn: &RustConnection, screen: usize, dpi: u16) -> Result<(), String> {
    let root = conn
        .setup()
        .roots
        .get(screen)
        .ok_or("the X server has no screen 0")?
        .root;
    conn.change_property8(
        PropMode::REPLACE,
        root,
        AtomEnum::RESOURCE_MANAGER,
        AtomEnum::STRING,
        resources(dpi).as_bytes(),
    )
    .map_err(|e| e.to_string())?;
    conn.sync().map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

    use super::*;

    #[test]
    fn hundred_percent_changes_nothing_in_gtk() {
        let env = environment(96);
        assert!(env.iter().all(|(k, _)| *k != "GDK_SCALE"));
        assert!(resources(96).contains("Xft.dpi:\t96\n"));
    }

    #[test]
    fn two_hundred_percent() {
        let env = environment(192);
        assert!(env.contains(&("GDK_SCALE", "2".into())));
        assert!(env.contains(&("GDK_DPI_SCALE", "0.5000".into())));
        assert!(resources(192).contains("Xcursor.size:\t48\n"));
    }

    #[test]
    fn fractional_scaling_uses_fonts_only() {
        assert!(environment(144).iter().all(|(k, _)| *k != "GDK_SCALE"));
        assert!(resources(144).contains("Xft.dpi:\t144\n"));
    }
}
