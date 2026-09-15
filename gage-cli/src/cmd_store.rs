use clap::{Args, Subcommand};
use gage_claude::project::shorten_home_path;
use gage_store::{
    DatasetRecord, EntryKind, InitOutcome, NoteInput, NoteRecord, StoreStatus, TreeEntry,
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
    /// ref path (`refs/gage/notes/<id>`), an object sha, or
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
}

#[derive(Subcommand)]
pub enum DatasetCommand {
    /// List datasets
    List,

    /// Add an empty dataset
    Add,

    /// Manage dataset sessions
    Session {
        #[command(subcommand)]
        command: DatasetSessionCommand,
    },
}

#[derive(Subcommand)]
pub enum DatasetSessionCommand {
    /// Add a session to a dataset
    Add(DatasetSessionAddArgs),
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
    /// Run `git fsck --full` after showing status
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

    /// Add a note
    Add(NoteAddArgs),

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
pub struct NoteAddArgs {
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
    match command {
        StoreCommand::Init => init(),
        StoreCommand::Status(args) => status(args),
        StoreCommand::Gc(args) => gc(args),
        StoreCommand::Ls(args) => ls(args),
        StoreCommand::Cat(args) => cat(args),
        StoreCommand::Note { command } => match command {
            NoteCommand::List => note_list(),
            NoteCommand::Show(args) => note_show(args),
            NoteCommand::Add(args) => note_add(args),
            NoteCommand::Edit(args) => note_edit(args),
            NoteCommand::Delete(args) => note_delete(args),
        },
        StoreCommand::Dataset { command } => match command {
            DatasetCommand::List => dataset_list(),
            DatasetCommand::Add => dataset_add(),
            DatasetCommand::Session { command } => match command {
                DatasetSessionCommand::Add(args) => dataset_session_add(args),
            },
        },
    }
}

fn init() {
    let outcome = match gage_store::init() {
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
    println!(
        "{verb} Gage store in {}/",
        gage_store::store_path().display()
    );
}

fn status(args: StatusArgs) {
    if args.check {
        match gage_store::fsck() {
            Ok(()) => println!("Integrity: OK"),
            Err(e) => {
                eprintln!("Integrity: FAIL ({e})");
                std::process::exit(1);
            }
        }
        return;
    }
    let status = match gage_store::status() {
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
    [
        ("path", shorten_home_path(&status.path)),
        (
            "objects",
            (status.loose_objects + status.packed_objects).to_string(),
        ),
        ("loose", status.loose_objects.to_string()),
        ("packs", status.packs.to_string()),
        ("size", format_size(status.size as i64)),
        ("refs", status.refs.to_string()),
        ("refs/gage/notes", status.note_refs.to_string()),
        ("remotes", remotes),
    ]
    .into_iter()
    .map(|(k, v)| vec![k.to_string(), v])
    .collect()
}

fn gc(args: GcArgs) {
    let outcome = match gage_store::gc(args.prune.as_deref()) {
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

fn dataset_add() {
    let id = match gage_store::dataset_add() {
        Ok(id) => id,
        Err(e) => {
            eprintln!("gage store dataset add: {e}");
            std::process::exit(1);
        }
    };
    println!("{id}");
}

fn dataset_list() {
    let records = match gage_store::dataset_list() {
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

fn dataset_session_add(args: DatasetSessionAddArgs) {
    let dataset_id = match gage_store::dataset_resolve_id(&args.dataset) {
        Ok(id) => id,
        Err(e) => {
            eprintln!("gage store dataset session add: {e}");
            std::process::exit(1);
        }
    };

    let mut registry = gage_registry::driver::DriverRegistry::new();
    registry.register(std::sync::Arc::new(gage_claude::driver::ClaudeDriver::new()));

    // Resolve every spec into a reader before touching the store, so any
    // driver failure short-circuits without a partial write.
    struct Resolved {
        driver_name: &'static str,
        driver_version: &'static str,
        reader: Box<dyn gage_session::SessionReader>,
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
        let driver = match registry.get(scheme) {
            Some(d) => d,
            None => {
                let known = registry.schemes().join(", ");
                eprintln!(
                    "gage store dataset session add: unknown driver {scheme:?} (known: {known})"
                );
                std::process::exit(1);
            }
        };
        let reader = match driver.resolve(id) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("gage store dataset session add: {spec}: {e}");
                std::process::exit(1);
            }
        };
        resolved.push(Resolved {
            driver_name: driver.name(),
            driver_version: driver.version(),
            reader,
        });
    }

    let specs: Vec<gage_store::SessionSpec<'_>> = resolved
        .iter_mut()
        .map(|r| gage_store::SessionSpec {
            driver_name: r.driver_name,
            driver_version: r.driver_version,
            reader: r.reader.as_mut(),
        })
        .collect();

    let outcomes = match gage_store::dataset_sessions_add(&dataset_id, specs) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("gage store dataset session add: {e}");
            std::process::exit(1);
        }
    };

    for outcome in &outcomes {
        let verb = match outcome.outcome {
            gage_store::SessionOutcome::Added => "Added",
            gage_store::SessionOutcome::Updated => "Updated",
            gage_store::SessionOutcome::NoOp => "No changes",
        };
        println!("{verb} sessions/{}", outcome.session_num);
    }
}

fn ls(args: LsArgs) {
    let entries = match gage_store::ls(&args.reference) {
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

fn cat(args: CatArgs) {
    if let Err(e) = gage_store::cat(&args.reference) {
        eprintln!("gage store cat: {e}");
        std::process::exit(1);
    }
}

fn note_list() {
    let records = match gage_store::note_list() {
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

fn note_show(args: NoteShowArgs) {
    let note = match gage_store::note_get(&args.id) {
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
        ("targets", note.targets.join("\n")),
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
            let value = if k == "targets" {
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

fn note_delete(args: NoteDeleteArgs) {
    let id = match gage_store::note_delete(&args.id) {
        Ok(id) => id,
        Err(e) => {
            eprintln!("gage store note delete: {e}");
            std::process::exit(1);
        }
    };
    println!("{id}");
}

fn note_edit(args: NoteEditArgs) {
    let id = match gage_store::note_edit(&args.id, &args.value) {
        Ok(id) => id,
        Err(e) => {
            eprintln!("gage store note edit: {e}");
            std::process::exit(1);
        }
    };
    println!("{id}");
}

fn note_add(args: NoteAddArgs) {
    let author = resolve_author(args.user);
    let id = match gage_store::note_add(NoteInput {
        name: &args.name,
        value: &args.value,
        author: &author,
        targets: &args.target,
    }) {
        Ok(id) => id,
        Err(e) => {
            eprintln!("gage store note add: {e}");
            std::process::exit(1);
        }
    };
    println!("{id}");
}
