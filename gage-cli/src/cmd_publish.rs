//! `gage publish`: the pending-publish life cycle commands.
//!
//! `init` stages a publish of a session; `redact`, `status`, `undo`,
//! `refresh`, the `filter` commands, `abort`, and `apply` follow.
//! `apply` is the only command that writes the store. The source session is resolved
//! through the session command's `--source` and `--stored`, the way
//! `gage scan` resolves sessions: a `--stored` session is used as is,
//! and a native session is added to the store first so it has an
//! object id and commit for `origin`.

use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::io;
use std::sync::Arc;

use clap::{Args, Subcommand};
use cliclack as cli;
use gage_core::style::IdHighlighter;
use gage_core::uuid::short_uuid;
use gage_publish::{
    Candidate, Filter, FilterEntry, FilterSpec, MARKER, NAMED_FILTERS, PublishDefinition,
    PublishError, Publisher, REASONS, RedactionInput, TextEntry,
};
use gage_session::{Driver, NativeSession, SourceOptions};
use gage_store::url::{LineSelection, parse_session_origin};
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

#[derive(Subcommand)]
pub enum PublishCommand {
    /// Start a publish of a session
    ///
    /// Stages a pending publish and prints the new session ID, the
    /// value the other publish commands take as their SESSION
    /// argument.
    Init(InitArgs),

    /// List pending publish sessions
    List,

    /// Manage text redactions of a pending publish
    Redact(RedactArgs),

    /// Manage segments of a pending publish
    Segment(SegmentArgs),

    /// Show filters, segments, and redactions of a pending publish
    Status(StatusArgs),

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
pub struct InitArgs {
    /// Session to publish (tag, ID, or prefix)
    #[arg(value_name = "SESSION", required = true)]
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
    #[arg(long, exclusive = true)]
    list_filters: bool,
}

#[derive(Args)]
pub struct RedactArgs {
    #[command(subcommand)]
    command: RedactCommand,
}

#[derive(Subcommand)]
pub enum RedactCommand {
    /// Add a text redaction to a pending publish
    ///
    /// Literal is matched case-insensitively against every text value
    /// of session, and a literal that matches nothing is an error.
    Add(RedactAddArgs),

    /// Change a text redaction of a pending publish
    ///
    /// Literal, reason, and mark change only when given.
    Edit(RedactEditArgs),

    /// Remove text redactions from a pending publish
    ///
    /// Staged session is rebuilt from original with remaining
    /// redactions. A literal a named filter found is allowed once
    /// removed: filter does not record it again.
    Remove(RedactRemoveArgs),

    /// List text values of a pending publish open to redaction
    ///
    /// One value per line as "LINE POINTER TEXT", with dropped lines
    /// and left-out values omitted. A value that holds a line break or
    /// runs past terminal width is listed as "LINE POINTER" with its
    /// text indented on lines that follow. With --applied, redacted
    /// text is struck through in yellow and followed by its marker.
    Candidate(CandidateArgs),

    /// List text redactions of a pending publish
    ///
    /// One row per redaction: its ID, the text, the reason, the
    /// author, and the number of instances. The ID is what redact
    /// edit and redact remove take.
    List(RedactListArgs),
}

#[derive(Args)]
pub struct RedactAddArgs {
    /// Pending publish session to redact (ID or prefix)
    #[arg(value_name = "SESSION", required = true)]
    session: Option<String>,

    /// Text to redact everywhere
    ///
    /// Read from a file as @PATH.
    #[arg(value_name = "TEXT", required = true)]
    text: Option<String>,

    /// Reason for redaction (default custom)
    ///
    /// A reason code; see --list-reasons.
    #[arg(short, long, value_name = "REASON")]
    reason: Option<String>,

    /// Remove redacted text without a marker
    ///
    /// Without it a redacted span is replaced by "[REDACTED]"; with it
    /// span is removed and surrounding whitespace is normalized.
    #[arg(short = 'n', long)]
    no_mark: bool,

    /// Skip confirmation prompt
    #[arg(short, long)]
    yes: bool,

    /// Show reason codes and exit
    #[arg(long, exclusive = true)]
    list_reasons: bool,
}

#[derive(Args)]
pub struct RedactEditArgs {
    /// Pending publish session to change (ID or prefix)
    #[arg(value_name = "SESSION")]
    session: String,

    /// Redaction to change
    ///
    /// An ID as status lists it.
    #[arg(value_name = "ID")]
    id: u64,

    /// New text to redact everywhere
    ///
    /// Read from a file as @PATH.
    #[arg(value_name = "TEXT")]
    text: Option<String>,

    /// New reason for redaction
    ///
    /// A reason code; see redact add --list-reasons.
    #[arg(short, long, value_name = "REASON")]
    reason: Option<String>,

    /// Replace redacted text with a marker
    #[arg(short, long, conflicts_with = "no_mark")]
    mark: bool,

    /// Remove redacted text without a marker
    #[arg(short = 'n', long)]
    no_mark: bool,

    /// Skip confirmation prompt
    #[arg(short, long)]
    yes: bool,
}

#[derive(Args)]
pub struct RedactRemoveArgs {
    /// Pending publish session to change (ID or prefix)
    #[arg(value_name = "SESSION")]
    session: String,

    /// Redactions to remove
    ///
    /// IDs as status lists them.
    #[arg(value_name = "ID", required = true)]
    ids: Vec<u64>,

    /// Skip confirmation prompt
    #[arg(short, long)]
    yes: bool,
}

#[derive(Args)]
pub struct CandidateArgs {
    /// Pending publish session to list (ID or prefix)
    #[arg(value_name = "SESSION")]
    session: String,

    /// Show redactions applied to text
    #[arg(long)]
    applied: bool,
}

#[derive(Args)]
pub struct SegmentArgs {
    #[command(subcommand)]
    command: SegmentCommand,
}

#[derive(Subcommand)]
pub enum SegmentCommand {
    /// Add a segment to a pending publish
    ///
    /// A segment is lines of original session that publication keeps,
    /// under a description. Once any segment has lines, every line
    /// outside every segment is dropped. A segment without lines drops
    /// nothing.
    Add(SegmentAddArgs),

    /// Change a segment of a pending publish
    ///
    /// Lines change only when given.
    Edit(SegmentEditArgs),

    /// Remove a segment from a pending publish
    Remove(SegmentRemoveArgs),

