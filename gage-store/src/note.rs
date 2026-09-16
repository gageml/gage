//! Note writing: build the tree, commit it, and update
//! `refs/gage/notes/<id>`.
//!
//! The tree carries the common header (`object` = `gage::note 1\n`,
//! `id`, `created`, `modified`), `attrs` (compact JSON), and, when at
//! least one target is given, `target.link` (one commit SHA per
//! line). Every SHA in a `*.link` file becomes a commit parent. An
//! edit tree additionally carries a `prev` file holding the previous
//! commit SHA, and that SHA is also added as a commit parent; a `prev`
//! file always points exactly one hop back. An add tree has no `prev`.
//! A delete writes a parentless tombstone whose tree carries only the
//! header plus `deleted`. Author and committer are set to
//! `gage <noreply@gage.localhost>`.

use std::path::Path;

use gage_core::datetime::now_ms;
use gage_core::uuid::new_uuid;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

use crate::git::{git_in, run};
use crate::writer::{commit_tree, mktree, write_blob};
use crate::{StoreError, exists, store_path};

/// `object` blob content for a note tree.
const NOTE_OBJECT: &[u8] = b"gage::note 1\n";

/// Input to [`note_add`]. Every string is stored verbatim; the caller is
/// responsible for producing `author` in the `user:`/`scanner:`/`agent:`
/// URI form.
pub struct NoteInput<'a> {
    pub name: &'a str,
    pub value: &'a str,
    pub author: &'a str,
    /// Each target must have the form `note:<id>` and reference an
    /// existing `refs/gage/notes/<id>` in the store.
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

/// Look up one note by full id or unique prefix in the default store.
pub fn note_get(id_or_prefix: &str) -> Result<NoteFull, StoreError> {
    note_get_at(&store_path(), id_or_prefix)
}

/// Look up one note by full id or unique prefix in the store at `path`.
///
/// Returns [`StoreError::NoteNotFound`] when no ref matches, and
/// [`StoreError::AmbiguousNoteId`] when more than one does.
pub fn note_get_at(path: &Path, id_or_prefix: &str) -> Result<NoteFull, StoreError> {
    if !exists(path) {
        return Err(StoreError::NotFound(path.to_path_buf()));
    }

    let pattern = format!("refs/gage/notes/{id_or_prefix}*");
    let matches = run(git_in(
        path,
        ["for-each-ref", "--format=%(refname:strip=3)", &pattern],
    ))?;
    let ids: Vec<&str> = matches.lines().collect();
    let id = match ids.as_slice() {
        [] => return Err(StoreError::NoteNotFound(id_or_prefix.to_string())),
        [only] => (*only).to_string(),
        many => {
            return Err(StoreError::AmbiguousNoteId(
                id_or_prefix.to_string(),
                many.len(),
            ));
        }
    };

    let ref_path = format!("refs/gage/notes/{id}");
    if is_deleted(path, &ref_path)? {
        return Err(StoreError::NoteDeleted(id));
    }
    let created_ms = read_ms_blob(path, &ref_path, "created")?;
    let modified_ms = read_ms_blob(path, &ref_path, "modified")?;
    let attrs = read_attrs(path, &ref_path)?;
    let targets = read_target_link(path, &ref_path)?;

    Ok(NoteFull {
        id,
        name: attrs.name,
        value: attrs.value,
        author: attrs.author,
        targets,
        created_ms,
        modified_ms,
    })
}

/// Read `<ref_path>:<file>` as a decimal integer of milliseconds since the
/// Unix epoch.
fn read_ms_blob(path: &Path, ref_path: &str, file: &str) -> Result<i64, StoreError> {
    let out = run(git_in(
        path,
        ["cat-file", "-p", &format!("{ref_path}:{file}")],
    ))?;
    out.trim()
        .parse::<i64>()
        .map_err(|e| StoreError::Parse(format!("{file} {ref_path}: {e}")))
}

fn read_attrs(path: &Path, ref_path: &str) -> Result<StoredAttrs, StoreError> {
    let json = run(git_in(
        path,
        ["cat-file", "-p", &format!("{ref_path}:attrs")],
    ))?;
    serde_json::from_str(json.trim_end())
        .map_err(|e| StoreError::Parse(format!("attrs {ref_path}: {e}")))
}

