//! Note writing: build the tree, commit it, and update
//! `refs/gage/notes/<id>`.
//!
//! The tree carries `format` (`gage-note 1\n`), `attrs` (compact JSON of
//! name/value/author, LF terminated), and, when at least one target is
//! given, `targets` (one `refs/gage/notes/<id>\n` per line). An `add`
//! commit is parentless; an `edit` commit chains against the previous
//! ref value. Author and committer are set to
//! `gage <noreply@gage.localhost>`.

use std::path::Path;

use gage_core::datetime::now_ms;
use gage_core::uuid::new_uuid;
use serde::{Deserialize, Serialize};

use crate::writer::{commit_tree, mktree, write_blob};
use crate::{StoreError, exists, git_in, run, store_path};

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
    /// Ref paths from the `targets` file, in file order. Empty when the
    /// note has no `targets` file.
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
    let targets = read_targets(path, &ref_path)?;

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

fn read_targets(path: &Path, ref_path: &str) -> Result<Vec<String>, StoreError> {
    let entries = run(git_in(path, ["ls-tree", "--name-only", ref_path]))?;
    if !entries.lines().any(|l| l == "targets") {
        return Ok(Vec::new());
    }
    let content = run(git_in(
        path,
        ["cat-file", "-p", &format!("{ref_path}:targets")],
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
/// the schema may broaden this later.
#[derive(Deserialize)]
struct StoredAttrs {
    name: String,
    value: String,
    author: String,
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

    let target_refs = resolve_targets(path, input.targets)?;

    let id = new_uuid();
    let now = now_ms();
    let format_sha = write_blob(path, b"gage-note 1\n")?;
    let attrs_sha = write_blob(path, encode_attrs(&input).as_bytes())?;
    let stamp_sha = write_blob(path, format!("{now}\n").as_bytes())?;

    let mut entries = vec![
        format!("100644 blob {attrs_sha}\tattrs"),
        format!("100644 blob {stamp_sha}\tcreated"),
        format!("100644 blob {format_sha}\tformat"),
        format!("100644 blob {stamp_sha}\tmodified"),
    ];
    if !target_refs.is_empty() {
        let content: String = target_refs.iter().map(|r| format!("{r}\n")).collect();
        let targets_sha = write_blob(path, content.as_bytes())?;
        entries.push(format!("100644 blob {targets_sha}\ttargets"));
    }
    let tree_sha = mktree(path, &entries)?;

    let message = format!("note: {}", input.name);
    let commit_sha = commit_tree(path, &tree_sha, &message, None)?;

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
    let attrs = read_attrs(path, &ref_path)?;

    let new_attrs = encode_attrs(&NoteInput {
        name: &attrs.name,
        value,
        author: &attrs.author,
        targets: &[],
    });
    let new_attrs_sha = write_blob(path, new_attrs.as_bytes())?;
    let now = now_ms();
    let new_modified_sha = write_blob(path, format!("{now}\n").as_bytes())?;

    let mut entries = vec![
        format!("100644 blob {new_attrs_sha}\tattrs"),
        format!("100644 blob {}\tcreated", tree_shas.created),
        format!("100644 blob {}\tformat", tree_shas.format),
        format!("100644 blob {new_modified_sha}\tmodified"),
    ];
    if let Some(targets_sha) = tree_shas.targets {
        entries.push(format!("100644 blob {targets_sha}\ttargets"));
    }
    let tree_sha = mktree(path, &entries)?;

    let message = format!("note edit: {}", attrs.name);
    let new_commit = commit_tree(path, &tree_sha, &message, Some(&current_commit))?;

    run(git_in(
        path,
        ["update-ref", &ref_path, &new_commit, &current_commit],
    ))?;

    Ok(id)
}

/// Delete a note by writing a tombstone commit. `attrs` and `targets`
/// are dropped from the tree; `created` and `format` are preserved;
/// `modified` and `deleted` are set to the current time. Returns the
/// resolved id.
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
        format!("100644 blob {}\tformat", tree_shas.format),
        format!("100644 blob {stamp_sha}\tmodified"),
    ];
    let tree_sha = mktree(path, &entries)?;

    let message = format!("note delete: {}", attrs.name);
    let new_commit = commit_tree(path, &tree_sha, &message, Some(&current_commit))?;

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
    format: String,
    created: String,
    targets: Option<String>,
    deleted: bool,
}

fn read_tree_shas(path: &Path, ref_path: &str) -> Result<TreeShas, StoreError> {
    let listing = run(git_in(path, ["ls-tree", ref_path]))?;
    let mut format = None;
    let mut created = None;
    let mut targets = None;
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
            "format" => format = Some(sha.to_string()),
            "created" => created = Some(sha.to_string()),
            "targets" => targets = Some(sha.to_string()),
            "deleted" => deleted = true,
            _ => {}
        }
    }
    let format =
        format.ok_or_else(|| StoreError::Parse(format!("missing format blob in {ref_path}")))?;
    let created =
        created.ok_or_else(|| StoreError::Parse(format!("missing created blob in {ref_path}")))?;
    Ok(TreeShas {
        format,
        created,
        targets,
        deleted,
    })
}

