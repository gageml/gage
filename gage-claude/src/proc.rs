use std::io;
use std::path::{Path, PathBuf};

/// Find the `claude` binary on PATH.
pub fn find_claude() -> io::Result<PathBuf> {
    which("claude")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "claude binary not found on PATH"))
}

/// Simple PATH lookup (avoids adding a dependency).
fn which(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var("PATH").ok()?;
    for dir in path_var.split(':') {
        let candidate = Path::new(dir).join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}
