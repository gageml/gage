use clap::{Args, Subcommand};
use gage_claude::project::shorten_home_path;
use gage_store::{
    DatasetRecord, DatasetStore, EntryKind, InitOutcome, NoteInput, NoteRecord, NoteStore,
    SessionOutcome, SessionSpec, Store, StoreStatus, TreeEntry,
};
use tabled::{
    Table,
    settings::{
        Color, Style, Width,
        object::{Cell, Columns, Object, Rows},
        peaker::PriorityMax,
    },
};

use crate::author::resolve_author;
use crate::human::{format_elapsed_ms, format_size};
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

    /// Manage store notes
    Note {
        #[command(subcommand)]
        command: NoteCommand,
    },

    /// Manage store datasets
    Dataset {
        #[command(subcommand)]
        command: DatasetCommand,
    },

    /// Browse the store's object graph
    ///
    /// Opens an interactive view of `refs/gage/object/*`: refs listing
    /// on the left, per-commit detail (header, parents classified as
    /// `parent` vs link, tree, resolved link files, and the `parent`
    /// chain) on the right. Payload agnostic --- object type names are
    /// shown but no `attrs.json` is interpreted.
    View,
}

#[derive(Subcommand)]
pub enum DatasetCommand {
    /// List datasets
    List,

    /// Manage dataset sessions
    Session {
        #[command(subcommand)]
        command: DatasetSessionCommand,
    },
}

#[derive(Subcommand)]
pub enum DatasetSessionCommand {
    /// List sessions in a dataset
    List(DatasetSessionListArgs),

    /// Show a session's entry lines
    Show(DatasetSessionShowArgs),

    /// Add a session to a dataset
    Add(DatasetSessionAddArgs),
}

#[derive(Args)]
pub struct DatasetSessionListArgs {
    /// Dataset id or unique prefix
    dataset: String,
}

#[derive(Args)]
pub struct DatasetSessionShowArgs {
    /// Dataset id or unique prefix
    dataset: String,

    /// Session number (decimal `<n>`) or the native session id
    session: String,
}

#[derive(Args)]
pub struct DatasetSessionAddArgs {
    /// Dataset id or unique prefix
    dataset: String,

    /// One or more session source specs, e.g. `claude:<session_id>`
    #[arg(required = true)]
    spec: Vec<String>,
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

#[derive(Subcommand)]
pub enum NoteCommand {
    /// List notes
    List,

    /// Show a note
    Show(NoteShowArgs),

    /// Create a note
    New(NoteNewArgs),

    /// Edit a note's value
    Edit(NoteEditArgs),

    /// Delete a note
    Delete(NoteDeleteArgs),
}

#[derive(Args)]
pub struct NoteEditArgs {
    /// Note id or unique prefix
    id: String,

    /// Replacement value
    value: String,
}

#[derive(Args)]
pub struct NoteDeleteArgs {
    /// Note id or unique prefix
    id: String,
}

#[derive(Args)]
pub struct NoteShowArgs {
    /// Note id or unique prefix
    id: String,
}

#[derive(Args)]
pub struct NoteNewArgs {
    /// Note name
    name: String,

    /// Note value
    value: String,

    /// Target note in the form `note:<id>` (repeatable)
    #[arg(short, long)]
    target: Vec<String>,