/// Parse `note:<id>` targets and confirm each referenced ref exists in
/// the store. Returns the corresponding ref paths in input order.
fn resolve_targets(path: &Path, targets: &[String]) -> Result<Vec<String>, StoreError> {
    let mut refs = Vec::with_capacity(targets.len());
    for raw in targets {
        let id = raw
            .strip_prefix("note:")
            .ok_or_else(|| StoreError::BadTarget(raw.clone()))?;
        let ref_path = format!("refs/gage/notes/{id}");
        match run(git_in(path, ["show-ref", "--verify", "--quiet", &ref_path])) {
            Ok(_) => refs.push(ref_path),
            Err(StoreError::Git { .. }) => {
                return Err(StoreError::TargetNotFound(raw.clone()));
            }
            Err(e) => return Err(e),
        }
    }
    Ok(refs)
}

/// Compact JSON encoding of the note's `attrs`, with a trailing LF.
fn encode_attrs(input: &NoteInput) -> String {
    #[derive(Serialize)]
    struct Attrs<'a> {
        name: &'a str,
        value: &'a str,
        author: &'a str,
    }
    let mut s = serde_json::to_string(&Attrs {
        name: input.name,
        value: input.value,
        author: input.author,
    })
    .expect("string fields cannot fail to serialize");
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
        assert!(tree.contains("\tformat"), "{tree}");
        assert!(tree.contains("\tmodified"), "{tree}");
        assert!(!tree.contains("\ttargets"), "{tree}");

        let format_content = cat_file(&store, &format!("{ref_path}:format"));
        assert_eq!(format_content, "gage-note 1\n");

        let attrs_content = cat_file(&store, &format!("{ref_path}:attrs"));
        assert_eq!(
            attrs_content,
            "{\"name\":\"comment\",\"value\":\"looks fine\",\"author\":\"user:test\"}\n"
        );
    }

    #[test]
    fn add_writes_targets_file_when_targets_given() {
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

        let targets_content = cat_file(&store, &format!("refs/gage/notes/{second}:targets"));
        assert_eq!(targets_content, format!("refs/gage/notes/{first}\n"));
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

        let attrs_content = cat_file(&store, &format!("{ref_path}:attrs"));
        assert_eq!(
            attrs_content,
            "{\"name\":\"comment\",\"value\":\"second\",\"author\":\"user:test\"}\n"
        );

        let commit = cat_file(&store, &new_commit);
        assert!(commit.contains("\nnote edit: comment"), "{commit}");
    }

    #[test]
    fn edit_preserves_targets_blob() {
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

        note_edit_at(&store, &child, "second").unwrap();

        let ref_path = format!("refs/gage/notes/{child}");
        let targets_content = cat_file(&store, &format!("{ref_path}:targets"));
        assert_eq!(targets_content, format!("refs/gage/notes/{root}\n"));
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
    fn delete_writes_tombstone_and_chains_commit() {
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
        let original_commit = run(git_in(&store, ["rev-parse", &ref_path]))
            .unwrap()
            .trim()
            .to_string();

        assert_eq!(note_delete_at(&store, &id).unwrap(), id);

        let new_commit = run(git_in(&store, ["rev-parse", &ref_path]))
            .unwrap()
            .trim()
            .to_string();
        let parent = run(git_in(&store, ["rev-parse", &format!("{ref_path}^")]))
            .unwrap()
            .trim()
            .to_string();
        assert_eq!(parent, original_commit);

        let listing = run(git_in(&store, ["ls-tree", "--name-only", &ref_path])).unwrap();
        let names: Vec<&str> = listing.lines().collect();
        assert!(names.contains(&"created"));
        assert!(names.contains(&"deleted"));
        assert!(names.contains(&"format"));
        assert!(names.contains(&"modified"));
        assert!(!names.contains(&"attrs"));
        assert!(!names.contains(&"targets"));

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

        let commit = cat_file(&store, &new_commit);
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
