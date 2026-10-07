//! The storage form of a key a scanner gives: watermark keys, note
//! carry-forward keys, issue keys, and attachment keys all take it.

use gage_runtime::error::Error;
use gage_runtime::validate::key_string;
use rune::runtime::Value;

/// A key in storage form: a string as given, or a tuple of strings
/// and integers colon-joined. The result is a path component and a
/// table value, so it must be non-empty and hold no `/`.
pub(crate) fn encode_key(key: &Value) -> Result<String, Error> {
    let key = match key.borrow_string_ref() {
        Ok(s) => s.to_string(),
        Err(_not_a_string) => key_string(key)?,
    };
    if key.is_empty() || key == "." || key == ".." || key.contains('/') {
        return Err(Error::Args(format!(
            "key must be non-empty and must not contain '/': {key:?}"
        )));
    }
    Ok(key)
}