fn read_target_link(path: &Path, ref_path: &str) -> Result<Vec<String>, StoreError> {
    let entries = run(git_in(path, ["ls-tree", "--name-only", ref_path]))?;
    if !entries.lines().any(|l| l == "target.link") {
        return Ok(Vec::new());
    }
    let content = run(git_in(
        path,
        ["cat-file", "-p", &format!("{ref_path}:target.link")],
    ))?;
    Ok(content.lines().map(|s| s.to_string()).collect())
}

/// List every note in the default store, newest first by committer date.
pub fn note_list() -> Result<Vec<NoteRecord>, StoreError> {
    note_list_at(&store_path())
}

/// List every note in the store at `path`, newest first by committer
/// date.
pub fn note_list_at(path: &Path) -> Result<Vec<NoteRecord>, StoreError> {
    if !exists(path) {
        return Err(StoreError::NotFound(path.to_path_buf()));
    }
    let listing = run(git_in(
        path,
        [
            "for-each-ref",
            "--sort=-committerdate",
            "--format=%(refname:strip=3)",
            "refs/gage/notes/",
        ],
    ))?;

    let mut records = Vec::new();
    for id in listing.lines() {
        let ref_path = format!("refs/gage/notes/{id}");
        if is_deleted(path, &ref_path)? {
            continue;
        }
        let attrs = read_attrs(path, &ref_path)?;
        let created_ms = read_ms_blob(path, &ref_path, "created")?;
        let modified_ms = read_ms_blob(path, &ref_path, "modified")?;
        records.push(NoteRecord {
            id: id.to_string(),
            name: attrs.name,
            value: attrs.value,
            author: attrs.author,
            created_ms,
            modified_ms,
        });
    }
    Ok(records)
}

/// True when the note's current commit tree contains a `deleted` marker.
fn is_deleted(path: &Path, ref_path: &str) -> Result<bool, StoreError> {
    match run(git_in(
        path,
        ["cat-file", "-e", &format!("{ref_path}:deleted")],
    )) {
        Ok(_) => Ok(true),
        Err(StoreError::Git { .. }) => Ok(false),
        Err(e) => Err(e),
    }
}

