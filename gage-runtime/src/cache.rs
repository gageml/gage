//! Session-scan memoization: `split_valid` / `carry_forward_notes`.
//!
//! Partitions a scan's sessions by a cached size validator so scanners
//! can skip sessions already processed under the same key, adopt the
//! notes a prior scan produced, and record validation state when a
//! session's work completes. See docs in `.local.design/session-caching.md`.

use std::collections::HashMap;

use gage_db::cache;
use gage_db::note;
use gage_db::scan::insert_scan_note;
use rune::runtime::{Function, Protocol, Ref, Value, Vec as RuneVec};
use rune::{Any, ContextError, Module};

use crate::error::Error;
use crate::scan::{Session, Sessions};
use crate::state::current_scan_ctx;

pub(crate) fn register(m: &mut Module) -> Result<(), ContextError> {
    m.function_meta(split_valid)?;
    m.associated_function(&Protocol::INTO_FUTURE, |q: SplitValid| async move {
        do_split_valid(q)
    })?;

    m.function("carry_forward_notes", CarryForward::new)
        .build()?;
    m.associated_function(&Protocol::INTO_FUTURE, |q: CarryForward| async move {
        do_carry_forward(q)
    })?;
    Ok(())
}

pub(crate) fn register_types(m: &mut Module) -> Result<(), ContextError> {
    m.ty::<SplitValid>()?;
    m.ty::<CarryForward>()?;
    Ok(())
}

/// Builder returned by `sessions.split_valid(key)`. The partition runs
/// when the value is awaited.
#[derive(Any)]
#[rune(item = ::gage)]
pub(crate) struct SplitValid {
    #[rune(skip)]
    sessions: Vec<Session>,
    #[rune(skip)]
    key: Value,
}

#[rune::function(instance, path = split_valid)]
fn split_valid(sessions: Ref<Sessions>, key: Value) -> SplitValid {
    SplitValid {
        sessions: sessions.remaining(),
        key,
    }
}

/// Render a scanner cache key — a tuple/vec of strings and integers —
/// as its colon-joined form, e.g. `("s", "findings", 1)` →
/// `"s:findings:1"`.
fn key_string(key: &Value) -> crate::Result<String> {
    let json = crate::value::value_to_json(key)
        .map_err(|e| Error::Args(format!("cache key could not be serialized: {e}")))?;
    let parts = match json {
        serde_json::Value::Array(items) => items,
        _ => return Err(Error::Args("cache key must be a tuple or vec".into())),
    };
    let mut out = Vec::with_capacity(parts.len());
    for p in parts {
        match p {
            serde_json::Value::String(s) => out.push(s),
            serde_json::Value::Number(n) => out.push(n.to_string()),
            other => {
                return Err(Error::Args(format!(
                    "cache key elements must be strings or integers, got {other}"
                )));
            }
        }
    }
    if out.is_empty() {
        return Err(Error::Args("cache key must not be empty".into()));
    }
    Ok(out.join(":"))
}

/// Partition sessions by cached size validator. Returns
/// `(prev, new, validate)`: `prev` are sessions whose recorded size
/// matches (cache hit), `new` need scanning, and `validate` is a
/// function of one session (or session id) that records the session's
/// size under the key — called by the scanner when that session's work
/// has completed.
fn do_split_valid(q: SplitValid) -> crate::Result<(Vec<Session>, Vec<Session>, Function)> {
    let key = key_string(&q.key)?;
    let ctx = current_scan_ctx();
    let sizes: HashMap<&str, u64> = ctx
        .run
        .selected
        .iter()
        .map(|s| (s.id.as_str(), s.size))
        .collect();

    let mut prev = Vec::new();
    let mut new = Vec::new();
    // session id -> (cache key, size at selection) for `validate`
    let mut entries: HashMap<String, (String, String)> = HashMap::new();
    {
        let db = ctx.db.lock().unwrap();
        for s in q.sessions {
            let size = *sizes
                .get(s.id.as_str())
                .ok_or_else(|| Error::Args(format!("session {} not in scan selection", s.id)))?;
            let cache_key = format!("session-valid:{key}:{}", s.id);
            let cached = cache::get(&db, &cache_key).map_err(|e| Error::Db(e.to_string()))?;
            if cached.as_deref() == Some(size.to_string().as_str()) {
                prev.push(s);
            } else {
                entries.insert(s.id.clone(), (cache_key, size.to_string()));
                new.push(s);
            }
        }
    }

    let validate = Function::new(move |session: Value| -> crate::Result<()> {
        let id = session_id(&session)?;
        let (cache_key, size) = entries
            .get(&id)
            .ok_or_else(|| Error::Args(format!("session {id} was not split as new")))?;
        let ctx = current_scan_ctx();
        let db = ctx.db.lock().unwrap();
        cache::put(&db, cache_key, size, None).map_err(|e| Error::Db(e.to_string()))
    })
    .unwrap();

    Ok((prev, new, validate))
}

/// A `Session` or a session id string.
fn session_id(v: &Value) -> crate::Result<String> {
    if let Ok(s) = rune::from_value::<Ref<Session>>(v.clone()) {
        return Ok(s.id.clone());
    }
    v.borrow_string_ref().map(|s| s.to_string()).map_err(|e| {
        Error::Args(format!(
            "expected a Session or session id string, got {}: {e}",
            v.type_info()
        ))
    })
}

/// Builder returned by `carry_forward_notes(sessions, names)`. The link
/// runs when the value is awaited.
#[derive(Any)]
#[rune(item = ::gage)]
pub(crate) struct CarryForward {
    #[rune(skip)]
    sessions: Value,
    #[rune(skip)]
    names: Value,
}

impl CarryForward {
    fn new(sessions: Value, names: Value) -> Self {
        Self { sessions, names }
    }
}

/// Link each session's prior notes matching `names` into the current
/// scan's `scan_note` rows, making them visible to downstream tasks as
/// if written by this scan. A dot-ended name selects its suffixed
/// family (see `write_note`). Returns the number of notes linked.
fn do_carry_forward(q: CarryForward) -> crate::Result<i64> {
    let items: RuneVec = rune::from_value(q.sessions)
        .map_err(|e| Error::Args(format!("'sessions' must be a list: {e}")))?;
    let mut session_ids = Vec::with_capacity(items.len());
    for item in items.iter() {
        session_ids.push(session_id(item)?);
    }

    let items: RuneVec = rune::from_value(q.names)
        .map_err(|e| Error::Args(format!("'names' must be a list: {e}")))?;
    let mut names = Vec::with_capacity(items.len());
    for item in items.iter() {
        let name = item
            .borrow_string_ref()
            .map_err(|e| Error::Args(format!("'names' entries must be strings: {e}")))?;
        names.push(name.to_string());
    }

    let ctx = current_scan_ctx();
    let db = ctx.db.lock().unwrap();
    let mut count = 0i64;
    for sid in &session_ids {
        for name in &names {
            let ids = note::ids_for_session_by_name(&db, sid, name)
                .map_err(|e| Error::Db(e.to_string()))?;
            for id in &ids {
                insert_scan_note(&db, &ctx.run.scan_id, id)
                    .map_err(|e| Error::Db(e.to_string()))?;
            }
            count += i64::try_from(ids.len()).unwrap();
        }
    }
    Ok(count)
}
