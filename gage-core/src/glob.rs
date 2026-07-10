//! `*`-only glob matching, used wherever user-specified name patterns
//! appear (scanner enable/disable config, task dependency `wants`).

/// Matches `name` against `pattern`, where `*` matches any run of
/// characters (including empty). All other characters match literally.
pub fn glob_match(pattern: &str, name: &str) -> bool {
    if !pattern.contains('*') {
        return pattern == name;
    }
    let parts: Vec<&str> = pattern.split('*').collect();
    let last = parts.len() - 1;
    let mut rest = name;
    for (i, part) in parts.iter().enumerate() {
        if i == 0 {
            match rest.strip_prefix(part) {
                Some(r) => rest = r,
                None => return false,
            }
        } else if i == last {
            return rest.ends_with(part);
        } else if part.is_empty() {
            continue;
        } else {
            match rest.find(part) {
                Some(idx) => rest = rest.split_at(idx + part.len()).1,
                None => return false,
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::glob_match;

    #[test]
    fn glob_literal() {
        assert!(glob_match("foo", "foo"));
        assert!(!glob_match("foo", "foobar"));
    }

    #[test]
    fn glob_wildcards() {
        assert!(glob_match("*", "anything"));
        assert!(glob_match("foo*", "foobar"));
        assert!(glob_match("*bar", "foobar"));
        assert!(glob_match("foo*bar", "foo-mid-bar"));
        assert!(glob_match("a*b*c", "a-b-c"));
        assert!(glob_match("a*b*c", "axxbyyc"));
        assert!(!glob_match("foo*", "fo"));
        assert!(!glob_match("*bar", "barx"));
        assert!(!glob_match("a*b*c", "abx"));
    }
}
