//! Note objects: `gage::note 1`.
//!
//! Content is `attrs.json` (name, author, and the optional spec
//! fields), `value.txt` (the note value as plain text), and, when at
//! least one target is given, `target.link` listing the target commit
//! SHAs. Tree construction, commit parents, edits, and tombstones are
//! the generic object model's job; see [`crate::object`].

use std::path::Path;

use gage_core::uuid::new_uuid;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

use crate::git::{git_in, run};
use crate::object::{
    EditOutcome, Object, ObjectTree, object_ref, read_object_at, require_type, resolve_id_at,
};
use crate::{StoreError, exists, object, store_path};

const OBJECT_TYPE: &str = "gage::note";
const OBJECT_VERSION: &str = "1";
const VALUE_FILE: &str = "value.txt";
const TARGET_LINK: &str = "target.link";

/// Input to [`note_new`]. Every string is stored verbatim; the caller
/// is responsible for producing `author` in the
/// `user:`/`scanner:`/`agent:` URI form.
pub struct NoteInput<'a> {
    pub name: &'a str,
    pub value: &'a str,
    pub author: &'a str,
    /// Each target must have the form `note:<id>` and reference an
    /// existing note in the store.
    pub targets: &'a [String],
}

/// A single note read from the store, projected into the fields the
/// list view needs.
#[derive(Debug, PartialEq, Eq)]
pub struct NoteRecord {
    pub id: String,
    pub name: String,
    pub value: String,
    pub author: String,
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
    pub value: String,
    pub author: String,
    /// Commit SHAs from the `target.link` file, in file order. Empty
    /// when the note has no `target.link` file.
    pub targets: Vec<String>,
    pub created_ms: i64,
    pub modified_ms: i64,
}

/// The `attrs.json` shape. Optional fields defined by the spec are held
/// so an edit round-trip preserves them; no writer sets them today.
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

/// Create a note in the default store. Returns the new note's id.
pub fn note_new(input: NoteInput) -> Result<String, StoreError> {
    note_new_at(&store_path(), input)
}

/// Create a note in the store at `path`. Returns the new note's id.
pub fn note_new_at(path: &Path, input: NoteInput) -> Result<String, StoreError> {
    if !exists(path) {
        return Err(StoreError::NotFound(path.to_path_buf()));
    }
    let target_shas = resolve_target_shas(path, input.targets)?;
    let attrs = NoteAttrs {
        name: input.name.to_string(),
        author: input.author.to_string(),
        target: None,
        line: None,
        line_end: None,
        metadata: None,
        scan: None,
    };
    let tree = build_tree(&attrs, input.value, target_shas)?;
    let id = new_uuid();
    let message = format!("note: {}", input.name);
    object::create(path, OBJECT_TYPE, OBJECT_VERSION, &id, &tree, &message)?;
    Ok(id)
}

fn build_tree(
    attrs: &NoteAttrs,
    value: &str,
    target_shas: Vec<String>,
) -> Result<ObjectTree, StoreError> {
    let mut tree = ObjectTree {
        attrs: Some(
            serde_json::to_value(attrs)
                .map_err(|e| StoreError::Parse(format!("note attrs encode: {e}")))?,
        ),
        ..ObjectTree::default()
    };
    tree.blobs
        .insert(VALUE_FILE.to_string(), value.as_bytes().to_vec());
    if !target_shas.is_empty() {
        tree.links.insert(TARGET_LINK.to_string(), target_shas);
    }
    Ok(tree)
}

