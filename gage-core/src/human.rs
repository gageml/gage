//! Human-readable forms of quantities shared by the CLI and the TUI.

/// `bytes` in decimal units with one decimal above a kilobyte:
/// `512 B`, `1.5 kB`, `2.0 MB`, `3.1 GB`.
pub fn format_size(bytes: i64) -> String {
    const KB: i64 = 1_000;
    const MB: i64 = 1_000_000;
    const GB: i64 = 1_000_000_000;

    if bytes < KB {
        format!("{bytes} B")
    } else if bytes < MB {
        format!("{:.1} kB", bytes as f64 / KB as f64)
    } else if bytes < GB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_size_steps_through_decimal_units() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(999), "999 B");
        assert_eq!(format_size(1_000), "1.0 kB");
        assert_eq!(format_size(1_536), "1.5 kB");
        assert_eq!(format_size(2_000_000), "2.0 MB");
        assert_eq!(format_size(3_100_000_000), "3.1 GB");
    }
}
