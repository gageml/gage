use clap::{Args, Subcommand};
use gage_claude::project::shorten_home_path;
use gage_store::{EntryKind, InitOutcome, Store, StoreStatus, TreeEntry};
use tabled::{
    Table,
    settings::{
        Color, Style,
        object::{Columns, Object, Rows},
    },
};

use crate::human::format_size;
use crate::style;

#[derive(Subcommand)]
pub enum StoreCommand {
    /// Create the Gage store
    ///
    /// Creates a bare Git repository at `$GAGE_HOME/store.git`. Running
    /// it on an existing store is harmless.
    Init,

    /// Show store status
    Status(StatusArgs),

    /// Garbage collect the store
    ///
    /// Runs `git gc` in the store. Use `--prune <EXPIRE>` to control the
    /// pruning window; the value is passed to `git gc --prune=<EXPIRE>`.
    /// Common values are `now` (delete all unreachable objects
    /// immediately) and `2.weeks.ago` (the git default).
    Gc(GcArgs),

    /// List entries under a tree-ish
    ///
    /// Runs `git ls-tree -l <REF>`. `<REF>` is any git tree-ish: a full
    /// ref path (`refs/gage/object/<id>`), an object sha, or
    /// `<ref>:<path>`.
    Ls(LsArgs),

    /// Dump the content of an object
    ///
    /// Runs `git cat-file -p <REF>`. `<REF>` is any git tree-ish or
    /// object sha.
    Cat(CatArgs),

    /// Browse the store's object graph
    ///
    /// Opens an interactive view of `refs/gage/object/*`: refs listing
    /// on the left, per-commit detail (header, parents classified as
    /// `parent` vs link, tree, resolved link files, and the `parent`
    /// chain) on the right. Payload agnostic --- object type names are
    /// shown but no `attrs.json` is interpreted.
    View(ViewArgs),
}

#[derive(Args)]
pub struct ViewArgs {
    /// Object id prefix to select when the view opens
    #[arg(value_name = "OBJECT")]
    object: Option<String>,
}

#[derive(Args)]
pub struct LsArgs {
    /// Git tree-ish
    reference: String,

    /// Include the object sha column
    #[arg(long)]
    sha: bool,
}

#[derive(Args)]
pub struct CatArgs {
    /// Git object reference
    reference: String,
}

#[derive(Args)]
pub struct StatusArgs {
    /// Run `git fsck --strict` after showing status
    #[arg(long)]
    check: bool,
}

#[derive(Args)]
pub struct GcArgs {
    /// Prune unreachable objects older than this
    #[arg(long, value_name = "EXPIRE")]
    prune: Option<String>,
}

pub fn run(command: StoreCommand) {
    if let StoreCommand::Init = command {
        init();
        return;
    }
    let store = open_store();
    match command {
        StoreCommand::Init => unreachable!("handled above"),
        StoreCommand::Status(args) => status(&store, args),
        StoreCommand::Gc(args) => gc(&store, args),
        StoreCommand::Ls(args) => ls(&store, args),
        StoreCommand::Cat(args) => cat(&store, args),
        StoreCommand::View(args) => view(store, args),
    }
}

/// Open the default store, or exit with the open error.
fn open_store() -> Store {
    match Store::open(&gage_store::store_path()) {
        Ok(store) => store,
        Err(e) => {
            eprintln!("gage store: {e}");
            std::process::exit(1);
        }
    }
}

fn init() {
    let path = gage_store::store_path();
    let outcome = match gage_store::init(&path) {
        Ok(outcome) => outcome,
        Err(e) => {
            eprintln!("gage store init: {e}");
            std::process::exit(1);
        }
    };
    let verb = match outcome {
        InitOutcome::Created => "Initialized empty",
        InitOutcome::Reinitialized => "Reinitialized existing",
    };
    println!("{verb} Gage store in {}/", path.display());
}

fn view(store: Store, args: ViewArgs) {
    if let Err(e) = gage_tui::store_view::run(store, args.object.as_deref()) {
        eprintln!("gage store view: {e}");
        std::process::exit(1);
    }
}

fn status(store: &Store, args: StatusArgs) {
    if args.check {
        match store.fsck() {
            Ok(lines) => {
                for line in lines {
                    println!("{line}");
                }
                println!("Integrity: OK");
            }
            Err(e) => {
                eprintln!("Integrity: FAIL ({e})");
                std::process::exit(1);
            }
        }
        return;
    }
    let status = match store.status() {
        Ok(status) => status,
        Err(e) => {
            eprintln!("gage store status: {e}");
            std::process::exit(1);
        }
    };
    let rows = status_rows(&status);
    let table = Table::from_iter(rows)
        .with(Style::rounded().horizontals([]))
        .modify(Columns::first(), style::dim())
        .to_string();
    println!("{table}");
}

fn status_rows(status: &StoreStatus) -> Vec<Vec<String>> {
    let remotes = status
        .remotes
        .iter()
        .map(|r| format!("{} {}", r.name, r.url))
        .collect::<Vec<_>>()
        .join("\n");
    let mut rows: Vec<Vec<String>> = vec![
        vec!["path".to_string(), shorten_home_path(&status.path)],
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

fn gc(store: &Store, args: GcArgs) {
    let outcome = match store.gc(args.prune.as_deref(), false) {
        Ok(outcome) => outcome,
        Err(e) => {
            eprintln!("gage store gc: {e}");
            std::process::exit(1);
        }
    };
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

fn ls(store: &Store, args: LsArgs) {
    let entries = match store.ls(&args.reference) {
        Ok(entries) => entries,
        Err(e) => {
            eprintln!("gage store ls: {e}");
            std::process::exit(1);
        }
    };
    if entries.is_empty() {
        return;
    }
    let mut header: Vec<String> = vec!["Name".into(), "Type".into(), "Size".into()];
    if args.sha {
        header.push("Sha".into());
    }
    let rows = std::iter::once(header).chain(entries.iter().map(|e| ls_row(e, args.sha)));
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
}

fn ls_row(entry: &TreeEntry, include_sha: bool) -> Vec<String> {
    let size = match entry.size {
        Some(bytes) => format_size(bytes as i64),
        None => "-".to_string(),
    };
    let kind = entry.kind.as_str().to_string();
    let name = match entry.kind {
        EntryKind::Tree => format!("{}/", entry.name),
        _ => entry.name.clone(),
    };
    let mut row = vec![name, kind, size];
    if include_sha {
        row.push(entry.sha.clone());
    }
    row
}

fn cat(store: &Store, args: CatArgs) {
    if let Err(e) = store.cat(&args.reference) {
        eprintln!("gage store cat: {e}");
        std::process::exit(1);
    }
}
