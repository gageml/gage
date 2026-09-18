//! Store administration: creating the bare repository, reading its
//! status, running `fsck`, and running `gc`. These operations touch
//! the store as a Git repository; the Gage-object concepts they do
//! not depend on live in [`crate::object`].

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use gage_core::config::gage_home;

use crate::StoreError;
use crate::git::{git_cmd, git_in, run};

/// Schema version stamped into `gage.version` at init. Bumped when the
/// ref layout or another store-wide convention changes.
pub const STORE_VERSION: u32 = 1;

/// Settings every store carries. Refs only advance under push, and every
/// object received over the wire is verified. See store-init.md.
const STORE_CONFIG: [(&str, &str); 3] = [
    ("receive.denyDeletes", "true"),
    ("receive.denyNonFastForwards", "true"),
    ("transfer.fsckObjects", "true"),
];

/// Path to the store: `<gage_home>/store.git`.
pub fn store_path() -> PathBuf {
    gage_home().join("store.git")
}

/// True when a bare Git repository is present at `path`.
pub(crate) fn exists(path: &Path) -> bool {
    path.join("HEAD").is_file()
}

#[derive(Debug, PartialEq, Eq)]
pub enum InitOutcome {
    Created,
    Reinitialized,
}

/// Creates the store at [`store_path`], or reinitializes it if present.
pub fn init() -> Result<InitOutcome, StoreError> {
    init_at(&store_path())
}

/// Creates a bare repository at `path`, or reinitializes it if present.
///
/// Leading directories are created as needed. The empty template keeps
/// sample hooks and `description` out of the store and ignores any
/// `init.templateDir` in the user's Git config. The initial branch is
/// fixed so `HEAD` does not depend on `init.defaultBranch`.
pub fn init_at(path: &Path) -> Result<InitOutcome, StoreError> {
    let existing = exists(path);
    let mut cmd = git_cmd();
    cmd.args([
        "init",
        "--bare",
        "--quiet",
        "--template=",
        "--initial-branch=main",
    ])
    .arg(path);
    run(cmd)?;
    for (key, value) in STORE_CONFIG {
        run(git_in(path, ["config", key, value]))?;
    }
    run(git_in(
        path,
        ["config", "gage.version", &STORE_VERSION.to_string()],
    ))?;
    Ok(if existing {
        InitOutcome::Reinitialized
    } else {
        InitOutcome::Created
    })
}

