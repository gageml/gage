//! `gage session publish`: the pending-publish life cycle commands.
//!
//! `init` stages a publish of a session; `redact`, `status`, `undo`,
//! `refresh`, the `filter` commands, `abort`, and `apply` follow.
//! `apply` is the only command that writes the store. The source session is resolved
//! through the session command's `--source` and `--stored`, the way
//! `gage scan` resolves sessions: a `--stored` session is used as is,
//! and a native session is added to the store first so it has an
//! object id and commit for `origin`.

use std::cmp::Reverse;
use std::io;
use std::sync::Arc;

use clap::{Args, Subcommand};
use cliclack as cli;
use gage_core::style::IdHighlighter;
use gage_core::uuid::short_uuid;
use gage_publish::{
    Filter, FilterEntry, FilterSpec, PublishDefinition, PublishError, Publisher, REASONS,
    RedactionInput,
};
use gage_session::{Driver, NativeSession, SourceOptions};
use gage_store::{
    PendingPublish, PublishStore, SESSION_TYPE, SessionRecord, SessionStore, Store, StoreError,
    TagStore,
};
use tabled::{
    Table,
    settings::{
        Alignment, Color, Style, Width,
        object::{Columns, Object, Rows},
        peaker::Priority,
    },
};

use crate::author::resolve_author;
use crate::cmd_note::value_cell;
use crate::cmd_tag::TAG_HELP;
use crate::dialog::{self, DialogError};
use crate::error::CliError;
use crate::human::{count, format_elapsed_ms};
use crate::session_filter::{SessionLine, preview_sessions};
use crate::source;
use crate::style::{self, IdKind};
use crate::text_arg::text_value;

#[derive(Args)]
pub struct PublishArgs {
    #[command(subcommand)]
    command: PublishCommand,
}

/// Overrides of the `gage session` global `--source` and `--stored`
/// for a publish command that takes neither. The override keeps the
/// inherited options out of the command's help, the way `find`
/// declares its own; a value given anyway is rejected. The override
/// is declared on each leaf command and not on the `publish` group,
/// because a declaration on the group captures the flags meant for
/// `init`.
#[derive(Args)]
struct NoSource {
    #[arg(id = "source", long = "source", hide = true)]
    source: Option<String>,

    #[arg(id = "stored", short = 'S', long = "stored", hide = true)]
    stored: bool,
}

impl NoSource {
    fn reject(&self, command: &str) -> Result<(), CliError> {
        if self.source.is_some() {
            return Err(CliError::message(format!(
                "{command}: --source does not apply; SESSION names a pending publish"
            )));
        }
        if self.stored {
            return Err(CliError::message(format!(
                "{command}: --stored does not apply; SESSION names a pending publish"
            )));
        }
        Ok(())
    }
}

#[derive(Subcommand)]
pub enum PublishCommand {
    /// Start a publish of a session
    ///
    /// Stages a pending publish and prints the new session ID, the
    /// value the other publish commands take as their SESSION
    /// argument.
    Init(InitArgs),

    /// List pending publish sessions
    List(ListArgs),

    /// Add redactions to a pending publish
    ///
    /// Every redaction given carries --reason and is applied to staged
    /// session at once. A text redaction is matched against every text
    /// value of session; a span redaction applies within one value.
    Redact(RedactArgs),

    /// Show filters and redactions of a pending publish
    Status(StatusArgs),

    /// Remove redactions from a pending publish
    ///
    /// Each ID is one listed by status. Staged session is rebuilt from
    /// original with remaining redactions. A named filter's redaction,
    /// once removed, is not restored when filter runs again.
    Undo(UndoArgs),

    /// Rebuild a pending publish from its definition
    ///
    /// Re-applies publish definition file as it is on disk, for a
    /// definition edited by hand. An invalid definition is reported and
    /// nothing changes.
    Refresh(RefreshArgs),

    /// Manage filters of a pending publish
    Filter(FilterArgs),

    /// Abort a pending publish
    ///
    /// Deletes staged publish. Nothing was written to store.
    Abort(AbortArgs),

    /// Publish a pending publish
    ///
    /// Checks that staged session is original with removals only, adds
    /// it to store as a new session, and deletes staged publish. This
    /// is the only publish command that writes to store.
    Apply(ApplyArgs),
}

#[derive(Args)]
#[command(override_usage = "gage session publish init [OPTIONS] <SESSION>")]
pub struct InitArgs {
    /// Session to publish (tag, ID, or prefix)
    #[arg(value_name = "SESSION", required_unless_present = "list_filters")]
    session: Option<String>,

    /// Session source
    #[arg(
        id = "source",
        short = 's',
        long = "source",
        value_name = "SOURCE",
        long_help = format!("Session source\n\n{}", source::SOURCE_HELP)
    )]
    source: Option<String>,

    /// Publish a stored session
    #[arg(id = "stored", short = 'S', long = "stored", conflicts_with = "source")]
    stored: bool,

    /// Filter to apply (repeatable)
    #[arg(
        short,
        long,
        value_name = "FILTER",
        long_help = format!("Filter to apply (repeatable)\n\n{FILTER_HELP}")
    )]
    filter: Vec<String>,

    /// Skip confirmation prompt
    #[arg(short, long)]
    yes: bool,

    /// Show available named filters and exit
    ///
    /// Like --help, prints the list and exits whatever else is given.
    #[arg(long)]
    list_filters: bool,
}

#[derive(Args)]
pub struct ListArgs {
    #[command(flatten)]
    no_source: NoSource,
}

#[derive(Args)]
#[command(override_usage = "gage session publish redact [OPTIONS] <SESSION>")]
pub struct RedactArgs {
    /// Pending publish session to redact (ID or prefix)
    #[arg(value_name = "SESSION", required_unless_present = "list_reasons")]
    session: Option<String>,

    /// Reason for every redaction given (default scope)
    ///
    /// A reason code; see --list-reasons. Required with --text and
    /// --span; lines dropped without a reason are recorded as "scope".
    #[arg(short, long, value_name = "REASON")]
    reason: Option<String>,

