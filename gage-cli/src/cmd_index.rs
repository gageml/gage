use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Context;
use clap::Args;
use gage_core::cache::{DRIVER_DIR, QUERY_DIR, STORE_DIR, read_corpus};
use gage_core::config::default_cache_root;
use gage_index::LockMode;
use gage_query::text::{StoreTextIndex, StoreTextOutcome, StoreTextStatus};
use gage_session::MaintenanceStatus;
use gage_store::OBJECTS_FILE;
use gage_store::Store;
use tabled::{
    Table,
    settings::{
        Color, Style,
        object::{Columns, Rows},
    },
};

use crate::dialog;
use crate::error::CliError;
use crate::human::format_size;
use crate::progress;
use crate::source;
use crate::style;

#[derive(Args)]
pub struct IndexArgs {
    /// Session source whose index to maintain
    #[arg(short, long, value_name = "SOURCE", long_help = format!("Session source whose index to maintain\n\n{}", source::SOURCE_HELP))]
    source: Option<String>,

    /// Maintain index of stored sessions
    #[arg(short = 'S', long, conflicts_with = "source")]
    stored: bool,

    /// Rebuild index
    #[arg(long)]
    rebuild: bool,

    /// Show index status
    #[arg(long)]
    status: bool,

    /// List every index under cache directory
    ///
    /// List every index under cache directory. Each row names a
    /// corpus as --remove takes it, the indexes it holds, the source
    /// it was built from, whether that source is still present, and
    /// its size. An entry from an earlier cache layout is listed as
    /// legacy
    #[arg(long, conflicts_with_all = ["source", "stored", "rebuild", "status", "remove"])]
    list: bool,

    /// Remove indexes of a corpus
    ///
    /// Remove indexes of a corpus. CORPUS is a corpus path --list
    /// shows, as store/1a2b3c4d5e6f7a8b; its whole directory is
    /// removed, whether or not its source is still present
    #[arg(long, value_name = "CORPUS", conflicts_with_all = ["source", "stored", "rebuild", "status"])]
    remove: Option<String>,
}

/// Maintain the derived indexes: run a source's or the store's
/// reconcile pass and exit, report its standing, list every index, or
/// remove a corpus's indexes. The pass keeps the first-build cost out
/// of interactive queries.
pub async fn run(args: IndexArgs) -> Result<(), CliError> {
    if args.list {
        return list_indexes(&default_cache_root());
    }
    if let Some(corpus) = &args.remove {
        return remove_corpus(&default_cache_root(), corpus);
    }
    if args.stored {
        return run_stored(args).await;
    }
    let source = source::open_source(args.source.as_deref().unwrap_or(""), "gage index")?;

    if args.status {
        let status = tokio::task::spawn_blocking(move || source.maintenance_status())
            .await
            .unwrap()
            .context("gage index")?;
        print_status(&status);
        return Ok(());
    }

    let rebuild = args.rebuild;
    let result = progress::busy_async(
        "Checking index",
        tokio::task::spawn_blocking(move || source.reconcile(rebuild)),
    )
    .await
    .unwrap();

    let outcome = result.context("gage index")?;
    println!(
        "{} sessions ({}): {} indexed, {} removed",
        outcome.discovered,
        format_elapsed(outcome.elapsed_ms),
        outcome.prepared,
        outcome.removed,
    );
    Ok(())
}

/// The store's text index: status, rebuild, or reconcile.
async fn run_stored(args: IndexArgs) -> Result<(), CliError> {
    let store = Store::open(&gage_store::store_path()).context("gage index")?;
    let index = Arc::new(StoreTextIndex::new(Arc::new(Mutex::new(store)), None));

    if args.status {
        let status = tokio::task::spawn_blocking(move || index.status())
            .await
            .unwrap()
            .context("gage index")?;
        print_stored_status(&status);
        return Ok(());
    }

    let rebuild = args.rebuild;
    let result = progress::busy_async(
        "Checking index",
        tokio::task::spawn_blocking(move || {
            if rebuild {
                index.rebuild()
            } else {
                index.reconcile(LockMode::Wait)
            }
        }),
    )
    .await
    .unwrap();

    let outcome: StoreTextOutcome = result.context("gage index")?;
    println!(
        "{} sessions ({}): {} indexed, {} removed",
        outcome.discovered,
        format_elapsed(outcome.elapsed_ms),
        outcome.indexed,
        outcome.removed,
    );
    Ok(())
}

