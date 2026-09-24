use std::sync::{Arc, Mutex};

use clap::{Args, Subcommand};
use cliclack as cli;
use console::style;
use datafusion::arrow::array::{Array, Int64Array, StringArray, TimestampMillisecondArray};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::prelude::SessionContext;
use gage_core::uuid::short_uuid;
use gage_db::note;
use gage_db::target::NoteTarget;
use gage_query2::ContextBuilder;
use gage_registry::scanner::ScannerRegistry;
use gage_store::{NOTE_TYPE, NoteEdit, NoteInput, NoteStore, NoteValue, SESSION_TYPE, Store, url};
use tabled::{
    Table,
    settings::{
        Color, Style, Width,
        object::{Cell, Columns, Object, Rows},
        peaker::PriorityMax,
    },
};

use crate::cmd_session::run_query;
use crate::dialog::{self, DialogError};
use crate::style::{self, IdKind, styled_id};

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
    /// Show notes with this name
    #[arg(long)]
    name: Option<String>,

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

    /// Note metadata as a JSON object
    #[arg(short, long, value_name = "JSON")]
    metadata: Option<String>,

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

    /// Show note docs
    #[arg(short, long)]
    doc: bool,
}

#[derive(Args)]
pub struct NoteEditArgs {
    /// Note ID (or prefix)
    id: String,

    /// New note text
    text: Option<String>,

    /// New note name
    #[arg(short, long)]
    name: Option<String>,

    /// New note target
    ///
    /// An object ID (or unique prefix), optionally followed by '#' and
    /// a line selection
    #[arg(short, long)]
    target: Option<String>,

    /// Store the text as JSON
    #[arg(long)]
    json: bool,

    /// Skip prompts
    ///
    /// Requires TEXT, --name, or --target; other values are kept
    #[arg(short, long)]
    yes: bool,
}

#[derive(Args)]
pub struct NoteDeleteArgs {
    /// Note IDs (or prefixes)
    #[arg(required = true)]
    ids: Vec<String>,

    /// Skip confirmation prompt
    #[arg(short, long)]
    yes: bool,
}

pub async fn list(args: NoteListArgs) {
    let store = match Store::open(&gage_store::store_path()) {
        Ok(store) => store,
        Err(e) => {
            eprintln!("gage note list: {e}");
            std::process::exit(1);
        }
    };
    let ctx = ContextBuilder::new(Some(Arc::new(Mutex::new(store))))
        .build()
        .await;
    let where_clause = match &args.name {
        Some(name) => format!(" WHERE name = '{}'", name.replace('\'', "''")),
        None => String::new(),
    };
    let limit_clause = match args.limit.fetch_limit() {
        Some(n) => format!(" LIMIT {n}"),
        None => String::new(),
    };
    let sql = format!(
        "SELECT id, id_prefix, name, text, value, target, created \
         FROM note{where_clause} ORDER BY created DESC{limit_clause}"
    );
    let batches = run_query(&ctx, &sql).await;
    let total = count_rows(&ctx, &format!("SELECT COUNT(*) FROM note{where_clause}")).await;
    if total == 0 {
        println!("No notes found");
        return;
    }

    let header: Vec<String> = ["Id", "Name", "Value", "Target", "Created"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let mut rows: Vec<Vec<String>> = Vec::new();
    for batch in &batches {
        let ids = column::<StringArray>(batch, 0);
        let prefixes = column::<StringArray>(batch, 1);
        let names = column::<StringArray>(batch, 2);
        let texts = column::<StringArray>(batch, 3);
        let values = column::<StringArray>(batch, 4);
        let targets = column::<StringArray>(batch, 5);
        let createds = column::<TimestampMillisecondArray>(batch, 6);
        for i in 0..batch.num_rows() {
            let id = ids.value(i);
            // A text note shows its text; a JSON note its compact JSON
            let raw = if texts.is_valid(i) {
                texts.value(i)
            } else {
                values.value(i)
            };
            let target = if targets.is_valid(i) {
                target_cell(targets.value(i))
            } else {
                String::new()
            };
            let created = if createds.is_valid(i) {
                crate::human::format_elapsed_ms(createds.value(i))
            } else {
                String::new()
            };
            rows.push(vec![
                styled_id(short_uuid(id), prefixes.value(i), IdKind::Gage),
                names.value(i).to_string(),
                value_cell(raw),
                target,
                created,
            ]);
        }
    }
    let shown = rows.len();

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

    args.limit.print_summary(shown, total, "note");
}

/// The single count a `SELECT COUNT(*)` query returns
pub(crate) async fn count_rows(ctx: &SessionContext, sql: &str) -> usize {
    let batches = run_query(ctx, sql).await;
    batches
        .first()
        .map(|b| column::<Int64Array>(b, 0).value(0) as usize)
        .unwrap_or(0)
}

fn column<T: 'static>(batch: &RecordBatch, idx: usize) -> &T {
    batch
        .column(idx)
        .as_any()
        .downcast_ref::<T>()
        .expect("column type matches note-table schema")
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

/// One-line cell for a note value: flattened to a single line and
/// cut at 400 chars
fn value_cell(raw: &str) -> String {
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
    let metadata = match args.metadata.as_deref().map(parse_metadata) {
        Some(Ok(m)) => Some(m),
        Some(Err(e)) => {
            eprintln!("gage note add: {e}");
            std::process::exit(1);
        }
        None => None,
    };

    dialog::run("Add note", || {
        if let Some(url) = &target {
            cli::log::step(format!("Target\n{}", style(url).dim()))?;
        }
        if let Some(m) = &metadata {
            cli::log::step(format!("Metadata\n{}", style(m).dim()))?;
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
                metadata: metadata.clone(),
            })
            .map_err(|e| DialogError::Failed(e.to_string()))?;
        Ok(format!("Note {} added", short_uuid(&id)).into())
    });
}

