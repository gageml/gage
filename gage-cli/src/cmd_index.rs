use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Context;
use clap::Args;
use cliclack as cli;
use gage_core::config::default_cache_root;
use gage_index::LockMode;
use gage_query::text::{StoreTextIndex, StoreTextOutcome, StoreTextStatus};
use gage_session::MaintenanceStatus;
use gage_store::Store;
use tabled::{
    Table,
    settings::{
        Color, Style,
        object::{Columns, Rows},
    },
};

use crate::error::CliError;
use crate::human::format_size;
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
    /// corpus as --remove takes it, the index kind, the source it was
    /// built from, and whether that source is still present
    #[arg(long, conflicts_with_all = ["source", "stored", "rebuild", "status", "remove"])]
    list: bool,

    /// Remove indexes of a corpus
    ///
    /// Remove indexes of a corpus. CORPUS is a name --list shows; every
    /// index kind of that corpus is removed, whether or not its source
    /// is still present
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
    let bar = cli::progress_bar(0);
    bar.start("Building index");
    let bar_for_task = bar.clone();
    let result = tokio::task::spawn_blocking(move || {
        source.reconcile(rebuild, &mut |done, total| {
            if done == 0 {
                bar_for_task.set_length(total as u64);
            }
            bar_for_task.set_position(done as u64);
        })
    })
    .await
    .unwrap();
    bar.clear();

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
    let bar = cli::progress_bar(0);
    bar.start("Building index");
    let bar_for_task = bar.clone();
    let result = tokio::task::spawn_blocking(move || {
        let mut on_progress = |done: usize, total: usize| {
            if done == 0 {
                bar_for_task.set_length(total as u64);
            }
            bar_for_task.set_position(done as u64);
        };
        if rebuild {
            index.rebuild(&mut on_progress)
        } else {
            index.reconcile(LockMode::Wait, &mut on_progress)
        }
    })
    .await
    .unwrap();
    bar.clear();

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
        None => "none".to_string(),
    };
    let text = match &s.text_dir {
        Some(dir) => format!("{} ({})", dir.display(), format_size(s.text_bytes as i64)),
        None => "none".to_string(),
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

/// The index categories under the cache root and the manifest field
/// naming each corpus's source
const CATEGORIES: [&str; 2] = ["summary", "text"];

/// Every index under `cache_root`, one row per corpus and category:
/// the corpus name, the category, the source, and whether the source
/// is present.
fn list_indexes(cache_root: &Path) -> Result<(), CliError> {
    let mut rows: Vec<[String; 4]> = Vec::new();
    for category in CATEGORIES {
        let dir = cache_root.join(category);
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(anyhow::Error::from(e).context("gage index").into()),
        };
        for entry in entries {
            let entry = entry.context("gage index")?;
            if !entry.path().is_dir() {
                continue;
            }
            let corpus = entry.file_name().to_string_lossy().into_owned();
            let source = manifest_source(&entry.path().join("manifest.json"));
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
            rows.push([
                corpus,
                category.to_string(),
                source.unwrap_or_else(|| "(no manifest)".to_string()),
                present.to_string(),
            ]);
        }
    }
    if rows.is_empty() {
        println!("No indexes found");
        return Ok(());
    }
    rows.sort();
    let header = ["Corpus", "Index", "Source", "Present"].map(String::from);
    let table = Table::from_iter(std::iter::once(header).chain(rows))
        .with(Style::rounded())
        .modify(Rows::first(), style::tty(Color::FG_BRIGHT_YELLOW))
        .to_string();
    println!("{table}");
    Ok(())
}

/// The `source` field of a manifest, as written: a path for a summary
/// cache, a URL for a text index.
fn manifest_source(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    value.get("source")?.as_str().map(str::to_string)
}

/// The directory a manifest source names: the path itself, or the
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

/// Remove every category's directory of `corpus`.
fn remove_corpus(cache_root: &Path, corpus: &str) -> Result<(), CliError> {
    if corpus.is_empty() || corpus.contains('/') || corpus.contains("..") {
        return Err(CliError::message(format!(
            "gage index: --remove {corpus}: not a corpus name"
        )));
    }
    let mut removed = 0;
    for category in CATEGORIES {
        let dir = cache_root.join(category).join(corpus);
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => removed += 1,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(anyhow::Error::from(e)
                    .context(format!("gage index: removing {}", dir.display()))
                    .into());
            }
        }
    }
    if removed == 0 {
        return Err(CliError::message(format!(
            "gage index: no index found for corpus {corpus}"
        )));
    }
    println!("Removed {removed} index directories of {corpus}");
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

    #[test]
    fn remove_corpus_deletes_every_category_and_rejects_paths() {
        let root = tempfile::tempdir().unwrap();
        for category in CATEGORIES {
            std::fs::create_dir_all(root.path().join(category).join("-c")).unwrap();
        }
        match remove_corpus(root.path(), "-c") {
            Ok(()) => {}
            Err(CliError::Other(e)) => panic!("{e:#}"),
            Err(CliError::Exit { message, .. }) => panic!("{message:?}"),
            Err(CliError::Usage(e)) => panic!("{e}"),
        }
        assert!(!root.path().join("summary").join("-c").exists());
        assert!(!root.path().join("text").join("-c").exists());
        assert!(remove_corpus(root.path(), "-c").is_err());
        assert!(remove_corpus(root.path(), "../x").is_err());
    }
}