fn format_elapsed(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        let secs = ms / 1000;
        format!("{}m{:02}s", secs / 60, secs % 60)
    }
}

fn print_status(s: &MaintenanceStatus) {
    let dir = match &s.state_dir {
        Some(dir) => format!("{} ({})", dir.display(), format_size(s.state_bytes as i64)),
        None => dialog::NONE.to_string(),
    };
    let text = match &s.text_dir {
        Some(dir) => format!("{} ({})", dir.display(), format_size(s.text_bytes as i64)),
        None => dialog::NONE.to_string(),
    };
    let rows = [
        [
            "sessions".to_string(),
            format!(
                "{} discovered, {} indexed, {} stale",
                s.discovered, s.prepared, s.stale
            ),
        ],
        [
            "last reconcile".to_string(),
            format_reconcile_time(s.last_reconcile_ms),
        ],
        ["cache dir".to_string(), dir],
        ["text index".to_string(), text],
    ];
    print_rows(rows);
}

fn print_stored_status(s: &StoreTextStatus) {
    let rows = [
        [
            "sessions".to_string(),
            format!(
                "{} discovered, {} indexed, {} stale",
                s.discovered, s.indexed, s.stale
            ),
        ],
        [
            "last reconcile".to_string(),
            format_reconcile_time(s.last_reconcile_ms),
        ],
        [
            "text index".to_string(),
            format!("{} ({})", s.dir.display(), format_size(s.bytes as i64)),
        ],
    ];
    print_rows(rows);
}

fn format_reconcile_time(ms: Option<i64>) -> String {
    match ms.and_then(chrono::DateTime::from_timestamp_millis) {
        Some(dt) => dt.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
        None => "never".to_string(),
    }
}

fn print_rows<const N: usize>(rows: [[String; 2]; N]) {
    let table = Table::from_iter(rows)
        .with(Style::rounded().horizontals([]))
        .modify(Columns::first(), style::dim())
        .to_string();
    println!("{table}");
}

/// One listed entry of the cache root: a corpus directory, or an
/// entry of an earlier layout.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct IndexRow {
    /// The path under the cache root, as --remove takes it
    corpus: String,
    /// The index categories present, or `legacy`
    indexes: String,
    source: String,
    present: String,
    bytes: u64,
}

/// Every index under `cache_root`, one row per corpus and one per
/// legacy entry.
fn list_indexes(cache_root: &Path) -> Result<(), CliError> {
    let rows = index_rows(cache_root)?;
    if rows.is_empty() {
        println!("No indexes found");
        return Ok(());
    }
    let header = ["Corpus", "Indexes", "Source", "Present", "Size"].map(String::from);
    let cells = rows.into_iter().map(|r| {
        [
            r.corpus,
            r.indexes,
            r.source,
            r.present,
            format_size(r.bytes as i64),
        ]
    });
    let table = Table::from_iter(std::iter::once(header).chain(cells))
        .with(Style::rounded())
        .modify(Rows::first(), style::tty(Color::FG_BRIGHT_YELLOW))
        .to_string();
    println!("{table}");
    Ok(())
}

/// The rows of `list_indexes`, sorted by corpus path. A corpus
/// directory is `store/<digest>` or `driver/<name>/<digest>`; any
/// other entry beside them, other than the query page cache, is from
/// an earlier layout.
fn index_rows(cache_root: &Path) -> Result<Vec<IndexRow>, CliError> {
    let mut rows = Vec::new();
    for entry in read_dir_or_empty(cache_root)? {
        let (name, path) = entry;
        if name == QUERY_DIR {
            continue;
        }
        if name == STORE_DIR {
            for (digest, dir) in read_dir_or_empty(&path)? {
                rows.push(corpus_row(format!("{STORE_DIR}/{digest}"), &dir)?);
            }
        } else if name == DRIVER_DIR {
            for (driver, driver_dir) in read_dir_or_empty(&path)? {
                for (digest, dir) in read_dir_or_empty(&driver_dir)? {
                    rows.push(corpus_row(format!("{DRIVER_DIR}/{driver}/{digest}"), &dir)?);
                }
            }
        } else {
            rows.push(IndexRow {
                corpus: name,
                indexes: "legacy".to_string(),
                source: dialog::NONE.to_string(),
                present: dialog::NONE.to_string(),
                bytes: tree_size(&path)?,
            });
        }
    }
    rows.sort();
    Ok(rows)
}

