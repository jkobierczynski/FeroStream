//! Coming back after the connection drops: the program keeps running on
//! the host, the windows stay on screen, and the client tries again, ever
//! less often, until it is back or the user closes the windows.

use std::time::Duration;

/// How long to wait before attempt `n` (0 = the first after the drop).
pub fn delay(n: u32) -> Duration {
    const STEPS: [u64; 6] = [1, 2, 4, 8, 15, 30];
    let i = (n as usize).min(STEPS.len() - 1);
    Duration::from_secs(STEPS.get(i).copied().unwrap_or(30))
}

/// A window's title while the connection is down.
pub fn title_while_away(title: &str) -> String {
    if title.is_empty() {
        "(reconnecting…)".into()
    } else {
        format!("{title} (reconnecting…)")
    }
}

/// The windows the client still shows that the session no longer has.
pub fn stale(shown: impl IntoIterator<Item = u32>, session: &[u32]) -> Vec<u32> {
    let mut out: Vec<u32> = shown.into_iter().filter(|w| !session.contains(w)).collect();
    out.sort_unstable();
    out
}

/// What to tell the user when they close the last window while the program
/// still runs on the host.
pub fn left_running(destination: &str, id: &str) -> String {
    format!(
        "The program is still running on {destination}.\n\n\
         To get its windows back:\n\
         ferostream --attach={id} {destination}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delays_grow_and_stop_growing() {
        let d: Vec<u64> = (0..8).map(|n| delay(n).as_secs()).collect();
        assert_eq!(d, [1, 2, 4, 8, 15, 30, 30, 30]);
        assert_eq!(delay(u32::MAX).as_secs(), 30);
    }

    #[test]
    fn titles_and_stale_windows() {
        assert_eq!(title_while_away("xterm"), "xterm (reconnecting…)");
        assert_eq!(title_while_away(""), "(reconnecting…)");
        assert_eq!(stale([5, 3, 9], &[3, 7]), vec![5, 9]);
        assert!(stale([], &[1]).is_empty());
        assert!(left_running("me@box", "0123456789ab").contains("--attach=0123456789ab me@box"));
    }
}