/// What Git reports about the store: object and pack counts, disk size,
/// ref count, and configured remotes.
#[derive(Debug, PartialEq, Eq)]
pub struct StoreStatus {
    pub path: PathBuf,
    pub loose_objects: u64,
    pub packed_objects: u64,
    pub packs: u64,
    /// Bytes on disk for loose objects and packs together
    pub size: u64,
    pub refs: u64,
    /// Count of refs under each proper prefix beneath `refs/gage/`,
    /// sorted by prefix. A ref `refs/gage/object/<id>` contributes to
    /// `refs/gage` and `refs/gage/object`; the leaf ref name itself is
    /// not a prefix.
    pub ref_prefixes: Vec<(String, u64)>,
    pub remotes: Vec<Remote>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Remote {
    pub name: String,
    pub url: String,
}

/// Reads the status of the store at [`store_path`].
pub fn status() -> Result<StoreStatus, StoreError> {
    status_at(&store_path())
}

/// Reads the status of the store at `path`. Fails with
/// [`StoreError::NotFound`] when no repository is there.
pub fn status_at(path: &Path) -> Result<StoreStatus, StoreError> {
    if !exists(path) {
        return Err(StoreError::NotFound(path.to_path_buf()));
    }
    let counts = parse_count_objects(&run(git_in(path, ["count-objects", "-v"]))?)?;
    let ref_names = run(git_in(path, ["for-each-ref", "--format=%(refname)"]))?;
    let refs = ref_names.lines().count() as u64;
    let ref_prefixes = compute_ref_prefixes(&ref_names);
    let remotes = parse_remotes(&run(git_in(path, ["remote", "-v"]))?);
    Ok(StoreStatus {
        path: path.to_path_buf(),
        loose_objects: counts.count,
        packed_objects: counts.in_pack,
        packs: counts.packs,
        size: (counts.size + counts.size_pack) * 1024,
        refs,
        ref_prefixes,
        remotes,
    })
}

/// Before-and-after counts for a `gc` run.
#[derive(Debug)]
pub struct GcOutcome {
    pub before: StoreStatus,
    pub after: StoreStatus,
}

/// Run `git fsck --full` on the default store. Output is inherited to
/// the caller's stdout/stderr.
pub fn fsck() -> Result<(), StoreError> {
    fsck_at(&store_path())
}

pub fn fsck_at(path: &Path) -> Result<(), StoreError> {
    if !exists(path) {
        return Err(StoreError::NotFound(path.to_path_buf()));
    }
    let mut cmd = git_in(path, ["fsck", "--full"]);
    let status = cmd.status().map_err(StoreError::Spawn)?;
    if !status.success() {
        return Err(StoreError::Git {
            status,
            stderr: String::new(),
        });
    }
    Ok(())
}

/// Run `git gc` on the default store, optionally with `--prune=<expire>`.
///
/// `git gc`'s output is passed through to the caller's stdout/stderr so
/// progress is visible.
pub fn gc(prune: Option<&str>) -> Result<GcOutcome, StoreError> {
    gc_at(&store_path(), prune)
}

/// Run `git gc` on the store at `path`.
pub fn gc_at(path: &Path, prune: Option<&str>) -> Result<GcOutcome, StoreError> {
    if !exists(path) {
        return Err(StoreError::NotFound(path.to_path_buf()));
    }
    let before = status_at(path)?;
    let mut cmd = git_in(path, ["gc"]);
    let prune_flag: String;
    if let Some(expire) = prune {
        prune_flag = format!("--prune={expire}");
        cmd.arg(&prune_flag);
    }
    let status = cmd.status().map_err(StoreError::Spawn)?;
    if !status.success() {
        return Err(StoreError::Git {
            status,
            stderr: String::new(),
        });
    }
    let after = status_at(path)?;
    Ok(GcOutcome { before, after })
}

/// The `git count-objects -v` fields this module reads. Sizes are in
/// KiB as git reports them.
#[derive(Debug, Default, PartialEq, Eq)]
struct CountObjects {
    count: u64,
    size: u64,
    in_pack: u64,
    packs: u64,
    size_pack: u64,
}

fn parse_count_objects(output: &str) -> Result<CountObjects, StoreError> {
    let mut counts = CountObjects::default();
    for line in output.lines() {
        let Some((key, value)) = line.split_once(": ") else {
            return Err(StoreError::Parse(format!("count-objects line: {line}")));
        };
        let value: u64 = value
            .trim()
            .parse()
            .map_err(|e| StoreError::Parse(format!("count-objects value: {line}: {e}")))?;
        match key {
            "count" => counts.count = value,
            "size" => counts.size = value,
            "in-pack" => counts.in_pack = value,
            "packs" => counts.packs = value,
            "size-pack" => counts.size_pack = value,
            _ => {}
        }
    }
    Ok(counts)
}

/// Counts every proper prefix under `refs/gage/` across the given
/// newline-separated ref names. `refs/gage/object/<id>` contributes to
/// `refs/gage` and `refs/gage/object`; the full ref name is not a
/// prefix. Refs outside `refs/gage/` are ignored.
fn compute_ref_prefixes(ref_names: &str) -> Vec<(String, u64)> {
    let mut counts: BTreeMap<String, u64> = BTreeMap::new();
    for name in ref_names.lines() {
        if !name.starts_with("refs/gage/") {
            continue;
        }
        let segments: Vec<&str> = name.split('/').collect();
        let mut prefix = String::new();
        for (i, seg) in segments.iter().enumerate() {
            if i + 1 == segments.len() {
                break;
            }
            if i > 0 {
                prefix.push('/');
            }
            prefix.push_str(seg);
            if i >= 1 {
                *counts.entry(prefix.clone()).or_insert(0) += 1;
            }
        }
    }
    counts.into_iter().collect()
}

/// Parses `git remote -v`, keeping the fetch line of each remote.
fn parse_remotes(output: &str) -> Vec<Remote> {
    output
        .lines()
        .filter_map(|line| {
            let (name, rest) = line.split_once('\t')?;
            let url = rest.strip_suffix(" (fetch)")?;
            Some(Remote {
                name: name.to_string(),
                url: url.to_string(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_creates_then_reinitializes() {
        let tmp = tempfile::tempdir().unwrap();
        let store = tmp.path().join("home").join("store.git");

        assert_eq!(init_at(&store).unwrap(), InitOutcome::Created);
        assert_eq!(
            std::fs::read_to_string(store.join("HEAD")).unwrap().trim(),
            "ref: refs/heads/main"
        );
        let config = std::fs::read_to_string(store.join("config")).unwrap();
        assert!(config.contains("bare = true"), "{config}");
        assert!(config.contains("denyDeletes = true"), "{config}");
        assert!(config.contains("denyNonFastForwards = true"), "{config}");
        assert!(config.contains("fsckObjects = true"), "{config}");
        assert!(
            config.contains(&format!("version = {STORE_VERSION}")),
            "{config}"
        );
        assert!(!store.join("hooks").exists());

        assert_eq!(init_at(&store).unwrap(), InitOutcome::Reinitialized);
    }

    #[test]
    fn init_reports_git_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("occupied");
        std::fs::write(&file, "").unwrap();

        match init_at(&file.join("store.git")) {
            Err(StoreError::Git { stderr, .. }) => assert!(!stderr.is_empty()),
            other => panic!("expected git failure, got {other:?}"),
        }
    }

    #[test]
    fn status_of_fresh_store() {
        let tmp = tempfile::tempdir().unwrap();
        let store = tmp.path().join("store.git");
        init_at(&store).unwrap();

        let status = status_at(&store).unwrap();
        assert_eq!(
            status,
            StoreStatus {
                path: store,
                loose_objects: 0,
                packed_objects: 0,
                packs: 0,
                size: 0,
                refs: 0,
                ref_prefixes: vec![],
                remotes: vec![],
            }
        );
    }

    #[test]
    fn status_of_missing_store() {
        let tmp = tempfile::tempdir().unwrap();
        match status_at(&tmp.path().join("store.git")) {
            Err(StoreError::NotFound(_)) => {}
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn parses_count_objects() {
        let output = "count: 5141\nsize: 67720\nin-pack: 9110\npacks: 4\nsize-pack: 2839\nprune-packable: 26\ngarbage: 0\nsize-garbage: 0\n";
        assert_eq!(
            parse_count_objects(output).unwrap(),
            CountObjects {
                count: 5141,
                size: 67720,
                in_pack: 9110,
                packs: 4,
                size_pack: 2839,
            }
        );
    }

    #[test]
    fn counts_ref_prefixes() {
        let refs = "\
refs/gage/notes/abc123\n\
refs/gage/datasets/def456\n\
refs/gage/sessions/claude/1/xyz\n\
refs/gage/sessions/claude/1/uvw\n\
refs/gage/sessions/codex/1/qrs\n\
refs/heads/main\n";
        let prefixes = compute_ref_prefixes(refs);
        assert_eq!(
            prefixes,
            vec![
                ("refs/gage".to_string(), 5),
                ("refs/gage/datasets".to_string(), 1),
                ("refs/gage/notes".to_string(), 1),
                ("refs/gage/sessions".to_string(), 3),
                ("refs/gage/sessions/claude".to_string(), 2),
                ("refs/gage/sessions/claude/1".to_string(), 2),
                ("refs/gage/sessions/codex".to_string(), 1),
                ("refs/gage/sessions/codex/1".to_string(), 1),
            ]
        );
    }

    #[test]
    fn ref_prefixes_of_empty_input_is_empty() {
        assert_eq!(compute_ref_prefixes(""), Vec::<(String, u64)>::new());
    }

    #[test]
    fn parses_remotes() {
        let output = "origin\tgit@github.com:x/y.git (fetch)\norigin\tgit@github.com:x/y.git (push)\nbackup\t/tmp/b (fetch)\nbackup\t/tmp/b (push)\n";
        let remotes = parse_remotes(output);
        assert_eq!(remotes.len(), 2);
        assert_eq!(remotes[0].name, "origin");
        assert_eq!(remotes[0].url, "git@github.com:x/y.git");
        assert_eq!(remotes[1].name, "backup");
    }
}
