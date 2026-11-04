//! The layout of the cache root, `~/.gage/cache` by default: the
//! derived data kept for every store and every native session source,
//! all of it rebuildable from its system of record.
//!
//! One directory per corpus, under a directory naming what the corpus
//! is: `store/<digest>` for a Gage store, written by the store and
//! query crates, and `driver/<name>/<digest>` for a native source of
//! the driver `<name>`, written by that driver. The digest is of the
//! corpus's source URL with its path canonicalized, so two spellings
//! of one location share a directory and two locations never do.
//! Each corpus directory opens with `corpus.json` naming the source,
//! so a listing reads one file per corpus. The categories inside are
//! `objects.db` for a store's object index, `summary/` for a native
//! source's per-session attributes, and `text/` for the full-text
//! index of either. `query/` beside the corpus directories holds
//! paged query results and belongs to no corpus.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use twox_hash::XxHash3_64;

/// The directory under the cache root holding store corpora
pub const STORE_DIR: &str = "store";

/// The directory under the cache root holding native corpora, one
/// subdirectory per driver name
pub const DRIVER_DIR: &str = "driver";

/// The directory under the cache root holding paged query results
pub const QUERY_DIR: &str = "query";

/// The file at the top of a corpus directory naming its source
pub const CORPUS_FILE: &str = "corpus.json";

/// The URL scheme of a store source and of a stored session URL
pub const STORE_SCHEME: &str = "gage";

/// The corpus directory of the store at `store`.
pub fn store_corpus_dir(cache_root: &Path, store: &Path) -> PathBuf {
    cache_root
        .join(STORE_DIR)
        .join(corpus_digest(STORE_SCHEME, store))
}

/// The corpus directory of the native source at `source` read by the
/// driver `driver`.
pub fn driver_corpus_dir(cache_root: &Path, driver: &str, source: &Path) -> PathBuf {
    cache_root
        .join(DRIVER_DIR)
        .join(driver)
        .join(corpus_digest(driver, source))
}

/// The source URL of the store at `store`: `gage:<canonical path>`.
pub fn store_source_url(store: &Path) -> String {
    format!("{STORE_SCHEME}:{}", canonical(store).display())
}

/// The digest naming the corpus directory of `scheme:<path>`: XxHash3
/// over the URL with the path canonicalized, as 16 hex characters. A
/// digest keys distinct locations to distinct directories, which a
/// readable encoding of the path cannot, since the characters a
/// directory name cannot carry are the ones that tell `a-b` from
/// `a/b`.
pub fn corpus_digest(scheme: &str, path: &Path) -> String {
    let mut input = Vec::new();
    input.extend_from_slice(scheme.as_bytes());
    input.push(b':');
    input.extend_from_slice(canonical(path).as_os_str().as_encoded_bytes());
    format!("{:016x}", XxHash3_64::oneshot(&input))
}

/// The canonical form of `path`, or `path` itself when it cannot be
/// canonicalized, typically because it does not exist yet.
fn canonical(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// The record at the top of a corpus directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Corpus {
    /// The source URL the corpus derives from
    pub source: String,
}

/// Create the corpus directory `dir` if absent and write `corpus.json`
/// naming `source` if absent. A present record is left as written.
pub fn ensure_corpus_dir(dir: &Path, source: &str) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    let file = dir.join(CORPUS_FILE);
    if file.exists() {
        return Ok(());
    }
    let corpus = Corpus {
        source: source.to_string(),
    };
    let text = serde_json::to_string_pretty(&corpus).map_err(io::Error::other)?;
    fs::write(file, text + "\n")
}

/// The record of the corpus directory `dir`.
pub fn read_corpus(dir: &Path) -> io::Result<Corpus> {
    let text = fs::read_to_string(dir.join(CORPUS_FILE))?;
    serde_json::from_str(&text).map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_is_a_fixed_width_hex_string_and_deterministic() {
        let digest = corpus_digest("gage", Path::new("/no/such/store.git"));
        assert_eq!(digest.len(), 16);
        assert!(
            digest
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
        assert_eq!(
            digest,
            corpus_digest("gage", Path::new("/no/such/store.git"))
        );
    }

    #[test]
    fn locations_that_differ_only_in_punctuation_get_distinct_digests() {
        let digests = [
            corpus_digest("gage", Path::new("/no/such/dir.git")),
            corpus_digest("gage", Path::new("/no/such/dir-git")),
            corpus_digest("gage", Path::new("/no/such/dir/git")),
            corpus_digest("gage", Path::new("/no/such_dir.git")),
            corpus_digest("claude", Path::new("/no/such/dir.git")),
        ];
        for (i, a) in digests.iter().enumerate() {
            for b in &digests[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }

    #[test]
    fn digest_follows_canonicalization() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("corpus");
        fs::create_dir(&dir).unwrap();
        let via_parent = tmp.path().join("corpus/../corpus");
        assert_eq!(
            corpus_digest("gage", &dir),
            corpus_digest("gage", &via_parent)
        );
        assert_eq!(
            store_corpus_dir(tmp.path(), &dir),
            store_corpus_dir(tmp.path(), &via_parent)
        );
    }

    #[test]
    fn corpus_dirs_are_typed_by_what_they_derive_from() {
        let root = Path::new("/cache");
        let store = store_corpus_dir(root, Path::new("/s/store.git"));
        assert!(store.starts_with("/cache/store/"), "{}", store.display());
        let claude = driver_corpus_dir(root, "claude", Path::new("/home/u/.claude"));
        assert!(
            claude.starts_with("/cache/driver/claude/"),
            "{}",
            claude.display()
        );
    }

    #[test]
    fn ensure_corpus_dir_writes_the_record_once() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("store").join("abc");
        ensure_corpus_dir(&dir, "gage:/s/store.git").unwrap();
        assert_eq!(
            read_corpus(&dir).unwrap(),
            Corpus {
                source: "gage:/s/store.git".to_string()
            }
        );
        ensure_corpus_dir(&dir, "gage:/other").unwrap();
        assert_eq!(read_corpus(&dir).unwrap().source, "gage:/s/store.git");
    }
}
