//! Project names: the slug a session's working directory encodes to,
//! and its display form.

use std::path::Path;

/// The project name of the directory `path`: the path with every
/// character that is not an ASCII letter or digit, the path separator
/// included, replaced by `-`. This is the directory name Claude Code
/// files a project's sessions under, and every driver names projects
/// with it so one directory is one project across drivers. The
/// encoding is lossy: two directories can share a name.
pub fn project_slug(path: &Path) -> String {
    let s = path.to_string_lossy();
    let mut encoded = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            encoded.push(c);
        } else {
            encoded.push('-');
        }
    }
    encoded
}

/// The display form of a project slug, shortened to at most
/// `max_chars` characters.
pub fn format_project_slug(name: &str, max_chars: usize) -> String {
    shorten_project_name(strip_home_prefix(name), max_chars)
}

/// The project name without a leading `-home-<user>-` or
/// `-Users-<user>-`, the encoded home directory in front of nearly
/// every project. The match is on the name's shape,
/// so names written on another machine shorten the same way.
fn strip_home_prefix(name: &str) -> &str {
    ["-home-", "-Users-"]
        .iter()
        .find_map(|prefix| {
            let rest = name.strip_prefix(prefix)?;
            let (user, project) = rest.split_once('-')?;
            (!user.is_empty() && !project.is_empty()).then_some(project)
        })
        .unwrap_or(name)
}

/// The name cut to `max_chars` characters. A long name keeps a head
/// of one quarter of the budget, then `…`, then as many of its final
/// characters as fill the rest. The tail carries the project's own
/// directory name, so it gets the larger share.
fn shorten_project_name(name: &str, max_chars: usize) -> String {
    const ELLIPSIS: char = '…';
    let count = name.chars().count();
    if count <= max_chars {
        return name.to_string();
    }
    if max_chars == 0 {
        return String::new();
    }
    let head_len = max_chars / 4;
    let tail_len = max_chars - head_len - 1;
    let head: String = name.chars().take(head_len).collect();
    let tail: String = name.chars().skip(count - tail_len).collect();
    format!("{head}{ELLIPSIS}{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_slug_replaces_every_character_that_is_not_alphanumeric() {
        assert_eq!(
            project_slug(Path::new("/home/u/my.proj_1")),
            "-home-u-my-proj-1"
        );
    }
}