/// A `--metadata` argument as a JSON object
fn parse_metadata(raw: &str) -> Result<serde_json::Value, String> {
    let value: serde_json::Value =
        serde_json::from_str(raw).map_err(|e| format!("--metadata is not valid JSON: {e}"))?;
    if !value.is_object() {
        return Err("--metadata must be a JSON object".to_string());
    }
    Ok(value)
}

/// `$USER` when set and non-empty
fn env_user() -> Option<String> {
    std::env::var_os("USER")
        .map(|u| u.to_string_lossy().into_owned())
        .filter(|u| !u.is_empty())
}

/// Resolve a `--target` value, an object id prefix with an optional
/// `#<line selection>`, to a Gage URL with the full id. The prefix
/// resolves as any other, preferring recent objects; with a line
/// selection only sessions are candidates. The line selection itself
/// is checked by the store.
fn resolve_target(store: &Store, input: &str) -> Result<String, String> {
    let (prefix, fragment) = match input.split_once('#') {
        Some((p, f)) => (p, Some(f)),
        None => (input, None),
    };
    if prefix.is_empty() {
        return Err(format!("target {input:?}: missing object ID"));
    }
    let scope = fragment.map(|_| SESSION_TYPE);
    let found = store
        .resolve_in(prefix, scope)
        .map_err(|e| format!("target {prefix}: {e}"))?;
    if found.deleted {
        return Err(format!("target {prefix}: object is deleted: {}", found.id));
    }
    if fragment.is_some() && found.object_type != SESSION_TYPE {
        return Err(format!(
            "target {prefix}: a line selection applies to a session, not a {}",
            type_name(&found.object_type)
        ));
    }
    let type_name = type_name(&found.object_type);
    Ok(match fragment {
        Some(f) => format!("{type_name}:{}#{f}", found.id),
        None => format!("{type_name}:{}", found.id),
    })
}

/// `gage::note` displays as `note`
fn type_name(object_type: &str) -> &str {
    object_type.strip_prefix("gage::").unwrap_or(object_type)
}

