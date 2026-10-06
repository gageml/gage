//! `--target` resolution shared by the commands that take one: an
//! object id prefix becomes the `<type>:<id>` Gage URL the store
//! expects, with the type read from the matched object.

use gage_store::{SESSION_TYPE, Store};

/// Resolve a `--target` value, an object id prefix with an optional
/// `#<line selection>`, to a Gage URL with the full id. The prefix
/// resolves as any other, preferring recent objects; with a line
/// selection only sessions are candidates. The line selection itself
/// is checked by the store.
pub(crate) fn resolve_target(store: &Store, input: &str) -> Result<String, String> {
    let (prefix, fragment) = match input.split_once('#') {
        Some((p, f)) => (p, Some(f)),
        None => (input, None),
    };
    if prefix.is_empty() {
        return Err(format!("target {input:?}: missing object ID"));
    }
    let scope = fragment.map(|_| SESSION_TYPE);
    let found = store
        .resolve_in(prefix, scope)
        .map_err(|e| format!("target {prefix}: {e}"))?;
    if found.deleted {
        return Err(format!("target {prefix}: object is deleted: {}", found.id));
    }
    if fragment.is_some() && found.object_type != SESSION_TYPE {
        return Err(format!(
            "target {prefix}: a line selection applies to a session, not a {}",
            type_name(&found.object_type)
        ));
    }
    let type_name = type_name(&found.object_type);
    Ok(match fragment {
        Some(f) => format!("{type_name}:{}#{f}", found.id),
        None => format!("{type_name}:{}", found.id),
    })
}

/// `gage::note` displays as `note`
fn type_name(object_type: &str) -> &str {
    object_type.strip_prefix("gage::").unwrap_or(object_type)
}
