//! Filesystem paths as people see them.

use std::env;
use std::path::Path;

/// `path` with a leading `$HOME` replaced by `~`, for display. The
/// path is returned as given when `HOME` is unset or is not a prefix.
pub fn shorten_home(path: &Path) -> String {
    match env::var_os("HOME") {
        Some(home) => shorten_home_in(path, Path::new(&home)),
        None => path.to_string_lossy().into_owned(),
    }
}

/// `path` with a leading `home` replaced by `~`. The prefix matches
/// on a component boundary, so `/home/ux` is not under `/home/u`.
pub fn shorten_home_in(path: &Path, home: &Path) -> String {
    if home.components().next().is_none() {
        return path.to_string_lossy().into_owned();
    }
    match path.strip_prefix(home) {
        Ok(rel) if rel.as_os_str().is_empty() => "~".to_string(),
        Ok(rel) => format!("~/{}", rel.display()),
        Err(_not_under_home) => path.to_string_lossy().into_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shorten(path: &str, home: &str) -> String {
        shorten_home_in(Path::new(path), Path::new(home))
    }

    #[test]
    fn substitutes_tilde_on_a_component_boundary() {
        assert_eq!(shorten("/home/me/proj", "/home/me"), "~/proj");
        assert_eq!(shorten("/home/me", "/home/me"), "~");
        assert_eq!(shorten("/home/me/", "/home/me"), "~");
        assert_eq!(shorten("/home/me/proj", "/home/me/"), "~/proj");
        assert_eq!(shorten("/opt/proj", "/home/me"), "/opt/proj");
        assert_eq!(shorten("/home/melon/proj", "/home/me"), "/home/melon/proj");
    }

    #[test]
    fn an_empty_home_leaves_the_path_alone() {
        assert_eq!(shorten("/x/y", ""), "/x/y");
    }
}