/// The entries of `dir` by name, or none when it does not exist.
fn read_dir_or_empty(dir: &Path) -> Result<Vec<(String, PathBuf)>, CliError> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(anyhow::Error::from(e)
                .context(format!("gage index: reading {}", dir.display()))
                .into());
        }
    };
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry.context("gage index")?;
        out.push((
            entry.file_name().to_string_lossy().into_owned(),
            entry.path(),
        ));
    }
    Ok(out)
}

/// The row of the corpus directory `dir`, listed as `corpus`.
fn corpus_row(corpus: String, dir: &Path) -> Result<IndexRow, CliError> {
    let mut indexes = Vec::new();
    if dir.join(OBJECTS_FILE).is_file() {
        indexes.push("objects");
    }
    for category in ["summary", "text"] {
        if dir.join(category).is_dir() {
            indexes.push(category);
        }
    }
    let source = match read_corpus(dir) {
        Ok(corpus) => Some(corpus.source),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => {
            tracing::warn!(dir = %dir.display(), "unreadable corpus record: {e}");
            None
        }
    };
    let present = match &source {
        Some(source) => {
            if source_path(source).is_dir() {
                "yes"
            } else {
                "no"
            }
        }
        None => "unknown",
    };
    Ok(IndexRow {
        corpus,
        indexes: if indexes.is_empty() {
            dialog::NONE.to_string()
        } else {
            indexes.join(", ")
        },
        source: source.unwrap_or_else(|| "(no record)".to_string()),
        present: present.to_string(),
        bytes: tree_size(dir)?,
    })
}

/// The bytes under `path`: the file's size, or the sum over a
/// directory tree.
fn tree_size(path: &Path) -> Result<u64, CliError> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("gage index: {}", path.display()))?;
    if !metadata.is_dir() {
        return Ok(metadata.len());
    }
    let mut total = 0;
    for (_, child) in read_dir_or_empty(path)? {
        total += tree_size(&child)?;
    }
    Ok(total)
}

/// The directory a corpus source names: the path itself, or the
/// body of a `scheme:path` URL.
fn source_path(source: &str) -> PathBuf {
    match source.split_once(':') {
        Some((scheme, body))
            if !scheme.is_empty() && scheme.chars().all(|c| c.is_ascii_alphanumeric()) =>
        {
            PathBuf::from(body)
        }
        _ => PathBuf::from(source),
    }
}

