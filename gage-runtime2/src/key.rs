//! The storage form of the keys a scanner gives. A work id (watermark
//! and carry-forward) is a path component and a table value, colon
//! joined. A ref key (issue and attachment) is projected to a ref under
//! `refs/gage/1/key/<type>/`, so a tuple is `/`-joined and the store
//! applies git's ref rules plus the lowercase rule at the write.

use gage_runtime::error::Error;
use gage_runtime::value::value_to_json;
use rune::runtime::Value;

/// A work id in storage form: a string as given, or a tuple of strings
/// and integers colon-joined. Non-empty and no `/`.
pub(crate) fn encode_key(key: &Value) -> Result<String, Error> {
    let key = match key.borrow_string_ref() {
        Ok(s) => s.to_string(),
        Err(_not_a_string) => parts(key)?.join(":"),
    };
    if key.is_empty() || key == "." || key == ".." || key.contains('/') {
        return Err(Error::Args(format!(
            "key must be non-empty and must not contain '/': {key:?}"
        )));
    }
    Ok(key)
}

/// A ref key in storage form: a string as given, or a tuple of strings
/// and integers `/`-joined. Non-empty and lowercase here; the store
/// applies the full ref rules when it writes the object.
pub(crate) fn encode_ref_key(key: &Value) -> Result<String, Error> {
    let key = match key.borrow_string_ref() {
        Ok(s) => s.to_string(),
        Err(_not_a_string) => parts(key)?.join("/"),
    };
    if key.is_empty() {
        return Err(Error::Args("key must be non-empty".into()));
    }
    if key.chars().any(|c| c.is_ascii_uppercase()) {
        return Err(Error::Args(format!("key must be lowercase: {key:?}")));
    }
    Ok(key)
}

/// The elements of a tuple key as strings.
fn parts(key: &Value) -> Result<Vec<String>, Error> {
    let json =
        value_to_json(key).map_err(|e| Error::Args(format!("key could not be serialized: {e}")))?;
    let serde_json::Value::Array(items) = json else {
        return Err(Error::Args("key must be a string or a tuple".into()));
    };
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        match item {
            serde_json::Value::String(s) => out.push(s),
            serde_json::Value::Number(n) => out.push(n.to_string()),
            other => {
                return Err(Error::Args(format!(
                    "key elements must be strings or integers, got {other}"
                )));
            }
        }
    }
    if out.is_empty() {
        return Err(Error::Args("key must not be empty".into()));
    }
    Ok(out)
}