/// Parse `note:<id>` targets, verify each references an existing note,
/// and return the corresponding tip SHAs in input order.
fn resolve_target_shas(path: &Path, targets: &[String]) -> Result<Vec<String>, StoreError> {
    let mut shas = Vec::with_capacity(targets.len());
    for raw in targets {
        let id = raw
            .strip_prefix("note:")
            .ok_or_else(|| StoreError::BadTarget(raw.clone()))?;
        let sha = match run(git_in(path, ["rev-parse", "--verify", &object_ref(id)])) {
            Ok(sha) => sha.trim().to_string(),
            Err(StoreError::Git { .. }) => return Err(StoreError::TargetNotFound(raw.clone())),
            Err(e) => return Err(e),
        };
        let target = read_object_at(path, &sha)?;
        require_type(&target, OBJECT_TYPE)?;
        shas.push(sha);
    }
    Ok(shas)
}

/// Look up one note by full id or unique prefix in the default store.
pub fn note_get(id_or_prefix: &str) -> Result<NoteFull, StoreError> {
    note_get_at(&store_path(), id_or_prefix)
}

/// Look up one note by full id or unique prefix in the store at `path`.
///
/// Returns [`StoreError::ObjectNotFound`] when no object matches,
/// [`StoreError::AmbiguousId`] when more than one does, and
/// [`StoreError::WrongType`] when the match is not a note.
pub fn note_get_at(path: &Path, id_or_prefix: &str) -> Result<NoteFull, StoreError> {
    let object = current(path, id_or_prefix)?;
    decode_full(&object)
}

/// Resolve `id_or_prefix` to its current commit, verified to be a live
/// note.
fn current(path: &Path, id_or_prefix: &str) -> Result<Object, StoreError> {
    let (id, sha) = resolve_id_at(path, id_or_prefix)?;
    let object = read_object_at(path, &sha)?;
    require_type(&object, OBJECT_TYPE)?;
    if object.header.is_tombstone() {
        return Err(StoreError::ObjectDeleted(id));
    }
    Ok(object)
}

