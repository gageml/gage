//! Note objects: `gage::note 1`, reached through [`NoteStore`].
//!
//! Content is `attrs.json` (name, author, target URL, and the optional
//! spec fields), the value as `value.txt` (plain text) or `value.json`
//! (structured), and, when a target is given, `target.link` naming
//! the target's commit. Tree construction, commit parents, edits, and
//! tombstones are the generic object model's job; see
//! [`crate::object`].

use gage_core::uuid::new_uuid;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

use crate::index::{ObjectQuery, Order};
use crate::object::{EditOutcome, Object, ObjectTree, object_ref, require_type};
use crate::url;
use crate::{Store, StoreError};

pub const OBJECT_TYPE: &str = "gage::note";
const OBJECT_VERSION: &str = "1";
/// Attribute paths the index extracts from a note's `attrs.json`.
pub(crate) const INDEXED_ATTRS: &[&str] = &["name"];
const TEXT_VALUE_FILE: &str = "value.txt";
const JSON_VALUE_FILE: &str = "value.json";
const TARGET_LINK: &str = "target.link";
/// The one scheme whose URLs may carry a fragment
const SESSION_SCHEME: &str = "session";

/// Note operations over an opened store.
pub struct NoteStore<'a> {
    store: &'a Store,
}

impl<'a> From<&'a Store> for NoteStore<'a> {
    fn from(store: &'a Store) -> Self {
        NoteStore { store }
    }
}

/// A note's value: plain text in `value.txt` or structured data in
/// `value.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoteValue {
    Text(String),
    Json(JsonValue),
}

/// Input to [`NoteStore::create`]. Every string is stored verbatim; the
/// caller is responsible for producing `author` in the `user:` or
/// `task:` URL form.
pub struct NoteInput<'a> {
    pub name: &'a str,
    pub value: NoteValue,
    pub author: &'a str,
    /// A Gage URL under an object scheme with a full id, e.g.
    /// `session:<id>#12-20` or `note:<id>`. The object must exist and
    /// be live, its type must match the scheme, and only `session:`
    /// accepts a fragment, which must be a line selection.
    pub target: Option<&'a str>,
    /// Writer-defined payload, stored and returned verbatim. Has no
    /// Gage schema.
    pub metadata: Option<JsonValue>,
}

/// Input to [`NoteStore::edit`]. Every field is optional; `None`
/// keeps the note's current value. The author is never changed.
#[derive(Default)]
pub struct NoteEdit<'a> {
    pub name: Option<&'a str>,
    pub value: Option<NoteValue>,
    /// A new target as a full Gage URL, validated as on `create`.
    /// The existing target cannot be cleared.
    pub target: Option<&'a str>,
}

/// A single note read from the store, projected into the fields the
/// list view needs.
#[derive(Debug, PartialEq, Eq)]
pub struct NoteRecord {
    pub id: String,
    pub name: String,
    pub value: NoteValue,
    pub author: String,
    /// The target URL from `attrs.target`, with the full id.
    pub target: Option<String>,
    pub metadata: Option<JsonValue>,
    /// From the `created` blob: milliseconds since the Unix epoch.
    pub created_ms: i64,
    /// From the `modified` blob: milliseconds since the Unix epoch.
    pub modified_ms: i64,
}

/// Everything a `show` view needs about one note.
#[derive(Debug, PartialEq, Eq)]
pub struct NoteFull {
    pub id: String,
    pub name: String,
    pub value: NoteValue,
    pub author: String,
    /// The target URL from `attrs.target`, with the full id.
    pub target: Option<String>,
    pub metadata: Option<JsonValue>,
    /// Commit SHAs from the `target.link` file, in file order. Empty
    /// when the note has no `target.link` file.
    pub targets: Vec<String>,
    pub created_ms: i64,
    pub modified_ms: i64,
}

/// The `attrs.json` shape. Optional fields defined by the spec are held
/// so an edit round-trip preserves them; `target` and `metadata` are
/// written by `create`, the others by no writer today.
#[derive(Deserialize, Serialize)]
struct NoteAttrs {
    name: String,
    author: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    target: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    line: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    line_end: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    metadata: Option<JsonValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    scan: Option<String>,
}

