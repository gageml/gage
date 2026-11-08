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

/// `path` cut to `max_chars` characters for a column of that width.
/// Whole `/`-separated components are dropped from the middle and
/// replaced by one `…` component, so the leading component and the
/// trailing components survive: `~/.gage/tmp/publish/abc` at 14
/// characters is `~/…/abc`. When the leading component, the `…`
/// component, and the last component together do not fit, the
/// characters are cut instead: the first quarter of the budget, an
/// ellipsis, and the rest from the end of the path.
pub fn shorten_path_middle(path: &str, max_chars: usize) -> String {
    const ELLIPSIS: &str = "…";
    if path.chars().count() <= max_chars {
        return path.to_string();
    }
    if let Some((head, rest)) = path.split_once('/') {
        // Longest tail first: the first tail that fits wins
        for tail in rest.match_indices('/').map(|(i, _)| &rest[i + 1..]) {
            let cut = format!("{head}/{ELLIPSIS}/{tail}");
            if cut.chars().count() <= max_chars {
                return cut;
            }
        }
    }
    shorten_chars_middle(path, max_chars)
}

fn shorten_chars_middle(text: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let count = text.chars().count();
    let head_len = max_chars / 4;
    let tail_len = max_chars - head_len - 1;
    let head: String = text.chars().take(head_len).collect();
    let tail: String = text.chars().skip(count - tail_len).collect();
    format!("{head}…{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shorten(path: &str, home: &str) -> String {
        shorten_home_in(Path::new(path), Path::new(home))
    }

    #[test]
    fn middle_cut_drops_whole_components_first() {
        let path = "~/.gage/tmp/publish/7kb6480ceg84xby1chvv1g06hd";
        assert_eq!(shorten_path_middle(path, 60), path);
        assert_eq!(shorten_path_middle(path, 46), path);
        assert_eq!(
            shorten_path_middle(path, 45),
            "~/…/tmp/publish/7kb6480ceg84xby1chvv1g06hd"
        );
        assert_eq!(
            shorten_path_middle(path, 40),
            "~/…/publish/7kb6480ceg84xby1chvv1g06hd"
        );
        assert_eq!(
            shorten_path_middle(path, 30),
            "~/…/7kb6480ceg84xby1chvv1g06hd"
        );
        assert_eq!(shorten_path_middle("/opt/data/x", 10), "/…/data/x");
    }

    #[test]
    fn middle_cut_falls_back_to_characters() {
        let path = "~/.gage/tmp/publish/7kb6480ceg84xby1chvv1g06hd";
        assert_eq!(
            shorten_path_middle(path, 29),
            "~/.gage…80ceg84xby1chvv1g06hd"
        );
        assert_eq!(shorten_path_middle(path, 12), "~/.…vv1g06hd");
        assert_eq!(shorten_path_middle("~/averylongname", 8), "~/…gname");
        assert_eq!(shorten_path_middle("abcdef", 3), "…ef");
        assert_eq!(shorten_path_middle("abcdef", 1), "…");
        assert_eq!(shorten_path_middle("abcdef", 0), "");
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
