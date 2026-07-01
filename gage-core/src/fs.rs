use std::path::{Path, PathBuf};

/// `remove_dir_all` with a guard against catastrophic targets. Panics
/// unless `dir` sits at least one directory below `$HOME` or the
/// system temp dir. Callers are responsible for any additional
/// domain-specific checks (e.g. path suffix).
pub fn remove_checked_dir_all(dir: &Path) -> std::io::Result<()> {
    check_under_home_or_temp(dir);
    match std::fs::remove_dir_all(dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

fn check_under_home_or_temp(dir: &Path) {
    let roots = [
        std::env::var_os("HOME").map(PathBuf::from),
        Some(std::env::temp_dir()),
    ];
    let ok = roots.iter().flatten().any(|root| {
        dir.strip_prefix(root)
            .map(|rest| rest.components().count() >= 1)
            .unwrap_or(false)
    });
    assert!(ok, "{}", dir.display());
}