/// Remove the corpus directory, or legacy entry, at `corpus` under
/// the cache root. The argument is a path --list shows: a corpus is
/// `store/<digest>` or `driver/<name>/<digest>`, and a legacy entry
/// is a single name beside those directories.
fn remove_corpus(cache_root: &Path, corpus: &str) -> Result<(), CliError> {
    let not_a_corpus = || CliError::message(format!("gage index: --remove {corpus}: not a corpus"));
    let parts: Vec<&str> = corpus.split('/').collect();
    if parts
        .iter()
        .any(|p| p.is_empty() || *p == "." || *p == "..")
    {
        return Err(not_a_corpus());
    }
    let shape_ok = match parts.as_slice() {
        [STORE_DIR, _] => true,
        [DRIVER_DIR, _, _] => true,
        [name] => *name != STORE_DIR && *name != DRIVER_DIR && *name != QUERY_DIR,
        _ => false,
    };
    if !shape_ok {
        return Err(not_a_corpus());
    }
    let path = cache_root.join(corpus);
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(CliError::message(format!(
                "gage index: no index found for corpus {corpus}"
            )));
        }
        Err(e) => {
            return Err(anyhow::Error::from(e)
                .context(format!("gage index: {}", path.display()))
                .into());
        }
    };
    let removed = if metadata.is_dir() {
        std::fs::remove_dir_all(&path)
    } else {
        std::fs::remove_file(&path)
    };
    removed.with_context(|| format!("gage index: removing {}", path.display()))?;
    println!("Removed {corpus}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_path_reads_a_bare_path_or_a_url_body() {
        assert_eq!(
            source_path("/home/a/.claude/projects"),
            PathBuf::from("/home/a/.claude/projects")
        );
        assert_eq!(
            source_path("claude:/home/a/.claude"),
            PathBuf::from("/home/a/.claude")
        );
        assert_eq!(
            source_path("gage:/home/a/.gage/store.git"),
            PathBuf::from("/home/a/.gage/store.git")
        );
    }

    /// A cache root with one store corpus whose source is `source`,
    /// one driver corpus, the query page cache, and one legacy entry.
    fn cache_root(source: &Path) -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        let store = root.path().join("store").join("1111111111111111");
        std::fs::create_dir_all(store.join("text")).unwrap();
        std::fs::write(store.join(OBJECTS_FILE), b"12345").unwrap();
        gage_core::cache::ensure_corpus_dir(&store, &format!("gage:{}", source.display())).unwrap();
        let driver = root
            .path()
            .join("driver")
            .join("claude")
            .join("2222222222222222");
        std::fs::create_dir_all(driver.join("summary")).unwrap();
        gage_core::cache::ensure_corpus_dir(&driver, "claude:/no/such/dir").unwrap();
        std::fs::create_dir_all(root.path().join("query")).unwrap();
        std::fs::write(root.path().join("object-index.sqlite"), b"old").unwrap();
        root
    }

    #[test]
    fn rows_list_each_corpus_with_its_indexes_and_legacy_entries() {
        let source = tempfile::tempdir().unwrap();
        let root = cache_root(source.path());
        let rows = match index_rows(root.path()) {
            Ok(rows) => rows,
            Err(CliError::Other(e)) => panic!("{e:#}"),
            Err(CliError::Exit { message, .. }) => panic!("{message:?}"),
            Err(CliError::Usage(e)) => panic!("{e}"),
        };
        let summary: Vec<(&str, &str, &str)> = rows
            .iter()
            .map(|r| (r.corpus.as_str(), r.indexes.as_str(), r.present.as_str()))
            .collect();
        assert_eq!(
            summary,
            vec![
                ("driver/claude/2222222222222222", "summary", "no"),
                ("object-index.sqlite", "legacy", dialog::NONE),
                ("store/1111111111111111", "objects, text", "yes"),
            ]
        );
        assert_eq!(rows[1].bytes, 3);
        assert!(rows[2].source.starts_with("gage:"), "{}", rows[2].source);
    }

    #[test]
    fn remove_corpus_takes_a_listed_path_and_rejects_others() {
        let source = tempfile::tempdir().unwrap();
        let root = cache_root(source.path());
        let ok = |corpus: &str| match remove_corpus(root.path(), corpus) {
            Ok(()) => {}
            Err(CliError::Other(e)) => panic!("{e:#}"),
            Err(CliError::Exit { message, .. }) => panic!("{message:?}"),
            Err(CliError::Usage(e)) => panic!("{e}"),
        };
        ok("store/1111111111111111");
        assert!(!root.path().join("store/1111111111111111").exists());
        ok("driver/claude/2222222222222222");
        assert!(!root.path().join("driver/claude/2222222222222222").exists());
        ok("object-index.sqlite");
        assert!(!root.path().join("object-index.sqlite").exists());

        assert!(remove_corpus(root.path(), "store/1111111111111111").is_err());
        assert!(remove_corpus(root.path(), "store").is_err());
        assert!(remove_corpus(root.path(), "driver/claude").is_err());
        assert!(remove_corpus(root.path(), "query").is_err());
        assert!(remove_corpus(root.path(), "../x").is_err());
        assert!(remove_corpus(root.path(), "store/../query").is_err());
        assert!(remove_corpus(root.path(), "").is_err());
        assert!(root.path().join("query").is_dir());
    }
}
