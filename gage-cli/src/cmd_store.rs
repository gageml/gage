use anyhow::Context;
use clap::{Args, Subcommand};
use cliclack as cli;
use gage_core::path::shorten_home;
use gage_core::uuid::short_uuid;
use gage_store::{EntryKind, InitOutcome, Store, StoreError, StoreStatus, TreeEntry};
use tabled::{
    Table,
    settings::{
        Color, Style,
        object::{Columns, Object, Rows},
    },
};

use crate::dialog::{self, DialogError};
use crate::error::CliError;
use crate::human::{count, format_size};
use crate::style;

#[derive(Subcommand)]
pub enum StoreCommand {
    /// Create Gage store
    ///
    /// Creates a bare Git repository at `$GAGE_HOME/store.git`. Running
    /// it on an existing store is harmless.
    Init,

    /// Show store status
    Status(StatusArgs),

    /// Show object index status
    ///
    /// The object index is a cache over the stored objects, kept
    /// current on every open and safe to delete at any time. --rebuild
    /// deletes it and builds it again from the store.
    Index(IndexArgs),

    /// Garbage collect store
    ///
    /// Runs `git gc` in the store. Use `--prune <EXPIRE>` to control the
    /// pruning window; the value is passed to `git gc --prune=<EXPIRE>`.
    /// Common values are `now` (delete all unreachable objects
    /// immediately) and `2.weeks.ago` (the git default).
    Gc(GcArgs),

    /// List files stored for an object
    Ls(LsArgs),

    /// Print a file stored for an object
    ///
    /// Prints the file's bytes as stored. Without FILE, prints the
    /// object's attributes.
    Cat(CatArgs),

    /// Delete objects of any type
    ///
    /// Deletes each object as its own delete command would, without
    /// that command's cleanup: a dataset that links a deleted object
    /// keeps its link, and a dataset's members are left in place. Use
    /// this on an object its own command cannot read.
    Delete(DeleteArgs),

    /// Browse stored objects
    ///
    /// Opens an interactive view: objects on the left, and on the
    /// right the selected object's versions, the objects it links,
    /// and its files.
    View(ViewArgs),
}

#[derive(Args)]
pub struct ViewArgs {
    /// Object to select on open (ID or prefix)
    #[arg(value_name = "OBJECT")]
    object: Option<String>,
}

#[derive(Args)]
pub struct DeleteArgs {
    /// Objects to delete (ID or prefix)
    #[arg(required = true)]
    ids: Vec<String>,

    /// Skip confirmation prompt
    #[arg(short, long)]
    yes: bool,
}

#[derive(Args)]
pub struct LsArgs {
    /// Object to list (tag, ID, or prefix)
    object: String,
}

#[derive(Args)]
pub struct CatArgs {
    /// Object to read (tag, ID, or prefix)
    object: String,

    /// File to print (default attrs.json)
    file: Option<String>,
}

#[derive(Args)]
pub struct IndexArgs {
    /// Delete and rebuild index from store
    #[arg(long)]
    rebuild: bool,
}

#[derive(Args)]
pub struct StatusArgs {
    /// Verify store integrity after showing status
    #[arg(long)]
    check: bool,
}

#[derive(Args)]
pub struct GcArgs {
    /// Prune unreachable objects older than this
    #[arg(long, value_name = "EXPIRE")]
    prune: Option<String>,
}

pub fn run(command: StoreCommand) -> Result<(), CliError> {
    match command {
        StoreCommand::Init => return init(),
        StoreCommand::Index(args) if args.rebuild => return rebuild_index(),
        _ => {}
    }
    let store = open_store()?;
    match command {
        StoreCommand::Init => unreachable!("handled above"),
        StoreCommand::Index(_) => index_status(&store),
        StoreCommand::Status(args) => status(&store, args),
        StoreCommand::Gc(args) => gc(&store, args),
        StoreCommand::Ls(args) => ls(&store, args),
        StoreCommand::Cat(args) => cat(&store, args),
        StoreCommand::Delete(args) => delete(&store, args),
        StoreCommand::View(args) => view(store, args),
    }
}