impl NoteStore<'_> {
    /// Create a note. Returns the new note's id.
    pub fn create(&self, input: NoteInput) -> Result<String, StoreError> {
        let target_sha = match input.target {
            Some(url) => Some(self.resolve_target(url)?),
            None => None,
        };
        let attrs = NoteAttrs {
            name: input.name.to_string(),
            author: input.author.to_string(),
            target: input.target.map(String::from),
            line: None,
            line_end: None,
            metadata: input.metadata,
            scan: None,
        };
        let tree = build_tree(&attrs, &input.value, target_sha.into_iter().collect())?;
        let id = new_uuid();
        let message = format!("note: {}", input.name);
        self.store
            .create(OBJECT_TYPE, OBJECT_VERSION, &id, &tree, &message)?;
        Ok(id)
    }

    /// Validate a target URL and return the tip SHA of the object it
    /// names. The body is a full id; the object must be live and of
    /// the scheme's type; a fragment is accepted for `session:` only
    /// and must be a line selection.
    fn resolve_target(&self, raw: &str) -> Result<String, StoreError> {
        let parsed = url::parse(raw)?;
        match parsed.fragment {
            Some(fragment) if parsed.scheme == SESSION_SCHEME => {
                url::validate_line_selection(fragment)?
            }
            Some(_) => return Err(StoreError::BadTarget(raw.to_string())),
            None => {}
        }
        let sha = self
            .store
            .rev_parse(&object_ref(parsed.body))?
            .ok_or_else(|| StoreError::TargetNotFound(raw.to_string()))?;
        let target = self.store.read_object(&sha)?;
        require_type(&target, &format!("gage::{}", parsed.scheme))?;
        if target.header.is_tombstone() {
            return Err(StoreError::ObjectDeleted(target.header.id));
        }
        Ok(sha)
    }

    /// Look up one note by full id or unique prefix.
    ///
    /// Returns [`StoreError::ObjectNotFound`] when no object matches,
    /// [`StoreError::AmbiguousId`] when more than one does, and
    /// [`StoreError::WrongType`] when the match is not a note.
    pub fn get(&self, id_or_prefix: &str) -> Result<NoteFull, StoreError> {
        let object = self.store.resolve_typed(id_or_prefix, OBJECT_TYPE)?;
        decode_full(&object)
    }

    /// Every live note, newest created first, read lazily.
    pub fn iter(
        &self,
    ) -> Result<impl Iterator<Item = Result<NoteRecord, StoreError>> + '_, StoreError> {
        self.query().iter()
    }

    /// Start a selection over notes.
    pub fn query(&self) -> NoteQuery<'_> {
        NoteQuery {
            store: self.store,
            query: ObjectQuery::new(OBJECT_TYPE),
        }
    }

    /// Edit an existing note. Fields left `None` in `edit` keep their
    /// current values; a new target is validated and re-linked, and an
    /// unchanged target keeps the commit it was linked at. Returns the
    /// resolved id.
    pub fn edit(&self, id_or_prefix: &str, edit: NoteEdit) -> Result<String, StoreError> {
        let object = self.store.resolve_typed(id_or_prefix, OBJECT_TYPE)?;
        let (mut attrs, current_value) = decode_content(&object)?;
        if let Some(name) = edit.name {
            attrs.name = name.to_string();
        }
        let value = edit.value.unwrap_or(current_value);
        let targets = match edit.target {
            Some(url) => {
                let sha = self.resolve_target(url)?;
                attrs.target = Some(url.to_string());
                vec![sha]
            }
            None => object
                .tree
                .links
                .get(TARGET_LINK)
                .cloned()
                .unwrap_or_default(),
        };
        let tree = build_tree(&attrs, &value, targets)?;
        let message = format!("note edit: {}", attrs.name);
        match self.store.edit(&object, &tree, &message)? {
            EditOutcome::Unchanged | EditOutcome::Written(_) => Ok(object.header.id),
        }
    }

    /// Delete a note by writing a parentless tombstone commit. Returns
    /// the resolved id.
    pub fn delete(&self, id_or_prefix: &str) -> Result<String, StoreError> {
        let object = self.store.resolve_typed(id_or_prefix, OBJECT_TYPE)?;
        let (attrs, _) = decode_content(&object)?;
        let message = format!("note delete: {}", attrs.name);
        self.store.delete(&object, &message)?;
        Ok(object.header.id)
    }
}