    /// List segments of a pending publish
    ///
    /// One row per segment: its ID, the author, the lines, and the
    /// description. The ID is what segment edit and segment remove
    /// take.
    List(SegmentListArgs),
}

#[derive(Args)]
pub struct SegmentAddArgs {
    /// Pending publish session to change (ID or prefix)
    #[arg(value_name = "SESSION")]
    session: String,

    /// Description of segment
    ///
    /// Read from a file as @PATH.
    #[arg(value_name = "TEXT")]
    text: String,

    /// Lines segment keeps
    ///
    /// Original session lines as "12-20,31": lines or inclusive ranges
    /// separated by commas.
    #[arg(short, long, value_name = "LINES")]
    lines: Option<LineSelection>,

    /// Skip confirmation prompt
    #[arg(short, long)]
    yes: bool,
}

#[derive(Args)]
pub struct SegmentEditArgs {
    /// Pending publish session to change (ID or prefix)
    #[arg(value_name = "SESSION")]
    session: String,

    /// Segment to change
    ///
    /// An ID as status lists it.
    #[arg(value_name = "ID")]
    id: u64,

    /// New description of segment
    ///
    /// Read from a file as @PATH.
    #[arg(value_name = "TEXT")]
    text: String,

    /// Lines segment keeps
    ///
    /// Original session lines as "12-20,31": lines or inclusive ranges
    /// separated by commas.
    #[arg(short, long, value_name = "LINES")]
    lines: Option<LineSelection>,

    /// Skip confirmation prompt
    #[arg(short, long)]
    yes: bool,
}

#[derive(Args)]
pub struct SegmentRemoveArgs {
    /// Pending publish session to change (ID or prefix)
    #[arg(value_name = "SESSION")]
    session: String,

    /// Segment to remove
    ///
    /// An ID as status lists it.
    #[arg(value_name = "ID")]
    id: u64,

    /// Skip confirmation prompt
    #[arg(short, long)]
    yes: bool,
}

#[derive(Args)]
pub struct StatusArgs {
    /// Pending publish session to show (ID or prefix)
    #[arg(value_name = "SESSION")]
    session: String,
}

#[derive(Args)]
pub struct RefreshArgs {
    /// Pending publish session to rebuild (ID or prefix)
    #[arg(value_name = "SESSION")]
    session: String,

    /// Skip confirmation prompt
    #[arg(short, long)]
    yes: bool,
}

#[derive(Args)]
pub struct AbortArgs {
    /// Pending publish session to abort (ID or prefix)
    #[arg(value_name = "SESSION")]
    session: String,

    /// Skip confirmation prompt
    #[arg(short, long)]
    yes: bool,
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

    /// Publish session even when no filter or segment changed it
    #[arg(long)]
    force: bool,

    /// Skip confirmation prompt
    #[arg(short, long)]
    yes: bool,
}

/// The paragraph `init --filter` and `filter add` share on what a
/// filter argument is.
const FILTER_HELP: &str = "A filter name; see --list-filters. Filter names are \"pii\", \
\"language\", \"secret\", \"system\", \"harness\", and \"custom\". \"custom\" takes --prompt as \
its whole instruction, so it is added with filter add.";

/// The `filter` group declares the `--source` and `--stored` override
/// itself: none of its commands takes them, so the group capturing a
/// flag given before the command is the rejection wanted.
#[derive(Args)]
pub struct FilterArgs {
    #[command(subcommand)]
    command: FilterCommand,
}

#[derive(Subcommand)]
pub enum FilterCommand {
    /// Add a filter to a pending publish
    ///
    /// A named filter's deterministic rules run at once. A filter
    /// already on the list is an error.
    Add(FilterAddArgs),

    /// Change instructions of a filter on a pending publish
    ///
    /// A prompt is instructions, not rules, so nothing is rebuilt.
    Edit(FilterEditArgs),

    /// Remove a filter from a pending publish
    ///
    /// Filter's redactions are removed with it. A literal allowed
    /// earlier stays allowed if filter is added again.
    Remove(FilterRemoveArgs),

    /// Run named filters of a pending publish
    ///
    /// Filter's earlier redactions are replaced by what its rules find
    /// now.
    Run(FilterRunArgs),

    /// List filters of a pending publish
    List(FilterListArgs),
}

#[derive(Args)]
pub struct FilterAddArgs {
    /// Pending publish session to change (ID or prefix)
    #[arg(value_name = "SESSION", required = true)]
    session: Option<String>,

    /// Filter to add
    #[arg(
        value_name = "FILTER",
        required = true,
        long_help = format!("Filter to add\n\n{FILTER_HELP}")
    )]
    filter: Option<String>,

    /// Instructions for filter
    ///
    /// Read from a file as @PATH. Required for "custom", whose prompt
    /// is its whole instruction; for any other filter it adds to
    /// filter's own instructions.
    #[arg(short, long, value_name = "TEXT")]
    prompt: Option<String>,

    /// Skip confirmation prompt
    #[arg(short, long)]
    yes: bool,

    /// Show available named filters and exit
    #[arg(long, exclusive = true)]
    list_filters: bool,
}

#[derive(Args)]
pub struct FilterEditArgs {
    /// Pending publish session to change (ID or prefix)
    #[arg(value_name = "SESSION")]
    session: String,

    /// Filter to change
    ///
    /// A filter name on the list.
    #[arg(value_name = "FILTER")]
    filter: String,

    /// Instructions for filter
    ///
    /// Read from a file as @PATH. For "custom", prompt is its whole
    /// instruction; for any other filter it adds to filter's own
    /// instructions.
    #[arg(short, long, value_name = "TEXT")]
    prompt: String,

    /// Skip confirmation prompt
    #[arg(short, long)]
    yes: bool,
}

#[derive(Args)]
pub struct FilterRemoveArgs {
    /// Pending publish session to change (ID or prefix)
    #[arg(value_name = "SESSION")]
    session: String,

