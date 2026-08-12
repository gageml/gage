use clap::{Args, Subcommand};
use cliclack as cli;
use gage_claude::session::one_session;
use gage_core::text_resolve::TextResolver;
use gage_core::uuid::short_uuid;
use gage_db::db;
use gage_db::note::{self, Note, NoteFilters};
use gage_db::target::{NoteTarget, SessionTarget};
use gage_registry::scanner::ScannerRegistry;
use gage_registry::scheme::{ErrorScheme, ScannerScheme};
use tabled::{
    Table,
    settings::{
        Color, Style, Width,
        object::{Cell, Columns, Object, Rows},
        peaker::PriorityMax,
    },
};

use crate::author::resolve_author;
use crate::dialog::{self, DialogError};
use crate::style;

#[derive(Subcommand)]
pub enum NoteCommand {
    /// List notes
    List(NoteListArgs),

    /// Add a note
    Add(NoteAddArgs),

    /// Show a note
    Show(NoteShowArgs),

    /// Edit a note
    Edit(NoteEditArgs),

    /// Delete notes
    Delete(NoteDeleteArgs),
}

#[derive(Args)]
pub struct NoteListArgs {
    #[command(flatten)]
    limit: crate::limit::LimitArgs,

    /// Filter by target session ID (or prefix)
    #[arg(long)]
    session: Option<String>,

    /// Filter by note name
    #[arg(long)]
    name: Option<String>,
}

#[derive(Args)]
pub struct NoteAddArgs {
    /// Target session with optional line
    ///
    /// Use full session ID. Append ':LINE' to specify a session line number.
    #[arg(short, long)]
    target: Option<String>,

    /// Note name
    ///
    /// Names must be unique for a target and author. Defaults to
    /// "comment.<random>"
    #[arg(short, long)]
    name: Option<String>,

    /// Note value (prompted if omitted)
    #[arg(short, long)]
    value: Option<String>,

    /// Author username (default: $USER)
    #[arg(short, long)]
    user: Option<String>,
}

#[derive(Args)]
pub struct NoteShowArgs {
    /// Note ID (or prefix)
    id: String,

    /// Show target content
    #[arg(short = 't', long = "target")]
    short_target: bool,

    /// Show note docs
    #[arg(short, long)]
    doc: bool,
}

#[derive(Args)]
pub struct NoteEditArgs {
    /// Note ID (or prefix)
    id: String,

    /// New value (prompted if omitted)
    #[arg(short, long)]
    value: Option<String>,
}

#[derive(Args)]
pub struct NoteDeleteArgs {
    /// Note IDs (or prefix)
    ids: Vec<String>,

    /// Skip confirmation prompt
    #[arg(short, long)]
    yes: bool,
}

pub fn list(args: NoteListArgs) {
    let conn = db::open_db().unwrap();
    let session = match args.session {
        Some(prefix) => match one_session(&prefix) {
            Ok(s) => Some(s.id),
            Err(e) => {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        },
        None => None,
    };
    let filter_only = NoteFilters {
        session,
        name: args.name,
        ..Default::default()
    };
    let total = match note::count_matching(&conn, &filter_only) {
        Ok(n) => n as usize,
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    };
    if total == 0 {
        println!("No notes found");
        return;
    }

    let show = args.limit.show_count(total);
    let notes = match note::find(
        &conn,
        &NoteFilters {
            limit: Some(show as u32),
            ..filter_only
        },
    ) {
        Ok(n) => n,
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    };

    let highlighter = style::IdHighlighter::new(match note::all_ids(&conn) {
        Ok(ids) => ids,
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    });

    let header: Vec<String> = ["Id", "Name", "Value", "Target", "Created"]
        .iter()
        .map(|s| s.to_string())
        .collect();

    let rows: Vec<Vec<String>> = notes
        .iter()
        .map(|n| {
            vec![
                highlighter.short(&n.id),
                n.name.clone(),
                format_value_cell(&n.value),
                shorten_target(&n.target),
                crate::human::format_elapsed_ms(n.created),
            ]
        })
        .collect();

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
            Columns::one(2).not(Rows::first()),
            style::tty(Color::FG_BRIGHT_CYAN),
        )
        .modify(Columns::new(3..5).not(Rows::first()), style::dim())
        .to_string();
    println!("{table}");

    args.limit.print_summary(show, total, "note");
}

