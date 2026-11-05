pub use gage_core::datetime::format_elapsed_ms;
pub use gage_core::human::format_size;

pub fn format_duration(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    let s = secs % 60;
    let m = (secs / 60) % 60;
    let h = (secs / 3600) % 24;
    let days = secs / 86400;
    if days > 0 {
        format!("{days:02}:{h:02}:{m:02}:{s:02}")
    } else {
        format!("{h:02}:{m:02}:{s:02}")
    }
}

/// `n` and `noun`, pluralized with an `s` when `n` is not one:
/// `1 note`, `3 notes`
pub fn count(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("{n} {noun}")
    } else {
        format!("{n} {noun}s")
    }
}