    /// Filter to remove
    ///
    /// A filter name on the list.
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

#[derive(Args)]
pub struct RedactListArgs {
    /// Pending publish session to show (ID or prefix)
    #[arg(value_name = "SESSION")]
    session: String,
}

#[derive(Args)]
pub struct SegmentListArgs {
    /// Pending publish session to show (ID or prefix)
    #[arg(value_name = "SESSION")]
    session: String,
}

pub async fn run(args: PublishArgs) -> Result<(), CliError> {
    match args.command {
        PublishCommand::Init(args) => init(args),
        PublishCommand::List => list(),
        PublishCommand::Redact(args) => match args.command {
            RedactCommand::Add(args) => redact_add(args),
            RedactCommand::Edit(args) => redact_edit(args),
            RedactCommand::Remove(args) => redact_remove(args),
            RedactCommand::Candidate(args) => candidate(args),
            RedactCommand::List(args) => redact_list(args),
        },
        PublishCommand::Segment(args) => match args.command {
            SegmentCommand::Add(args) => segment_add(args),
            SegmentCommand::Edit(args) => segment_edit(args),
            SegmentCommand::Remove(args) => segment_remove(args),
            SegmentCommand::List(args) => segment_list(args),
        },
        PublishCommand::Status(args) => status(args),
        PublishCommand::Refresh(args) => refresh(args),
        PublishCommand::Filter(args) => match args.command {
            FilterCommand::Add(args) => filter_add(args),
            FilterCommand::Edit(args) => filter_edit(args),
            FilterCommand::Remove(args) => filter_remove(args),
            FilterCommand::Run(args) => filter_run(args),
            FilterCommand::List(args) => filter_list(args),
        },
        PublishCommand::Abort(args) => abort(args),
        PublishCommand::Apply(args) => apply(args),
    }
}

/// List the pending publish working trees, read from the file system,
/// newest first. Ids are highlighted against the stored sessions plus
/// the pending set, the peers a pending prefix resolves against.
fn list() -> Result<(), CliError> {
    let fail = |e: String| CliError::message(format!("gage publish list: {e}"));
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

    let header: Vec<String> = ["Id", "Origin", "Title", "Redactions", "Modified"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let publisher = Publisher::new(&store);
    let rows = pending_rows(pending, &highlighter, |id| redaction_count(&publisher, id))
        .map_err(|e| fail(e.to_string()))?;
    let table = Table::from_iter(std::iter::once(header).chain(rows))
        .with(Style::rounded())
        .modify(Rows::first(), style::tty(Color::FG_BRIGHT_YELLOW))
        .modify(Columns::one(3), Alignment::right())
        .modify(Columns::one(2).not(Rows::first()), style::dim())
        .modify(Columns::one(4).not(Rows::first()), style::dim())
        .to_string();
    println!("{table}");
    Ok(())
}

/// One row per pending publish, newest first: the highlighted short
/// id, the `origin` and title from `attrs.json`, the redaction count
/// `redactions` gives for the id, and the age of the working tree.
fn pending_rows(
    mut pending: Vec<PendingPublish>,
    highlighter: &IdHighlighter,
    redactions: impl Fn(&str) -> Result<String, PublishError>,
) -> Result<Vec<Vec<String>>, PublishError> {
    pending.sort_by_key(|p| Reverse(p.modified_ms));
    pending
        .iter()
        .map(|p| {
            Ok(vec![
                highlighter.short(&p.id),
                origin_cell(&p.attrs.origin, highlighter),
                p.attrs.summary.title.clone().unwrap_or_default(),
                redactions(&p.id)?,
                format_elapsed_ms(p.modified_ms),
            ])
        })
        .collect()
}

/// The redaction count of the pending publish `id` from its
/// definition, or no count when the definition was deleted.
fn redaction_count(publisher: &Publisher<'_>, id: &str) -> Result<String, PublishError> {
    match publisher.definition(id) {
        Ok(definition) => Ok(definition.text.len().to_string()),
        Err(PublishError::Store(StoreError::ObjectNotFound(_))) => Ok(String::new()),
        Err(e) => Err(e),
    }
}

/// A derived origin, `session:<id>@<commit>`, as the highlighted short
/// id of the original session, a space, and the first 8 characters of
/// the commit dimmed. Any other origin is shown as stored.
fn origin_cell(origin: &str, highlighter: &IdHighlighter) -> String {
    let Ok((id, commit)) = parse_session_origin(origin) else {
        return origin.to_string();
    };
    let commit: String = commit.chars().take(8).collect();
    format!("{} {}", highlighter.short(id), console::style(commit).dim())
}

/// A session origin for the status table: the full original session
/// id, a space, and the first 8 characters of the commit dimmed. Any
/// other origin is shown as stored.
fn origin_status_cell(origin: &str) -> String {
    let Ok((id, commit)) = parse_session_origin(origin) else {
        return origin.to_string();
    };
    let commit: String = commit.chars().take(8).collect();
    format!("{} {}", id, console::style(commit).dim())
}

/// Stage a publish after confirmation. The original is resolved
/// before the dialog opens, so a bad argument fails without a prompt,
/// and a native session is added to the store only after the user
/// confirms.
fn init(args: InitArgs) -> Result<(), CliError> {
    let fail = |e: String| CliError::message(format!("gage publish init: {e}"));
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
        .map(|name| parse_filter("gage publish init", "--filter", name, None))
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
        report_filter_redactions(&publisher, &outcome.id)
            .map_err(|e| DialogError::Failed(e.to_string()))?;
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
/// A filter argument: a filter name, with the prompt read through
/// `@PATH`. A name outside the named filters is a usage error in
/// clap's layout, since the argument accepts any text and the names
/// are checked here; a filter that requires a prompt and has none is
/// a plain error.
fn parse_filter(
    command: &str,
    option: &str,
    name: &str,
    prompt: Option<&str>,
) -> Result<FilterSpec, CliError> {
    if !NAMED_FILTERS.contains(&name) {
        return Err(CliError::invalid_value(
            command,
            option,
            name,
            NAMED_FILTERS,
        ));
    }
    let prompt = prompt
        .map(|p| text_value(command, "--prompt", p))
        .transpose()
        .map_err(|e| CliError::message(format!("{command}: {e}")))?;
    FilterSpec::new(name, prompt.map(|p| p.trim_end().to_string()))
        .map_err(|e| CliError::message(format!("{command}: {e}")))
}

/// The `Filters` step of the dialog: one line per filter, or `none`.
fn filters_step(filters: &[FilterSpec]) -> io::Result<()> {
    if filters.is_empty() {
        return dialog::step_value("Filters", "none");
    }
    dialog::step_list("Filters", filters.iter().map(|f| f.to_string()))
}

/// Report what the named filters found: the text redactions each
/// recorded and the spans they produce.
fn report_filter_redactions(publisher: &Publisher<'_>, id: &str) -> Result<(), PublishError> {
    let definition = publisher.definition(id)?;
    let counts = publisher.instances(id)?;
    let by_filter: Vec<(String, usize, usize)> = definition
        .filters
        .iter()
        .map(|f| f.name.clone())
        .map(|name| {
            let author = format!("filter:{name}");
            let mine: Vec<&TextEntry> = definition
                .text
                .iter()
                .filter(|t| t.author == author)
                .collect();
            let instances = mine
                .iter()
                .map(|t| counts.get(&t.id).copied().unwrap_or(0))
                .sum();
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
    )?;
    Ok(())
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
    format!(
        "text {:?} ({}{})",
        input.literal,
        input.reason,
        no_mark(input.mark)
    )
}

/// The suffix a dialog line carries for a redaction without a marker.
fn no_mark(mark: bool) -> &'static str {
    if mark { "" } else { ", no mark" }
}

/// Show the filters, segments, and text redactions of a pending
/// publish, with the spans each redaction produces.
fn status(args: StatusArgs) -> Result<(), CliError> {
    let fail = |e: String| CliError::message(format!("gage publish status: {e}"));
    let store = Store::open(&gage_store::store_path()).map_err(|e| fail(e.to_string()))?;
    let publisher = Publisher::new(&store);
    let pending = publisher
        .store()
        .get(&args.session)
        .map_err(|e| fail(e.to_string()))?;
    let definition = publisher
        .definition(&pending.id)
        .map_err(|e| fail(e.to_string()))?;
    let counts = publisher
        .instances(&pending.id)
        .map_err(|e| fail(e.to_string()))?;

    let filters: Vec<String> = definition.filters.iter().map(filter_cell).collect();
    let rows = [
        ["Session".to_string(), pending.id.clone()],
        [
            "Origin".to_string(),
            origin_status_cell(&pending.attrs.origin),
        ],
        [
            "Filters".to_string(),
            if filters.is_empty() {
                "none".to_string()
            } else {
                filters.join(", ")
            },
        ],
    ];
    println!("{}", properties_table(rows));
    if !definition.segments.is_empty() {
        println!("{}", segments_table(&definition));
    }
    if !definition.allow.is_empty() {
        let allowed: Vec<String> = definition
            .allow
            .iter()
            .map(|a| format!("{:?} ({})", a.literal, a.author))
            .collect();
        println!("Allowed: {}", allowed.join(", "));
    }
    if definition.text.is_empty() {
        println!("No redactions");
        return Ok(());
    }
    println!("{}", redactions_table(&definition, &counts));
    Ok(())
}

/// The segments of a pending publish as a table: id, author, lines,
/// and description. Assumes at least one segment.
fn segments_table(definition: &PublishDefinition) -> String {
    let header: Vec<String> = ["Id", "Author", "Lines", "Description"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let rows = definition.segments.iter().map(|s| {
        vec![
            s.id.to_string(),
            s.author.clone(),
            lines_cell(&s.lines),
            s.description.clone(),
        ]
    });
    records_table(header, rows)
}

/// The text redactions of a pending publish as a table: id, text,
/// reason, author, and instance count. Assumes at least one redaction.
fn redactions_table(definition: &PublishDefinition, counts: &BTreeMap<u64, usize>) -> String {
    let header: Vec<String> = ["Id", "Text", "Reason", "Author", "Instances"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let rows = definition.text.iter().map(|t| {
        vec![
            t.id.to_string(),
            format!("{}{}", t.literal, no_mark(t.mark)),
            t.reason.clone(),
            t.author.clone(),
            counts.get(&t.id).copied().unwrap_or(0).to_string(),
        ]
    });
    records_table(header, rows)
}

/// List the segments of a pending publish.
fn segment_list(args: SegmentListArgs) -> Result<(), CliError> {
    let fail = |e: String| CliError::message(format!("gage publish segment list: {e}"));
    let store = Store::open(&gage_store::store_path()).map_err(|e| fail(e.to_string()))?;
    let publisher = Publisher::new(&store);
    let pending = publisher
        .store()
        .get(&args.session)
        .map_err(|e| fail(e.to_string()))?;
    let definition = publisher
        .definition(&pending.id)
        .map_err(|e| fail(e.to_string()))?;
    if definition.segments.is_empty() {
        println!("No segments");
        return Ok(());
    }
    println!("{}", segments_table(&definition));
    Ok(())
}

/// List the text redactions of a pending publish.
fn redact_list(args: RedactListArgs) -> Result<(), CliError> {
    let fail = |e: String| CliError::message(format!("gage publish redact list: {e}"));
    let store = Store::open(&gage_store::store_path()).map_err(|e| fail(e.to_string()))?;
    let publisher = Publisher::new(&store);
    let pending = publisher
        .store()
        .get(&args.session)
        .map_err(|e| fail(e.to_string()))?;
    let definition = publisher
        .definition(&pending.id)
        .map_err(|e| fail(e.to_string()))?;
    let counts = publisher
        .instances(&pending.id)
        .map_err(|e| fail(e.to_string()))?;
    if definition.text.is_empty() {
        println!("No redactions");
        return Ok(());
    }
    println!("{}", redactions_table(&definition, &counts));
    Ok(())
}

/// A property table: labels down the left column, highlighted as a
/// heading.
fn properties_table(rows: impl IntoIterator<Item = [String; 2]>) -> String {
    Table::from_iter(rows)
        .with(Style::rounded().horizontals([]))
        .modify(Columns::first(), style::tty(Color::FG_BRIGHT_YELLOW))
        .to_string()
}

/// A status table: the header highlighted, the id column plain, the
/// other columns dimmed, and the last column right-aligned.
fn records_table(header: Vec<String>, rows: impl Iterator<Item = Vec<String>>) -> String {
    let width = header.len();
    Table::from_iter(std::iter::once(header).chain(rows))
        .with(Style::rounded())
        .modify(Rows::first(), style::tty(Color::FG_BRIGHT_YELLOW))
        .modify(Columns::one(width - 1), Alignment::right())
        .modify(Columns::new(1..width - 1).not(Rows::first()), style::dim())
        .to_string()
}

/// Add a text redaction after confirmation. The literal is checked
/// before the dialog opens.
fn redact_add(args: RedactAddArgs) -> Result<(), CliError> {
    let command = "gage publish redact add";
    let fail = |e: String| CliError::message(format!("{command}: {e}"));
    if args.list_reasons {
        list_reasons();
        return Ok(());
    }
    let (Some(session), Some(text)) = (args.session.as_deref(), args.text.as_deref()) else {
        return Err(fail("SESSION and TEXT are required".into()));
    };
    let text = text_value(command, "TEXT", text).map_err(|e| fail(e.to_string()))?;
    let reason = args.reason.as_deref().unwrap_or("custom");
    let input = RedactionInput::parse_text(text.trim_end(), reason, !args.no_mark)
        .map_err(|e| fail(e.to_string()))?;
    let store = Store::open(&gage_store::store_path()).map_err(|e| fail(e.to_string()))?;
    let publisher = Publisher::new(&store);
    let pending = publisher
        .store()
        .get(session)
        .map_err(|e| fail(e.to_string()))?;
    let author = resolve_author(None);

    dialog::run("Add redaction", || {
        dialog::step_value("Session", short_uuid(&pending.id))?;
        dialog::step_value("Redaction", describe_input(&input))?;
        if !args.yes {
            let confirmed = cli::confirm("Continue?").initial_value(true).interact()?;
            if !confirmed {
                return Err(DialogError::Canceled);
            }
        }
        let applied = publisher
            .redact(&pending.id, std::slice::from_ref(&input), &author)
            .map_err(|e| DialogError::Failed(e.to_string()))?;
        let applied = applied
            .into_iter()
            .next()
            .expect("one input gives one applied redaction");
        Ok(format!(
            "Added redaction {} ({})",
            applied.id,
            count(applied.instances, "instance")
        )
        .into())
    })
}

/// Change a text redaction after confirmation. The id is resolved
/// before the dialog opens, and a call that changes nothing is an
/// error.
fn redact_edit(args: RedactEditArgs) -> Result<(), CliError> {
    let command = "gage publish redact edit";
    let fail = |e: String| CliError::message(format!("{command}: {e}"));
    let text = args
        .text
        .as_deref()
        .map(|t| text_value(command, "TEXT", t))
        .transpose()
        .map_err(|e| fail(e.to_string()))?
        .map(|t| t.trim_end().to_string());
    let mark = match (args.mark, args.no_mark) {
        (true, _) => Some(true),
        (_, true) => Some(false),
        _ => None,
    };
    if text.is_none() && args.reason.is_none() && mark.is_none() {
        return Err(fail(
            "nothing to change; give TEXT, --reason, --mark, or --no-mark".into(),
        ));
    }
    let store = Store::open(&gage_store::store_path()).map_err(|e| fail(e.to_string()))?;
    let publisher = Publisher::new(&store);
    let pending = publisher
        .store()
        .get(&args.session)
        .map_err(|e| fail(e.to_string()))?;
    let definition = publisher
        .definition(&pending.id)
        .map_err(|e| fail(e.to_string()))?;
    let target = definition
        .text
        .iter()
        .find(|t| t.id == args.id)
        .ok_or_else(|| fail(format!("no redaction with id {}", args.id)))?;
    let mut changes = Vec::new();
    if let Some(text) = &text {
        changes.push(format!("text {text:?}"));
    }
    if let Some(reason) = &args.reason {
        changes.push(format!("reason {reason}"));
    }
    if let Some(mark) = mark {
        changes.push(if mark { "mark" } else { "no mark" }.to_string());
    }

    dialog::run("Edit redaction", || {
        dialog::step_value("Session", short_uuid(&pending.id))?;
        dialog::step_value("Redaction", describe_entry(target))?;
        dialog::step_list("Changes", changes.iter().cloned())?;
        if !args.yes {
            let confirmed = cli::confirm("Continue?").initial_value(true).interact()?;
            if !confirmed {
                return Err(DialogError::Canceled);
            }
        }
        let applied = publisher
            .redact_edit(
                &pending.id,
                args.id,
                text.as_deref(),
                args.reason.as_deref(),
                mark,
            )
            .map_err(|e| DialogError::Failed(e.to_string()))?;
        Ok(format!(
            "Changed redaction {} ({})",
            applied.id,
            count(applied.instances, "instance")
        )
        .into())
    })
}

/// Remove text redactions after confirmation. Every id is checked
/// against the definition before the dialog opens.
fn redact_remove(args: RedactRemoveArgs) -> Result<(), CliError> {
    let fail = |e: String| CliError::message(format!("gage publish redact remove: {e}"));
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
        let t = definition
            .text
            .iter()
            .find(|t| t.id == *id)
            .ok_or_else(|| fail(format!("no redaction with id {id}")))?;
        targets.push(t.clone());
    }
    let author = resolve_author(None);

    dialog::run("Remove redactions", || {
        dialog::step_value("Session", short_uuid(&pending.id))?;
        dialog::step_list("Redactions", targets.iter().map(describe_entry))?;
        if targets.iter().any(|t| t.author.starts_with("filter:")) {
            cli::log::remark(
                "A filter's literal is allowed once removed: the filter does not record it again",
            )?;
        }
        if !args.yes {
            let confirmed = cli::confirm("Continue?").initial_value(true).interact()?;
            if !confirmed {
                return Err(DialogError::Canceled);
            }
        }
        let removed = publisher
            .redact_remove(&pending.id, &args.ids, &author)
            .map_err(|e| DialogError::Failed(e.to_string()))?;
        Ok(format!("Removed {}", count(removed.len(), "redaction")).into())
    })
}

/// A text redaction as a dialog lists it: its id, literal, reason,
/// mark, and author.
fn describe_entry(t: &TextEntry) -> String {
    format!(
        "{} {:?} ({}{}, {})",
        t.id,
        t.literal,
        t.reason,
        no_mark(t.mark),
        t.author
    )
}

/// List the text values open to redaction, one per line where the
/// value fits, else as a header with the text indented below it.
fn candidate(args: CandidateArgs) -> Result<(), CliError> {
    let fail = |e: String| CliError::message(format!("gage publish redact candidate: {e}"));
    let store = Store::open(&gage_store::store_path()).map_err(|e| fail(e.to_string()))?;
    let publisher = Publisher::new(&store);
    let pending = publisher
        .store()
        .get(&args.session)
        .map_err(|e| fail(e.to_string()))?;
    let candidates = publisher
        .candidates(&pending.id)
        .map_err(|e| fail(e.to_string()))?;
    let width = console::Term::stdout().size().1 as usize;
    for c in &candidates {
        let head = format!("{} {}", c.line, c.pointer);
        let text = if args.applied {
            applied_text(c)
        } else {
            c.text.clone()
        };
        let one_line =
            !c.text.contains('\n') && head.chars().count() + 1 + c.text.chars().count() <= width;
        let head = console::style(&head).dim();
        if one_line {
            println!("{head} {text}");
        } else {
            println!("{head}");
            for line in text.lines() {
                println!("  {line}");
            }
        }
    }
    Ok(())
}

/// A candidate's text with each redacted span struck through in
/// yellow and, where a marker replaces it, the marker in yellow after
/// it.
fn applied_text(c: &Candidate) -> String {
    let slice = |range: std::ops::Range<usize>| {
        c.text
            .get(range)
            .expect("a candidate's spans lie within its text on character boundaries")
    };
    let mut out = String::with_capacity(c.text.len());
    let mut cursor = 0;
    for &(start, end, mark) in &c.spans {
        out.push_str(slice(cursor..start));
        out.push_str(
            &console::style(slice(start..end + 1))
                .yellow()
                .strikethrough()
                .to_string(),
        );
        if mark {
            out.push_str(&console::style(MARKER).yellow().to_string());
        }
        cursor = end + 1;
    }
    out.push_str(slice(cursor..c.text.len()));
    out
}

/// Add a segment after confirmation.
fn segment_add(args: SegmentAddArgs) -> Result<(), CliError> {
    let command = "gage publish segment add";
    let fail = |e: String| CliError::message(format!("{command}: {e}"));
    let text = text_value(command, "TEXT", &args.text).map_err(|e| fail(e.to_string()))?;
    let text = text.trim_end().to_string();
    let lines = args
        .lines
        .as_ref()
        .map(ranges)
        .transpose()?
        .unwrap_or_default();
    let store = Store::open(&gage_store::store_path()).map_err(|e| fail(e.to_string()))?;
    let publisher = Publisher::new(&store);
    let pending = publisher
        .store()
        .get(&args.session)
        .map_err(|e| fail(e.to_string()))?;
    let author = resolve_author(None);

    dialog::run("Add segment", || {
        dialog::step_value("Session", short_uuid(&pending.id))?;
        dialog::step_value("Description", value_cell(&text))?;
        dialog::step_value("Lines", lines_cell(&lines))?;
        if !args.yes {
            let confirmed = cli::confirm("Continue?").initial_value(true).interact()?;
            if !confirmed {
                return Err(DialogError::Canceled);
            }
        }
        let id = publisher
            .segment_add(&pending.id, &text, &lines, &author)
            .map_err(|e| DialogError::Failed(e.to_string()))?;
        Ok(format!("Added segment {id}").into())
    })
}

/// Change a segment after confirmation. The id is resolved before the
/// dialog opens.
fn segment_edit(args: SegmentEditArgs) -> Result<(), CliError> {
    let command = "gage publish segment edit";
    let fail = |e: String| CliError::message(format!("{command}: {e}"));
    let text = text_value(command, "TEXT", &args.text).map_err(|e| fail(e.to_string()))?;
    let text = text.trim_end().to_string();
    let lines = args.lines.as_ref().map(ranges).transpose()?;
    let store = Store::open(&gage_store::store_path()).map_err(|e| fail(e.to_string()))?;
    let publisher = Publisher::new(&store);
    let pending = publisher
        .store()
        .get(&args.session)
        .map_err(|e| fail(e.to_string()))?;
    let definition = publisher
        .definition(&pending.id)
        .map_err(|e| fail(e.to_string()))?;
    let target = definition
        .segments
        .iter()
        .find(|s| s.id == args.id)
        .ok_or_else(|| fail(format!("no segment with id {}", args.id)))?;

    dialog::run("Edit segment", || {
        dialog::step_value("Session", short_uuid(&pending.id))?;
        dialog::step_value(
            "Segment",
            format!("{} {}", target.id, value_cell(&target.description)),
        )?;
        dialog::step_value("Description", value_cell(&text))?;
        if let Some(lines) = &lines {
            dialog::step_value("Lines", lines_cell(lines))?;
        }
        if !args.yes {
            let confirmed = cli::confirm("Continue?").initial_value(true).interact()?;
            if !confirmed {
                return Err(DialogError::Canceled);
            }
        }
        let segment = publisher
            .segment_edit(&pending.id, args.id, Some(&text), lines.as_deref())
            .map_err(|e| DialogError::Failed(e.to_string()))?;
        Ok(format!("Changed segment {}", segment.id).into())
    })
}

/// Remove a segment after confirmation. The id is resolved before the
/// dialog opens.
fn segment_remove(args: SegmentRemoveArgs) -> Result<(), CliError> {
    let fail = |e: String| CliError::message(format!("gage publish segment remove: {e}"));
    let store = Store::open(&gage_store::store_path()).map_err(|e| fail(e.to_string()))?;
    let publisher = Publisher::new(&store);
    let pending = publisher
        .store()
        .get(&args.session)
        .map_err(|e| fail(e.to_string()))?;
    let definition = publisher
        .definition(&pending.id)
        .map_err(|e| fail(e.to_string()))?;
    let target = definition
        .segments
        .iter()
        .find(|s| s.id == args.id)
        .ok_or_else(|| fail(format!("no segment with id {}", args.id)))?;

    dialog::run("Remove segment", || {
        dialog::step_value("Session", short_uuid(&pending.id))?;
        dialog::step_value(
            "Segment",
            format!("{} {}", target.id, value_cell(&target.description)),
        )?;
        dialog::step_value("Lines", lines_cell(&target.lines))?;
        if !args.yes {
            let confirmed = cli::confirm("Continue?").initial_value(true).interact()?;
            if !confirmed {
                return Err(DialogError::Canceled);
            }
        }
        let segment = publisher
            .segment_remove(&pending.id, args.id)
            .map_err(|e| DialogError::Failed(e.to_string()))?;
        Ok(format!("Removed segment {}", segment.id).into())
    })
}

/// The inclusive line ranges of `selection`.
fn ranges(selection: &LineSelection) -> Result<Vec<(u32, u32)>, CliError> {
    let line = |n: u64| u32::try_from(n).map_err(|e| CliError::message(format!("line {n}: {e}")));
    selection
        .ranges()
        .iter()
        .map(|&(start, end)| Ok((line(start)?, line(end)?)))
        .collect()
}

/// Line ranges as a dialog shows them, `12-20,31`, or `none`.
fn lines_cell(lines: &[(u32, u32)]) -> String {
    if lines.is_empty() {
        return "none".to_string();
    }
    lines
        .iter()
        .map(|(start, end)| {
            if start == end {
                start.to_string()
            } else {
                format!("{start}-{end}")
            }
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// Rebuild a pending publish from its definition after confirmation.
fn refresh(args: RefreshArgs) -> Result<(), CliError> {
    let fail = |e: String| CliError::message(format!("gage publish refresh: {e}"));
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
                "{}, {}, {}",
                count(definition.filters.len(), "filter"),
                count(definition.segments.len(), "segment"),
                count(definition.text.len(), "redaction")
            ),
        )?;
        if !args.yes {
            let confirmed = cli::confirm("Continue?").initial_value(true).interact()?;
            if !confirmed {
                return Err(DialogError::Canceled);
            }
        }
        let instances = publisher
            .refresh(&pending.id)
            .map_err(|e| DialogError::Failed(e.to_string()))?;
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
    let command = "gage publish filter add";
    let fail = |e: String| CliError::message(format!("{command}: {e}"));
    if args.list_filters {
        list_filters();
        return Ok(());
    }
    let (Some(session), Some(filter)) = (args.session.as_deref(), args.filter.as_deref()) else {
        return Err(fail("SESSION and FILTER are required".into()));
    };
    let filter = parse_filter(command, "FILTER", filter, args.prompt.as_deref())?;
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
        publisher
            .filter_add(&pending.id, &filter)
            .map_err(|e| DialogError::Failed(e.to_string()))?;
        report_filter_redactions(&publisher, &pending.id)
            .map_err(|e| DialogError::Failed(e.to_string()))?;
        Ok(format!("Added filter {}", value_cell(&filter.to_string())).into())
    })
}

/// Change a filter's prompt after confirmation. The filter is resolved
/// before the dialog opens.
fn filter_edit(args: FilterEditArgs) -> Result<(), CliError> {
    let command = "gage publish filter edit";
    let fail = |e: String| CliError::message(format!("{command}: {e}"));
    let prompt = text_value(command, "--prompt", &args.prompt).map_err(|e| fail(e.to_string()))?;
    let prompt = prompt.trim_end().to_string();
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

    dialog::run("Edit publish filter", || {
        dialog::step_value("Session", short_uuid(&pending.id))?;
        dialog::step_value("Filter", &target)?;
        dialog::step_value("Prompt", value_cell(&prompt))?;
        if !args.yes {
            let confirmed = cli::confirm("Continue?").initial_value(true).interact()?;
            if !confirmed {
                return Err(DialogError::Canceled);
            }
        }
        publisher
            .filter_edit(&pending.id, &args.filter, Some(prompt.clone()))
            .map_err(|e| DialogError::Failed(e.to_string()))?;
        Ok(format!("Changed filter {target}").into())
    })
}

/// Remove a filter after confirmation. The selector is resolved
/// against the definition before the dialog opens.
fn filter_remove(args: FilterRemoveArgs) -> Result<(), CliError> {
    let fail = |e: String| CliError::message(format!("gage publish filter remove: {e}"));
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
    let author = entry.author();
    let redactions = definition
        .text
        .iter()
        .filter(|t| t.author == author)
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
    let fail = |e: String| CliError::message(format!("gage publish filter run: {e}"));
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
        publisher
            .filter_run(&pending.id, args.name.as_deref())
            .map_err(|e| DialogError::Failed(e.to_string()))?;
        report_filter_redactions(&publisher, &pending.id)
            .map_err(|e| DialogError::Failed(e.to_string()))?;
        Ok(format!("Ran {}", count(names.len(), "filter")).into())
    })
}

/// Print the filters of a pending publish as a table: the name or the
/// instruction on one line, its type, and the redactions it authored.
fn filter_list(args: FilterListArgs) -> Result<(), CliError> {
    let fail = |e: String| CliError::message(format!("gage publish filter list: {e}"));
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
    let header: Vec<String> = ["Filter", "Prompt", "Redactions"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let rows = definition.filters.iter().map(|f| {
        let author = f.author();
        let redactions = definition
            .text
            .iter()
            .filter(|t| t.author == author)
            .count();
        vec![
            filter_cell(f),
            f.prompt.clone().unwrap_or_default(),
            redactions.to_string(),
        ]
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

/// Delete a pending publish after confirmation. The id is resolved
/// before the dialog opens, so a bad argument fails without a prompt.
fn abort(args: AbortArgs) -> Result<(), CliError> {
    let fail = |e: String| CliError::message(format!("gage publish abort: {e}"));
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
    let fail = |e: String| CliError::message(format!("gage publish apply: {e}"));
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
        .verify(&pending.id, args.force)
        .map_err(|e| match e {
            PublishError::Unchanged => fail(format!("{e} (use --force to publish it unchanged)")),
            e => fail(e.to_string()),
        })?;
    let tags = TagStore::from(&store);
    if let Some(name) = &args.tag {
        tags.check_available(name, args.force_tag)
            .map_err(|e| match e {
                StoreError::TagExists { .. } => fail(format!("{e} (use --force-tag to move it)")),
                e => fail(e.to_string()),
            })?;
    }
    let instances: usize = publisher
        .instances(&pending.id)
        .map_err(|e| fail(e.to_string()))?
        .values()
        .sum();

    dialog::run("Apply publish", || {
        dialog::step_value("Session", short_uuid(&pending.id))?;
        dialog::step_value("Origin", &pending.attrs.origin)?;
        dialog::step_value(
            "Definition",
            format!(
                "{}, {}, {}, {}",
                count(definition.filters.len(), "filter"),
                count(definition.segments.len(), "segment"),
                count(definition.text.len(), "redaction"),
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
            .apply(&pending.id, args.force)
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
    let fail = |e: String| CliError::message(format!("gage publish init: {e}"));
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

    use clap::Parser;
    use gage_core::datetime::now_ms;
    use gage_store::{SessionAttrsRecord, SummaryAttrs};

    use super::*;
    use crate::{Cli, Command};

    /// An unknown filter name reports as clap reports a value outside
    /// a known set, naming the argument as its help does, the valid
    /// names, and the nearest one; an instruction is not a name.
    #[test]
    fn unknown_filter_name_is_a_usage_error_in_claps_layout() {
        let rendered =
            |command: &str, option: &str| match parse_filter(command, option, "secrets", None) {
                Err(CliError::Usage(e)) => {
                    assert_eq!(e.exit_code(), 2);
                    e.render().to_string()
                }
                Err(_) => panic!("not a usage error"),
                Ok(f) => panic!("{f} parsed"),
            };
        let text = rendered("gage publish init", "--filter");
        assert!(
            text.contains("invalid value 'secrets' for '--filter <FILTER>'"),
            "{text}"
        );
        assert!(
            text.contains("[possible values: pii, language, secret, system, harness, custom]"),
            "{text}"
        );
        assert!(text.contains("a similar value exists: 'secret'"), "{text}");
        let text = rendered("gage publish filter add", "FILTER");
        assert!(text.contains("for '<FILTER>'"), "{text}");
        assert!(parse_filter("gage publish init", "--filter", "pii", None).is_ok());
        let custom = parse_filter("gage publish filter add", "FILTER", "custom", None);
        assert!(
            matches!(custom, Err(CliError::Exit { message: Some(ref m), .. }) if m.contains("requires a prompt")),
            "custom without a prompt is a plain error"
        );
        assert!(
            parse_filter(
                "gage publish filter add",
                "FILTER",
                "custom",
                Some("Remove every duck")
            )
            .is_ok()
        );
    }

    /// The publish commands live under `gage publish`, not `gage
    /// session`. `init` keeps its own --source and --stored; the other
    /// commands take neither.
    #[test]
    fn publish_is_a_top_level_command() {
        Cli::try_parse_from(["gage", "publish", "list"]).unwrap();
        Cli::try_parse_from(["gage", "publish", "init", "--stored", "abc"]).unwrap();
        Cli::try_parse_from(["gage", "publish", "status", "abc"]).unwrap();
        // A list flag stands alone: the positionals it bypasses are
        // required otherwise
        Cli::try_parse_from(["gage", "publish", "init", "--list-filters"]).unwrap();
        Cli::try_parse_from(["gage", "publish", "filter", "add", "--list-filters"]).unwrap();
        assert!(Cli::try_parse_from(["gage", "publish", "init"]).is_err());
        assert!(Cli::try_parse_from(["gage", "publish", "filter", "add", "abc"]).is_err());
        assert!(Cli::try_parse_from(["gage", "publish", "init", "abc", "--list-filters"]).is_err());
        assert!(Cli::try_parse_from(["gage", "session", "publish", "list"]).is_err());
        for argv in [
            vec!["gage", "publish", "redact", "add", "abc", "hunter2"],
            vec!["gage", "publish", "redact", "add", "--list-reasons"],
            vec![
                "gage", "publish", "redact", "edit", "abc", "3", "--reason", "pii",
            ],
            vec!["gage", "publish", "redact", "remove", "abc", "3", "4"],
            vec!["gage", "publish", "redact", "candidate", "abc", "--applied"],
            vec![
                "gage", "publish", "segment", "add", "abc", "Reply", "--lines", "2-3,9",
            ],
            vec!["gage", "publish", "segment", "edit", "abc", "1", "Reply"],
            vec!["gage", "publish", "segment", "remove", "abc", "1"],
            vec![
                "gage", "publish", "filter", "edit", "abc", "pii", "--prompt", "x",
            ],
        ] {
            Cli::try_parse_from(&argv).unwrap_or_else(|e| panic!("{argv:?}: {e}"));
        }
        assert!(Cli::try_parse_from(["gage", "publish", "undo", "abc", "1"]).is_err());
        assert!(Cli::try_parse_from(["gage", "publish", "redact", "add", "abc"]).is_err());
        assert!(
            Cli::try_parse_from([
                "gage",
                "publish",
                "redact",
                "edit",
                "abc",
                "3",
                "--mark",
                "--no-mark"
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "gage", "publish", "segment", "add", "abc", "x", "--lines", "9-2"
            ])
            .is_err()
        );
        assert!(Cli::try_parse_from(["gage", "publish", "status", "--stored", "abc"]).is_err());
    }

    /// `apply --force` stands alone; `--force-tag` still needs `--tag`.
    #[test]
    fn apply_force_is_separate_from_force_tag() {
        let cli = Cli::try_parse_from(["gage", "publish", "apply", "abc", "--force"]).unwrap();
        let Command::Publish(PublishArgs {
            command: PublishCommand::Apply(args),
        }) = cli.command
        else {
            panic!("expected publish apply");
        };
        assert!(args.force);
        assert!(!args.force_tag);
        assert!(Cli::try_parse_from(["gage", "publish", "apply", "abc", "--force-tag"]).is_err());
    }

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
    /// title, the redaction count, and the age.
    #[test]
    fn pending_rows_are_newest_first_with_id_origin_count_and_age() {
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

        let rows = pending_rows(vec![older.clone(), newer.clone()], &highlighter, |id| {
            Ok(if id == newer.id {
                "3".to_string()
            } else {
                String::new()
            })
        })
        .unwrap();

        assert_eq!(rows.len(), 2);
        let plain = |cell: &str| console::strip_ansi_codes(cell).into_owned();
        assert_eq!(plain(&rows[0][0]), short_uuid(&newer.id));
        assert_eq!(
            plain(&rows[0][1]),
            format!("{} fedcba98", short_uuid(orig2))
        );
        assert_eq!(rows[0][2], "Fix the duck");
        assert_eq!(rows[0][3], "3");
        assert_eq!(rows[0][4], "5s ago");
        assert_eq!(plain(&rows[1][0]), short_uuid(&older.id));
        assert_eq!(
            plain(&rows[1][1]),
            format!("{} 01234567", short_uuid(orig1))
        );
        assert_eq!(rows[1][2], "");
        assert_eq!(rows[1][3], "", "a deleted definition shows no count");
        assert_eq!(rows[1][4], "1h ago");
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
        let mut expected = NAMED_FILTERS.to_vec();
        expected.sort_unstable();
        assert_eq!(names, expected);
        assert!(rows.iter().all(|r| r.len() == 2 && !r[1].is_empty()));
    }

    /// A candidate's text keeps everything outside its spans, and each
    /// marked span is followed by the marker. Styles are off outside a
    /// terminal, so the plain text is checked.
    #[test]
    fn applied_text_follows_each_marked_span_with_the_marker() {
        console::set_colors_enabled(false);
        let c = Candidate {
            line: 1,
            pointer: "/message/content".into(),
            text: "key AAAA then BBBB end".into(),
            spans: vec![(4, 7, true), (14, 17, false)],
        };
        assert_eq!(applied_text(&c), "key AAAA[REDACTED] then BBBB end");
        let none = Candidate {
            spans: Vec::new(),
            ..c
        };
        assert_eq!(applied_text(&none), "key AAAA then BBBB end");
    }

    #[test]
    fn lines_cells_join_ranges() {
        assert_eq!(lines_cell(&[]), "none");
        assert_eq!(lines_cell(&[(2, 3), (9, 9)]), "2-3,9");
    }

    #[test]
    fn inputs_are_described_for_the_dialog() {
        assert_eq!(
            describe_input(&RedactionInput::parse_text("abc 123", "secret", true).unwrap()),
            "text \"abc 123\" (secret)"
        );
        assert_eq!(
            describe_input(&RedactionInput::parse_text("damn", "language", false).unwrap()),
            "text \"damn\" (language, no mark)"
        );
    }
}
