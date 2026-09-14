use clap::{Args, Subcommand};
use gage_claude::project::shorten_home_path;
use gage_store::{InitOutcome, NoteInput, NoteRecord, StoreStatus};
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
    Status,

    /// Garbage collect the store
    ///
    /// Runs `git gc` in the store. Use `--prune <EXPIRE>` to control the
    /// pruning window; the value is passed to `git gc --prune=<EXPIRE>`.
    /// Common values are `now` (delete all unreachable objects
    /// immediately) and `2.weeks.ago` (the git default).
    Gc(GcArgs),

    /// Manage store notes
    Note {
        #[command(subcommand)]
        command: NoteCommand,
    },
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
        StoreCommand::Status => status(),
        StoreCommand::Gc(args) => gc(args),
        StoreCommand::Note { command } => match command {
            NoteCommand::List => note_list(),
            NoteCommand::Show(args) => note_show(args),
            NoteCommand::Add(args) => note_add(args),
            NoteCommand::Edit(args) => note_edit(args),
            NoteCommand::Delete(args) => note_delete(args),
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

fn status() {
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

    let header: Vec<String> = ["Id", "Name", "Value", "Author", "Modified"]
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
        format_elapsed_ms(r.modified_ms),
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