fn delete(store: &Store, args: DeleteArgs) -> Result<(), CliError> {
    // Resolve every argument before writing anything, so one bad
    // argument leaves the store untouched
    let mut targets: Vec<(String, String)> = Vec::with_capacity(args.ids.len());
    let mut errors = 0;
    for prefix in &args.ids {
        match store
            .resolve_id(prefix)
            .and_then(|(id, sha)| Ok((id, store.read_header(&sha)?)))
        {
            Ok((id, header)) if header.is_tombstone() => {
                eprintln!("gage store delete: {} is already deleted", short_uuid(&id));
                errors += 1;
            }
            Ok((id, header)) => {
                let type_name = header
                    .object_type
                    .strip_prefix("gage::")
                    .unwrap_or(&header.object_type)
                    .to_string();
                if !targets.iter().any(|(t, _)| *t == id) {
                    targets.push((id, type_name));
                }
            }
            Err(e) => {
                eprintln!("gage store delete: {e}");
                errors += 1;
            }
        }
    }
    if errors > 0 {
        return Err(CliError::exit(1));
    }

    let total = targets.len();
    dialog::run("Delete objects", || {
        dialog::step_list(
            "Objects",
            targets
                .iter()
                .map(|(id, type_name)| format!("{type_name} {}", short_uuid(id))),
        )?;
        cli::log::remark("This cannot be undone")?;
        if !args.yes {
            let confirmed = cli::confirm("Continue?").initial_value(false).interact()?;
            if !confirmed {
                return Err(DialogError::Canceled);
            }
        }
        let mut deleted = 0;
        for (id, _) in &targets {
            match store.delete_object(id) {
                Ok(_) => deleted += 1,
                Err(e) => cli::log::warning(format!("{}: {e}", short_uuid(id)))?,
            }
        }
        if deleted < total {
            return Err(dialog::partial("Deleted", deleted, total, "object"));
        }
        Ok(format!("Deleted {}", count(deleted, "object")).into())
    })
}

/// Open the default store, or `gage store: <error>`
fn open_store() -> Result<Store, CliError> {
    let store = Store::open(&gage_store::store_path()).context("gage store")?;
    Ok(store)
}

fn init() -> Result<(), CliError> {
    let path = gage_store::store_path();
    let outcome = gage_store::init(&path).context("gage store init")?;
    let verb = match outcome {
        InitOutcome::Created => "Initialized empty",
        InitOutcome::Reinitialized => "Reinitialized existing",
    };
    println!("{verb} Gage store in {}/", path.display());
    Ok(())
}

/// The rebuild replaces the index file, so it opens the store itself
/// rather than taking the handle `open_store` would have built.
fn rebuild_index() -> Result<(), CliError> {
    let store = Store::rebuild_index(&gage_store::store_path()).context("gage store index")?;
    index_status(&store)
}

fn index_status(store: &Store) -> Result<(), CliError> {
    let status = store.index_status().context("gage store index")?;
    let rows = [
        ["path".to_string(), shorten_home(&status.path)],
        ["refs".to_string(), status.counts.refs.to_string()],
        ["objects".to_string(), status.counts.objects.to_string()],
        ["size".to_string(), format_size(status.bytes as i64)],
    ];
    let table = Table::from_iter(rows)
        .with(Style::rounded().horizontals([]))
        .modify(Columns::first(), style::dim())
        .to_string();
    println!("{table}");
    Ok(())
}

fn view(store: Store, args: ViewArgs) -> Result<(), CliError> {
    // A prefix that names nothing is an error before the terminal is
    // taken over; an ambiguous one opens the view on its first match
    if let Some(prefix) = &args.object {
        match store.resolve_id(prefix) {
            Ok(_) | Err(StoreError::AmbiguousId(..)) => {}
            Err(e) => Err(e).context("gage store view")?,
        }
    }
    gage_tui::store_view::run(store, args.object.as_deref()).context("gage store view")?;
    Ok(())
}

fn status(store: &Store, args: StatusArgs) -> Result<(), CliError> {
    if args.check {
        let lines = store
            .fsck()
            .map_err(|e| CliError::message(format!("Integrity: FAIL ({e})")))?;
        for line in lines {
            println!("{line}");
        }
        println!("Integrity: OK");
        return Ok(());
    }
    let status = store.status().context("gage store status")?;
    let rows = status_rows(&status);
    let table = Table::from_iter(rows)
        .with(Style::rounded().horizontals([]))
        .modify(Columns::first(), style::dim())
        .to_string();
    println!("{table}");
    Ok(())
}