/// A selection over notes: filters on the indexed attributes, an
/// order, and a limit. `iter` reads matching notes one at a time.
pub struct NoteQuery<'a> {
    store: &'a Store,
    query: ObjectQuery,
}

impl<'a> NoteQuery<'a> {
    /// Select notes whose `name` equals `name`.
    pub fn name(mut self, name: &str) -> Self {
        self.query.attrs.push(("name", name.to_string()));
        self
    }

    pub fn order(mut self, order: Order) -> Self {
        self.query.order = order;
        self
    }

    pub fn limit(mut self, limit: usize) -> Self {
        self.query.limit = Some(limit);
        self
    }

    /// The number of notes the selection matches, ignoring any limit.
    /// Served by the index; no object is read.
    pub fn count(&self) -> Result<usize, StoreError> {
        let unlimited = ObjectQuery {
            limit: None,
            ..self.query.clone()
        };
        Ok(self.store.select(&unlimited)?.len())
    }

    /// Run the selection. Matching tips are resolved by the index in
    /// one step; each note is read from the repository as the iterator
    /// advances.
    pub fn iter(
        self,
    ) -> Result<impl Iterator<Item = Result<NoteRecord, StoreError>> + 'a, StoreError> {
        let store = self.store;
        let tips = store.select(&self.query)?;
        Ok(tips.into_iter().map(move |tip| {
            let object = store.read_object(&tip.sha)?;
            let full = decode_full(&object)?;
            Ok(NoteRecord {
                id: full.id,
                name: full.name,
                value: full.value,
                author: full.author,
                target: full.target,
                metadata: full.metadata,
                created_ms: full.created_ms,
                modified_ms: full.modified_ms,
            })
        }))
    }
}

fn build_tree(
    attrs: &NoteAttrs,
    value: &NoteValue,
    target_shas: Vec<String>,
) -> Result<ObjectTree, StoreError> {
    let mut tree = ObjectTree {
        attrs: Some(
            serde_json::to_value(attrs)
                .map_err(|e| StoreError::Parse(format!("note attrs encode: {e}")))?,
        ),
        ..ObjectTree::default()
    };
    let (file, bytes) = match value {
        NoteValue::Text(text) => (TEXT_VALUE_FILE, text.as_bytes().to_vec()),
        NoteValue::Json(json) => {
            let mut bytes = serde_json::to_vec(json)
                .map_err(|e| StoreError::Parse(format!("note value encode: {e}")))?;
            bytes.push(b'\n');
            (JSON_VALUE_FILE, bytes)
        }
    };
    tree.blobs.insert(file.to_string(), bytes);
    if !target_shas.is_empty() {
        tree.links.insert(TARGET_LINK.to_string(), target_shas);
    }
    Ok(tree)
}

fn decode_full(object: &Object) -> Result<NoteFull, StoreError> {
    let (attrs, value) = decode_content(object)?;
    Ok(NoteFull {
        id: object.header.id.clone(),
        name: attrs.name,
        value,
        author: attrs.author,
        target: attrs.target,
        metadata: attrs.metadata,
        targets: object
            .tree
            .links
            .get(TARGET_LINK)
            .cloned()
            .unwrap_or_default(),
        created_ms: marker_ms(object, "created", object.header.created_ms)?,
        modified_ms: marker_ms(object, "modified", object.header.modified_ms)?,
    })
}

