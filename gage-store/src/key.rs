//! Keys: the identity an issue or an attachment carries from creation
//! in `attrs.key`, projected to the ref `refs/gage/1/key/<type>/<key>`
//! so that at most one live object carries it. A key is therefore a
//! ref name component sequence and follows git's ref rules, with one
//! addition: it is lowercase, so two keys cannot differ only by case
//! on a case-insensitive filesystem.

use crate::StoreError;

/// Check `key` against the ref name rules. The checks restate what
/// `git check-ref-format` enforces for a component sequence, so no
/// git process is needed, plus the lowercase rule.
pub fn validate_key(key: &str) -> Result<(), StoreError> {
    let bad = || Err(StoreError::KeyName(key.to_string()));
    if key.is_empty() || key.starts_with('-') || key.ends_with('/') || key.ends_with('.') {
        return bad();
    }
    if key.contains("..") || key.contains("@{") || key.contains("//") {
        return bad();
    }
    if key
        .chars()
        .any(|c| c.is_ascii_uppercase() || c.is_ascii_control() || " ~^:?*[\\\x7f".contains(c))
    {
        return bad();
    }
    if key
        .split('/')
        .any(|part| part.is_empty() || part.starts_with('.') || part.ends_with(".lock"))
    {
        return bad();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_ref_safe_lowercase_keys() {
        for key in [
            "session-retention",
            "claude-config/5f3a9c1e2b7d4406",
            "a/b.c_d",
        ] {
            assert!(validate_key(key).is_ok(), "{key}");
        }
    }

    #[test]
    fn rejects_keys_git_or_the_lowercase_rule_refuse() {
        for key in [
            "", "-x", "x/", "x.", "a..b", "a@{1}", "a//b", "Upper", "a b", "a~b", "a^b", "a?b",
            "a*b", "a[b", "a\\b", "/x", "a/.b", "a.lock", "a.lock/b",
        ] {
            assert!(
                matches!(validate_key(key), Err(StoreError::KeyName(k)) if k == key),
                "{key}"
            );
        }
    }
}