    /// Lines to drop (repeatable)
    ///
    /// "LINE" or "LINE LINE_END", session entry lines as in entry
    /// table.
    #[arg(short, long, value_name = "LINES")]
    lines: Vec<String>,

    /// Text to redact everywhere (repeatable)
    ///
    /// Matched case-insensitively against every text value of session.
    #[arg(short, long, value_name = "TEXT")]
    text: Vec<String>,

    /// Span to redact within one value (repeatable)
    ///
    /// "LINE SEQ TEXT". LINE and SEQ name a text_block row, and TEXT runs
    /// to end of value.
    #[arg(short, long, value_name = "SPAN")]
    span: Vec<String>,

    /// Remove redacted text without a marker
    ///
    /// Applies to every --text and --span redaction given. Without it a
    /// redacted span is replaced by "[REDACTED]"; with it the span is
    /// removed and surrounding whitespace is normalized.
    #[arg(short = 'n', long)]
    no_mark: bool,

    /// Skip confirmation prompt
    #[arg(short, long)]
    yes: bool,

    /// Show reason codes and exit
    ///
    /// Like --help, prints the list and exits whatever else is given.
    #[arg(long)]
    list_reasons: bool,

    #[command(flatten)]
    no_source: NoSource,
}

#[derive(Args)]
pub struct StatusArgs {
    /// Pending publish session to show (ID or prefix)
    #[arg(value_name = "SESSION")]
    session: String,

    #[command(flatten)]
    no_source: NoSource,
}

#[derive(Args)]
pub struct UndoArgs {
    /// Pending publish session to change (ID or prefix)
    #[arg(value_name = "SESSION")]
    session: String,

    /// Redaction IDs to remove
    ///
    /// IDs as status lists them.
    #[arg(value_name = "ID", required = true)]
    ids: Vec<u64>,

    /// Skip confirmation prompt
    #[arg(short, long)]
    yes: bool,

    #[command(flatten)]
    no_source: NoSource,
}

#[derive(Args)]
pub struct RefreshArgs {
    /// Pending publish session to rebuild (ID or prefix)
    #[arg(value_name = "SESSION")]
    session: String,

    /// Skip confirmation prompt
    #[arg(short, long)]
    yes: bool,

    #[command(flatten)]
    no_source: NoSource,
}

#[derive(Args)]
pub struct AbortArgs {
    /// Pending publish session to abort (ID or prefix)
    #[arg(value_name = "SESSION")]
    session: String,

    /// Skip confirmation prompt
    #[arg(short, long)]
    yes: bool,

    #[command(flatten)]
    no_source: NoSource,
}

#[derive(Args)]
pub struct ApplyArgs {
    /// Pending publish session to publish (ID or prefix)
    #[arg(value_name = "SESSION")]
    session: String,

    /// Tag published session
    #[arg(
        short,
        long,
        value_name = "NAME",
        long_help = format!("Tag published session\n\n{TAG_HELP}")
    )]
    tag: Option<String>,

    /// Move tag if it exists
    #[arg(long, requires = "tag")]
    force_tag: bool,

    /// Skip confirmation prompt
    #[arg(short, long)]
    yes: bool,

    #[command(flatten)]
    no_source: NoSource,
}

/// The paragraph `init --filter` and `filter add` share on what a
/// filter argument is.
const FILTER_HELP: &str = "A filter name, or an instruction to an LLM describing what to \
remove. An instruction contains a space and may be given as @PATH to read it from a file. \
Filter names are \"pii\", \"language\", and \"secret\".";

/// The `filter` group declares the `--source` and `--stored` override
/// itself: none of its commands takes them, so the group capturing a
/// flag given before the command is the rejection wanted.
#[derive(Args)]
pub struct FilterArgs {
    #[command(subcommand)]
    command: FilterCommand,

    #[command(flatten)]
    no_source: NoSource,
}

#[derive(Subcommand)]
pub enum FilterCommand {
    /// Add a filter to a pending publish
    ///
    /// A named filter's deterministic rules run at once. A filter
    /// already on the list is an error.
    Add(FilterAddArgs),

    /// Remove a filter from a pending publish
    ///
    /// Filter's redactions are removed with it. A redaction undone
    /// earlier stays suppressed if filter is added again.
    Remove(FilterRemoveArgs),

    /// Run named filters of a pending publish again
    ///
    /// Filter's earlier redactions are replaced by what its rules find
    /// now.
    Run(FilterRunArgs),

    /// List filters of a pending publish
    List(FilterListArgs),
}

#[derive(Args)]
#[command(override_usage = "gage session publish filter add [OPTIONS] <SESSION> <FILTER>")]
pub struct FilterAddArgs {
    /// Pending publish session to change (ID or prefix)
    #[arg(value_name = "SESSION", required_unless_present = "list_filters")]
    session: Option<String>,

    /// Filter to add
    #[arg(
        value_name = "FILTER",
        required_unless_present = "list_filters",
        long_help = format!("Filter to add\n\n{FILTER_HELP}")
    )]
    filter: Option<String>,

    /// Skip confirmation prompt
    #[arg(short, long)]
    yes: bool,

    /// Show available named filters and exit
    ///
    /// Like --help, prints the list and exits whatever else is given.
    #[arg(long)]
    list_filters: bool,
}

#[derive(Args)]
pub struct FilterRemoveArgs {
    /// Pending publish session to change (ID or prefix)
    #[arg(value_name = "SESSION")]
    session: String,

    /// Filter to remove
    ///
    /// A filter name, else a prefix of one instruction on the list. A
    /// prefix of more than one instruction is an error.
    #[arg(value_name = "FILTER")]
    filter: String,

    /// Skip confirmation prompt
    #[arg(short, long)]
    yes: bool,
}

#[derive(Args)]
pub struct FilterRunArgs {
    /// Pending publish session to change (ID or prefix)
    #[arg(value_name = "SESSION")]
    session: String,

