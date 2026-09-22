use clap::{Args, Subcommand};
use cliclack as cli;
use console::style;
use gage_core::uuid::short_uuid;
use gage_db::db;
use gage_db::note::{self, Note};
use gage_db::target::NoteTarget;
use gage_registry::scanner::ScannerRegistry;
use gage_store::{NoteInput, NoteRecord, NoteStore, NoteValue, Store, url};
use tabled::{
    Table,
    settings::{
        Color, Style, Width,
        object::{Cell, Columns, Object, Rows},
        peaker::PriorityMax,
    },
};

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
}

#[derive(Args)]
pub struct NoteAddArgs {
    /// Note text (prompted if omitted)
    text: Option<String>,

    /// Note target
    ///
    /// An object ID (or unique prefix), optionally followed by '#' and
    /// a line selection such as '12', '12-20', or '12-20,31'. A line
    /// selection applies to a session and limits the match to
    /// sessions.
    #[arg(short, long)]
    target: Option<String>,

    /// Note name (default: "comment")
    #[arg(short, long)]
    name: Option<String>,

    /// Author username (default: $USER)
    #[arg(short, long)]
    user: Option<String>,

    /// Store the text as JSON
    #[arg(long)]
    json: bool,

    /// Skip prompts
    ///
    /// Requires TEXT; other values take their defaults
    #[arg(short, long)]
    yes: bool,
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
    let store = match Store::open(&gage_store::store_path()) {
        Ok(store) => store,
        Err(e) => {
            eprintln!("gage note list: {e}");
            std::process::exit(1);
        }
    };
    let notes = NoteStore::from(&store);
    let total = match notes.query().count() {
        Ok(n) => n,
        Err(e) => {
            eprintln!("gage note list: {e}");
            std::process::exit(1);
        }
    };
    if total == 0 {
        println!("No notes found");
        return;
    }
    let show = args.limit.show_count(total);
    let records: Vec<NoteRecord> =
        match notes.query().limit(show).iter().and_then(|it| it.collect()) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("gage note list: {e}");
                std::process::exit(1);
            }
        };

    // A prefix resolves against every object ref in the store, not
    // only notes, so the peer set is every object id
    let peers: Vec<String> = match store.list_object_refs() {
        Ok(refs) => refs.into_iter().map(|r| r.id).collect(),
        Err(e) => {
            eprintln!("gage note list: {e}");
            std::process::exit(1);
        }
    };
    let highlighter = style::IdHighlighter::new(peers);

    let header: Vec<String> = ["Id", "Name", "Value", "Target", "Created"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let rows: Vec<Vec<String>> = records
        .iter()
        .map(|n| {
            vec![
                highlighter.short(&n.id),
                n.name.clone(),
                stored_value_cell(&n.value),
                n.target.as_deref().map(target_cell).unwrap_or_default(),
                crate::human::format_elapsed_ms(n.created_ms),
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

    args.limit.print_summary(records.len(), total, "note");
}

/// Target cell: the type name, a space, and the short id with any
/// line selection, e.g. `session 6tyx7fs2#12-20`. A value that is not
/// a Gage URL is shown as stored.
fn target_cell(target: &str) -> String {
    let Ok(parsed) = url::parse(target) else {
        return target.to_string();
    };
    let id = short_uuid(parsed.body);
    match parsed.fragment {
        Some(fragment) => format!("{} {id}#{fragment}", parsed.scheme),
        None => format!("{} {id}", parsed.scheme),
    }
}

/// One-line cell for a stored note value: text flattened to a single
/// line and cut at 400 chars, JSON in its compact form
fn stored_value_cell(value: &NoteValue) -> String {
    let raw = match value {
        NoteValue::Text(text) => text.clone(),
        NoteValue::Json(json) => json.to_string(),
    };
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

pub fn add(args: NoteAddArgs) {
    if args.yes && args.text.is_none() {
        eprintln!("gage note add: --yes requires TEXT");
        std::process::exit(1);
    }
    let store = match Store::open(&gage_store::store_path()) {
        Ok(store) => store,
        Err(e) => {
            eprintln!("gage note add: {e}");
            std::process::exit(1);
        }
    };
    let target = match args.target.as_deref() {
        Some(input) => match resolve_target(&store, input) {
            Ok(url) => Some(url),
            Err(e) => {
                eprintln!("gage note add: {e}");
                std::process::exit(1);
            }
        },
        None => None,
    };

    dialog::run("Add note", || {
        if let Some(url) = &target {
            cli::log::step(format!("Target\n{}", style(url).dim()))?;
        }
        let label = if args.json { "JSON" } else { "Text" };
        let text: String = match args.text {
            Some(ref t) => {
                cli::log::step(format!("{label}\n{}", style(t).dim()))?;
                t.clone()
            }
            None if args.json => cli::input(label)
                .validate(|s: &String| {
                    serde_json::from_str::<serde_json::Value>(s)
                        .map(|_| ())
                        .map_err(|e| format!("not valid JSON: {e}"))
                })
                .interact()?,
            None => cli::input(label).interact()?,
        };
        // Validate the value before any further prompt so a bad
        // argument fails first
        let value = if args.json {
            let json = serde_json::from_str(&text)
                .map_err(|e| DialogError::Failed(format!("text is not valid JSON: {e}")))?;
            NoteValue::Json(json)
        } else {
            NoteValue::Text(text)
        };
        let name: String = match args.name {
            Some(ref n) => n.clone(),
            None if args.yes => "comment".to_string(),
            None => cli::input("Name")
                .default_input("comment")
                .placeholder("comment")
                .interact()?,
        };
        let username: String = match args.user.clone().or_else(env_user) {
            Some(u) => u,
            None if args.yes => {
                return Err(DialogError::Failed(
                    "--yes requires --user when $USER is not set".into(),
                ));
            }
            None => cli::input("User")
                .placeholder("your user name")
                .interact()?,
        };

        if !args.yes {
            let confirmed = cli::confirm("Add this note?")
                .initial_value(true)
                .interact()?;
            if !confirmed {
                return Err(DialogError::Canceled);
            }
        }

        let author = format!("user:{username}");
        let id = NoteStore::from(&store)
            .create(NoteInput {
                name: &name,
                value,
                author: &author,
                target: target.as_deref(),
            })
            .map_err(|e| DialogError::Failed(e.to_string()))?;
        Ok(format!("Note {} added", short_uuid(&id)).into())
    });
}

/// `$USER` when set and non-empty
fn env_user() -> Option<String> {
    std::env::var_os("USER")
        .map(|u| u.to_string_lossy().into_owned())
        .filter(|u| !u.is_empty())
}

/// Resolve a `--target` value, an object id prefix with an optional
/// `#<line selection>`, to a Gage URL with the full id. The prefix
/// must match exactly one object; with a line selection only sessions
/// are candidates. The line selection itself is checked by the store.
fn resolve_target(store: &Store, input: &str) -> Result<String, String> {
    let (prefix, fragment) = match input.split_once('#') {
        Some((p, f)) => (p, Some(f)),
        None => (input, None),
    };
    if prefix.is_empty() {
        return Err(format!("target {input:?}: missing object ID"));
    }
    let refs = store.list_object_refs().map_err(|e| e.to_string())?;
    let mut matches: Vec<(String, String, bool)> = Vec::new();
    for r in refs.into_iter().filter(|r| r.id.starts_with(prefix)) {
        let header = store
            .read_header(&r.tip_sha)
            .map_err(|e| format!("{}: {e}", r.id))?;
        let type_name = header
            .object_type
            .strip_prefix("gage::")
            .unwrap_or(&header.object_type)
            .to_string();
        matches.push((r.id, type_name, header.is_tombstone()));
    }
    if fragment.is_some() {
        matches.retain(|(_, type_name, _)| type_name == "session");
    }
    match matches.as_slice() {
        [] => {
            let what = if fragment.is_some() {
                "session"
            } else {
                "object"
            };
            Err(format!("target {prefix}: no {what} matches"))
        }
        [(id, _, true)] => Err(format!("target {prefix}: object is deleted: {id}")),
        [(id, type_name, false)] => Ok(match fragment {
            Some(f) => format!("{type_name}:{id}#{f}"),
            None => format!("{type_name}:{id}"),
        }),
        many => {
            let mut lines = vec![format!(
                "target {prefix}: ambiguous prefix matches {} objects:",
                many.len()
            )];
            lines.extend(
                many.iter()
                    .map(|(id, type_name, _)| format!("  {type_name} {id}")),
            );
            Err(lines.join("\n"))
        }
    }
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

    let mut attrs = vec![
        ("id", note.id.clone()),
        ("name", note.name.clone()),
        ("value", format_value(&note.value)),
        ("target", note.target.to_uri()),
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
        // Note targets name sessions in the default source until a
        // target carries its own source
        let source = crate::source::open_source_or_exit("gage note show", "");
        let ctx = match gage_query::create_context(source.as_ref()).await {
            Ok(ctx) => ctx,
            Err(e) => {
                eprintln!("gage note show: {e}");
                std::process::exit(1);
            }
        };
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

/// Glyph-prefixed short display form of a note target: ids reduced to
/// their 8-char short form. Shared with the scan view's notes table.
pub(crate) fn target_label(target: &NoteTarget) -> String {
    let (glyph, s) = match target {
        NoteTarget::Session(t) => ("▪", t.to_uri()),
        NoteTarget::Scan(t) => ("≡", short_uuid(&t.scan_id).to_string()),
        NoteTarget::Project(t) => ("⊡", t.to_shortened_path()),
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