pub async fn show(args: NoteShowArgs) {
    let store = match Store::open(&gage_store::store_path()) {
        Ok(store) => store,
        Err(e) => {
            eprintln!("gage note show: {e}");
            std::process::exit(1);
        }
    };
    // The prefix resolves through the store; the row comes from SQL
    let id = match store.resolve_in(&args.id, Some(NOTE_TYPE)) {
        Ok(found) if found.deleted => {
            eprintln!("gage note show: note {} is deleted", found.id);
            std::process::exit(1);
        }
        Ok(found) => found.id,
        Err(e) => {
            eprintln!("gage note show: {e}");
            std::process::exit(1);
        }
    };
    let ctx = ContextBuilder::new(Some(Arc::new(Mutex::new(store))))
        .build()
        .await;
    let sql = format!(
        "SELECT name, value, text, metadata, target, author, created, modified \
         FROM note WHERE id = '{id}'"
    );
    let batches = run_query(&ctx, &sql).await;
    let Some(batch) = batches.iter().find(|b| b.num_rows() > 0) else {
        eprintln!("gage note show: note {id} not found");
        std::process::exit(1);
    };
    let text_col = column::<StringArray>(batch, 2);
    let value = if text_col.is_valid(0) {
        NoteValue::Text(text_col.value(0).to_string())
    } else {
        match serde_json::from_str(column::<StringArray>(batch, 1).value(0)) {
            Ok(json) => NoteValue::Json(json),
            Err(e) => {
                eprintln!("gage note show: note {id} value: {e}");
                std::process::exit(1);
            }
        }
    };
    let metadata_col = column::<StringArray>(batch, 3);
    let metadata: Option<serde_json::Value> = if metadata_col.is_valid(0) {
        match serde_json::from_str(metadata_col.value(0)) {
            Ok(json) => Some(json),
            Err(e) => {
                eprintln!("gage note show: note {id} metadata: {e}");
                std::process::exit(1);
            }
        }
    } else {
        None
    };
    let opt_string = |idx: usize| {
        let col = column::<StringArray>(batch, idx);
        col.is_valid(0).then(|| col.value(0).to_string())
    };
    let iso = |idx: usize| {
        let col = column::<TimestampMillisecondArray>(batch, idx);
        if col.is_valid(0) {
            gage_core::datetime::ms_to_iso8601(col.value(0))
        } else {
            String::new()
        }
    };
    let name = column::<StringArray>(batch, 0).value(0).to_string();

    let mut attrs = vec![
        ("id", id.clone()),
        ("name", name.clone()),
        ("value", String::new()),
        ("target", opt_string(4).unwrap_or_default()),
        ("author", opt_string(5).unwrap_or_default()),
        ("metadata", String::new()),
        ("created", iso(6)),
        ("modified", iso(7)),
    ];
    if args.doc {
        let registry = ScannerRegistry::load();
        let doc = registry
            .note_doc(&name)
            .unwrap_or_else(|| "(no scanner declares this note)".to_string());
        attrs.push(("doc", doc));
    }

    let label_width = attrs.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
    let (_, term_width) = console::Term::stdout().size();
    // Borders + padding: "│ " + " │ " + " │" = 8 chars
    let value_width = (term_width as usize)
        .saturating_sub(label_width + 8)
        .max(20);

    // A JSON value is pretty-printed and colored token by token; text
    // is wrapped and colored as one cell
    let (value_cell, value_is_json) = match &value {
        NoteValue::Text(text) => (textwrap::fill(text, value_width), false),
        NoteValue::Json(json) => (crate::json::render(json), true),
    };
    let metadata_cell = metadata
        .as_ref()
        .map(crate::json::render)
        .unwrap_or_default();
    let rows: Vec<Vec<String>> = attrs
        .into_iter()
        .map(|(k, v)| {
            let value = match k {
                "value" => value_cell.clone(),
                "metadata" => metadata_cell.clone(),
                "doc" => crate::markdown::render(&v, value_width),
                _ => textwrap::fill(&v, value_width),
            };
            vec![k.to_string(), value]
        })
        .collect();

    let mut table = Table::from_iter(rows);
    table
        .with(Style::rounded())
        .modify(Columns::first(), style::tty(Color::FG_BRIGHT_YELLOW));
    if !value_is_json {
        table.modify(Cell::new(2, 1), style::tty(Color::FG_BRIGHT_CYAN));
    }
    println!("{table}");
}