fn decode_content(object: &Object) -> Result<(NoteAttrs, NoteValue), StoreError> {
    let id = &object.header.id;
    let attrs_value = object
        .tree
        .attrs
        .clone()
        .ok_or_else(|| StoreError::Parse(format!("note {id}: missing attrs.json")))?;
    let attrs: NoteAttrs = serde_json::from_value(attrs_value)
        .map_err(|e| StoreError::Parse(format!("note {id} attrs.json: {e}")))?;
    let value = if let Some(bytes) = object.tree.blobs.get(TEXT_VALUE_FILE) {
        NoteValue::Text(
            String::from_utf8(bytes.clone())
                .map_err(|e| StoreError::Parse(format!("note {id} {TEXT_VALUE_FILE}: {e}")))?,
        )
    } else if let Some(bytes) = object.tree.blobs.get(JSON_VALUE_FILE) {
        NoteValue::Json(
            serde_json::from_slice(bytes)
                .map_err(|e| StoreError::Parse(format!("note {id} {JSON_VALUE_FILE}: {e}")))?,
        )
    } else {
        return Err(StoreError::Parse(format!(
            "note {id}: missing {TEXT_VALUE_FILE} or {JSON_VALUE_FILE}"
        )));
    };
    Ok((attrs, value))
}

fn marker_ms(object: &Object, name: &str, value: Option<i64>) -> Result<i64, StoreError> {
    value.ok_or_else(|| StoreError::Parse(format!("note {}: missing {name}", object.header.id)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::{git_in, run};
    use crate::test_support::open_store;
    use crate::{DatasetStore, SessionStore};

    fn cat_file(store: &Store, spec: &str) -> String {
        run(git_in(store.path(), ["cat-file", "-p", spec])).unwrap()
    }

    fn rev_parse(store: &Store, id: &str) -> String {
        store.rev_parse(&object_ref(id)).unwrap().unwrap()
    }

    fn note(store: &Store, name: &str, value: &str, target: Option<&str>) -> String {
        NoteStore::from(store)
            .create(NoteInput {
                name,
                value: NoteValue::Text(value.to_string()),
                author: "user:test",
                target,
                metadata: None,
            })
            .unwrap()
    }

    fn session(store: &Store, native_id: &str) -> String {
        use crate::session::tests::{FakeDriver, fake};
        SessionStore::from(store)
            .add(
                &FakeDriver,
                &mut fake(native_id, &[("session.jsonl", "{}\n")]),
            )
            .unwrap()
            .id
    }

    #[test]
    fn create_writes_ref_tree_and_commit() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());

        let id = note(&store, "comment", "looks fine", None);
        assert_eq!(id.len(), 26);

        let ref_path = object_ref(&id);
        let commit_sha = rev_parse(&store, &id);
        let commit = cat_file(&store, &commit_sha);
        assert!(
            commit.contains("author gage <noreply@gage.localhost>"),
            "{commit}"
        );
        assert!(
            commit.contains("committer gage <noreply@gage.localhost>"),
            "{commit}"
        );
        assert!(commit.contains("\nnote: comment"), "{commit}");
        assert!(!commit.contains("\nparent "), "{commit}");

        let listing = run(git_in(store.path(), ["ls-tree", "--name-only", &ref_path])).unwrap();
        let names: Vec<&str> = listing.lines().collect();
        assert_eq!(
            names,
            vec![
                "attrs.json",
                "created",
                "id",
                "modified",
                "type",
                "value.txt"
            ]
        );
        assert_eq!(
            cat_file(&store, &format!("{ref_path}:type")),
            "gage::note 1\n"
        );
        assert_eq!(
            cat_file(&store, &format!("{ref_path}:id")),
            format!("{id}\n")
        );
        assert_eq!(
            cat_file(&store, &format!("{ref_path}:attrs.json")),
            "{\"author\":\"user:test\",\"name\":\"comment\"}\n"
        );
        assert_eq!(
            cat_file(&store, &format!("{ref_path}:value.txt")),
            "looks fine"
        );
    }

    #[test]
    fn create_writes_target_link_when_targets_given() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());

        let first = note(&store, "root", "v", None);
        let first_commit = rev_parse(&store, &first);
        let second = note(&store, "reply", "v2", Some(&format!("note:{first}")));

        let target_content = cat_file(&store, &format!("{}:target.link", object_ref(&second)));
        assert_eq!(target_content, format!("{first_commit}\n"));

        let commit = cat_file(&store, &rev_parse(&store, &second));
        assert!(
            commit.contains(&format!("parent {first_commit}")),
            "{commit}"
        );
    }

    fn create_with_target(store: &Store, target: &str) -> Result<String, StoreError> {
        NoteStore::from(store).create(NoteInput {
            name: "n",
            value: NoteValue::Text("v".into()),
            author: "user:test",
            target: Some(target),
            metadata: None,
        })
    }

    #[test]
    fn create_rejects_target_missing_scheme() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        assert!(matches!(
            create_with_target(&store, "abc").unwrap_err(),
            StoreError::BadUrl(t) if t == "abc"
        ));
    }

    #[test]
    fn create_rejects_target_pointing_at_missing_ref() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        assert!(matches!(
            create_with_target(&store, "note:doesnotexist").unwrap_err(),
            StoreError::TargetNotFound(t) if t == "note:doesnotexist"
        ));
    }

    #[test]
    fn create_rejects_target_of_another_type() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let dataset = DatasetStore::from(&store).create().unwrap();
        assert!(matches!(
            create_with_target(&store, &format!("note:{dataset}")).unwrap_err(),
            StoreError::WrongType { id, expected, actual }
                if id == dataset && expected == "gage::note" && actual == "gage::dataset"
        ));
        // The scheme is what fixes the expected type
        assert!(create_with_target(&store, &format!("dataset:{dataset}")).is_ok());
    }

    #[test]
    fn create_rejects_deleted_target() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let gone = note(&store, "gone", "v", None);
        NoteStore::from(&store).delete(&gone).unwrap();
        assert!(matches!(
            create_with_target(&store, &format!("note:{gone}")).unwrap_err(),
            StoreError::ObjectDeleted(id) if id == gone
        ));
    }

    #[test]
    fn session_target_carries_line_selection_and_others_refuse_fragments() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let session_id = session(&store, "s1");
        let session_commit = rev_parse(&store, &session_id);
        let target = format!("session:{session_id}#12-20,31");

        let id = create_with_target(&store, &target).unwrap();
        let full = NoteStore::from(&store).get(&id).unwrap();
        assert_eq!(full.target.as_deref(), Some(target.as_str()));
        assert_eq!(full.targets, vec![session_commit.clone()]);
        let attrs = cat_file(&store, &format!("{}:attrs.json", object_ref(&id)));
        assert!(
            attrs.contains(&format!("\"target\":\"{target}\"")),
            "{attrs}"
        );

        assert!(matches!(
            create_with_target(&store, &format!("session:{session_id}#0")).unwrap_err(),
            StoreError::BadLineSelection(f) if f == "0"
        ));
        let other = note(&store, "n", "v", None);
        let bad = format!("note:{other}#1");
        assert!(matches!(
            create_with_target(&store, &bad).unwrap_err(),
            StoreError::BadTarget(t) if t == bad
        ));
        assert!(create_with_target(&store, &format!("session:{session_id}")).is_ok());
    }

    #[test]
    fn edit_changes_name_and_target_and_keeps_the_rest() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let notes = NoteStore::from(&store);
        let first = note(&store, "root", "v", None);
        let session_id = session(&store, "s1");
        let id = note(&store, "comment", "hello", Some(&format!("note:{first}")));

        // Name only: value and target stay
        notes
            .edit(
                &id,
                NoteEdit {
                    name: Some("summary"),
                    ..NoteEdit::default()
                },
            )
            .unwrap();
        let full = notes.get(&id).unwrap();
        assert_eq!(full.name, "summary");
        assert_eq!(full.value, NoteValue::Text("hello".into()));
        assert_eq!(
            full.target.as_deref(),
            Some(format!("note:{first}").as_str())
        );
        assert_eq!(full.targets, vec![rev_parse(&store, &first)]);

        // Target only: re-linked to the new object's commit
        let target = format!("session:{session_id}#3");
        notes
            .edit(
                &id,
                NoteEdit {
                    target: Some(&target),
                    ..NoteEdit::default()
                },
            )
            .unwrap();
        let full = notes.get(&id).unwrap();
        assert_eq!(full.name, "summary");
        assert_eq!(full.target.as_deref(), Some(target.as_str()));
        assert_eq!(full.targets, vec![rev_parse(&store, &session_id)]);
        let commit = cat_file(&store, &rev_parse(&store, &id));
        assert!(commit.contains("\nnote edit: summary"), "{commit}");

        // A bad target changes nothing
        let tip = rev_parse(&store, &id);
        assert!(matches!(
            notes
                .edit(
                    &id,
                    NoteEdit {
                        target: Some("note:missing"),
                        ..NoteEdit::default()
                    },
                )
                .unwrap_err(),
            StoreError::TargetNotFound(_)
        ));
        assert_eq!(rev_parse(&store, &id), tip);
    }

    /// `metadata` is written to `attrs.json` verbatim and comes back
    /// on both the full and the list record; a note without it reads
    /// as `None`.
    #[test]
    fn metadata_round_trips_through_attrs() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let notes = NoteStore::from(&store);
        let metadata = serde_json::json!({"model": "m", "lines": [1, 2]});
        let with = notes
            .create(NoteInput {
                name: "summary",
                value: NoteValue::Text("t".into()),
                author: "user:test",
                target: None,
                metadata: Some(metadata.clone()),
            })
            .unwrap();
        let without = notes
            .create(NoteInput {
                name: "summary",
                value: NoteValue::Text("t".into()),
                author: "user:test",
                target: None,
                metadata: None,
            })
            .unwrap();

        assert_eq!(notes.get(&with).unwrap().metadata, Some(metadata.clone()));
        assert_eq!(notes.get(&without).unwrap().metadata, None);
        let listed: Vec<NoteRecord> = notes.iter().unwrap().map(Result::unwrap).collect();
        let by_id = |id: &str| listed.iter().find(|n| n.id == id).unwrap();
        assert_eq!(by_id(&with).metadata, Some(metadata));
        assert_eq!(by_id(&without).metadata, None);
    }

    #[test]
    fn json_value_round_trips_through_value_json() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let notes = NoteStore::from(&store);
        let json = serde_json::json!({"score": 3, "tags": ["a", "b"]});
        let id = notes
            .create(NoteInput {
                name: "rating",
                value: NoteValue::Json(json.clone()),
                author: "user:test",
                target: None,
                metadata: None,
            })
            .unwrap();
        let listing = run(git_in(
            store.path(),
            ["ls-tree", "--name-only", &object_ref(&id)],
        ))
        .unwrap();
        assert_eq!(
            listing.lines().collect::<Vec<_>>(),
            vec![
                "attrs.json",
                "created",
                "id",
                "modified",
                "type",
                "value.json"
            ]
        );
        assert_eq!(notes.get(&id).unwrap().value, NoteValue::Json(json));

        // An edit may switch the value's form; the old file goes away
        notes
            .edit(
                &id,
                NoteEdit {
                    value: Some(NoteValue::Text("plain".into())),
                    ..NoteEdit::default()
                },
            )
            .unwrap();
        let listing = run(git_in(
            store.path(),
            ["ls-tree", "--name-only", &object_ref(&id)],
        ))
        .unwrap();
        assert!(
            listing.contains("value.txt") && !listing.contains("value.json"),
            "{listing}"
        );
        assert_eq!(
            notes.get(&id).unwrap().value,
            NoteValue::Text("plain".into())
        );
    }

    #[test]
    fn list_returns_notes_only() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());

        let first = note(&store, "a", "one", None);
        let second = note(&store, "b", "two", None);
        DatasetStore::from(&store).create().unwrap();

        let records: Vec<NoteRecord> = NoteStore::from(&store)
            .iter()
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(records.len(), 2);
        let ids: Vec<&str> = records.iter().map(|r| r.id.as_str()).collect();
        assert!(ids.contains(&first.as_str()));
        assert!(ids.contains(&second.as_str()));
        for r in &records {
            assert!(r.created_ms > 0);
            assert_eq!(r.created_ms, r.modified_ms);
            assert_eq!(r.author, "user:test");
        }
    }

    #[test]
    fn get_returns_full_record() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let root = note(&store, "root", "v", None);
        let root_commit = rev_parse(&store, &root);
        let id = note(
            &store,
            "reply",
            "hello\nworld",
            Some(&format!("note:{root}")),
        );

        let full = NoteStore::from(&store).get(&id[..8]).unwrap();
        assert_eq!(full.id, id);
        assert_eq!(full.name, "reply");
        assert_eq!(full.value, NoteValue::Text("hello\nworld".into()));
        assert_eq!(full.author, "user:test");
        assert_eq!(full.targets, vec![root_commit]);
        assert_eq!(full.created_ms, full.modified_ms);
    }

    #[test]
    fn get_rejects_other_types() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let dataset = DatasetStore::from(&store).create().unwrap();
        assert!(matches!(
            NoteStore::from(&store).get(&dataset).unwrap_err(),
            StoreError::WrongType { actual, .. } if actual == "gage::dataset"
        ));
    }

    #[test]
    fn create_writes_created_and_modified_blobs() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());

        let before = gage_core::datetime::now_ms();
        let id = note(&store, "n", "v", None);
        let after = gage_core::datetime::now_ms();

        let ref_path = object_ref(&id);
        let created = cat_file(&store, &format!("{ref_path}:created"))
            .trim()
            .parse::<i64>()
            .unwrap();
        let modified = cat_file(&store, &format!("{ref_path}:modified"))
            .trim()
            .parse::<i64>()
            .unwrap();
        assert_eq!(created, modified);
        assert!((before..=after).contains(&created));
    }

    #[test]
    fn edit_preserves_created_and_bumps_modified() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());

        let id = note(&store, "n", "v", None);
        let ref_path = object_ref(&id);
        let created_before = cat_file(&store, &format!("{ref_path}:created"));
        let modified_before = cat_file(&store, &format!("{ref_path}:modified"))
            .trim()
            .parse::<i64>()
            .unwrap();

        std::thread::sleep(std::time::Duration::from_millis(2));
        NoteStore::from(&store)
            .edit(
                &id,
                NoteEdit {
                    value: Some(NoteValue::Text("v2".into())),
                    ..NoteEdit::default()
                },
            )
            .unwrap();

        let created_after = cat_file(&store, &format!("{ref_path}:created"));
        let modified_after = cat_file(&store, &format!("{ref_path}:modified"))
            .trim()
            .parse::<i64>()
            .unwrap();
        assert_eq!(created_before, created_after);
        assert!(modified_after > modified_before);
    }

    #[test]
    fn iter_of_empty_store_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        assert_eq!(NoteStore::from(&store).iter().unwrap().count(), 0);
    }

    #[test]
    fn query_filters_by_name_and_limits() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let notes = NoteStore::from(&store);
        let a1 = note(&store, "a", "1", None);
        std::thread::sleep(std::time::Duration::from_millis(2));
        let a2 = note(&store, "a", "2", None);
        note(&store, "b", "3", None);

        let ids: Vec<String> = notes
            .query()
            .name("a")
            .iter()
            .unwrap()
            .map(|r| r.unwrap().id)
            .collect();
        assert_eq!(ids, vec![a2.clone(), a1.clone()]);

        let ids: Vec<String> = notes
            .query()
            .name("a")
            .order(Order::CreatedAsc)
            .limit(1)
            .iter()
            .unwrap()
            .map(|r| r.unwrap().id)
            .collect();
        assert_eq!(ids, vec![a1]);

        assert_eq!(notes.query().name("zzz").iter().unwrap().count(), 0);
    }

    #[test]
    fn edit_replaces_value_and_chains_commit() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());

        let id = note(&store, "comment", "first", None);
        let ref_path = object_ref(&id);
        let original_commit = rev_parse(&store, &id);

        assert_eq!(
            NoteStore::from(&store)
                .edit(
                    &id,
                    NoteEdit {
                        value: Some(NoteValue::Text("second".into())),
                        ..NoteEdit::default()
                    }
                )
                .unwrap(),
            id
        );

        let new_commit = rev_parse(&store, &id);
        assert_ne!(new_commit, original_commit);
        let parent = run(git_in(store.path(), ["rev-parse", &format!("{ref_path}^")]))
            .unwrap()
            .trim()
            .to_string();
        assert_eq!(parent, original_commit);
        assert_eq!(
            cat_file(&store, &format!("{ref_path}:parent")),
            format!("{original_commit}\n")
        );
        assert_eq!(cat_file(&store, &format!("{ref_path}:value.txt")), "second");
        assert_eq!(
            cat_file(&store, &format!("{ref_path}:attrs.json")),
            "{\"author\":\"user:test\",\"name\":\"comment\"}\n"
        );
        let commit = cat_file(&store, &new_commit);
        assert!(commit.contains("\nnote edit: comment"), "{commit}");
    }

    #[test]
    fn edit_with_same_value_writes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let id = note(&store, "n", "same", None);
        let before = rev_parse(&store, &id);
        NoteStore::from(&store)
            .edit(
                &id,
                NoteEdit {
                    value: Some(NoteValue::Text("same".into())),
                    ..NoteEdit::default()
                },
            )
            .unwrap();
        assert_eq!(rev_parse(&store, &id), before);
    }

    #[test]
    fn edit_preserves_target_link() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());

        let root = note(&store, "root", "v", None);
        let root_commit = rev_parse(&store, &root);
        let child = note(&store, "reply", "first", Some(&format!("note:{root}")));
        NoteStore::from(&store)
            .edit(
                &child,
                NoteEdit {
                    value: Some(NoteValue::Text("second".into())),
                    ..NoteEdit::default()
                },
            )
            .unwrap();

        let target_content = cat_file(&store, &format!("{}:target.link", object_ref(&child)));
        assert_eq!(target_content, format!("{root_commit}\n"));
        let commit = cat_file(&store, &rev_parse(&store, &child));
        assert!(
            commit.contains(&format!("parent {root_commit}")),
            "{commit}"
        );
    }

    #[test]
    fn edit_accepts_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let id = note(&store, "n", "v", None);
        assert_eq!(
            NoteStore::from(&store)
                .edit(
                    &id[..8],
                    NoteEdit {
                        value: Some(NoteValue::Text("v2".into())),
                        ..NoteEdit::default()
                    }
                )
                .unwrap(),
            id
        );
    }

    #[test]
    fn edit_errors_on_missing_note() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let err = NoteStore::from(&store)
            .edit(
                "doesnotexist",
                NoteEdit {
                    value: Some(NoteValue::Text("v".into())),
                    ..NoteEdit::default()
                },
            )
            .unwrap_err();
        assert!(matches!(err, StoreError::ObjectNotFound(id) if id == "doesnotexist"));
    }

    #[test]
    fn delete_writes_parentless_tombstone() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());

        let id = note(&store, "n", "v", None);
        let ref_path = object_ref(&id);
        let created_before = cat_file(&store, &format!("{ref_path}:created"));

        assert_eq!(NoteStore::from(&store).delete(&id).unwrap(), id);

        let commit = cat_file(&store, &rev_parse(&store, &id));
        assert!(!commit.contains("\nparent "), "{commit}");
        assert!(commit.contains("\nnote delete: n"), "{commit}");

        let listing = run(git_in(store.path(), ["ls-tree", "--name-only", &ref_path])).unwrap();
        let names: Vec<&str> = listing.lines().collect();
        assert_eq!(names, vec!["created", "deleted", "id", "modified", "type"]);
        assert_eq!(
            cat_file(&store, &format!("{ref_path}:created")),
            created_before
        );
        assert_eq!(
            cat_file(&store, &format!("{ref_path}:deleted")),
            cat_file(&store, &format!("{ref_path}:modified"))
        );
    }

    #[test]
    fn delete_hides_from_list() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let notes = NoteStore::from(&store);

        let keep = note(&store, "keep", "v", None);
        let gone = note(&store, "gone", "v", None);
        notes.delete(&gone).unwrap();

        let ids: Vec<String> = notes.iter().unwrap().map(|r| r.unwrap().id).collect();
        assert_eq!(ids, vec![keep]);
    }

    #[test]
    fn get_and_edit_refuse_deleted_notes() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let notes = NoteStore::from(&store);

        let id = note(&store, "n", "v", None);
        notes.delete(&id).unwrap();

        assert!(matches!(
            notes.get(&id).unwrap_err(),
            StoreError::ObjectDeleted(x) if x == id
        ));
        assert!(matches!(
            notes.edit(&id, NoteEdit { value: Some(NoteValue::Text("v2".into())), ..NoteEdit::default() }).unwrap_err(),
            StoreError::ObjectDeleted(x) if x == id
        ));
        assert!(matches!(
            notes.delete(&id).unwrap_err(),
            StoreError::ObjectDeleted(x) if x == id
        ));
    }
}
