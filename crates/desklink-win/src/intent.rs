//! When did a person last press a key or click on this machine?
//!
//! Pasting is always preceded by a deliberate action (Ctrl+V, Shift+Insert,
//! a click on a menu or button). An app reading the clipboard on its own is
//! not. File transfers for a paste are only started shortly after such
//! input, so a clipboard watcher can't trigger transfers.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant};

/// Milliseconds since `epoch()` of the last key press or click, + 1 (0 = never).
static LAST: AtomicU64 = AtomicU64::new(0);

fn epoch() -> Instant {
    static E: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    *E.get_or_init(Instant::now)
}

/// Record a key press or mouse click, physical (from the hook) or remote
/// (injected for the machine controlling us).
pub fn note() {
    LAST.store(epoch().elapsed().as_millis() as u64 + 1, Relaxed);
}

/// Was there a key press or click within `window`?
pub fn recent(window: Duration) -> bool {
    match LAST.load(Relaxed) {
        0 => false,
        t => epoch().elapsed().as_millis() as u64 + 1 - t <= window.as_millis() as u64,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_and_expires() {
        note();
        assert!(recent(Duration::from_secs(3)));
        std::thread::sleep(Duration::from_millis(30));
        assert!(!recent(Duration::from_millis(10)));
    }
}