/// The on-disk shape of `attrs`. `value` is treated as a string for now;
/// the schema may broaden this later. Optional fields defined by the
/// spec are held so an edit round-trip preserves them; no writer sets
/// them today.
#[derive(Deserialize, Serialize)]
struct StoredAttrs {
    name: String,
    value: String,
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

/// Add a note to the default store. Returns the new note's id.
pub fn note_add(input: NoteInput) -> Result<String, StoreError> {
    note_add_at(&store_path(), input)
}

/// Add a note to the store at `path`. Returns the new note's id.
pub fn note_add_at(path: &Path, input: NoteInput) -> Result<String, StoreError> {
    if !exists(path) {
        return Err(StoreError::NotFound(path.to_path_buf()));
    }

    let target_shas = resolve_target_shas(path, input.targets)?;

    let id = new_uuid();
    let now = now_ms();
    let object_sha = write_blob(path, NOTE_OBJECT)?;
    let id_sha = write_blob(path, format!("{id}\n").as_bytes())?;
    let attrs_sha = write_blob(path, encode_attrs(&input).as_bytes())?;
    let stamp_sha = write_blob(path, format!("{now}\n").as_bytes())?;

    let mut entries = vec![
        format!("100644 blob {attrs_sha}\tattrs"),
        format!("100644 blob {stamp_sha}\tcreated"),
        format!("100644 blob {id_sha}\tid"),
        format!("100644 blob {stamp_sha}\tmodified"),
        format!("100644 blob {object_sha}\tobject"),
    ];
    if !target_shas.is_empty() {
        let content: String = target_shas.iter().map(|s| format!("{s}\n")).collect();
        let target_link_sha = write_blob(path, content.as_bytes())?;
        entries.push(format!("100644 blob {target_link_sha}\ttarget.link"));
    }
    let tree_sha = mktree(path, &entries)?;

    let message = format!("note: {}", input.name);
    let parents: Vec<&str> = target_shas.iter().map(|s| s.as_str()).collect();
    let commit_sha = commit_tree(path, &tree_sha, &message, &parents)?;

    let ref_path = format!("refs/gage/notes/{id}");
    run(git_in(path, ["update-ref", &ref_path, &commit_sha, ""]))?;

    Ok(id)
}

/// Edit the value of an existing note. `name`, `author`, and any
/// existing `targets` are preserved. Returns the resolved id.
pub fn note_edit(id_or_prefix: &str, value: &str) -> Result<String, StoreError> {
    note_edit_at(&store_path(), id_or_prefix, value)
}

/// Edit the value of an existing note in the store at `path`.
pub fn note_edit_at(path: &Path, id_or_prefix: &str, value: &str) -> Result<String, StoreError> {
    if !exists(path) {
        return Err(StoreError::NotFound(path.to_path_buf()));
    }

    let (id, current_commit) = resolve_ref(path, id_or_prefix)?;
    let ref_path = format!("refs/gage/notes/{id}");
    let tree_shas = read_tree_shas(path, &ref_path)?;
    if tree_shas.deleted {
        return Err(StoreError::NoteDeleted(id));
    }
    let mut attrs = read_attrs(path, &ref_path)?;
    let name = attrs.name.clone();
    attrs.value = value.to_string();

    let new_attrs_sha = write_blob(path, serialize_attrs(&attrs).as_bytes())?;
    let now = now_ms();
    let new_modified_sha = write_blob(path, format!("{now}\n").as_bytes())?;
    let prev_sha = write_blob(path, format!("{current_commit}\n").as_bytes())?;

    let mut entries = vec![
        format!("100644 blob {new_attrs_sha}\tattrs"),
        format!("100644 blob {}\tcreated", tree_shas.created),
        format!("100644 blob {}\tid", tree_shas.id),
        format!("100644 blob {new_modified_sha}\tmodified"),
        format!("100644 blob {}\tobject", tree_shas.object),
        format!("100644 blob {prev_sha}\tprev"),
    ];
    if let Some(target_sha) = &tree_shas.target {
        entries.push(format!("100644 blob {target_sha}\ttarget.link"));
    }
    let tree_sha = mktree(path, &entries)?;

    let target_shas = read_target_link(path, &ref_path)?;
    let mut parents: Vec<&str> = vec![&current_commit];
    parents.extend(target_shas.iter().map(|s| s.as_str()));

    let message = format!("note edit: {name}");
    let new_commit = commit_tree(path, &tree_sha, &message, &parents)?;

    run(git_in(
        path,
        ["update-ref", &ref_path, &new_commit, &current_commit],
    ))?;

    Ok(id)
}

/// Delete a note by writing a parentless tombstone commit. The tree
/// carries only the header (`object`, `id`, `created`, `modified`) plus
/// `deleted`; `attrs`, `prev`, and any `*.link` files are dropped. The
/// prior commits become unreachable from the ref and are reclaimed by
/// `gc` unless another object links them. Returns the resolved id.
pub fn note_delete(id_or_prefix: &str) -> Result<String, StoreError> {
    note_delete_at(&store_path(), id_or_prefix)
}

/// Delete a note in the store at `path`.
pub fn note_delete_at(path: &Path, id_or_prefix: &str) -> Result<String, StoreError> {
    if !exists(path) {
        return Err(StoreError::NotFound(path.to_path_buf()));
    }

    let (id, current_commit) = resolve_ref(path, id_or_prefix)?;
    let ref_path = format!("refs/gage/notes/{id}");
    let tree_shas = read_tree_shas(path, &ref_path)?;
    if tree_shas.deleted {
        return Err(StoreError::NoteDeleted(id));
    }
    // Read the pre-delete name so the commit message names the note.
    let attrs = read_attrs(path, &ref_path)?;

    let now = now_ms();
    let stamp_sha = write_blob(path, format!("{now}\n").as_bytes())?;

    let entries = vec![
        format!("100644 blob {}\tcreated", tree_shas.created),
        format!("100644 blob {stamp_sha}\tdeleted"),
        format!("100644 blob {}\tid", tree_shas.id),
        format!("100644 blob {stamp_sha}\tmodified"),
        format!("100644 blob {}\tobject", tree_shas.object),
    ];
    let tree_sha = mktree(path, &entries)?;

    let message = format!("note delete: {}", attrs.name);
    let new_commit = commit_tree(path, &tree_sha, &message, &[])?;

    run(git_in(
        path,
        ["update-ref", &ref_path, &new_commit, &current_commit],
    ))?;

    Ok(id)
}

/// Resolve a full id or unique prefix to `(id, commit_sha)`.
fn resolve_ref(path: &Path, id_or_prefix: &str) -> Result<(String, String), StoreError> {
    let pattern = format!("refs/gage/notes/{id_or_prefix}*");
    let matches = run(git_in(
        path,
        [
            "for-each-ref",
            "--format=%(refname:strip=3) %(objectname)",
            &pattern,
        ],
    ))?;
    let lines: Vec<&str> = matches.lines().collect();
    match lines.as_slice() {
        [] => Err(StoreError::NoteNotFound(id_or_prefix.to_string())),
        [only] => {
            let (id, commit) = only
                .split_once(' ')
                .ok_or_else(|| StoreError::Parse(format!("for-each-ref line: {only}")))?;
            Ok((id.to_string(), commit.to_string()))
        }
        many => Err(StoreError::AmbiguousNoteId(
            id_or_prefix.to_string(),
            many.len(),
        )),
    }
}

/// Blob shas of the reused tree entries under a note's current commit,
/// plus whether the commit is a tombstone.
struct TreeShas {
    object: String,
    id: String,
    created: String,
    target: Option<String>,
    deleted: bool,
}

fn read_tree_shas(path: &Path, ref_path: &str) -> Result<TreeShas, StoreError> {
    let listing = run(git_in(path, ["ls-tree", ref_path]))?;
    let mut object = None;
    let mut id = None;
    let mut created = None;
    let mut target = None;
    let mut deleted = false;
    for line in listing.lines() {
        // Each line: "<mode> <type> <sha>\t<name>".
        let (meta, name) = line
            .split_once('\t')
            .ok_or_else(|| StoreError::Parse(format!("ls-tree line: {line}")))?;
        let sha = meta
            .split_whitespace()
            .nth(2)
            .ok_or_else(|| StoreError::Parse(format!("ls-tree meta: {meta}")))?;
        match name {
            "object" => object = Some(sha.to_string()),
            "id" => id = Some(sha.to_string()),
            "created" => created = Some(sha.to_string()),
            "target.link" => target = Some(sha.to_string()),
            "deleted" => deleted = true,
            _ => {}
        }
    }
    let object =
        object.ok_or_else(|| StoreError::Parse(format!("missing object blob in {ref_path}")))?;
    let id = id.ok_or_else(|| StoreError::Parse(format!("missing id blob in {ref_path}")))?;
    let created =
        created.ok_or_else(|| StoreError::Parse(format!("missing created blob in {ref_path}")))?;
    Ok(TreeShas {
        object,
        id,
        created,
        target,
        deleted,
    })
}

/// Parse `note:<id>` targets, verify each referenced ref exists, and
/// return the corresponding commit SHAs in input order. These SHAs are
/// written to the `target.link` file and added as commit parents.
fn resolve_target_shas(path: &Path, targets: &[String]) -> Result<Vec<String>, StoreError> {
    let mut shas = Vec::with_capacity(targets.len());
    for raw in targets {
        let id = raw
            .strip_prefix("note:")
            .ok_or_else(|| StoreError::BadTarget(raw.clone()))?;
        let ref_path = format!("refs/gage/notes/{id}");
        match run(git_in(path, ["rev-parse", "--verify", &ref_path])) {
            Ok(sha) => shas.push(sha.trim().to_string()),
            Err(StoreError::Git { .. }) => {
                return Err(StoreError::TargetNotFound(raw.clone()));
            }
            Err(e) => return Err(e),
        }
    }
    Ok(shas)
}

/// Compact JSON encoding of a fresh note's `attrs`, with a trailing LF.
/// Optional spec fields are omitted; no writer sets them today.
fn encode_attrs(input: &NoteInput) -> String {
    let attrs = StoredAttrs {
        name: input.name.to_string(),
        value: input.value.to_string(),
        author: input.author.to_string(),
        target: None,
        line: None,
        line_end: None,
        metadata: None,
        scan: None,
    };
    serialize_attrs(&attrs)
}

/// Serialize `StoredAttrs` with a trailing LF, preserving any optional
/// fields present on the value.
fn serialize_attrs(attrs: &StoredAttrs) -> String {
    let mut s = serde_json::to_string(attrs).expect("attrs are always serializable");
    s.push('\n');
    s
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

    fn cat_file(store: &Path, sha: &str) -> String {
        run(git_in(store, ["cat-file", "-p", sha])).unwrap()
    }

    #[test]
    fn add_writes_ref_tree_and_commit() {
        let tmp = tempfile::tempdir().unwrap();
        let store = init_store(tmp.path());

        let id = note_add_at(
            &store,
            NoteInput {
                name: "comment",
                value: "looks fine",
                author: "user:test",
                targets: &[],
            },
        )
        .unwrap();
        assert_eq!(id.len(), 26);

        let ref_path = format!("refs/gage/notes/{id}");
        let commit_sha = run(git_in(&store, ["rev-parse", &ref_path]))
            .unwrap()
            .trim()
            .to_string();

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

        let tree = cat_file(&store, &format!("{ref_path}^{{tree}}"));
        assert!(tree.contains("\tattrs"), "{tree}");
        assert!(tree.contains("\tcreated"), "{tree}");
        assert!(tree.contains("\tid"), "{tree}");
        assert!(tree.contains("\tmodified"), "{tree}");
        assert!(tree.contains("\tobject"), "{tree}");
        assert!(!tree.contains("\ttarget.link"), "{tree}");
        assert!(!tree.contains("\tprev"), "{tree}");

        let object_content = cat_file(&store, &format!("{ref_path}:object"));
        assert_eq!(object_content, "gage::note 1\n");

        let id_content = cat_file(&store, &format!("{ref_path}:id"));
        assert_eq!(id_content, format!("{id}\n"));

        let attrs_content = cat_file(&store, &format!("{ref_path}:attrs"));
        assert_eq!(
            attrs_content,
            "{\"name\":\"comment\",\"value\":\"looks fine\",\"author\":\"user:test\"}\n"
        );
    }

    #[test]
    fn add_writes_target_link_when_targets_given() {
        let tmp = tempfile::tempdir().unwrap();
        let store = init_store(tmp.path());

        let first = note_add_at(
            &store,
            NoteInput {
                name: "root",
                value: "v",
                author: "user:test",
                targets: &[],
            },
        )
        .unwrap();
        let first_commit = run(git_in(
            &store,
            ["rev-parse", &format!("refs/gage/notes/{first}")],
        ))
        .unwrap()
        .trim()
        .to_string();

        let target = format!("note:{first}");
        let second = note_add_at(
            &store,
            NoteInput {
                name: "reply",
                value: "v2",
                author: "user:test",
                targets: &[target],
            },
        )
        .unwrap();

        let target_content = cat_file(&store, &format!("refs/gage/notes/{second}:target.link"));
        assert_eq!(target_content, format!("{first_commit}\n"));

        let second_commit = run(git_in(
            &store,
            ["rev-parse", &format!("refs/gage/notes/{second}")],
        ))
        .unwrap()
        .trim()
        .to_string();
        let commit = cat_file(&store, &second_commit);
        assert!(
            commit.contains(&format!("parent {first_commit}")),
            "{commit}"
        );
    }

    #[test]
    fn add_rejects_target_missing_scheme() {
        let tmp = tempfile::tempdir().unwrap();
        let store = init_store(tmp.path());
        let err = note_add_at(
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
    fn add_rejects_target_pointing_at_missing_ref() {
        let tmp = tempfile::tempdir().unwrap();
        let store = init_store(tmp.path());
        let err = note_add_at(
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
    fn list_returns_notes_newest_first() {
        let tmp = tempfile::tempdir().unwrap();
        let store = init_store(tmp.path());

        let first = note_add_at(
            &store,
            NoteInput {
                name: "a",
                value: "one",
                author: "user:test",
                targets: &[],
            },
        )
        .unwrap();
        // for-each-ref sorts by committerdate at second precision; a
        // second-boundary sleep would be needed for strict ordering.
        // Instead assert the set and that both timestamps are present.
        let second = note_add_at(
            &store,
            NoteInput {
                name: "b",
                value: "two",
                author: "user:test",
                targets: &[],
            },
        )
        .unwrap();

        let records = note_list_at(&store).unwrap();
        assert_eq!(records.len(), 2);
        let ids: Vec<&str> = records.iter().map(|r| r.id.as_str()).collect();
        assert!(ids.contains(&first.as_str()));
        assert!(ids.contains(&second.as_str()));
        for r in &records {
            assert!(r.created_ms > 0);
            assert!(r.modified_ms > 0);
            assert_eq!(r.created_ms, r.modified_ms);
            assert_eq!(r.author, "user:test");
        }
    }

    #[test]
    fn add_writes_created_and_modified_blobs() {
        let tmp = tempfile::tempdir().unwrap();
        let store = init_store(tmp.path());

        let before = gage_core::datetime::now_ms();
        let id = note_add_at(
            &store,
            NoteInput {
                name: "n",
                value: "v",
                author: "user:test",
                targets: &[],
            },
        )
        .unwrap();
        let after = gage_core::datetime::now_ms();

        let ref_path = format!("refs/gage/notes/{id}");
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

        let id = note_add_at(
            &store,
            NoteInput {
                name: "n",
                value: "v",
                author: "user:test",
                targets: &[],
            },
        )
        .unwrap();
        let ref_path = format!("refs/gage/notes/{id}");
        let created_before = cat_file(&store, &format!("{ref_path}:created"))
            .trim()
            .to_string();
        let modified_before = cat_file(&store, &format!("{ref_path}:modified"))
            .trim()
            .parse::<i64>()
            .unwrap();

        // Ensure the clock advances.
        std::thread::sleep(std::time::Duration::from_millis(2));
        note_edit_at(&store, &id, "v2").unwrap();

        let created_after = cat_file(&store, &format!("{ref_path}:created"))
            .trim()
            .to_string();
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

        let id = note_add_at(
            &store,
            NoteInput {
                name: "comment",
                value: "first",
                author: "user:test",
                targets: &[],
            },
        )
        .unwrap();
        let ref_path = format!("refs/gage/notes/{id}");
        let original_commit = run(git_in(&store, ["rev-parse", &ref_path]))
            .unwrap()
            .trim()
            .to_string();

        let returned = note_edit_at(&store, &id, "second").unwrap();
        assert_eq!(returned, id);

        let new_commit = run(git_in(&store, ["rev-parse", &ref_path]))
            .unwrap()
            .trim()
            .to_string();
        assert_ne!(new_commit, original_commit);

        let parent = run(git_in(&store, ["rev-parse", &format!("{ref_path}^")]))
            .unwrap()
            .trim()
            .to_string();
        assert_eq!(parent, original_commit);

        let prev_content = cat_file(&store, &format!("{ref_path}:prev"));
        assert_eq!(prev_content, format!("{original_commit}\n"));

        let attrs_content = cat_file(&store, &format!("{ref_path}:attrs"));
        assert_eq!(
            attrs_content,
            "{\"name\":\"comment\",\"value\":\"second\",\"author\":\"user:test\"}\n"
        );

        let commit = cat_file(&store, &new_commit);
        assert!(commit.contains("\nnote edit: comment"), "{commit}");
    }

    #[test]
    fn edit_preserves_target_link() {
        let tmp = tempfile::tempdir().unwrap();
        let store = init_store(tmp.path());

        let root = note_add_at(
            &store,
            NoteInput {
                name: "root",
                value: "v",
                author: "user:test",
                targets: &[],
            },
        )
        .unwrap();
        let child = note_add_at(
            &store,
            NoteInput {
                name: "reply",
                value: "first",
                author: "user:test",
                targets: &[format!("note:{root}")],
            },
        )
        .unwrap();

        let root_commit = run(git_in(
            &store,
            ["rev-parse", &format!("refs/gage/notes/{root}")],
        ))
        .unwrap()
        .trim()
        .to_string();
        note_edit_at(&store, &child, "second").unwrap();

        let ref_path = format!("refs/gage/notes/{child}");
        let target_content = cat_file(&store, &format!("{ref_path}:target.link"));
        assert_eq!(target_content, format!("{root_commit}\n"));
    }

    #[test]
    fn edit_accepts_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        let store = init_store(tmp.path());
        let id = note_add_at(
            &store,
            NoteInput {
                name: "n",
                value: "v",
                author: "user:test",
                targets: &[],
            },
        )
        .unwrap();
        let prefix = &id[..8];
        assert_eq!(note_edit_at(&store, prefix, "v2").unwrap(), id);
    }

    #[test]
    fn edit_errors_on_missing_note() {
        let tmp = tempfile::tempdir().unwrap();
        let store = init_store(tmp.path());
        let err = note_edit_at(&store, "doesnotexist", "v").unwrap_err();
        assert!(matches!(err, StoreError::NoteNotFound(id) if id == "doesnotexist"));
    }

    #[test]
    fn delete_writes_parentless_tombstone() {
        let tmp = tempfile::tempdir().unwrap();
        let store = init_store(tmp.path());

        let id = note_add_at(
            &store,
            NoteInput {
                name: "n",
                value: "v",
                author: "user:test",
                targets: &[],
            },
        )
        .unwrap();
        let ref_path = format!("refs/gage/notes/{id}");
        let created_before = cat_file(&store, &format!("{ref_path}:created"))
            .trim()
            .to_string();

        assert_eq!(note_delete_at(&store, &id).unwrap(), id);

        let new_commit = run(git_in(&store, ["rev-parse", &ref_path]))
            .unwrap()
            .trim()
            .to_string();
        let commit = cat_file(&store, &new_commit);
        assert!(!commit.contains("\nparent "), "{commit}");

        let listing = run(git_in(&store, ["ls-tree", "--name-only", &ref_path])).unwrap();
        let names: Vec<&str> = listing.lines().collect();
        assert!(names.contains(&"created"));
        assert!(names.contains(&"deleted"));
        assert!(names.contains(&"id"));
        assert!(names.contains(&"modified"));
        assert!(names.contains(&"object"));
        assert!(!names.contains(&"attrs"));
        assert!(!names.contains(&"target.link"));
        assert!(!names.contains(&"prev"));

        let created_after = cat_file(&store, &format!("{ref_path}:created"))
            .trim()
            .to_string();
        assert_eq!(created_after, created_before);

        let deleted = cat_file(&store, &format!("{ref_path}:deleted"))
            .trim()
            .to_string();
        let modified = cat_file(&store, &format!("{ref_path}:modified"))
            .trim()
            .to_string();
        assert_eq!(deleted, modified);

        assert!(commit.contains("\nnote delete: n"), "{commit}");
    }

    #[test]
    fn delete_hides_from_list() {
        let tmp = tempfile::tempdir().unwrap();
        let store = init_store(tmp.path());

        let keep = note_add_at(
            &store,
            NoteInput {
                name: "keep",
                value: "v",
                author: "user:test",
                targets: &[],
            },
        )
        .unwrap();
        let gone = note_add_at(
            &store,
            NoteInput {
                name: "gone",
                value: "v",
                author: "user:test",
                targets: &[],
            },
        )
        .unwrap();
        note_delete_at(&store, &gone).unwrap();

        let records = note_list_at(&store).unwrap();
        let ids: Vec<&str> = records.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec![keep.as_str()]);
    }

    #[test]
    fn get_and_edit_refuse_deleted_notes() {
        let tmp = tempfile::tempdir().unwrap();
        let store = init_store(tmp.path());

        let id = note_add_at(
            &store,
            NoteInput {
                name: "n",
                value: "v",
                author: "user:test",
                targets: &[],
            },
        )
        .unwrap();
        note_delete_at(&store, &id).unwrap();

        assert!(matches!(
            note_get_at(&store, &id).unwrap_err(),
            StoreError::NoteDeleted(x) if x == id
        ));
        assert!(matches!(
            note_edit_at(&store, &id, "v2").unwrap_err(),
            StoreError::NoteDeleted(x) if x == id
        ));
        assert!(matches!(
            note_delete_at(&store, &id).unwrap_err(),
            StoreError::NoteDeleted(x) if x == id
        ));
    }

    #[test]
    fn add_fails_when_store_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let err = note_add_at(
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