pub fn add(args: NoteAddArgs) {
    dialog::run("Add note", || {
        let target_input = match args.target {
            Some(ref t) => t.clone(),
            None => cli::input("Target")
                .placeholder("session-id or session-id:line")
                .interact()?,
        };
        let target = resolve_target(&target_input)
            .map_err(|e| DialogError::Other(anyhow::anyhow!("{e}")))?;

        let name: String = match args.name {
            Some(ref n) => n.clone(),
            None => cli::input("Name")
                .default_input("comment")
                .placeholder("e.g. summary, tag, comment")
                .interact()?,
        };

        let value: String = match args.value {
            Some(ref v) => v.clone(),
            None => cli::input("Value").placeholder("note content").interact()?,
        };

        let author = resolve_author(args.user);
        let note = Note::new(target, &name, parse_note_value(&value), &author);
        let conn = db::open_db().unwrap();
        note::insert(&conn, &note)
            .map_err(|e| DialogError::Other(anyhow::Error::msg(e.to_string())))?;

        cli::log::remark(format!("id: {}", note.id))?;
        Ok("Note added".into())
    });
}

pub async fn show(args: NoteShowArgs) {
    let conn = db::open_db().unwrap();
    let note = match note::get(&conn, &args.id) {
        Ok(n) => n,
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    };

    let explanation_display = resolve_display(&note, note.explanation.as_deref());

    let mut attrs = vec![
        ("id", note.id.clone()),
        ("name", note.name.clone()),
        ("value", format_value(&note.value)),
        ("target", note.target.to_uri()),
        ("explanation", explanation_display),
        ("author", note.author.clone()),
        ("created", gage_core::datetime::ms_to_iso8601(note.created)),
        (
            "modified",
            note.modified
                .map(gage_core::datetime::ms_to_iso8601)
                .unwrap_or_default(),
        ),
        (
            "metadata",
            note.metadata
                .as_deref()
                .map(pretty_json)
                .unwrap_or_default(),
        ),
    ];

    if args.doc {
        let registry = ScannerRegistry::load();
        let doc = registry
            .note_doc(&note.name)
            .unwrap_or_else(|| "(no scanner declares this note)".to_string());
        attrs.push(("doc", doc));
    }

    let label_width = attrs.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
    let (_, term_width) = console::Term::stdout().size();
    // Borders + padding: "│ " + " │ " + " │" = 8 chars
    let value_width = (term_width as usize)
        .saturating_sub(label_width + 8)
        .max(20);

    let target_cell = if args.short_target {
        let ctx = gage_query::create_context_default().await;
        match crate::target_content::render_target_cell(&ctx, &note.target, value_width).await {
            Ok(s) => Some(s),
            Err(e) => {
                eprintln!("Error rendering target content: {e}");
                std::process::exit(1);
            }
        }
    } else {
        None
    };

    let rows: Vec<Vec<String>> = attrs
        .into_iter()
        .map(|(k, v)| {
            let value = if k == "target" {
                if let Some(ref cell) = target_cell {
                    cell.clone()
                } else {
                    textwrap::fill(&v, value_width)
                }
            } else if k == "doc" {
                crate::markdown::render(&v, value_width)
            } else if k == "metadata" {
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
        .modify(Cell::new(2, 1), style::tty(Color::FG_BRIGHT_CYAN))
        .to_string();
    println!("{table}");
}

fn note_text_resolver(note: &Note) -> TextResolver {
    let registry = ScannerRegistry::load();
    let r = TextResolver::new();
    match note.author.strip_prefix("scanner:") {
        Some(name) => match ScannerScheme::with_scanner_name(&registry, name) {
            Ok(s) => r.with_scheme("scanner", s),
            Err(e) => r.with_scheme("scanner", ErrorScheme::new(e.to_string())),
        },
        None => r.with_scheme("scanner", ScannerScheme::absolute_only()),
    }
}

fn resolve_display(note: &Note, value: Option<&str>) -> String {
    let Some(raw) = value else {
        return String::new();
    };
    let resolver = note_text_resolver(note);
    match resolver.resolve(raw.to_string()) {
        Ok(text) => text,
        Err(e) => format!("(unresolved {raw}: {e})"),
    }
}

pub(crate) fn format_value(value: &note::NoteValue) -> String {
    match &value.0 {
        serde_json::Value::String(s) => s.clone(),
        _ => value.to_json(),
    }
}

/// Pretty-print a raw JSON string (2-space indent). Falls back to the
/// raw text if the string does not parse as JSON.
fn pretty_json(raw: &str) -> String {
    serde_json::from_str::<serde_json::Value>(raw)
        .and_then(|v| serde_json::to_string_pretty(&v))
        .unwrap_or_else(|_| raw.to_string())
}

/// One-line display form of a note value: bare strings unquoted,
/// other JSON compact, flattened and truncated for table cells.
pub(crate) fn format_value_cell(value: &note::NoteValue) -> String {
    let raw = format_value(value);
    let flattened: String = raw
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

/// Interpret CLI value input as JSON, falling back to a plain string.
/// `true`, `42`, `{"k":1}` parse as their JSON types; a bare word like
/// `comment` isn't valid JSON, so it's stored as a JSON string.
fn parse_note_value(input: &str) -> note::NoteValue {
    match serde_json::from_str::<serde_json::Value>(input) {
        Ok(v) => note::NoteValue(v),
        Err(_) => note::NoteValue::from(input),
    }
}

pub fn edit(args: NoteEditArgs) {
    dialog::run("Edit note", || {
        let conn = db::open_db().unwrap();
        let note = note::get(&conn, &args.id)
            .map_err(|e| DialogError::Other(anyhow::Error::msg(e.to_string())))?;

        let default_input = note.value.to_json();
        let value: String = match args.value {
            Some(ref v) => v.clone(),
            None => cli::input("Value")
                .default_input(&default_input)
                .placeholder("new value")
                .interact()?,
        };

        let modified = gage_core::datetime::now_ms();
        let note_value = parse_note_value(&value);
        note::update(&conn, &note.id, &note_value, modified)
            .map_err(|e| DialogError::Other(anyhow::Error::msg(e.to_string())))?;

        cli::log::remark(format!("id: {}", note.id))?;
        Ok("Note updated".into())
    });
}

pub fn delete(args: NoteDeleteArgs) {
    if args.ids.is_empty() {
        eprintln!(
            "gage note delete: provide one or more note IDs\n\n\
             Use 'gage note list' to show notes"
        );
        std::process::exit(1);
    }

    let conn = db::open_db().unwrap();

    let mut notes: Vec<Note> = Vec::new();
    let mut errors = 0;
    for prefix in &args.ids {
        match note::get(&conn, prefix) {
            Ok(n) => notes.push(n),
            Err(e) => {
                eprintln!("{e}");
                errors += 1;
            }
        }
    }
    if errors > 0 {
        std::process::exit(1);
    }

    let count = notes.len();

    dialog::run("Delete notes", || {
        let plural = if count == 1 { "note" } else { "notes" };
        cli::log::remark(format!("{count} {plural}"))?;

        if !args.yes {
            let prompt = format!("Permanently delete {count} {plural}? This cannot be undone.");
            let confirmed = cli::confirm(prompt).initial_value(false).interact()?;
            if !confirmed {
                return Err(DialogError::Canceled);
            }
        }

        let mut deleted = 0;
        for note in &notes {
            if let Err(e) = note::delete(&conn, &note.id) {
                eprintln!("warning: failed to delete {}: {e}", short_uuid(&note.id));
            } else {
                deleted += 1;
            }
        }

        let plural = if deleted == 1 { "note" } else { "notes" };
        Ok(format!("Deleted {deleted} {plural}").into())
    });
}

fn resolve_target(input: &str) -> Result<NoteTarget, String> {
    let (prefix, rest) = match input.split_once(':') {
        Some((p, r)) => (p, Some(r)),
        None => (input, None),
    };
    let session = one_session(prefix).map_err(|e| e.to_string())?;
    let resolved = match rest {
        Some(r) => format!("{}:{r}", session.id),
        None => session.id,
    };
    SessionTarget::parse(&resolved)
        .map(NoteTarget::Session)
        .map_err(|e| e.to_string())
}

/// Glyph-prefixed short display form of a note target: ids reduced to
/// their 8-char short form. Shared with the scan view's notes table.
pub(crate) fn shorten_target(target: &NoteTarget) -> String {
    let (glyph, s) = match target {
        NoteTarget::Session(t) => ("▪", t.to_uri()),
        NoteTarget::Scan(t) => ("≡", short_uuid(&t.scan_id).to_string()),
        NoteTarget::Project(t) => ("⊡", t.project_path.clone()),
    };
    // Session uris open with a 36-char uuid, optionally followed by a
    // line ref; keep the short id plus the suffix
    let shortened = if s.len() >= 36 {
        format!("{}{}", &s[..8], &s[36..])
    } else {
        s
    };
    format!("{glyph} {shortened}")
}