    /// Author username (default: $USER)
    #[arg(short, long)]
    user: Option<String>,
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
        StoreCommand::Note { command } => {
            let notes = NoteStore::from(&store);
            match command {
                NoteCommand::List => note_list(&notes),
                NoteCommand::Show(args) => note_show(&notes, args),
                NoteCommand::New(args) => note_new(&notes, args),
                NoteCommand::Edit(args) => note_edit(&notes, args),
                NoteCommand::Delete(args) => note_delete(&notes, args),
            }
        }
        StoreCommand::Dataset { command } => {
            let datasets = DatasetStore::from(&store);
            match command {
                DatasetCommand::List => dataset_list(&datasets),
                DatasetCommand::Session { command } => match command {
                    DatasetSessionCommand::List(args) => dataset_session_list(&datasets, args),
                    DatasetSessionCommand::Show(args) => dataset_session_show(&datasets, args),
                    DatasetSessionCommand::Add(args) => dataset_session_add(&datasets, args),
                },
            }
        }
        StoreCommand::View => view(store),
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

fn view(store: Store) {
    if let Err(e) = gage_tui::store_view::run(store) {
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

fn dataset_list(datasets: &DatasetStore) {
    let records: Vec<DatasetRecord> = match datasets.iter().and_then(|it| it.collect()) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("gage store dataset list: {e}");
            std::process::exit(1);
        }
    };
    if records.is_empty() {
        println!("No datasets found");
        return;
    }

    let header: Vec<String> = ["Id", "Created"].iter().map(|s| s.to_string()).collect();
    let rows: Vec<Vec<String>> = records.iter().map(dataset_row).collect();

    let table = Table::from_iter(std::iter::once(header).chain(rows))
        .with(Style::rounded())
        .modify(Rows::first(), style::tty(Color::FG_BRIGHT_YELLOW))
        .modify(
            Columns::one(0).not(Rows::first()),
            style::tty(Color::FG_YELLOW),
        )
        .modify(Columns::one(1).not(Rows::first()), style::dim())
        .to_string();
    println!("{table}");
}

fn dataset_row(r: &DatasetRecord) -> Vec<String> {
    vec![r.id.clone(), format_elapsed_ms(r.created_ms)]
}

fn dataset_session_list(datasets: &DatasetStore, args: DatasetSessionListArgs) {
    let dataset_id = match datasets.resolve_id(&args.dataset) {
        Ok(id) => id,
        Err(e) => {
            eprintln!("gage store dataset session list: {e}");
            std::process::exit(1);
        }
    };
    let sessions = match datasets.sessions_list(&dataset_id) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("gage store dataset session list: {e}");
            std::process::exit(1);
        }
    };
    if sessions.is_empty() {
        println!("No sessions found");
        return;
    }
    let header: Vec<String> = ["Num", "Type", "Session Id", "Size"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let rows: Vec<Vec<String>> = sessions
        .iter()
        .map(|s| {
            vec![
                s.session_num.to_string(),
                s.session_type.clone(),
                s.native_id.clone(),
                s.size.map(|b| format_size(b as i64)).unwrap_or_default(),
            ]
        })
        .collect();
    let table = Table::from_iter(std::iter::once(header).chain(rows))
        .with(Style::rounded())
        .modify(Rows::first(), style::tty(Color::FG_BRIGHT_YELLOW))
        .modify(
            Columns::one(0).not(Rows::first()),
            style::tty(Color::FG_YELLOW),
        )
        .modify(Columns::one(3).not(Rows::first()), style::dim())
        .to_string();
    println!("{table}");
}

fn dataset_session_show(datasets: &DatasetStore, args: DatasetSessionShowArgs) {
    let dataset_id = match datasets.resolve_id(&args.dataset) {
        Ok(id) => id,
        Err(e) => {
            eprintln!("gage store dataset session show: {e}");
            std::process::exit(1);
        }
    };
    let meta = match datasets.session_meta(&dataset_id, &args.session) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("gage store dataset session show: {e}");
            std::process::exit(1);
        }
    };
    let registry = gage_registry::driver::DriverRegistry::new()
        .add(std::sync::Arc::new(gage_claude::driver::ClaudeDriver::new()));
    let driver = match registry.for_scheme(&meta.driver_name) {
        Some(d) => d,
        None => {
            eprintln!(
                "gage store dataset session show: unknown driver {:?}",
                meta.driver_name
            );
            std::process::exit(1);
        }
    };
    let source = match datasets.session_content(&dataset_id, meta.session_num) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("gage store dataset session show: {e}");
            std::process::exit(1);
        }
    };
    let mut session = match driver.read_stored(meta.native_id, &meta.content_format, source) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("gage store dataset session show: {e}");
            std::process::exit(1);
        }
    };
    for entry in session.entries() {
        match entry {
            Ok(e) => println!("{}", e.raw()),
            Err(e) => {
                eprintln!("gage store dataset session show: {e}");
                std::process::exit(1);
            }
        }
    }
}

fn dataset_session_add(datasets: &DatasetStore, args: DatasetSessionAddArgs) {
    let dataset_id = match datasets.resolve_id(&args.dataset) {
        Ok(id) => id,
        Err(e) => {
            eprintln!("gage store dataset session add: {e}");
            std::process::exit(1);
        }
    };

    let registry = gage_registry::driver::DriverRegistry::new()
        .add(std::sync::Arc::new(gage_claude::driver::ClaudeDriver::new()));

    // Resolve every spec into a reader before touching the store, so any
    // driver failure short-circuits without a partial write.
    struct Resolved {
        driver: std::sync::Arc<dyn gage_session::Driver>,
        reader: Box<dyn gage_session::NativeSession>,
    }
    let mut resolved: Vec<Resolved> = Vec::with_capacity(args.spec.len());
    for spec in &args.spec {
        let (scheme, id) = match spec.split_once(':') {
            Some((s, i)) if !s.is_empty() && !i.is_empty() => (s, i),
            _ => {
                eprintln!(
                    "gage store dataset session add: invalid spec {spec:?}: expected <driver>:<session_id>"
                );
                std::process::exit(1);
            }
        };
        let driver = match registry.for_scheme(scheme) {
            Some(d) => d,
            None => {
                eprintln!("gage store dataset session add: unknown driver {scheme:?}");
                std::process::exit(1);
            }
        };
        let reader = match driver.open_source("").and_then(|s| s.open_native(id)) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("gage store dataset session add: {spec}: {e}");
                std::process::exit(1);
            }
        };
        resolved.push(Resolved { driver, reader });
    }

    let specs: Vec<SessionSpec<'_>> = resolved
        .iter_mut()
        .map(|r| SessionSpec {
            driver: r.driver.as_ref(),
            session: r.reader.as_mut(),
        })
        .collect();

    let outcomes = match datasets.sessions_add(&dataset_id, specs) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("gage store dataset session add: {e}");
            std::process::exit(1);
        }
    };

    for outcome in &outcomes {
        let verb = match outcome.outcome {
            SessionOutcome::Added => "Added",
            SessionOutcome::Updated => "Updated",
            SessionOutcome::Unchanged => "Unchanged",
        };
        println!("{verb} session {}", outcome.session_num);
    }
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

