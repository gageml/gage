use clap::{Args, Subcommand};
use gage_claude::project::shorten_home_path;
use gage_store::{InitOutcome, NoteInput, NoteRecord, StoreStatus};
use tabled::{
    Table,
    settings::{
        Color, Style, Width,
        object::{Columns, Object, Rows},
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

    /// Manage store notes
    Note {
        #[command(subcommand)]
        command: NoteCommand,
    },
}

#[derive(Subcommand)]
pub enum NoteCommand {
    /// List notes
    List,

    /// Add a note
    Add(NoteAddArgs),
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
        StoreCommand::Note { command } => match command {
            NoteCommand::List => note_list(),
            NoteCommand::Add(args) => note_add(args),
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