pub(crate) fn format_value(value: &note::NoteValue) -> String {
    match &value.0 {
        serde_json::Value::String(s) => s.clone(),
        _ => value.to_json(),
    }
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

pub fn edit(args: NoteEditArgs) {
    if args.yes && args.text.is_none() && args.name.is_none() && args.target.is_none() {
        eprintln!("gage note edit: --yes requires TEXT, --name, or --target");
        std::process::exit(1);
    }
    let store = match Store::open(&gage_store::store_path()) {
        Ok(store) => store,
        Err(e) => {
            eprintln!("gage note edit: {e}");
            std::process::exit(1);
        }
    };
    let notes = NoteStore::from(&store);
    let current = match notes.get(&args.id) {
        Ok(full) => full,
        Err(e) => {
            eprintln!("gage note edit: {e}");
            std::process::exit(1);
        }
    };
    let given_target = match args.target.as_deref() {
        Some(input) => match resolve_target(&store, input) {
            Ok(url) => Some(url),
            Err(e) => {
                eprintln!("gage note edit: {e}");
                std::process::exit(1);
            }
        },
        None => None,
    };

    dialog::run("Edit note", || {
        cli::log::step(format!("Note\n{}", style(short_uuid(&current.id)).dim()))?;

        // Target and name are never prompted: the given value or the
        // current one, shown either way
        let target: Option<String> = given_target.clone().or_else(|| current.target.clone());
        if let Some(url) = &target {
            cli::log::step(format!("Target\n{}", style(url).dim()))?;
        }
        let name: String = args.name.clone().unwrap_or_else(|| current.name.clone());
        cli::log::step(format!("Name\n{}", style(&name).dim()))?;

        // The value keeps its form unless TEXT or --json says otherwise
        let as_json =
            args.json || (args.text.is_none() && matches!(current.value, NoteValue::Json(_)));
        let label = if as_json { "JSON" } else { "Text" };
        let current_text = match &current.value {
            NoteValue::Text(text) => text.clone(),
            NoteValue::Json(json) => json.to_string(),
        };
        let text: String = match args.text {
            Some(ref t) => {
                cli::log::step(format!("{label}\n{}", style(t).dim()))?;
                t.clone()
            }
            None if args.yes => current_text.clone(),
            None if as_json => cli::input(label)
                .default_input(&current_text)
                .validate(|s: &String| {
                    serde_json::from_str::<serde_json::Value>(s)
                        .map(|_| ())
                        .map_err(|e| format!("not valid JSON: {e}"))
                })
                .interact()?,
            None => cli::input(label).default_input(&current_text).interact()?,
        };
        let value = if as_json {
            let json = serde_json::from_str(&text)
                .map_err(|e| DialogError::Failed(format!("text is not valid JSON: {e}")))?;
            NoteValue::Json(json)
        } else {
            NoteValue::Text(text)
        };

        if !args.yes {
            let confirmed = cli::confirm("Apply these changes?")
                .initial_value(true)
                .interact()?;
            if !confirmed {
                return Err(DialogError::Canceled);
            }
        }

        // Only what differs is sent, so an unchanged target keeps the
        // commit it was linked at
        let edit = NoteEdit {
            name: (name != current.name).then_some(name.as_str()),
            value: (value != current.value).then_some(value),
            target: target
                .as_deref()
                .filter(|url| Some(*url) != current.target.as_deref()),
        };
        if edit.name.is_none() && edit.value.is_none() && edit.target.is_none() {
            return Ok(format!("Note {} unchanged", short_uuid(&current.id)).into());
        }
        notes
            .edit(&current.id, edit)
            .map_err(|e| DialogError::Failed(e.to_string()))?;
        Ok(format!("Note {} updated", short_uuid(&current.id)).into())
    });
}

pub fn delete(args: NoteDeleteArgs) {
    let store = match Store::open(&gage_store::store_path()) {
        Ok(store) => store,
        Err(e) => {
            eprintln!("gage note delete: {e}");
            std::process::exit(1);
        }
    };
    let notes = NoteStore::from(&store);

    // Resolve every argument before writing anything, so one bad
    // argument leaves the store untouched
    let mut ids: Vec<String> = Vec::with_capacity(args.ids.len());
    let mut errors = 0;
    for prefix in &args.ids {
        match notes.get(prefix) {
            Ok(full) => ids.push(full.id),
            Err(e) => {
                eprintln!("gage note delete: {e}");
                errors += 1;
            }
        }
    }
    if errors > 0 {
        std::process::exit(1);
    }

    let count = ids.len();
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
        for id in &ids {
            if let Err(e) = notes.delete(id) {
                eprintln!("warning: failed to delete {}: {e}", short_uuid(id));
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