    /// Named filter to run (default every named filter on list)
    #[arg(value_name = "NAME")]
    name: Option<String>,

    /// Skip confirmation prompt
    #[arg(short, long)]
    yes: bool,
}

#[derive(Args)]
pub struct FilterListArgs {
    /// Pending publish session to show (ID or prefix)
    #[arg(value_name = "SESSION")]
    session: String,
}

/// Every publish command declares its own `--source` and `--stored`
/// in place of the `gage session` globals: `init` takes them to
/// resolve the original, and the rest reject them.
pub async fn run(args: PublishArgs) -> Result<(), CliError> {
    match args.command {
        PublishCommand::Init(args) => init(args),
        PublishCommand::List(args) => {
            args.no_source.reject("gage session publish list")?;
            list()
        }
        PublishCommand::Redact(args) => {
            args.no_source.reject("gage session publish redact")?;
            redact(args)
        }
        PublishCommand::Status(args) => {
            args.no_source.reject("gage session publish status")?;
            status(args)
        }
        PublishCommand::Undo(args) => {
            args.no_source.reject("gage session publish undo")?;
            undo(args)
        }
        PublishCommand::Refresh(args) => {
            args.no_source.reject("gage session publish refresh")?;
            refresh(args)
        }
        PublishCommand::Filter(args) => {
            args.no_source.reject("gage session publish filter")?;
            match args.command {
                FilterCommand::Add(args) => filter_add(args),
                FilterCommand::Remove(args) => filter_remove(args),
                FilterCommand::Run(args) => filter_run(args),
                FilterCommand::List(args) => filter_list(args),
            }
        }
        PublishCommand::Abort(args) => {
            args.no_source.reject("gage session publish abort")?;
            abort(args)
        }
        PublishCommand::Apply(args) => {
            args.no_source.reject("gage session publish apply")?;
            apply(args)
        }
    }
}

