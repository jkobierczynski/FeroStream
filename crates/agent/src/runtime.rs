//! Where sessions live while they run, so a client can come back to one.
//!
//! ```text
//! /tmp/ferostream-<uid>/          0700, this user's only
//!     <id>/                       one per session (12 hex digits)
//!         sock                    the session process listens here
//!         pid                     its process id
//!         program                 what it runs (for --list)
//!         detached                present while no client is attached
//!         log                     its own and the program's error output
//!         Xauthority              the display's cookie
//! ```
//!
//! `/tmp`, not `$XDG_RUNTIME_DIR`: the latter is removed when the user's
//! last login ends, which is exactly when a session must live on (tmux and
//! screen use `/tmp` for the same reason). `FEROSTREAM_RUNTIME_DIR` replaces
//! the base directory (for tests).

use std::fs;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use ferostream_proto::{SESSION_ID_LEN, is_session_id};

/// The base directory, made when missing and refused when it is not this
/// user's alone (someone else could otherwise reach the sessions).
pub fn base() -> Result<PathBuf, String> {
    let uid = fs::metadata("/proc/self")
        .map_err(|e| format!("cannot read /proc/self: {e}"))?
        .uid();
    let dir = match std::env::var_os("FEROSTREAM_RUNTIME_DIR") {
        Some(d) => PathBuf::from(d),
        None => std::env::temp_dir().join(format!("ferostream-{uid}")),
    };
    private_dir(&dir, uid)?;
    Ok(dir)
}

/// Makes `dir` (mode 0700) or checks the one there: a real directory, owned
/// by `uid`, that nobody else can enter.
fn private_dir(dir: &Path, uid: u32) -> Result<(), String> {
    match fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(format!("could not make {}: {e}", dir.display())),
    }
    let m = fs::symlink_metadata(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    if !m.is_dir() || m.uid() != uid || m.permissions().mode() & 0o077 != 0 {
        return Err(format!(
            "{} is not a private directory of this user; remove it and try again",
            dir.display()
        ));
    }
    Ok(())
}

/// A new session id.
pub fn new_id() -> Result<String, String> {
    let mut b = [0u8; SESSION_ID_LEN / 2];
    getrandom::fill(&mut b).map_err(|e| format!("no random numbers: {e}"))?;
    Ok(b.iter().map(|b| format!("{b:02x}")).collect())
}

/// Makes a new session's directory.
pub fn create(base: &Path, id: &str) -> Result<PathBuf, String> {
    let dir = base.join(id);
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&dir)
        .map_err(|e| format!("could not make {}: {e}", dir.display()))?;
    Ok(dir)
}

/// A running session, as `--list` shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Info {
    pub id: String,
    pub dir: PathBuf,
    /// When the last client left, or `None` while one is attached.
    pub detached: Option<SystemTime>,
    pub program: String,
}

/// The sessions whose process still runs. Directories of sessions that
/// died without cleaning up (killed, the host crashed) are removed.
pub fn list(base: &Path) -> Vec<Info> {
    let Ok(entries) = fs::read_dir(base) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in entries.flatten() {
        let id = e.file_name().to_string_lossy().into_owned();
        if !is_session_id(&id) {
            continue;
        }
        let dir = e.path();
        let pid = fs::read_to_string(dir.join("pid")).ok();
        let alive = pid
            .as_deref()
            .map(str::trim)
            .and_then(|p| p.parse::<u32>().ok())
            .is_some_and(|p| Path::new("/proc").join(p.to_string()).exists());
        if !alive {
            // A session that is still starting has no pid file yet: leave
            // directories younger than a minute alone.
            let young = fs::metadata(&dir)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age.as_secs() < 60);
            if pid.is_some() || !young {
                let _ = fs::remove_dir_all(&dir);
            }
            continue;
        }
        let detached = fs::metadata(dir.join("detached"))
            .and_then(|m| m.modified())
            .ok();
        let program = fs::read_to_string(dir.join("program"))
            .unwrap_or_default()
            .trim()
            .to_string();
        out.push(Info {
            id,
            dir,
            detached,
            program,
        });
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// The session whose client left most recently.
pub fn newest_detached(sessions: &[Info]) -> Option<&Info> {
    sessions
        .iter()
        .filter_map(|s| s.detached.map(|t| (t, s)))
        .max_by_key(|(t, _)| *t)
        .map(|(_, s)| s)
}

/// Marks the session detached (now) or attached.
pub fn mark_detached(dir: &Path, detached: bool) {
    let path = dir.join("detached");
    if detached {
        let _ = fs::write(path, b"");
    } else {
        let _ = fs::remove_file(path);
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
    use std::time::Duration;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "ferostream-runtime-test-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn ids_are_session_ids() {
        let a = new_id().unwrap();
        assert!(is_session_id(&a), "{a}");
        assert_ne!(a, new_id().unwrap());
    }

    #[test]
    fn base_must_be_private() {
        let uid = fs::metadata("/proc/self").unwrap().uid();
        let d = temp("private");
        private_dir(&d, uid).unwrap();
        assert_eq!(
            fs::metadata(&d).unwrap().permissions().mode() & 0o777,
            0o700
        );
        fs::set_permissions(&d, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(private_dir(&d, uid).is_err());
        fs::remove_dir_all(&d).unwrap();
        // A symlink to a directory is refused too.
        let real = temp("real");
        fs::create_dir(&real).unwrap();
        fs::set_permissions(&real, fs::Permissions::from_mode(0o700)).unwrap();
        std::os::unix::fs::symlink(&real, &d).unwrap();
        assert!(private_dir(&d, uid).is_err());
        fs::remove_file(&d).unwrap();
        fs::remove_dir_all(&real).unwrap();
    }

    #[test]
    fn lists_live_sessions_and_removes_dead_ones() {
        let base = temp("list");
        fs::create_dir(&base).unwrap();
        // Live: our own pid; detached a moment ago.
        let live = create(&base, "aaaaaaaaaaaa").unwrap();
        fs::write(live.join("pid"), std::process::id().to_string()).unwrap();
        fs::write(live.join("program"), "xterm -T x\n").unwrap();
        mark_detached(&live, true);
        // Attached, also live.
        let busy = create(&base, "bbbbbbbbbbbb").unwrap();
        fs::write(busy.join("pid"), std::process::id().to_string()).unwrap();
        // Dead: a pid that cannot exist.
        let dead = create(&base, "cccccccccccc").unwrap();
        fs::write(dead.join("pid"), "4294967295").unwrap();
        // Starting: no pid yet, just made.
        let starting = create(&base, "dddddddddddd").unwrap();
        // Not a session at all.
        fs::create_dir(base.join("other")).unwrap();

        let l = list(&base);
        let ids: Vec<&str> = l.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["aaaaaaaaaaaa", "bbbbbbbbbbbb"]);
        assert_eq!(l[0].program, "xterm -T x");
        assert!(l[0].detached.is_some() && l[1].detached.is_none());
        assert!(!dead.exists());
        assert!(starting.exists());
        assert!(base.join("other").exists());
        assert_eq!(newest_detached(&l).unwrap().id, "aaaaaaaaaaaa");

        std::thread::sleep(Duration::from_millis(20));
        mark_detached(&busy, true);
        assert_eq!(newest_detached(&list(&base)).unwrap().id, "bbbbbbbbbbbb");
        mark_detached(&busy, false);
        mark_detached(&live, false);
        assert_eq!(newest_detached(&list(&base)), None);
        fs::remove_dir_all(&base).unwrap();
    }
}