fn decode_full(object: &Object) -> Result<NoteFull, StoreError> {
    let (attrs, value) = decode_content(object)?;
    Ok(NoteFull {
        id: object.header.id.clone(),
        name: attrs.name,
        value,
        author: attrs.author,
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

fn decode_content(object: &Object) -> Result<(NoteAttrs, String), StoreError> {
    let attrs_value = object.tree.attrs.clone().ok_or_else(|| {
        StoreError::Parse(format!("note {}: missing attrs.json", object.header.id))
    })?;
    let attrs: NoteAttrs = serde_json::from_value(attrs_value)
        .map_err(|e| StoreError::Parse(format!("note {} attrs.json: {e}", object.header.id)))?;
    let value_bytes = object.tree.blobs.get(VALUE_FILE).ok_or_else(|| {
        StoreError::Parse(format!("note {}: missing {VALUE_FILE}", object.header.id))
    })?;
    let value = String::from_utf8(value_bytes.clone())
        .map_err(|e| StoreError::Parse(format!("note {} {VALUE_FILE}: {e}", object.header.id)))?;
    Ok((attrs, value))
}

fn marker_ms(object: &Object, name: &str, value: Option<i64>) -> Result<i64, StoreError> {
    value.ok_or_else(|| StoreError::Parse(format!("note {}: missing {name}", object.header.id)))
}

/// List every note in the default store, newest first by committer date.
pub fn note_list() -> Result<Vec<NoteRecord>, StoreError> {
    note_list_at(&store_path())
}

/// List every note in the store at `path`, newest first by committer
/// date. Tombstones and objects of other types are skipped.
pub fn note_list_at(path: &Path) -> Result<Vec<NoteRecord>, StoreError> {
    if !exists(path) {
        return Err(StoreError::NotFound(path.to_path_buf()));
    }
    let listing = run(git_in(
        path,
        [
            "for-each-ref",
            "--sort=-committerdate",
            "--format=%(objectname)",
            "refs/gage/object/",
        ],
    ))?;
    let mut records = Vec::new();
    for sha in listing.lines() {
        let object = read_object_at(path, sha)?;
        if object.header.object_type != OBJECT_TYPE || object.header.is_tombstone() {
            continue;
        }
        let full = decode_full(&object)?;
        records.push(NoteRecord {
            id: full.id,
            name: full.name,
            value: full.value,
            author: full.author,
            created_ms: full.created_ms,
            modified_ms: full.modified_ms,
        });
    }
    Ok(records)
}

/// Edit the value of an existing note. `name`, `author`, and any
/// existing targets are preserved. Returns the resolved id.
pub fn note_edit(id_or_prefix: &str, value: &str) -> Result<String, StoreError> {
    note_edit_at(&store_path(), id_or_prefix, value)
}

/// Edit the value of an existing note in the store at `path`.
pub fn note_edit_at(path: &Path, id_or_prefix: &str, value: &str) -> Result<String, StoreError> {
    if !exists(path) {
        return Err(StoreError::NotFound(path.to_path_buf()));
    }
    let object = current(path, id_or_prefix)?;
    let (attrs, _) = decode_content(&object)?;
    let targets = object
        .tree
        .links
        .get(TARGET_LINK)
        .cloned()
        .unwrap_or_default();
    let tree = build_tree(&attrs, value, targets)?;
    let message = format!("note edit: {}", attrs.name);
    match object::edit(path, &object, &tree, &message)? {
        EditOutcome::Unchanged | EditOutcome::Written(_) => Ok(object.header.id),
    }
}

/// Delete a note by writing a parentless tombstone commit. Returns the
/// resolved id.
pub fn note_delete(id_or_prefix: &str) -> Result<String, StoreError> {
    note_delete_at(&store_path(), id_or_prefix)
}

/// Delete a note in the store at `path`.
pub fn note_delete_at(path: &Path, id_or_prefix: &str) -> Result<String, StoreError> {
    if !exists(path) {
        return Err(StoreError::NotFound(path.to_path_buf()));
    }
    let object = current(path, id_or_prefix)?;
    let (attrs, _) = decode_content(&object)?;
    let message = format!("note delete: {}", attrs.name);
    object::delete(path, &object, &message)?;
    Ok(object.header.id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init_at;

    fn init_store(dir: &Path) -> std::path::PathBuf {
        let store = dir.join("store.git");
        init_at(&store).unwrap();
        store
    }

    fn cat_file(store: &Path, spec: &str) -> String {
        run(git_in(store, ["cat-file", "-p", spec])).unwrap()
    }

    fn rev_parse(store: &Path, id: &str) -> String {
        run(git_in(store, ["rev-parse", &object_ref(id)]))
            .unwrap()
            .trim()
            .to_string()
    }

    fn note(store: &Path, name: &str, value: &str, targets: &[String]) -> String {
        note_new_at(
            store,
            NoteInput {
                name,
                value,
                author: "user:test",
                targets,
            },
        )
        .unwrap()
    }

    #[test]
    fn new_writes_ref_tree_and_commit() {
        let tmp = tempfile::tempdir().unwrap();
        let store = init_store(tmp.path());

        let id = note(&store, "comment", "looks fine", &[]);
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

        let listing = run(git_in(&store, ["ls-tree", "--name-only", &ref_path])).unwrap();
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
    fn new_writes_target_link_when_targets_given() {
        let tmp = tempfile::tempdir().unwrap();
        let store = init_store(tmp.path());

        let first = note(&store, "root", "v", &[]);
        let first_commit = rev_parse(&store, &first);
        let second = note(&store, "reply", "v2", &[format!("note:{first}")]);

        let target_content = cat_file(&store, &format!("{}:target.link", object_ref(&second)));
        assert_eq!(target_content, format!("{first_commit}\n"));

        let commit = cat_file(&store, &rev_parse(&store, &second));
        assert!(
            commit.contains(&format!("parent {first_commit}")),
            "{commit}"
        );
    }

    #[test]
    fn new_rejects_target_missing_scheme() {
        let tmp = tempfile::tempdir().unwrap();
        let store = init_store(tmp.path());
        let err = note_new_at(
            &store,
            NoteInput {
                name: "n",
                value: "v",
                author: "user:test",
                targets: &["abc".to_string()],
            },
        )
        .unwrap_err();
        assert!(matches!(err, StoreError::BadTarget(t) if t == "abc"));
    }

    #[test]
    fn new_rejects_target_pointing_at_missing_ref() {
        let tmp = tempfile::tempdir().unwrap();
        let store = init_store(tmp.path());
        let err = note_new_at(
            &store,
            NoteInput {
                name: "n",
                value: "v",
                author: "user:test",
                targets: &["note:doesnotexist".to_string()],
            },
        )
        .unwrap_err();
        assert!(matches!(err, StoreError::TargetNotFound(t) if t == "note:doesnotexist"));
    }

    #[test]
    fn new_rejects_target_of_another_type() {
        let tmp = tempfile::tempdir().unwrap();
        let store = init_store(tmp.path());
        let dataset = crate::dataset_new_at(&store).unwrap();
        let err = note_new_at(
            &store,
            NoteInput {
                name: "n",
                value: "v",
                author: "user:test",
                targets: &[format!("note:{dataset}")],
            },
        )
        .unwrap_err();
        assert!(matches!(
            err,
            StoreError::WrongType { id, expected, actual }
                if id == dataset && expected == "gage::note" && actual == "gage::dataset"
        ));
    }

    #[test]
    fn list_returns_notes_only() {
        let tmp = tempfile::tempdir().unwrap();
        let store = init_store(tmp.path());

        let first = note(&store, "a", "one", &[]);
        let second = note(&store, "b", "two", &[]);
        crate::dataset_new_at(&store).unwrap();

        let records = note_list_at(&store).unwrap();
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
        let store = init_store(tmp.path());
        let root = note(&store, "root", "v", &[]);
        let root_commit = rev_parse(&store, &root);
        let id = note(&store, "reply", "hello\nworld", &[format!("note:{root}")]);

        let full = note_get_at(&store, &id[..8]).unwrap();
        assert_eq!(full.id, id);
        assert_eq!(full.name, "reply");
        assert_eq!(full.value, "hello\nworld");
        assert_eq!(full.author, "user:test");
        assert_eq!(full.targets, vec![root_commit]);
        assert_eq!(full.created_ms, full.modified_ms);
    }

    #[test]
    fn get_rejects_other_types() {
        let tmp = tempfile::tempdir().unwrap();
        let store = init_store(tmp.path());
        let dataset = crate::dataset_new_at(&store).unwrap();
        assert!(matches!(
            note_get_at(&store, &dataset).unwrap_err(),
            StoreError::WrongType { actual, .. } if actual == "gage::dataset"
        ));
    }

    #[test]
    fn new_writes_created_and_modified_blobs() {
        let tmp = tempfile::tempdir().unwrap();
        let store = init_store(tmp.path());

        let before = gage_core::datetime::now_ms();
        let id = note(&store, "n", "v", &[]);
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
        let store = init_store(tmp.path());

        let id = note(&store, "n", "v", &[]);
        let ref_path = object_ref(&id);
        let created_before = cat_file(&store, &format!("{ref_path}:created"));
        let modified_before = cat_file(&store, &format!("{ref_path}:modified"))
            .trim()
            .parse::<i64>()
            .unwrap();

        std::thread::sleep(std::time::Duration::from_millis(2));
        note_edit_at(&store, &id, "v2").unwrap();

        let created_after = cat_file(&store, &format!("{ref_path}:created"));
        let modified_after = cat_file(&store, &format!("{ref_path}:modified"))
            .trim()
            .parse::<i64>()
            .unwrap();
        assert_eq!(created_before, created_after);
        assert!(modified_after > modified_before);
    }

    #[test]
    fn list_empty_store_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let store = init_store(tmp.path());
        assert!(note_list_at(&store).unwrap().is_empty());
    }

    #[test]
    fn edit_replaces_value_and_chains_commit() {
        let tmp = tempfile::tempdir().unwrap();
        let store = init_store(tmp.path());

        let id = note(&store, "comment", "first", &[]);
        let ref_path = object_ref(&id);
        let original_commit = rev_parse(&store, &id);

        assert_eq!(note_edit_at(&store, &id, "second").unwrap(), id);

        let new_commit = rev_parse(&store, &id);
        assert_ne!(new_commit, original_commit);
        let parent = run(git_in(&store, ["rev-parse", &format!("{ref_path}^")]))
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
        let store = init_store(tmp.path());
        let id = note(&store, "n", "same", &[]);
        let before = rev_parse(&store, &id);
        note_edit_at(&store, &id, "same").unwrap();
        assert_eq!(rev_parse(&store, &id), before);
    }

    #[test]
    fn edit_preserves_target_link() {
        let tmp = tempfile::tempdir().unwrap();
        let store = init_store(tmp.path());

        let root = note(&store, "root", "v", &[]);
        let root_commit = rev_parse(&store, &root);
        let child = note(&store, "reply", "first", &[format!("note:{root}")]);
        note_edit_at(&store, &child, "second").unwrap();

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
        let store = init_store(tmp.path());
        let id = note(&store, "n", "v", &[]);
        assert_eq!(note_edit_at(&store, &id[..8], "v2").unwrap(), id);
    }

    #[test]
    fn edit_errors_on_missing_note() {
        let tmp = tempfile::tempdir().unwrap();
        let store = init_store(tmp.path());
        let err = note_edit_at(&store, "doesnotexist", "v").unwrap_err();
        assert!(matches!(err, StoreError::ObjectNotFound(id) if id == "doesnotexist"));
    }

    #[test]
    fn delete_writes_parentless_tombstone() {
        let tmp = tempfile::tempdir().unwrap();
        let store = init_store(tmp.path());

        let id = note(&store, "n", "v", &[]);
        let ref_path = object_ref(&id);
        let created_before = cat_file(&store, &format!("{ref_path}:created"));

        assert_eq!(note_delete_at(&store, &id).unwrap(), id);

        let commit = cat_file(&store, &rev_parse(&store, &id));
        assert!(!commit.contains("\nparent "), "{commit}");
        assert!(commit.contains("\nnote delete: n"), "{commit}");

        let listing = run(git_in(&store, ["ls-tree", "--name-only", &ref_path])).unwrap();
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
        let store = init_store(tmp.path());

        let keep = note(&store, "keep", "v", &[]);
        let gone = note(&store, "gone", "v", &[]);
        note_delete_at(&store, &gone).unwrap();

        let records = note_list_at(&store).unwrap();
        let ids: Vec<&str> = records.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec![keep.as_str()]);
    }

    #[test]
    fn get_and_edit_refuse_deleted_notes() {
        let tmp = tempfile::tempdir().unwrap();
        let store = init_store(tmp.path());

        let id = note(&store, "n", "v", &[]);
        note_delete_at(&store, &id).unwrap();

        assert!(matches!(
            note_get_at(&store, &id).unwrap_err(),
            StoreError::ObjectDeleted(x) if x == id
        ));
        assert!(matches!(
            note_edit_at(&store, &id, "v2").unwrap_err(),
            StoreError::ObjectDeleted(x) if x == id
        ));
        assert!(matches!(
            note_delete_at(&store, &id).unwrap_err(),
            StoreError::ObjectDeleted(x) if x == id
        ));
    }

    #[test]
    fn new_fails_when_store_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let err = note_new_at(
            &tmp.path().join("nope.git"),
            NoteInput {
                name: "n",
                value: "v",
                author: "user:test",
                targets: &[],
            },
        )
        .unwrap_err();
        assert!(matches!(err, StoreError::NotFound(_)));
    }
}