/// List the pending publish working trees, read from the file system,
/// newest first. Ids are highlighted against the stored sessions plus
/// the pending set, the peers a pending prefix resolves against.
fn list() -> Result<(), CliError> {
    let fail = |e: String| CliError::message(format!("gage session publish list: {e}"));
    let store = Store::open(&gage_store::store_path()).map_err(|e| fail(e.to_string()))?;
    let pending = PublishStore::from(&store)
        .list()
        .map_err(|e| fail(e.to_string()))?;
    if pending.is_empty() {
        println!("No pending publish sessions");
        return Ok(());
    }
    let mut peers = store
        .short_prefix_ids(Some(SESSION_TYPE))
        .map_err(|e| fail(e.to_string()))?;
    peers.extend(pending.iter().map(|p| p.id.clone()));
    let highlighter = IdHighlighter::new(peers);

    let header: Vec<String> = ["Id", "Origin", "Title", "Modified"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let rows = pending_rows(pending, &highlighter);
    let table = Table::from_iter(std::iter::once(header).chain(rows))
        .with(Style::rounded())
        .modify(Rows::first(), style::tty(Color::FG_BRIGHT_YELLOW))
        .modify(Columns::new(2..4).not(Rows::first()), style::dim())
        .to_string();
    println!("{table}");
    Ok(())
}

/// One row per pending publish, newest first: the highlighted short
/// id, the `origin` and title from `attrs.json`, and the age of the
/// working tree.
fn pending_rows(mut pending: Vec<PendingPublish>, highlighter: &IdHighlighter) -> Vec<Vec<String>> {
    pending.sort_by_key(|p| Reverse(p.modified_ms));
    pending
        .iter()
        .map(|p| {
            vec![
                highlighter.short(&p.id),
                origin_cell(&p.attrs.origin, highlighter),
                p.attrs.summary.title.clone().unwrap_or_default(),
                format_elapsed_ms(p.modified_ms),
            ]
        })
        .collect()
}

/// A derived origin, `session:<id>@<commit>`, as the highlighted short
/// id of the original session, a space, and the first 8 characters of
/// the commit dimmed. Any other origin is shown as stored.
fn origin_cell(origin: &str, highlighter: &IdHighlighter) -> String {
    let Some((id, commit)) = origin
        .strip_prefix("session:")
        .and_then(|rest| rest.split_once('@'))
    else {
        return origin.to_string();
    };
    let commit: String = commit.chars().take(8).collect();
    format!("{} {}", highlighter.short(id), console::style(commit).dim())
}

/// Stage a publish after confirmation. The original is resolved
/// before the dialog opens, so a bad argument fails without a prompt,
/// and a native session is added to the store only after the user
/// confirms.
fn init(args: InitArgs) -> Result<(), CliError> {
    let fail = |e: String| CliError::message(format!("gage session publish init: {e}"));
    if args.list_filters {
        list_filters();
        return Ok(());
    }
    let Some(session) = args.session.as_deref() else {
        return Err(fail("SESSION is required".into()));
    };
    let (source, stored) = (args.source.clone(), args.stored);
    let store = Store::open(&gage_store::store_path()).map_err(|e| fail(e.to_string()))?;
    let filters = args
        .filter
        .iter()
        .map(|text| parse_filter("gage session publish init", "--filter", text))
        .collect::<Result<Vec<_>, _>>()?;
    let original = resolve_original(&store, source.as_deref(), stored, session)?;

    dialog::run("Publish session", || {
        let (line, kind) = match &original {
            Original::Stored(record) => (
                SessionLine {
                    id: record.id.clone(),
                    title: record.attrs.summary.title.clone(),
                },
                IdKind::Gage,
            ),
            Original::Native {
                driver,
                spec,
                native,
            } => {
                dialog::step_value("Source", source::describe_source(driver.as_ref(), spec))?;
                (
                    SessionLine {
                        id: native.id().to_string(),
                        title: native.attrs().title().map(String::from),
                    },
                    IdKind::Native,
                )
            }
        };
        preview_sessions(&[line], kind, None)?;
        filters_step(&filters)?;
        if !args.yes {
            let confirmed = cli::confirm("Continue?").initial_value(true).interact()?;
            if !confirmed {
                return Err(DialogError::Canceled);
            }
        }
        let record = original.into_stored(&store).map_err(DialogError::Failed)?;
        let publisher = Publisher::new(&store);
        let outcome = publisher
            .init(&record, &filters)
            .map_err(|e| DialogError::Failed(e.to_string()))?;
        let definition = publisher
            .definition(&outcome.id)
            .map_err(|e| DialogError::Failed(e.to_string()))?;
        report_filter_redactions(&definition)?;
        Ok(format!("Staged publish {}", short_uuid(&outcome.id)).into())
    })
}

/// Print the named filters as a table, the way `gage scan
/// --list-scanners` prints scanners.
fn list_filters() {
    let header: Vec<String> = ["Filter", "Description"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let rows = filter_rows();
    let term_width = console::Term::stdout().size().1 as usize;
    let table = Table::from_iter(std::iter::once(header).chain(rows))
        .with(Style::rounded())
        .with(
            Width::wrap(term_width)
                .keep_words(true)
                .priority(Priority::max(true)),
        )
        .modify(Rows::first(), style::tty(Color::FG_BRIGHT_YELLOW))
        .modify(
            Columns::one(0).not(Rows::first()),
            style::tty(Color::FG_BRIGHT_YELLOW),
        )
        .modify(Columns::one(1).not(Rows::first()), style::dim())
        .to_string();
    println!("{table}");
}

/// One row per named filter: name and description.
fn filter_rows() -> Vec<Vec<String>> {
    Filter::all()
        .iter()
        .map(|f| vec![f.name.to_string(), f.description.to_string()])
        .collect()
}

/// The filter `text` denotes, with `@PATH` read as a file. An unknown
/// name is an error naming `command` and `option`.
fn parse_filter(command: &str, option: &str, text: &str) -> Result<FilterSpec, CliError> {
    let fail = |e: String| CliError::message(format!("{command}: {e}"));
    let text = text_value(command, option, text).map_err(|e| fail(e.to_string()))?;
    text.trim_end()
        .parse()
        .map_err(|e: PublishError| fail(format!("{option}: {e}")))
}

/// The `Filters` step of the dialog: one line per filter, or `none`.
fn filters_step(filters: &[FilterSpec]) -> io::Result<()> {
    if filters.is_empty() {
        return dialog::step_value("Filters", "none");
    }
    dialog::step_list("Filters", filters.iter().map(|f| f.to_string()))
}

/// Report what the named filters' deterministic rules redacted.
fn report_filter_redactions(definition: &PublishDefinition) -> io::Result<()> {
    let by_filter: Vec<(String, usize, usize)> = definition
        .filters
        .iter()
        .filter_map(|f| f.spec().name().map(String::from))
        .map(|name| {
            let author = format!("filter:{name}");
            let mine: Vec<_> = definition
                .redactions
                .iter()
                .filter(|r| r.author == author)
                .collect();
            let instances = mine.iter().map(|r| r.instances).sum();
            (name, mine.len(), instances)
        })
        .collect();
    if by_filter.is_empty() {
        return Ok(());
    }
    dialog::step_list(
        "Filter redactions",
        by_filter.into_iter().map(|(name, redactions, instances)| {
            format!(
                "{name}: {}, {}",
                count(redactions, "redaction"),
                count(instances, "instance")
            )
        }),
    )
}

/// Add redactions to a pending publish after confirmation. Every spec
/// is parsed before the dialog opens, and every redaction is resolved
/// against the session before any is recorded.
fn redact(args: RedactArgs) -> Result<(), CliError> {
    let fail = |e: String| CliError::message(format!("gage session publish redact: {e}"));
    if args.list_reasons {
        list_reasons();
        return Ok(());
    }
    let Some(session) = args.session.as_deref() else {
        return Err(fail("SESSION is required".into()));
    };
    // Every spec is parsed before the store is opened. Lines default to
    // "scope"; text and spans need the caller's reason.
    let lines_reason = args.reason.as_deref().unwrap_or("scope");
    let mut inputs = Vec::new();
    for spec in &args.lines {
        inputs.push(
            RedactionInput::parse_lines(spec, lines_reason).map_err(|e| fail(e.to_string()))?,
        );
    }
    if !args.text.is_empty() || !args.span.is_empty() {
        let reason = args
            .reason
            .as_deref()
            .ok_or_else(|| fail("--reason is required with --text and --span".into()))?;
        let mark = !args.no_mark;
        for literal in &args.text {
            inputs.push(
                RedactionInput::parse_text(literal, reason, mark)
                    .map_err(|e| fail(e.to_string()))?,
            );
        }
        for spec in &args.span {
            inputs.push(
                RedactionInput::parse_span(spec, reason, mark).map_err(|e| fail(e.to_string()))?,
            );
        }
    }
    if inputs.is_empty() {
        return Err(fail(
            "no redactions given; use --lines, --text, or --span".into(),
        ));
    }
    let store = Store::open(&gage_store::store_path()).map_err(|e| fail(e.to_string()))?;
    let publisher = Publisher::new(&store);
    let pending = publisher
        .store()
        .get(session)
        .map_err(|e| fail(e.to_string()))?;
    let author = resolve_author(None);

    dialog::run("Redact publish", || {
        dialog::step_value("Session", short_uuid(&pending.id))?;
        dialog::step_list("Redactions", inputs.iter().map(describe_input))?;
        if !args.yes {
            let confirmed = cli::confirm("Continue?").initial_value(true).interact()?;
            if !confirmed {
                return Err(DialogError::Canceled);
            }
        }
        let applied = publisher
            .redact(&pending.id, &inputs, &author)
            .map_err(|e| DialogError::Failed(e.to_string()))?;
        dialog::step_list(
            "Applied",
            applied.iter().map(|a| {
                format!(
                    "{} {} {} ({})",
                    a.id,
                    a.kind,
                    a.detail,
                    count(a.instances, "instance")
                )
            }),
        )?;
        let instances: usize = applied.iter().map(|a| a.instances).sum();
        Ok(format!(
            "Applied {}, {}",
            count(applied.len(), "redaction"),
            count(instances, "instance")
        )
        .into())
    })
}

/// Print the reason codes as a table.
fn list_reasons() {
    let header: Vec<String> = ["Reason", "Description"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let rows = REASONS
        .iter()
        .map(|r| vec![r.code.to_string(), r.description.to_string()]);
    let term_width = console::Term::stdout().size().1 as usize;
    let table = Table::from_iter(std::iter::once(header).chain(rows))
        .with(Style::rounded())
        .with(
            Width::wrap(term_width)
                .keep_words(true)
                .priority(Priority::max(true)),
        )
        .modify(Rows::first(), style::tty(Color::FG_BRIGHT_YELLOW))
        .modify(
            Columns::one(0).not(Rows::first()),
            style::tty(Color::FG_BRIGHT_YELLOW),
        )
        .modify(Columns::one(1).not(Rows::first()), style::dim())
        .to_string();
    println!("{table}");
}

/// A redaction input as the dialog lists it, with its reason.
fn describe_input(input: &RedactionInput) -> String {
    match input {
        RedactionInput::Lines {
            line,
            line_end,
            reason,
        } if line == line_end => format!("lines {line} ({reason})"),
        RedactionInput::Lines {
            line,
            line_end,
            reason,
        } => format!("lines {line}-{line_end} ({reason})"),
        RedactionInput::Text {
            reason,
            literal,
            mark,
        } => format!("text {literal:?} ({reason}{})", no_mark(*mark)),
        RedactionInput::Span {
            line,
            seq,
            reason,
            literal,
            mark,
        } => format!("span {line} {seq} {literal:?} ({reason}{})", no_mark(*mark)),
    }
}

/// The suffix a dialog line carries for a redaction without a marker.
fn no_mark(mark: bool) -> &'static str {
    if mark { "" } else { ", no mark" }
}

/// Show the filters and redactions of a pending publish.
fn status(args: StatusArgs) -> Result<(), CliError> {
    let fail = |e: String| CliError::message(format!("gage session publish status: {e}"));
    let store = Store::open(&gage_store::store_path()).map_err(|e| fail(e.to_string()))?;
    let publisher = Publisher::new(&store);
    let pending = publisher
        .store()
        .get(&args.session)
        .map_err(|e| fail(e.to_string()))?;
    let definition = publisher
        .definition(&pending.id)
        .map_err(|e| fail(e.to_string()))?;

    println!("Session: {}", short_uuid(&pending.id));
    println!("Origin:  {}", definition.origin);
    let filters: Vec<String> = definition
        .filters
        .iter()
        .map(|f| f.spec().to_string())
        .collect();
    println!(
        "Filters: {}",
        if filters.is_empty() {
            "none".to_string()
        } else {
            filters.join(", ")
        }
    );
    if definition.redactions.is_empty() {
        println!("No redactions");
        return Ok(());
    }
    let header: Vec<String> = ["Id", "Author", "Type", "Reason", "Detail", "Instances"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let rows = definition.redactions.iter().map(|r| {
        vec![
            r.id.to_string(),
            r.author.clone(),
            r.kind.kind_name().to_string(),
            r.kind.reason().to_string(),
            r.kind.detail(),
            r.instances.to_string(),
        ]
    });
    let table = Table::from_iter(std::iter::once(header).chain(rows))
        .with(Style::rounded())
        .modify(Rows::first(), style::tty(Color::FG_BRIGHT_YELLOW))
        .modify(Columns::one(5), Alignment::right())
        .modify(Columns::new(1..4).not(Rows::first()), style::dim())
        .to_string();
    println!("{table}");
    Ok(())
}

/// Remove redactions after confirmation. Every id is checked against
/// the definition before the dialog opens.
fn undo(args: UndoArgs) -> Result<(), CliError> {
    let fail = |e: String| CliError::message(format!("gage session publish undo: {e}"));
    let store = Store::open(&gage_store::store_path()).map_err(|e| fail(e.to_string()))?;
    let publisher = Publisher::new(&store);
    let pending = publisher
        .store()
        .get(&args.session)
        .map_err(|e| fail(e.to_string()))?;
    let definition = publisher
        .definition(&pending.id)
        .map_err(|e| fail(e.to_string()))?;
    let mut targets = Vec::new();
    for id in &args.ids {
        let r = definition
            .redactions
            .iter()
            .find(|r| r.id == *id)
            .ok_or_else(|| fail(format!("no redaction with id {id}")))?;
        targets.push(r.clone());
    }

    dialog::run("Undo redactions", || {
        dialog::step_value("Session", short_uuid(&pending.id))?;
        dialog::step_list(
            "Redactions",
            targets.iter().map(|r| {
                format!(
                    "{} {} {} ({}, {})",
                    r.id,
                    r.kind.kind_name(),
                    r.kind.detail(),
                    r.kind.reason(),
                    r.author
                )
            }),
        )?;
        if targets.iter().any(|r| r.author.starts_with("filter:")) {
            cli::log::remark("A filter's redaction stays removed when the filter runs again")?;
        }
        if !args.yes {
            let confirmed = cli::confirm("Continue?").initial_value(true).interact()?;
            if !confirmed {
                return Err(DialogError::Canceled);
            }
        }
        let removed = publisher
            .undo(&pending.id, &args.ids)
            .map_err(|e| DialogError::Failed(e.to_string()))?;
        Ok(format!("Removed {}", count(removed.len(), "redaction")).into())
    })
}

/// Rebuild a pending publish from its definition after confirmation.
fn refresh(args: RefreshArgs) -> Result<(), CliError> {
    let fail = |e: String| CliError::message(format!("gage session publish refresh: {e}"));
    let store = Store::open(&gage_store::store_path()).map_err(|e| fail(e.to_string()))?;
    let publisher = Publisher::new(&store);
    let pending = publisher
        .store()
        .get(&args.session)
        .map_err(|e| fail(e.to_string()))?;
    let definition = publisher
        .definition(&pending.id)
        .map_err(|e| fail(e.to_string()))?;
    definition.validate().map_err(|e| fail(e.to_string()))?;

    dialog::run("Refresh publish", || {
        dialog::step_value("Session", short_uuid(&pending.id))?;
        dialog::step_value(
            "Definition",
            format!(
                "{}, {}",
                count(definition.filters.len(), "filter"),
                count(definition.redactions.len(), "redaction")
            ),
        )?;
        if !args.yes {
            let confirmed = cli::confirm("Continue?").initial_value(true).interact()?;
            if !confirmed {
                return Err(DialogError::Canceled);
            }
        }
        let rebuilt = publisher
            .refresh(&pending.id)
            .map_err(|e| DialogError::Failed(e.to_string()))?;
        let instances: usize = rebuilt.redactions.iter().map(|r| r.instances).sum();
        Ok(format!(
            "Refreshed publish {}, {}",
            short_uuid(&pending.id),
            count(instances, "instance")
        )
        .into())
    })
}

/// Add a filter after confirmation. The filter is parsed before the
/// dialog opens, so an unknown name fails without a prompt.
fn filter_add(args: FilterAddArgs) -> Result<(), CliError> {
    let command = "gage session publish filter add";
    let fail = |e: String| CliError::message(format!("{command}: {e}"));
    if args.list_filters {
        list_filters();
        return Ok(());
    }
    let (Some(session), Some(filter)) = (args.session.as_deref(), args.filter.as_deref()) else {
        return Err(fail("SESSION and FILTER are required".into()));
    };
    let filter = parse_filter(command, "FILTER", filter)?;
    let store = Store::open(&gage_store::store_path()).map_err(|e| fail(e.to_string()))?;
    let publisher = Publisher::new(&store);
    let pending = publisher
        .store()
        .get(session)
        .map_err(|e| fail(e.to_string()))?;

    dialog::run("Add publish filter", || {
        dialog::step_value("Session", short_uuid(&pending.id))?;
        dialog::step_value("Filter", value_cell(&filter.to_string()))?;
        if !args.yes {
            let confirmed = cli::confirm("Continue?").initial_value(true).interact()?;
            if !confirmed {
                return Err(DialogError::Canceled);
            }
        }
        let definition = publisher
            .filter_add(&pending.id, &filter)
            .map_err(|e| DialogError::Failed(e.to_string()))?;
        report_filter_redactions(&definition)?;
        Ok(format!("Added filter {}", value_cell(&filter.to_string())).into())
    })
}

/// Remove a filter after confirmation. The selector is resolved
/// against the definition before the dialog opens.
fn filter_remove(args: FilterRemoveArgs) -> Result<(), CliError> {
    let fail = |e: String| CliError::message(format!("gage session publish filter remove: {e}"));
    let store = Store::open(&gage_store::store_path()).map_err(|e| fail(e.to_string()))?;
    let publisher = Publisher::new(&store);
    let pending = publisher
        .store()
        .get(&args.session)
        .map_err(|e| fail(e.to_string()))?;
    let definition = publisher
        .definition(&pending.id)
        .map_err(|e| fail(e.to_string()))?;
    let (_, entry) = definition
        .find_filter(&args.filter)
        .map_err(|e| fail(e.to_string()))?;
    let target = filter_cell(entry);
    let author = filter_author(entry);
    let redactions = definition
        .redactions
        .iter()
        .filter(|r| Some(r.author.as_str()) == author.as_deref())
        .count();

    dialog::run("Remove publish filter", || {
        dialog::step_value("Session", short_uuid(&pending.id))?;
        dialog::step_value("Filter", &target)?;
        dialog::step_value("Redactions", count(redactions, "redaction"))?;
        if !args.yes {
            let confirmed = cli::confirm("Continue?").initial_value(true).interact()?;
            if !confirmed {
                return Err(DialogError::Canceled);
            }
        }
        publisher
            .filter_remove(&pending.id, &args.filter)
            .map_err(|e| DialogError::Failed(e.to_string()))?;
        Ok(format!("Removed filter {target}").into())
    })
}

/// Re-run one named filter, or every named filter, after confirmation.
/// A name not on the list fails before the dialog opens.
fn filter_run(args: FilterRunArgs) -> Result<(), CliError> {
    let fail = |e: String| CliError::message(format!("gage session publish filter run: {e}"));
    let store = Store::open(&gage_store::store_path()).map_err(|e| fail(e.to_string()))?;
    let publisher = Publisher::new(&store);
    let pending = publisher
        .store()
        .get(&args.session)
        .map_err(|e| fail(e.to_string()))?;
    let definition = publisher
        .definition(&pending.id)
        .map_err(|e| fail(e.to_string()))?;
    let names = match &args.name {
        Some(name) => {
            if !definition.named_filters().iter().any(|n| n == name) {
                return Err(fail(format!("no named filter {name:?} is on the list")));
            }
            vec![name.clone()]
        }
        None => definition.named_filters(),
    };
    if names.is_empty() {
        return Err(fail("no named filters are on the list".into()));
    }

    dialog::run("Run publish filters", || {
        dialog::step_value("Session", short_uuid(&pending.id))?;
        dialog::step_list("Filters", names.iter().cloned())?;
        if !args.yes {
            let confirmed = cli::confirm("Continue?").initial_value(true).interact()?;
            if !confirmed {
                return Err(DialogError::Canceled);
            }
        }
        let definition = publisher
            .filter_run(&pending.id, args.name.as_deref())
            .map_err(|e| DialogError::Failed(e.to_string()))?;
        report_filter_redactions(&definition)?;
        Ok(format!("Ran {}", count(names.len(), "filter")).into())
    })
}

/// Print the filters of a pending publish as a table: the name or the
/// instruction on one line, its type, and the redactions it authored.
fn filter_list(args: FilterListArgs) -> Result<(), CliError> {
    let fail = |e: String| CliError::message(format!("gage session publish filter list: {e}"));
    let store = Store::open(&gage_store::store_path()).map_err(|e| fail(e.to_string()))?;
    let publisher = Publisher::new(&store);
    let pending = publisher
        .store()
        .get(&args.session)
        .map_err(|e| fail(e.to_string()))?;
    let definition = publisher
        .definition(&pending.id)
        .map_err(|e| fail(e.to_string()))?;
    if definition.filters.is_empty() {
        println!("No filters");
        return Ok(());
    }
    let header: Vec<String> = ["Filter", "Type", "Redactions"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let rows = definition.filters.iter().map(|f| {
        let author = filter_author(f);
        let redactions = definition
            .redactions
            .iter()
            .filter(|r| Some(r.author.as_str()) == author.as_deref())
            .count();
        let kind = match author {
            Some(_) => "name",
            None => "instruction",
        };
        vec![filter_cell(f), kind.to_string(), redactions.to_string()]
    });
    let term_width = console::Term::stdout().size().1 as usize;
    let table = Table::from_iter(std::iter::once(header).chain(rows))
        .with(Style::rounded())
        .with(
            Width::truncate(term_width)
                .suffix("…")
                .priority(Priority::max(true)),
        )
        .modify(Rows::first(), style::tty(Color::FG_BRIGHT_YELLOW))
        .modify(Columns::one(2), Alignment::right())
        .modify(Columns::one(1).not(Rows::first()), style::dim())
        .to_string();
    println!("{table}");
    Ok(())
}

/// A filter as a one-line cell: its name, or its instruction
/// flattened and cut.
fn filter_cell(filter: &FilterEntry) -> String {
    value_cell(&filter.spec().to_string())
}

/// The author a named filter's redactions carry, `filter:<name>`.
fn filter_author(filter: &FilterEntry) -> Option<String> {
    filter.name.as_ref().map(|name| format!("filter:{name}"))
}

/// Delete a pending publish after confirmation. The id is resolved
/// before the dialog opens, so a bad argument fails without a prompt.
fn abort(args: AbortArgs) -> Result<(), CliError> {
    let fail = |e: String| CliError::message(format!("gage session publish abort: {e}"));
    let store = Store::open(&gage_store::store_path()).map_err(|e| fail(e.to_string()))?;
    let publish = PublishStore::from(&store);
    let pending = publish
        .get(&args.session)
        .map_err(|e| fail(e.to_string()))?;

    dialog::run("Abort publish", || {
        dialog::step_value("Session", short_uuid(&pending.id))?;
        dialog::step_value("Origin", &pending.attrs.origin)?;
        cli::log::remark("Original session is unaffected")?;
        if !args.yes {
            let confirmed = cli::confirm("Continue?").initial_value(false).interact()?;
            if !confirmed {
                return Err(DialogError::Canceled);
            }
        }
        publish
            .abort(&pending.id)
            .map_err(|e| DialogError::Other(e.into()))?;
        Ok(format!("Aborted publish {}", short_uuid(&pending.id)).into())
    })
}

/// Publish a pending publish after confirmation. The staged session is
/// checked and the tag's availability confirmed before the dialog
/// opens, so a refusal leaves the store untouched and shows no prompt.
fn apply(args: ApplyArgs) -> Result<(), CliError> {
    let fail = |e: String| CliError::message(format!("gage session publish apply: {e}"));
    let store = Store::open(&gage_store::store_path()).map_err(|e| fail(e.to_string()))?;
    let publisher = Publisher::new(&store);
    let pending = publisher
        .store()
        .get(&args.session)
        .map_err(|e| fail(e.to_string()))?;
    let definition = publisher
        .definition(&pending.id)
        .map_err(|e| fail(e.to_string()))?;
    publisher
        .check(&pending.id)
        .map_err(|e| fail(e.to_string()))?;
    let tags = TagStore::from(&store);
    if let Some(name) = &args.tag {
        tags.check_available(name, args.force_tag)
            .map_err(|e| match e {
                StoreError::TagExists { .. } => fail(format!("{e} (use --force-tag to move it)")),
                e => fail(e.to_string()),
            })?;
    }
    let instances: usize = definition.redactions.iter().map(|r| r.instances).sum();

    dialog::run("Apply publish", || {
        dialog::step_value("Session", short_uuid(&pending.id))?;
        dialog::step_value("Origin", &pending.attrs.origin)?;
        dialog::step_value(
            "Definition",
            format!(
                "{}, {}, {}",
                count(definition.filters.len(), "filter"),
                count(definition.redactions.len(), "redaction"),
                count(instances, "instance")
            ),
        )?;
        if let Some(name) = &args.tag {
            dialog::step_value("Tag", name)?;
        }
        if !args.yes {
            let confirmed = cli::confirm("Continue?").initial_value(true).interact()?;
            if !confirmed {
                return Err(DialogError::Canceled);
            }
        }
        publisher
            .apply(&pending.id)
            .map_err(|e| DialogError::Failed(e.to_string()))?;
        if let Some(name) = &args.tag {
            let added = tags
                .add(name, &pending.id, args.force_tag)
                .map_err(|e| DialogError::Failed(e.to_string()))?;
            match added.previous_id {
                Some(previous) => cli::log::info(format!(
                    "Moved tag {name} from {} to session {}",
                    short_uuid(&previous),
                    short_uuid(&pending.id)
                ))?,
                None => cli::log::info(format!(
                    "Tagged session {} as {name}",
                    short_uuid(&pending.id)
                ))?,
            }
        }
        Ok(format!("Published session {}", short_uuid(&pending.id)).into())
    })
}

/// The session a publish starts from, resolved but not yet written.
enum Original {
    /// A session already in the store, used as is.
    Stored(Box<SessionRecord>),
    /// A native session, added to the store when the publish is
    /// confirmed so it has an object id and commit for `origin`.
    Native {
        driver: Arc<dyn Driver>,
        spec: String,
        native: Box<dyn NativeSession>,
    },
}

impl Original {
    /// The stored session to publish: the record as is, or the native
    /// session added to the store now.
    fn into_stored(self, store: &Store) -> Result<SessionRecord, String> {
        match self {
            Original::Stored(record) => Ok(*record),
            Original::Native {
                driver, mut native, ..
            } => {
                let sessions = SessionStore::from(store);
                let outcome = sessions
                    .add(driver.as_ref(), native.as_mut())
                    .map_err(|e| e.to_string())?;
                sessions
                    .at_commit(&outcome.commit_sha)
                    .map_err(|e| e.to_string())
            }
        }
    }
}

/// Resolve `prefix` without writing. `--stored` resolves it in the
/// store; otherwise it is a native session in `source`.
fn resolve_original(
    store: &Store,
    source: Option<&str>,
    stored: bool,
    prefix: &str,
) -> Result<Original, CliError> {
    let fail = |e: String| CliError::message(format!("gage session publish init: {e}"));
    if stored {
        return SessionStore::from(store)
            .get(prefix)
            .map(|record| Original::Stored(Box::new(record)))
            .map_err(|e| fail(format!("{prefix}: {e}")));
    }
    let registry = source::driver_registry();
    let (driver, spec) = source::resolve_source(&registry, source).map_err(fail)?;
    let opened = driver
        .open_source(&SourceOptions::new(&spec))
        .map_err(|e| fail(format!("{spec}: {e}")))?;
    let native_id = opened
        .find_native(prefix)
        .map_err(|e| fail(format!("{prefix}: {e}")))?;
    let native = opened
        .open_native(&native_id)
        .map_err(|e| fail(format!("{native_id}: {e}")))?;
    Ok(Original::Native {
        driver,
        spec,
        native,
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use gage_core::datetime::now_ms;
    use gage_store::{SessionAttrsRecord, SummaryAttrs};

    use super::*;

    fn pending(id: &str, origin: &str, title: Option<&str>, modified_ms: i64) -> PendingPublish {
        PendingPublish {
            id: id.to_string(),
            attrs: SessionAttrsRecord {
                driver: "claude 1".to_string(),
                origin: origin.to_string(),
                native_mtime: 0,
                native_size: 0,
                session_type: "claude".to_string(),
                content_format: "claude-jsonl 1".to_string(),
                size: 0,
                project: None,
                summary: SummaryAttrs {
                    title: title.map(String::from),
                    ..SummaryAttrs::default()
                },
            },
            root: PathBuf::from("/tmp/publish").join(id),
            modified_ms,
        }
    }

    /// Rows come newest first and carry the short id, the origin, the
    /// title, and the age.
    #[test]
    fn pending_rows_are_newest_first_with_id_origin_and_age() {
        let now = now_ms();
        let orig1 = "cccccccc-0000-4000-8000-000000000003";
        let orig2 = "dddddddd-0000-4000-8000-000000000004";
        let older = pending(
            "aaaaaaaa-0000-4000-8000-000000000001",
            &format!("session:{orig1}@0123456789abcdef0123456789abcdef01234567"),
            None,
            now - 3_600_000,
        );
        let newer = pending(
            "bbbbbbbb-0000-4000-8000-000000000002",
            &format!("session:{orig2}@fedcba9876543210fedcba9876543210fedcba98"),
            Some("Fix the duck"),
            now - 5_000,
        );
        let highlighter = IdHighlighter::new(vec![
            older.id.clone(),
            newer.id.clone(),
            orig1.to_string(),
            orig2.to_string(),
        ]);

        let rows = pending_rows(vec![older.clone(), newer.clone()], &highlighter);

        assert_eq!(rows.len(), 2);
        let plain = |cell: &str| console::strip_ansi_codes(cell).into_owned();
        assert_eq!(plain(&rows[0][0]), short_uuid(&newer.id));
        assert_eq!(
            plain(&rows[0][1]),
            format!("{} fedcba98", short_uuid(orig2))
        );
        assert_eq!(rows[0][2], "Fix the duck");
        assert_eq!(rows[0][3], "5s ago");
        assert_eq!(plain(&rows[1][0]), short_uuid(&older.id));
        assert_eq!(
            plain(&rows[1][1]),
            format!("{} 01234567", short_uuid(orig1))
        );
        assert_eq!(rows[1][2], "");
        assert_eq!(rows[1][3], "1h ago");
    }

    /// An origin that is not a derived `session:<id>@<commit>` value is
    /// shown as stored.
    #[test]
    fn origin_cell_shows_a_native_origin_as_stored() {
        let highlighter = IdHighlighter::new(vec![]);
        assert_eq!(
            console::strip_ansi_codes(&origin_cell("claude:/home/u/.claude/abc", &highlighter)),
            "claude:/home/u/.claude/abc"
        );
    }

    #[test]
    fn filter_rows_list_every_named_filter_with_a_description() {
        let rows = filter_rows();
        let mut names: Vec<&str> = rows.iter().map(|r| r[0].as_str()).collect();
        names.sort_unstable();
        let mut expected = gage_publish::NAMED_FILTERS.to_vec();
        expected.sort_unstable();
        assert_eq!(names, expected);
        assert!(rows.iter().all(|r| r.len() == 2 && !r[1].is_empty()));
    }

    #[test]
    fn inputs_are_described_for_the_dialog() {
        assert_eq!(
            describe_input(&RedactionInput::parse_lines("3", "scope").unwrap()),
            "lines 3 (scope)"
        );
        assert_eq!(
            describe_input(&RedactionInput::parse_text("abc 123", "secret", true).unwrap()),
            "text \"abc 123\" (secret)"
        );
        assert_eq!(
            describe_input(&RedactionInput::parse_span("1 2 damn", "language", false).unwrap()),
            "span 1 2 \"damn\" (language, no mark)"
        );
    }
}
