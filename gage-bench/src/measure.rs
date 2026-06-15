//! Timing and on-disk size measurement.

use std::path::Path;
use std::time::{Duration, Instant};

/// Sums file sizes under `dir` (one level deep). The bench writes the
/// files itself, so a missing directory or unreadable entry is a
/// programmer error — crash, don't paper over.
pub fn dir_bytes(dir: &Path) -> u64 {
    let mut total = 0;
    for entry in std::fs::read_dir(dir).unwrap() {
        total += entry.unwrap().metadata().unwrap().len();
    }
    total
}

pub fn time<R, F: FnOnce() -> R>(f: F) -> (R, Duration) {
    let t = Instant::now();
    let r = f();
    (r, t.elapsed())
}