fn status_rows(status: &StoreStatus) -> Vec<Vec<String>> {
    let remotes = status
        .remotes
        .iter()
        .map(|r| {
            let backup = if r.backup { " (backup)" } else { "" };
            format!("{} {}{backup}", r.name, r.url)
        })
        .collect::<Vec<_>>()
        .join("\n");
    let mut rows: Vec<Vec<String>> = vec![
        vec!["path".to_string(), shorten_home(&status.path)],
        vec![
            "objects".to_string(),
            (status.loose_objects + status.packed_objects).to_string(),
        ],
        vec!["loose".to_string(), status.loose_objects.to_string()],
        vec!["packs".to_string(), status.packs.to_string()],
        vec!["size".to_string(), format_size(status.size as i64)],
        vec!["refs".to_string(), status.refs.to_string()],
    ];
    for (prefix, count) in &status.ref_prefixes {
        rows.push(vec![prefix.clone(), count.to_string()]);
    }
    rows.push(vec!["remotes".to_string(), remotes]);
    rows
}

fn gc(store: &Store, args: GcArgs) -> Result<(), CliError> {
    let outcome = store
        .gc(args.prune.as_deref(), false)
        .context("gage store gc")?;
    println!();
    let header: Vec<String> = ["", "Before", "After"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let rows = std::iter::once(header).chain(gc_summary_rows(&outcome));
    let table = Table::from_iter(rows)
        .with(Style::rounded())
        .modify(Rows::first(), style::tty(Color::FG_BRIGHT_YELLOW))
        .modify(Columns::first(), style::dim())
        .to_string();
    println!("{table}");
    if outcome.pruned_commits > 0 {
        println!(
            "Pruned {} unreachable commits from the object index",
            outcome.pruned_commits
        );
    }
    Ok(())
}

fn gc_summary_rows(outcome: &gage_store::GcOutcome) -> Vec<Vec<String>> {
    let b = &outcome.before;
    let a = &outcome.after;
    let objects_b = b.loose_objects + b.packed_objects;
    let objects_a = a.loose_objects + a.packed_objects;
    [
        ("objects", objects_b.to_string(), objects_a.to_string()),
        (
            "loose",
            b.loose_objects.to_string(),
            a.loose_objects.to_string(),
        ),
        (
            "packed",
            b.packed_objects.to_string(),
            a.packed_objects.to_string(),
        ),
        ("packs", b.packs.to_string(), a.packs.to_string()),
        (
            "size",
            format_size(b.size as i64),
            format_size(a.size as i64),
        ),
    ]
    .into_iter()
    .map(|(k, before, after)| vec![k.to_string(), before, after])
    .collect()
}

fn ls(store: &Store, args: LsArgs) -> Result<(), CliError> {
    let tip = resolve_tip(store, "gage store ls", &args.object)?;
    let entries = store.ls(&tip).context("gage store ls")?;
    if entries.is_empty() {
        return Ok(());
    }
    let header: Vec<String> = vec!["Name".into(), "Type".into(), "Size".into()];
    let rows = std::iter::once(header).chain(entries.iter().map(ls_row));
    let table = Table::from_iter(rows)
        .with(Style::rounded())
        .modify(Rows::first(), style::tty(Color::FG_BRIGHT_YELLOW))
        .modify(
            Columns::one(0).not(Rows::first()),
            style::tty(Color::FG_YELLOW),
        )
        .modify(Columns::one(2).not(Rows::first()), style::dim())
        .to_string();
    println!("{table}");
    Ok(())
}

fn ls_row(entry: &TreeEntry) -> Vec<String> {
    let size = match entry.size {
        Some(bytes) => format_size(bytes as i64),
        None => "-".to_string(),
    };
    let kind = entry.kind.as_str().to_string();
    let name = match entry.kind {
        EntryKind::Tree => format!("{}/", entry.name),
        _ => entry.name.clone(),
    };
    vec![name, kind, size]
}

fn cat(store: &Store, args: CatArgs) -> Result<(), CliError> {
    use std::io::Write;

    let tip = resolve_tip(store, "gage store cat", &args.object)?;
    let file = args.file.as_deref().unwrap_or("attrs.json");
    let bytes = match store.read_blob_bytes(&format!("{tip}:{file}")) {
        Ok(bytes) => bytes,
        Err(StoreError::MissingObject(_)) => {
            return Err(CliError::message(format!(
                "gage store cat: {}: no such file: {file}",
                args.object
            )));
        }
        Err(e) => Err(e).context("gage store cat")?,
    };
    std::io::stdout()
        .write_all(&bytes)
        .context("gage store cat")?;
    Ok(())
}

/// The stored version an object name resolves to, or `command: <error>`
fn resolve_tip(store: &Store, command: &str, object: &str) -> Result<String, CliError> {
    let found = store
        .resolve_in(object, None)
        .with_context(|| command.to_string())?;
    Ok(found.tip_sha)
}