fn note_list(notes: &NoteStore) {
    let records: Vec<NoteRecord> = match notes.iter().and_then(|it| it.collect()) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("gage store note list: {e}");
            std::process::exit(1);
        }
    };
    if records.is_empty() {
        println!("No notes found");
        return;
    }

    let header: Vec<String> = ["Id", "Name", "Value", "Author", "Created"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let rows: Vec<Vec<String>> = records.iter().map(note_row).collect();

    let term_width = console::Term::stdout().size().1 as usize;
    let table = Table::from_iter(std::iter::once(header).chain(rows))
        .with(Style::rounded())
        .with(
            Width::truncate(term_width)
                .suffix("…")
                .priority(PriorityMax::left()),
        )
        .modify(Rows::first(), style::tty(Color::FG_BRIGHT_YELLOW))
        .modify(
            Columns::one(0).not(Rows::first()),
            style::tty(Color::FG_YELLOW),
        )
        .modify(
            Columns::one(2).not(Rows::first()),
            style::tty(Color::FG_BRIGHT_CYAN),
        )
        .modify(Columns::new(3..5).not(Rows::first()), style::dim())
        .to_string();
    println!("{table}");
}

fn note_row(r: &NoteRecord) -> Vec<String> {
    vec![
        r.id.clone(),
        r.name.clone(),
        format_value_cell(&r.value),
        r.author.clone(),
        format_elapsed_ms(r.created_ms),
    ]
}

/// Collapse newlines and truncate long values to a table-friendly cell.
/// Mirrors `cmd_note::format_value_cell` for the plain-string values the
/// store currently produces.
fn format_value_cell(value: &str) -> String {
    let flattened: String = value
        .split(['\n', '\r'])
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if flattened.len() > 400 {
        let mut end = 400;
        while !flattened.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &flattened[..end])
    } else {
        flattened
    }
}

fn note_show(notes: &NoteStore, args: NoteShowArgs) {
    let note = match notes.get(&args.id) {
        Ok(n) => n,
        Err(e) => {
            eprintln!("gage store note show: {e}");
            std::process::exit(1);
        }
    };

    let attrs: Vec<(&str, String)> = vec![
        ("id", note.id),
        ("name", note.name),
        ("value", note.value),
        ("author", note.author),
        ("target", note.targets.join("\n")),
        (
            "created",
            gage_core::datetime::ms_to_iso8601(note.created_ms),
        ),
        (
            "modified",
            gage_core::datetime::ms_to_iso8601(note.modified_ms),
        ),
    ];

    let label_width = attrs.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
    let (_, term_width) = console::Term::stdout().size();
    // Borders + padding: "│ " + " │ " + " │" = 8 chars
    let value_width = (term_width as usize)
        .saturating_sub(label_width + 8)
        .max(20);

    let id_row_idx = attrs.iter().position(|(k, _)| *k == "id").unwrap();
    let value_row_idx = attrs.iter().position(|(k, _)| *k == "value").unwrap();
    let rows: Vec<Vec<String>> = attrs
        .into_iter()
        .map(|(k, v)| {
            let value = if k == "target" {
                v
            } else {
                textwrap::fill(&v, value_width)
            };
            vec![k.to_string(), value]
        })
        .collect();

    let table = Table::from_iter(rows)
        .with(Style::rounded())
        .modify(Columns::first(), style::tty(Color::FG_BRIGHT_YELLOW))
        .modify(Cell::new(id_row_idx, 1), style::tty(Color::FG_YELLOW))
        .modify(
            Cell::new(value_row_idx, 1),
            style::tty(Color::FG_BRIGHT_CYAN),
        )
        .to_string();
    println!("{table}");
}

fn note_delete(notes: &NoteStore, args: NoteDeleteArgs) {
    let id = match notes.delete(&args.id) {
        Ok(id) => id,
        Err(e) => {
            eprintln!("gage store note delete: {e}");
            std::process::exit(1);
        }
    };
    println!("{id}");
}

fn note_edit(notes: &NoteStore, args: NoteEditArgs) {
    let id = match notes.edit(&args.id, &args.value) {
        Ok(id) => id,
        Err(e) => {
            eprintln!("gage store note edit: {e}");
            std::process::exit(1);
        }
    };
    println!("{id}");
}

fn note_new(notes: &NoteStore, args: NoteNewArgs) {
    let author = resolve_author(args.user);
    let id = match notes.create(NoteInput {
        name: &args.name,
        value: &args.value,
        author: &author,
        targets: &args.target,
    }) {
        Ok(id) => id,
        Err(e) => {
            eprintln!("gage store note new: {e}");
            std::process::exit(1);
        }
    };
    println!("{id}");
}
