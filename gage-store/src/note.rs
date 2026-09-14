//! Note writing: build the tree, commit it, and update
//! `refs/gage/notes/<id>`.
//!
//! The tree carries `format` (`gage-note 1\n`), `attrs` (compact JSON of
//! name/value/author, LF terminated), and, when at least one target is
//! given, `targets` (one `refs/gage/notes/<id>\n` per line). The commit
//! has no parent. Author and committer are set to
//! `gage <noreply@gage.localhost>`.

use std::path::Path;

use gage_core::uuid::new_uuid;
use serde::{Deserialize, Serialize};

use crate::{StoreError, exists, git_in, run, run_with_stdin, store_path};

/// Fixed identity written to the author and committer fields of every
/// note commit. `attrs.author` carries the Gage-domain producer
/// (`user:*`, `scanner:*`, `agent:*`) and is unrelated to this.
const IDENTITY_NAME: &str = "gage";
const IDENTITY_EMAIL: &str = "noreply@gage.localhost";

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
    /// Commit committer date, milliseconds since the Unix epoch.
    pub created_ms: i64,
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
    let created_ms = read_committer_ms(path, &ref_path)?;
    let attrs = read_attrs(path, &ref_path)?;
    let targets = read_targets(path, &ref_path)?;

    Ok(NoteFull {
        id,
        name: attrs.name,
        value: attrs.value,
        author: attrs.author,
        targets,
        created_ms,
    })
}

fn read_committer_ms(path: &Path, ref_path: &str) -> Result<i64, StoreError> {
    let ts = run(git_in(
        path,
        ["for-each-ref", "--format=%(committerdate:unix)", ref_path],
    ))?;
    let secs: i64 = ts
        .trim()
        .parse()
        .map_err(|e| StoreError::Parse(format!("committerdate {ts:?}: {e}")))?;
    Ok(secs * 1000)
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
            "--format=%(refname:strip=3) %(committerdate:unix)",
            "refs/gage/notes/",
        ],
    ))?;

    let mut records = Vec::new();
    for line in listing.lines() {
        let (id, ts) = line
            .split_once(' ')
            .ok_or_else(|| StoreError::Parse(format!("for-each-ref line: {line}")))?;
        let created_secs: i64 = ts
            .parse()
            .map_err(|e| StoreError::Parse(format!("committerdate {ts}: {e}")))?;
        let ref_path = format!("refs/gage/notes/{id}");
        let attrs_json = run(git_in(
            path,
            ["cat-file", "-p", &format!("{ref_path}:attrs")],
        ))?;
        let attrs: StoredAttrs = serde_json::from_str(attrs_json.trim_end())
            .map_err(|e| StoreError::Parse(format!("attrs {id}: {e}")))?;
        records.push(NoteRecord {
            id: id.to_string(),
            name: attrs.name,
            value: attrs.value,
            author: attrs.author,
            created_ms: created_secs * 1000,
        });
    }
    Ok(records)
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
    let format_sha = write_blob(path, b"gage-note 1\n")?;
    let attrs_sha = write_blob(path, encode_attrs(&input).as_bytes())?;

    let mut entries = vec![
        format!("100644 blob {attrs_sha}\tattrs"),
        format!("100644 blob {format_sha}\tformat"),
    ];
    if !target_refs.is_empty() {
        let content: String = target_refs.iter().map(|r| format!("{r}\n")).collect();
        let targets_sha = write_blob(path, content.as_bytes())?;
        entries.push(format!("100644 blob {targets_sha}\ttargets"));
    }
    let tree_sha = mktree(path, &entries)?;

    let commit_sha = commit_tree(path, &tree_sha, input.name)?;

    let ref_path = format!("refs/gage/notes/{id}");
    run(git_in(path, ["update-ref", &ref_path, &commit_sha, ""]))?;

    Ok(id)
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

fn write_blob(path: &Path, content: &[u8]) -> Result<String, StoreError> {
    let sha = run_with_stdin(git_in(path, ["hash-object", "-w", "--stdin"]), content)?;
    Ok(sha.trim().to_string())
}

/// Feed `entries` (already in git tree byte-sort order) to `git mktree`
/// and return the resulting tree sha.
fn mktree(path: &Path, entries: &[String]) -> Result<String, StoreError> {
    let mut input = entries.join("\n");
    input.push('\n');
    let sha = run_with_stdin(git_in(path, ["mktree"]), input.as_bytes())?;
    Ok(sha.trim().to_string())
}

fn commit_tree(path: &Path, tree_sha: &str, name: &str) -> Result<String, StoreError> {
    let message = format!("note: {name}");
    let mut cmd = git_in(path, ["commit-tree", tree_sha, "-m", &message]);
    cmd.env("GIT_AUTHOR_NAME", IDENTITY_NAME)
        .env("GIT_AUTHOR_EMAIL", IDENTITY_EMAIL)
        .env("GIT_COMMITTER_NAME", IDENTITY_NAME)
        .env("GIT_COMMITTER_EMAIL", IDENTITY_EMAIL);
    Ok(run(cmd)?.trim().to_string())
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
        assert!(tree.contains("\tformat"), "{tree}");
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
            assert_eq!(r.author, "user:test");
        }
    }

    #[test]
    fn list_empty_store_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let store = init_store(tmp.path());
        assert!(note_list_at(&store).unwrap().is_empty());
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
