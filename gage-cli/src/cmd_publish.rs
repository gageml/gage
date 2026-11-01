//! `gage publish`: the pending-publish life cycle commands, and the
//! publish of selected sessions through Claude Code when no
//! subcommand is given.
//!
//! `init` stages a pending publish per selected session and writes
//! its definition file; `edit` and `refresh` rebuild the staged
//! session from the file; `show` prints it; `candidate` prints the
//! text open to redaction; `abort` and `apply` end the publish.
//! `apply` is the only command that writes the store. The original
//! sessions are resolved through `--source` and `--stored`, the way
//! `gage scan` resolves sessions: a `--stored` session is used as is,
//! and a native session is added to the store first so it has an
//! object id and commit for `origin`.

use std::cmp::Reverse;
use std::env;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use clap::{Args, Subcommand};
use cliclack as cli;
use gage_core::config::gage_home;
use gage_core::style::IdHighlighter;
use gage_core::uuid::short_uuid;
use gage_publish::{
    Audience, Filter, FilterSpec, InitArgs as InitRecord, NAMED_FILTERS, Problem, PublishError,
    Publisher, Use, candidate,
};
use gage_session::{Driver, NativeSession, SkillMode, SkillSession, SourceOptions};
use gage_store::url::parse_session_origin;
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
use crate::cmd_query::{self, QueryRunArgs};
use crate::cmd_session::{self, ViewOptionArgs};
use crate::cmd_tag::TAG_HELP;
use crate::dialog::{self, DialogError, DialogResult};
use crate::error::CliError;
use crate::human::{count, format_elapsed_ms};
use crate::model_prompt;
use crate::session_filter::{
    FILTER_ARG_NAMES, SessionFilterArgs, SessionLine, SessionSelect, SessionSelectError,
    native_session_lines, preview_sessions, select_sessions, stored_session_lines,
};
use crate::source;
use crate::style::{self, IdKind};
use crate::text_arg::text_value;

#[derive(Args)]
#[command(args_conflicts_with_subcommands = true)]
pub struct PublishArgs {
    #[command(subcommand)]
    command: Option<PublishCommand>,

    #[command(flatten)]
    run_args: PublishRunArgs,
}

#[derive(Args)]
pub struct PublishRunArgs {
    /// Sessions to publish (ID or prefix)
    #[arg(value_name = "SESSION", conflicts_with_all = FILTER_ARG_NAMES)]
    sessions: Vec<String>,

    /// Session source
    #[arg(
        long,
        value_name = "SOURCE",
        long_help = format!("Session source\n\n{}", source::SOURCE_HELP),
        conflicts_with = "stored",
    )]
    source: Option<String>,

    /// Publish stored sessions
    #[arg(short = 'S', long)]
    stored: bool,

    #[command(flatten)]
    select: SessionFilterArgs,

    /// Audience sessions are published for
    #[arg(
        short = 'A',
        long,
        value_name = "AUDIENCE",
        long_help = format!("Audience sessions are published for\n\n{AUDIENCE_HELP}")
    )]
    audience: Option<String>,

    /// Use sessions are published for
    #[arg(
        short = 'U',
        long,
        value_name = "USE",
        long_help = format!("Use sessions are published for\n\n{USE_HELP}")
    )]
    r#use: Option<String>,

    /// Filter to apply (repeatable)
    #[arg(
        short,
        long,
        value_name = "FILTER",
        long_help = format!("Filter to apply (repeatable)\n\n{FILTER_HELP}")
    )]
    filter: Vec<String>,

    /// Segment to add (repeatable)
    ///
    /// A segment starts without lines and drops nothing until segment
    /// edit gives it lines.
    #[arg(short = 'g', long, value_name = "TEXT")]
    segment: Vec<String>,

    /// Notes about use, audience, segments
    ///
    /// Read from a file as @PATH.
    #[arg(short = 'N', long, value_name = "TEXT")]
    notes: Option<String>,

    /// Model for session
    ///
    /// If omitted, dialog prompts for a model.
    #[arg(short, long)]
    model: Option<String>,

    /// Run without an interactive session
    ///
    /// Run without an interactive session. Claude Code runs the publish
    /// skill to completion and prints its response. Without it, Claude
    /// Code opens an interactive session with the skill and the
    /// arguments given.
    #[arg(short, long)]
    batch: bool,

    /// Skip confirmation prompt
    ///
    /// Without --model, model takes its default
    #[arg(short, long)]
    yes: bool,

    /// Show available named filters and exit
    #[arg(long, exclusive = true)]
    list_filters: bool,

    /// Additional arguments passed through to harness
    #[arg(last = true, value_name = "ARGS")]
    extra_args: Vec<String>,
}

/// The paragraph the `--audience` options share.
const AUDIENCE_HELP: &str = "Selects every filter that lists audience. Values are \"public\" \
(everyone; requires the most filtering), \"trusted\" (only trusted users; some filtering), \
and \"self\" (only you can read; no filter lists it). Values are independent: one implies \
nothing about another, and a value not given selects nothing.";

/// The paragraph the `--use` options share.
const USE_HELP: &str = "Selects every filter that lists use. Values are \"reading\" (a \
narrative for reading; entries a reader does not want are left out), \"provenance\" (a \
record of work; no filter lists it), and \"other\" (another use; no filter lists it). Values \
are independent: one implies nothing about another, and a value not given selects nothing.";

#[derive(Subcommand)]
pub enum PublishCommand {
    /// Stage a publish of selected sessions
    ///
    /// Stage a publish of selected sessions. Stages one pending
    /// publish per session and prints its ID, the value the other
    /// publish commands take as their SESSION argument. Named filters
    /// run at once and their redactions are recorded in definition
    /// file.
    Init(Box<InitArgs>),

    /// List pending publish sessions
    List,

    /// View a pending publish session
    ///
    /// View a pending publish session. Opens session view on staged
    /// session, as gage session view does. Without SESSION, a picker
    /// lists pending publish sessions.
    View(ViewArgs),

    /// Query pending publish sessions
    ///
    /// Query pending publish sessions with SQL, as gage query does.
    /// Tables session, entry, message, tool_use, and changes hold
    /// staged sessions and nothing else; no other table is present.
    Query(QueryRunArgs),

    /// Show definition of a pending publish
    ///
    /// Show definition of a pending publish. Prints definition file.
    /// With --instructions, prints redaction instructions of each
    /// filter as markdown instead: a heading naming filter, then its
    /// instructions with prompt given for it in place of its
    /// placeholder. With --check, prints nothing and reports each
    /// problem of definition file by line instead.
    Show(ShowArgs),

    /// Edit definition of a pending publish in system editor
    ///
    /// Edit definition of a pending publish in system editor. Opens
    /// definition file in $VISUAL or $EDITOR and rebuilds staged
    /// session when editor exits, as refresh does. An invalid
    /// definition is reported and nothing changes; file is kept as
    /// saved.
    Edit(EditArgs),

    /// Rebuild a pending publish from its definition
    ///
    /// Rebuild a pending publish from its definition. Re-applies
    /// definition file as it is on disk, for a definition edited
    /// elsewhere. Every named filter runs again, and a redaction
    /// without an author takes author of this run. An invalid
    /// definition is reported and nothing changes.
    Refresh(RefreshArgs),

    /// Print candidate text of a pending publish
    ///
    /// Print candidate text of a pending publish. Each distinct text
    /// value of kept lines, with left-out values omitted, once, one
    /// per line as a JSON string, in order of first occurrence. Text
    /// is divided into splits bounded by --split, one per file with
    /// --split-files. This is what a redaction agent reads.
    Candidate(CandidateArgs),

    /// Abort pending publishes
    ///
    /// Deletes each staged publish. Nothing was written to store. Every
    /// session is resolved before any is deleted, so an unknown or
    /// ambiguous ID aborts nothing.
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
    /// Sessions to publish (ID or prefix)
    #[arg(value_name = "SESSION", conflicts_with_all = FILTER_ARG_NAMES)]
    sessions: Vec<String>,

    /// Session source
    #[arg(
        id = "source",
        short = 's',
        long = "source",
        value_name = "SOURCE",
        long_help = format!("Session source\n\n{}", source::SOURCE_HELP),
        conflicts_with = "stored",
    )]
    source: Option<String>,

    /// Publish stored sessions
    #[arg(id = "stored", short = 'S', long = "stored")]
    stored: bool,

    #[command(flatten)]
    select: SessionFilterArgs,

    /// Audience sessions are published for
    #[arg(
        short = 'A',
        long,
        value_name = "AUDIENCE",
        long_help = format!("Audience sessions are published for\n\n{AUDIENCE_HELP}")
    )]
    audience: Option<String>,

    /// Use sessions are published for
    #[arg(
        short = 'U',
        long,
        value_name = "USE",
        long_help = format!("Use sessions are published for\n\n{USE_HELP}")
    )]
    r#use: Option<String>,

    /// Filter to apply (repeatable)
    #[arg(
        short,
        long,
        value_name = "FILTER",
        long_help = format!("Filter to apply (repeatable)\n\n{FILTER_HELP}")
    )]
    filter: Vec<String>,

    /// Segment to add (repeatable)
    ///
    /// A segment starts without lines and drops nothing until
    /// definition gives it lines.
    #[arg(short = 'g', long, value_name = "TEXT")]
    segment: Vec<String>,

    /// Notes about use, audience, segments
    ///
    /// Read from a file as @PATH. Recorded in definition with
    /// audience and use.
    #[arg(short = 'N', long, value_name = "TEXT")]
    notes: Option<String>,

    /// Skip confirmation prompt
    #[arg(short, long)]
    yes: bool,

    /// Show available named filters and exit
    #[arg(long, exclusive = true)]
    list_filters: bool,
}

/// The paragraph `--filter` options share on what a filter argument
/// is.
const FILTER_HELP: &str = "A filter name; see --list-filters. Filter names are \"pii\", \
\"language\", \"secret\", \"system\", \"harness-context\", \"harness-records\", and \
\"custom\". \"custom\" takes a prompt as its whole instruction, so it is added in definition \
file. A filter given here is added to those --audience and --use select.";

#[derive(Args)]
pub struct ViewArgs {
    /// Pending publish session to view (ID or prefix)
    ///
    /// A trailing ":LINES" opens the view at a session line, as
    /// "7260:2956". LINES is a line ("42"), a range ("10-20"), or a
    /// comma-separated list of them ("10-20,42"); the view opens at
    /// the first line
    #[arg(value_name = "SESSION")]
    session: Option<String>,

    #[command(flatten)]
    view: ViewOptionArgs,
}

#[derive(Args)]
pub struct ShowArgs {
    /// Pending publish session to show (ID or prefix)
    #[arg(value_name = "SESSION")]
    session: String,

    /// Print redaction instructions of each filter as markdown
    #[arg(long)]
    instructions: bool,

    /// Check definition file and report each problem by line
    ///
    /// Check definition file and report each problem by line. A
    /// problem is one line on standard error, as "PATH:LINE:COLUMN:
    /// error: MESSAGE". A file without problems prints nothing. Exit
    /// status is 1 when there is a problem.
    #[arg(long, conflicts_with = "instructions")]
    check: bool,
}

#[derive(Args)]
pub struct EditArgs {
    /// Pending publish session to edit (ID or prefix)
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
pub struct CandidateArgs {
    /// Pending publish session to list (ID or prefix)
    #[arg(value_name = "SESSION")]
    session: String,

    /// Bytes per split (default 400000)
    ///
    /// Candidate text is each distinct text value once, one per line as
    /// a JSON string. A split closes when its next line would carry it
    /// past this size, and a line larger than this size is a split by
    /// itself. In standard output a line holding null marks a boundary
    /// between splits.
    #[arg(long, value_name = "BYTES")]
    split: Option<usize>,

    /// Write splits to files and print their paths
    ///
    /// Files are named 000001.jsonl onward in tmp/candidate/SESSION_ID
    /// under Gage home unless --split-files-dir names a directory. Split
    /// files already in that directory are removed first.
    #[arg(long)]
    split_files: bool,

    /// Directory for split files
    ///
    /// Default is tmp/candidate/SESSION_ID under Gage home. Implies
    /// --split-files.
    #[arg(long, value_name = "DIR")]
    split_files_dir: Option<PathBuf>,
}

/// Bytes per candidate split when `--split` is not given.
const DEFAULT_SPLIT_BYTES: usize = 400_000;

#[derive(Args)]
pub struct AbortArgs {
    /// Pending publish sessions to abort (ID or prefix; repeatable)
    #[arg(value_name = "SESSION", required = true)]
    session: Vec<String>,

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

    /// Publish session even when definition changed nothing in it
    #[arg(long)]
    force: bool,

    /// Skip confirmation prompt
    #[arg(short, long)]
    yes: bool,
}

pub async fn run(args: PublishArgs) -> Result<(), CliError> {
    let Some(command) = args.command else {
        return publish_sessions(args.run_args).await;
    };
    match command {
        PublishCommand::Init(args) => init(*args).await,
        PublishCommand::List => list(),
        PublishCommand::View(args) => {
            cmd_session::view_pending(args.session.as_deref(), &args.view).await
        }
        PublishCommand::Query(args) => cmd_query::publish(args).await,
        PublishCommand::Show(args) => show(args),
        PublishCommand::Edit(args) => edit(args),
        PublishCommand::Refresh(args) => refresh(args),
        PublishCommand::Candidate(args) => candidate(args),
        PublishCommand::Abort(args) => abort(args),
        PublishCommand::Apply(args) => apply(args),
    }
}

/// Publish the selected sessions through Claude Code. The dialog
/// settles the inputs and spells them as `gage publish init`
/// arguments for the skill; the skill session runs after it, since
/// the harness owns the terminal from then on.
async fn publish_sessions(args: PublishRunArgs) -> Result<(), CliError> {
    let fail = |e: String| CliError::message(format!("gage publish: {e}"));
    if args.list_filters {
        list_filters();
        return Ok(());
    }
    check_run_args(&args).map_err(fail)?;
    let audience = parse_audience("gage publish", args.audience.as_deref())?;
    let uses = parse_use("gage publish", args.r#use.as_deref())?;
    // Filters and segments are checked here and handed to the run;
    // the dialog does not show them
    for name in &args.filter {
        parse_filter("gage publish", "--filter", name, None)?;
    }
    let segments = args
        .segment
        .iter()
        .map(|text| {
            text_value("gage publish", "--segment", text)
                .map(|t| t.trim_end().to_string())
                .map_err(|e| fail(e.to_string()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let notes = match &args.notes {
        Some(text) => Some(
            text_value("gage publish", "--notes", text)
                .map(|t| t.trim().to_string())
                .map_err(|e| fail(e.to_string()))?,
        ),
        None => None,
    };
    let store = Store::open(&gage_store::store_path()).map_err(|e| fail(e.to_string()))?;

    let mut setup: Option<PublishSetup> = None;
    dialog::run_async("Publish sessions", || async {
        let ids = if selects_sessions(&args) {
            preview_selection(&args, &store).await?
        } else {
            Vec::new()
        };
        let prompt = !args.yes;
        let audience = match audience {
            Some(audience) => {
                dialog::step_value("Audience", audience)?;
                Some(audience)
            }
            None if prompt => Some(prompt_audience()?),
            None => None,
        };
        let uses = match uses {
            Some(uses) => {
                dialog::step_value("Use", uses)?;
                Some(uses)
            }
            None if prompt => Some(prompt_use()?),
            None => None,
        };
        let notes = match notes {
            Some(notes) => {
                dialog::step_value("Notes", value_cell(&notes))?;
                Some(notes)
            }
            None if prompt => prompt_notes()?,
            None => None,
        };
        let model = model_prompt::model_step(args.model.as_deref(), args.yes)?;
        if !args.yes {
            dialog::usage_notice(usage_notice(args.batch))?;
            dialog::confirm(false, true)?;
        }
        let inputs = InitInputs {
            audience,
            uses,
            filters: &args.filter,
            segments: &segments,
            notes: notes.as_deref(),
        };
        setup = Some(PublishSetup {
            skill_args: skill_args(&args, &inputs, &ids),
            model,
        });
        Ok(DialogResult::from(if args.batch {
            "Publishing sessions"
        } else {
            "Starting publish session"
        }))
    })
    .await?;
    let Some(setup) = setup else {
        return Ok(());
    };
    let session = SkillSession {
        name: "Publish sessions",
        skill: "gage:publish",
        args: &setup.skill_args,
        model: setup.model.as_deref(),
        extra_args: &args.extra_args,
        mode: if args.batch {
            SkillMode::Batch
        } else {
            SkillMode::Interactive
        },
    };
    source::run_skill_session("gage publish", &session)
}

/// What the dialog settled for the skill session
struct PublishSetup {
    skill_args: Vec<String>,
    model: Option<String>,
}

/// The publish inputs the dialog settled, as the skill receives them.
struct InitInputs<'a> {
    audience: Option<Audience>,
    uses: Option<Use>,
    filters: &'a [String],
    segments: &'a [String],
    notes: Option<&'a str>,
}

/// The skill's arguments: the `gage publish init` options that spell
/// the settled inputs, then the selected session ids. The source
/// comes first, so the ids read against it; a session selection is
/// already resolved to ids, so no selection option is passed.
fn skill_args(args: &PublishRunArgs, inputs: &InitInputs<'_>, ids: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if args.stored {
        out.push("--stored".to_string());
    } else if let Some(source) = &args.source {
        out.extend(["--source".to_string(), source.clone()]);
    }
    if let Some(audience) = inputs.audience {
        out.extend(["--audience".to_string(), audience.to_string()]);
    }
    if let Some(uses) = inputs.uses {
        out.extend(["--use".to_string(), uses.to_string()]);
    }
    for name in inputs.filters {
        out.extend(["--filter".to_string(), name.clone()]);
    }
    for text in inputs.segments {
        out.extend(["--segment".to_string(), text.clone()]);
    }
    if let Some(notes) = inputs.notes {
        out.extend(["--notes".to_string(), notes.to_string()]);
    }
    out.extend(ids.iter().cloned());
    out
}

/// The notice before the confirmation: an interactive session is
/// started, or the publish runs to completion with `--batch`.
fn usage_notice(batch: bool) -> &'static str {
    if batch {
        "You are about to publish sessions using the selected model. Standard token usage \
         applies."
    } else {
        "You are about to start an interactive session to publish sessions. Standard token \
         usage applies for the selected model."
    }
}

fn prompt_audience() -> Result<Audience, DialogError> {
    Ok(cli::select("Audience")
        .item(
            Audience::Public,
            "public",
            "Everyone - requires the most filtering",
        )
        .item(
            Audience::Trusted,
            "trusted",
            "Only trusted users - some filtering",
        )
        .item(
            Audience::Private,
            "self",
            "Only you can read - filtering not required",
        )
        .interact()?)
}

fn prompt_use() -> Result<Use, DialogError> {
    Ok(cli::select("Use")
        .item(Use::Reading, "reading", "Share as a narrative for reading")
        .item(Use::Provenance, "provenance", "Export as a record of work")
        .item(Use::Other, "other", "Another use")
        .interact()?)
}

/// The notes prompt; `None` when nothing was entered.
fn prompt_notes() -> Result<Option<String>, DialogError> {
    let text: String = cli::input("Notes")
        .placeholder("Additional notes (optional)")
        .required(false)
        .interact()?;
    let text = text.trim();
    Ok((!text.is_empty()).then(|| text.to_string()))
}

/// The rule clap does not express: `--batch` runs without a session
/// to ask in, so it needs a session selection and a filter or a
/// segment. An interactive run takes whatever is given.
fn check_run_args(args: &PublishRunArgs) -> Result<(), String> {
    if !args.batch {
        return Ok(());
    }
    if !selects_sessions(args) {
        return Err("--batch requires a session selection".to_string());
    }
    if args.filter.is_empty() && args.segment.is_empty() {
        return Err("--batch requires a filter or segment".to_string());
    }
    Ok(())
}

/// A positional session or a filter axis was given.
fn selects_sessions(args: &PublishRunArgs) -> bool {
    !args.sessions.is_empty() || !args.select.is_empty()
}

/// The `Sessions` step for a selection, after the `Source` step when
/// the sessions are native. Returns the selected ids: stored ids for
/// `--stored`, native ids otherwise.
async fn preview_selection(
    args: &PublishRunArgs,
    store: &Store,
) -> Result<Vec<String>, DialogError> {
    let select = if args.stored {
        SessionSelect::Stored(store)
    } else {
        SessionSelect::Native(args.source.as_deref())
    };
    let ids = select_sessions(select, &args.sessions, &args.select)
        .await
        .map_err(SessionSelectError::into_dialog_error)?;
    if ids.is_empty() {
        return Err(DialogError::Failed(
            "No sessions matched the selection".to_string(),
        ));
    }
    if args.stored {
        let lines =
            stored_session_lines(store, &ids).map_err(|e| DialogError::Failed(e.to_string()))?;
        preview_sessions(&lines, IdKind::Gage, None)?;
        return Ok(ids);
    }
    let registry = source::driver_registry();
    let (driver, spec) =
        source::resolve_source(&registry, args.source.as_deref()).map_err(DialogError::Failed)?;
    dialog::step_value("Source", source::describe_source(driver.as_ref(), &spec))?;
    let lines = native_session_lines(args.source.as_deref(), &ids)
        .await
        .map_err(|e| DialogError::Failed(format!("{e}")))?;
    preview_sessions(&lines, IdKind::Native, None)?;
    Ok(ids)
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
    let stored = store
        .short_prefix_ids(Some(SESSION_TYPE))
        .map_err(|e| fail(e.to_string()))?;
    let (ids, origins) = list_highlighters(&pending, stored);

    let header: Vec<String> = ["Id", "Origin", "Title", "Redactions", "Modified"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let publisher = Publisher::new(&store);
    let rows = pending_rows(pending, &ids, &origins, |id| {
        redaction_count(&publisher, id)
    })
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

/// The listing's highlighters: a pending id resolves among the pending
/// publishes alone, as the publish commands resolve it, and an origin
/// among the store's short-prefix set of sessions.
fn list_highlighters(
    pending: &[PendingPublish],
    stored: Vec<String>,
) -> (IdHighlighter, IdHighlighter) {
    let ids = IdHighlighter::new(pending.iter().map(|p| p.id.clone()).collect());
    (ids, IdHighlighter::new(stored))
}

/// One row per pending publish, newest first: the short id highlighted
/// by `ids`, the `origin` highlighted by `origins` and title from
/// `attrs.json`, the redaction count `redactions` gives for the id,
/// and the age of the working tree.
fn pending_rows(
    mut pending: Vec<PendingPublish>,
    ids: &IdHighlighter,
    origins: &IdHighlighter,
    redactions: impl Fn(&str) -> Result<String, PublishError>,
) -> Result<Vec<Vec<String>>, PublishError> {
    pending.sort_by_key(|p| Reverse(p.modified_ms));
    pending
        .iter()
        .map(|p| {
            Ok(vec![
                ids.short(&p.id),
                origin_cell(&p.attrs.origin, origins),
                p.attrs.summary.title.clone().unwrap_or_default(),
                redactions(&p.id)?,
                format_elapsed_ms(p.modified_ms),
            ])
        })
        .collect()
}

/// The redaction count of the pending publish `id` from its
/// definition, "invalid" for a definition that does not read, or no
/// count when the definition was deleted.
fn redaction_count(publisher: &Publisher<'_>, id: &str) -> Result<String, PublishError> {
    match publisher.definition(id) {
        Ok(definition) => Ok(definition.redact.len().to_string()),
        Err(PublishError::Definition(_)) => Ok("invalid".into()),
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

/// Stage a publish per selected session after confirmation. The
/// sessions are resolved before the dialog opens, so a bad argument
/// fails without a prompt, and a native session is added to the store
/// only after the user confirms.
async fn init(args: InitArgs) -> Result<(), CliError> {
    let fail = |e: String| CliError::message(format!("gage publish init: {e}"));
    if args.list_filters {
        list_filters();
        return Ok(());
    }
    check_init_args(&args).map_err(fail)?;
    let store = Store::open(&gage_store::store_path()).map_err(|e| fail(e.to_string()))?;
    let audience = parse_audience("gage publish init", args.audience.as_deref())?;
    let uses = parse_use("gage publish init", args.r#use.as_deref())?;
    let added = args
        .filter
        .iter()
        .map(|name| parse_filter("gage publish init", "--filter", name, None))
        .collect::<Result<Vec<_>, _>>()?;
    let filters = selected_filters(audience, uses, added);
    let segments = args
        .segment
        .iter()
        .map(|text| {
            text_value("gage publish init", "--segment", text)
                .map(|t| t.trim_end().to_string())
                .map_err(|e| fail(e.to_string()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let notes = args
        .notes
        .as_deref()
        .map(|text| {
            text_value("gage publish init", "--notes", text)
                .map(|t| t.trim().to_string())
                .map_err(|e| fail(e.to_string()))
        })
        .transpose()?;
    let record = InitRecord {
        audience: audience.map(|a| a.to_string()),
        uses: uses.map(|u| u.to_string()),
        notes,
    };
    let originals = resolve_originals(
        &store,
        args.source.as_deref(),
        args.stored,
        &args.sessions,
        &args.select,
    )
    .await?;
    let author = default_author();

    let title = if originals.len() == 1 {
        "Publish session"
    } else {
        "Publish sessions"
    };
    dialog::run(title, || {
        originals.preview()?;
        if let Some(audience) = audience {
            dialog::step_value("Audience", audience)?;
        }
        if let Some(uses) = uses {
            dialog::step_value("Use", uses)?;
        }
        if let Some(notes) = &record.notes {
            dialog::step_value("Notes", value_cell(notes))?;
        }
        filters_step(&filters)?;
        if !segments.is_empty() {
            dialog::step_list("Segments", segments.iter().map(|s| value_cell(s)))?;
        }
        if !args.yes {
            let confirmed = cli::confirm("Continue?").initial_value(true).interact()?;
            if !confirmed {
                return Err(DialogError::Canceled);
            }
        }
        let publisher = Publisher::new(&store);
        let total = originals.len();
        let mut staged: Vec<String> = Vec::new();
        for original in originals.into_stored(&store) {
            let stage = || -> Result<String, String> {
                let record_of = original?;
                let outcome = publisher
                    .init(&record_of, &filters, &segments, &author, &record)
                    .map_err(|e| e.to_string())?;
                cli::log::info(format!(
                    "Staged publish {} of session {}",
                    short_uuid(&outcome.id),
                    short_uuid(&record_of.id)
                ))
                .map_err(|e| e.to_string())?;
                report_filter_redactions(&publisher, &outcome.id).map_err(|e| e.to_string())?;
                Ok(outcome.id)
            };
            match stage() {
                Ok(id) => staged.push(id),
                Err(e) if staged.is_empty() => return Err(DialogError::Failed(e)),
                Err(e) => {
                    cli::log::error(e)?;
                    return Err(dialog::partial("Staged", staged.len(), total, "publish"));
                }
            }
        }
        Ok(match staged.as_slice() {
            [id] => format!("Staged publish {}", short_uuid(id)),
            _ => format!("Staged {}", count(staged.len(), "publish")),
        }
        .into())
    })
}

/// The rule clap does not express: a session or a selection option is
/// required.
fn check_init_args(args: &InitArgs) -> Result<(), String> {
    if args.sessions.is_empty() && args.select.is_empty() {
        return Err("SESSION or a selection option is required".to_string());
    }
    Ok(())
}

/// The author of what this run writes: the Claude Code session named
/// in `CLAUDE_CODE_SESSION_ID` as `session:<id>`, else the user.
fn default_author() -> String {
    match env::var_os("CLAUDE_CODE_SESSION_ID") {
        Some(id) if !id.is_empty() => format!("session:{}", id.to_string_lossy()),
        _ => resolve_author(None),
    }
}

/// Print the named filters as a table, the way `gage scan
/// --list-scanners` prints scanners.
fn list_filters() {
    let header: Vec<String> = ["Filter", "Audience", "Use", "Description"]
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
        .modify(Columns::one(3).not(Rows::first()), style::dim())
        .to_string();
    println!("{table}");
}

/// One row per named filter: name, the audiences and uses it lists
/// (empty when it lists none), and description.
fn filter_rows() -> Vec<Vec<String>> {
    fn joined<T: fmt::Display>(values: &[T]) -> String {
        values
            .iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    }
    Filter::all()
        .iter()
        .map(|f| {
            vec![
                f.name.to_string(),
                joined(&f.audience),
                joined(&f.uses),
                f.description.to_string(),
            ]
        })
        .collect()
}

/// The audience `text` names, `None` when there is none. A name
/// outside the list is a usage error in clap's layout.
fn parse_audience(command: &str, text: Option<&str>) -> Result<Option<Audience>, CliError> {
    let Some(text) = text else {
        return Ok(None);
    };
    if !Audience::NAMES.contains(&text) {
        return Err(CliError::invalid_value(
            command,
            "--audience",
            text,
            Audience::NAMES,
        ));
    }
    Ok(Some(text.parse().expect("a listed audience name parses")))
}

/// The use `text` names, `None` when there is none. A name outside
/// the list is a usage error in clap's layout.
fn parse_use(command: &str, text: Option<&str>) -> Result<Option<Use>, CliError> {
    let Some(text) = text else {
        return Ok(None);
    };
    if !Use::NAMES.contains(&text) {
        return Err(CliError::invalid_value(command, "--use", text, Use::NAMES));
    }
    Ok(Some(text.parse().expect("a listed use name parses")))
}

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

/// The filters an init stages: those `audience` or `uses` selects, in
/// listing order, then each of `added` not already among them.
fn selected_filters(
    audience: Option<Audience>,
    uses: Option<Use>,
    added: Vec<FilterSpec>,
) -> Vec<FilterSpec> {
    let mut filters: Vec<FilterSpec> = Filter::selected(audience, uses)
        .into_iter()
        .map(|f| {
            FilterSpec::new(f.name, None)
                .expect("custom lists no value, so no selected filter needs a prompt")
        })
        .collect();
    for spec in added {
        if !filters.iter().any(|f| f.name == spec.name) {
            filters.push(spec);
        }
    }
    filters
}

/// The `Filters` step of the dialog: one line per filter, or `None`.
fn filters_step(filters: &[FilterSpec]) -> io::Result<()> {
    if filters.is_empty() {
        return dialog::step_none("Filters");
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
            let mine: Vec<usize> = definition
                .redact
                .iter()
                .enumerate()
                .filter(|(_, t)| t.author == author)
                .map(|(i, _)| i)
                .collect();
            let instances = mine
                .iter()
                .map(|i| counts.get(i).copied().unwrap_or(0))
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

/// Print the definition file of a pending publish, the redaction
/// instructions of its filters, or the problems the file has.
fn show(args: ShowArgs) -> Result<(), CliError> {
    let fail = |e: String| CliError::message(format!("gage publish show: {e}"));
    let store = Store::open(&gage_store::store_path()).map_err(|e| fail(e.to_string()))?;
    let publisher = Publisher::new(&store);
    let pending = publisher
        .store()
        .get(&args.session)
        .map_err(|e| fail(e.to_string()))?;
    let path = publisher.store().definition_file(&pending.id);
    if args.check {
        return match publisher.definition(&pending.id) {
            Ok(_) => Ok(()),
            Err(PublishError::Definition(problems)) => {
                for problem in &problems {
                    eprintln!("{}", diagnostic(&path, problem));
                }
                Err(CliError::exit(1))
            }
            Err(e) => Err(fail(e.to_string())),
        };
    }
    if args.instructions {
        let definition = publisher
            .definition(&pending.id)
            .map_err(|e| fail(definition_failure(&path, &e)))?;
        let markdown = definition
            .instructions_markdown()
            .map_err(|e| fail(e.to_string()))?;
        print!("{markdown}");
        return Ok(());
    }
    let text = fs::read_to_string(&path).map_err(|e| fail(format!("{}: {e}", path.display())))?;
    print!("{text}");
    Ok(())
}

/// A problem of the definition file at `path` in the form a compiler
/// reports, `path:line:column: error: message`.
fn diagnostic(path: &Path, problem: &Problem) -> String {
    format!(
        "{}:{}:{}: error: {}",
        path.display(),
        problem.line,
        problem.column,
        problem.message
    )
}

/// The error of a definition read as its message, or, for a file with
/// problems, one diagnostic per line.
fn definition_failure(path: &Path, e: &PublishError) -> String {
    match e {
        PublishError::Definition(problems) => problems
            .iter()
            .map(|p| diagnostic(path, p))
            .collect::<Vec<_>>()
            .join("\n"),
        e => e.to_string(),
    }
}

/// Open the definition file in the editor and rebuild when the editor
/// exits. The editor save is the confirmation. A file the editor left
/// as it was rebuilds nothing.
fn edit(args: EditArgs) -> Result<(), CliError> {
    let fail = |e: String| CliError::message(format!("gage publish edit: {e}"));
    let store = Store::open(&gage_store::store_path()).map_err(|e| fail(e.to_string()))?;
    let publisher = Publisher::new(&store);
    let pending = publisher
        .store()
        .get(&args.session)
        .map_err(|e| fail(e.to_string()))?;
    let path = publisher.store().definition_file(&pending.id);
    let read =
        |path: &PathBuf| fs::read(path).map_err(|e| fail(format!("{}: {e}", path.display())));
    let before = read(&path)?;

    let editor = env::var_os("VISUAL")
        .or_else(|| env::var_os("EDITOR"))
        .unwrap_or_else(|| "vi".into());
    let status = Command::new(&editor).arg(&path).status().map_err(|e| {
        fail(format!(
            "cannot launch editor {}: {e}",
            editor.to_string_lossy()
        ))
    })?;
    if !status.success() {
        return Err(fail(format!("editor exited with status {status}")));
    }
    if read(&path)? == before {
        println!("No change");
        return Ok(());
    }
    refresh_dialog("Edit publish", &publisher, &pending, false)
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
    refresh_dialog("Refresh publish", &publisher, &pending, !args.yes)
}

/// The dialog that rebuilds `pending` from its definition: the
/// definition is read and checked, its counts shown, the rebuild
/// confirmed when `confirm` is set, and every redaction that matches
/// nothing reported by its position. A definition with problems ends
/// the dialog, and the problems follow it one per line as `show
/// --check` prints them.
fn refresh_dialog(
    title: &str,
    publisher: &Publisher<'_>,
    pending: &PendingPublish,
    confirm: bool,
) -> Result<(), CliError> {
    let author = default_author();
    let path = publisher.store().definition_file(&pending.id);
    let mut problems = Vec::new();
    let result = dialog::run(title, || {
        dialog::step_value("Session", short_uuid(&pending.id))?;
        let mut definition = publisher.definition(&pending.id).map_err(|e| match e {
            PublishError::Definition(found) => {
                problems = found;
                DialogError::Failed(format!(
                    "Publish definition for {} has errors",
                    short_uuid(&pending.id)
                ))
            }
            e => DialogError::Failed(e.to_string()),
        })?;
        definition.fill_authors(&author);
        definition
            .validate()
            .map_err(|e| DialogError::Failed(e.to_string()))?;
        dialog::step_value(
            "Definition",
            format!(
                "{}, {}, {}",
                count(definition.filters.len(), "filter"),
                count(definition.segments.len(), "segment"),
                count(definition.redact.len(), "redaction")
            ),
        )?;
        if confirm {
            let confirmed = cli::confirm("Continue?").initial_value(true).interact()?;
            if !confirmed {
                return Err(DialogError::Canceled);
            }
        }
        let refreshed = publisher
            .refresh(&pending.id, &author)
            .map_err(|e| DialogError::Failed(e.to_string()))?;
        for (position, literal) in &refreshed.unmatched {
            cli::log::warning(format!("Redaction {position} matches nothing: {literal:?}"))?;
        }
        Ok(format!(
            "Refreshed publish {}, {}",
            short_uuid(&pending.id),
            count(refreshed.instances, "instance")
        )
        .into())
    });
    for problem in &problems {
        eprintln!("{}", diagnostic(&path, problem));
    }
    result
}

/// List the text values open to redaction, one per line where the
/// value fits, else as a header with the text indented below it.
fn candidate(args: CandidateArgs) -> Result<(), CliError> {
    let fail = |e: String| CliError::message(format!("gage publish candidate: {e}"));
    let store = Store::open(&gage_store::store_path()).map_err(|e| fail(e.to_string()))?;
    let publisher = Publisher::new(&store);
    let pending = publisher
        .store()
        .get(&args.session)
        .map_err(|e| fail(e.to_string()))?;
    let texts = publisher
        .candidate_texts(&pending.id)
        .map_err(|e| fail(e.to_string()))?;
    let splits = candidate::splits(&texts, args.split.unwrap_or(DEFAULT_SPLIT_BYTES));
    if args.split_files || args.split_files_dir.is_some() {
        let dir = args
            .split_files_dir
            .unwrap_or_else(|| candidate_files_dir(&pending.id));
        let paths = candidate::write_files(&dir, &splits).map_err(|e| {
            fail(format!(
                "cannot write candidate files to {}: {e}",
                dir.display()
            ))
        })?;
        for path in paths {
            println!("{}", path.display());
        }
    } else {
        print!("{}", candidate::render(&splits));
    }
    Ok(())
}

/// Where `--split-files` writes when no directory is named:
/// `tmp/candidate/<pending id>` under Gage home.
fn candidate_files_dir(id: &str) -> PathBuf {
    gage_home().join("tmp").join("candidate").join(id)
}

/// Delete a pending publish after confirmation. The id is resolved
/// before the dialog opens, so a bad argument fails without a prompt.
fn abort(args: AbortArgs) -> Result<(), CliError> {
    let fail = |e: String| CliError::message(format!("gage publish abort: {e}"));
    let store = Store::open(&gage_store::store_path()).map_err(|e| fail(e.to_string()))?;
    let publish = PublishStore::from(&store);
    let mut pending: Vec<PendingPublish> = Vec::new();
    for session in &args.session {
        let found = publish.get(session).map_err(|e| fail(e.to_string()))?;
        if !pending.iter().any(|p| p.id == found.id) {
            pending.push(found);
        }
    }

    dialog::run("Abort publish", || {
        let lines: Vec<SessionLine> = pending.iter().map(SessionLine::from).collect();
        preview_sessions(&lines, IdKind::Gage, None)?;
        cli::log::remark("Original sessions are unaffected")?;
        if !args.yes {
            let confirmed = cli::confirm("Continue?").initial_value(false).interact()?;
            if !confirmed {
                return Err(DialogError::Canceled);
            }
        }
        for p in &pending {
            publish
                .abort(&p.id)
                .map_err(|e| DialogError::Other(e.into()))?;
        }
        Ok(format!("Aborted {}", count(pending.len(), "publish")).into())
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
    let path = publisher.store().definition_file(&pending.id);
    let definition = publisher
        .definition(&pending.id)
        .map_err(|e| fail(definition_failure(&path, &e)))?;
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
                count(definition.redact.len(), "redaction"),
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

/// The sessions a publish starts from, resolved but not yet written.
enum Originals {
    /// Sessions already in the store, used as they are.
    Stored(Vec<SessionRecord>),
    /// Native sessions, each added to the store when the publish is
    /// confirmed so it has an object id and commit for `origin`.
    Native {
        driver: Arc<dyn Driver>,
        spec: String,
        natives: Vec<Box<dyn NativeSession>>,
    },
}

impl Originals {
    fn len(&self) -> usize {
        match self {
            Originals::Stored(records) => records.len(),
            Originals::Native { natives, .. } => natives.len(),
        }
    }

    /// The `Source` step for native sessions, then the `Sessions`
    /// step.
    fn preview(&self) -> io::Result<()> {
        match self {
            Originals::Stored(records) => {
                let lines: Vec<SessionLine> = records.iter().map(SessionLine::from).collect();
                preview_sessions(&lines, IdKind::Gage, None)
            }
            Originals::Native {
                driver,
                spec,
                natives,
            } => {
                dialog::step_value("Source", source::describe_source(driver.as_ref(), spec))?;
                let lines: Vec<SessionLine> = natives
                    .iter()
                    .map(|native| SessionLine {
                        id: native.id().to_string(),
                        title: native.attrs().title().map(String::from),
                    })
                    .collect();
                preview_sessions(&lines, IdKind::Native, None)
            }
        }
    }

    /// The stored sessions to publish, in order: each record as it
    /// is, or each native session added to the store now.
    fn into_stored<'a>(
        self,
        store: &'a Store,
    ) -> Box<dyn Iterator<Item = Result<SessionRecord, String>> + 'a> {
        match self {
            Originals::Stored(records) => Box::new(records.into_iter().map(Ok)),
            Originals::Native {
                driver, natives, ..
            } => Box::new(natives.into_iter().map(move |mut native| {
                let sessions = SessionStore::from(store);
                let outcome = sessions
                    .add(driver.as_ref(), native.as_mut())
                    .map_err(|e| e.to_string())?;
                sessions
                    .at_commit(&outcome.commit_sha)
                    .map_err(|e| e.to_string())
            })),
        }
    }
}

/// Resolve the selection without writing. `--stored` resolves it in
/// the store; otherwise the sessions are native sessions in `source`.
/// A selection that matches nothing is an error.
async fn resolve_originals(
    store: &Store,
    source: Option<&str>,
    stored: bool,
    sessions: &[String],
    select: &SessionFilterArgs,
) -> Result<Originals, CliError> {
    let fail = |e: String| CliError::message(format!("gage publish init: {e}"));
    let selection = if stored {
        SessionSelect::Stored(store)
    } else {
        SessionSelect::Native(source)
    };
    let ids = select_sessions(selection, sessions, select)
        .await
        .map_err(|e| fail(e.to_string()))?;
    if ids.is_empty() {
        return Err(fail("no sessions matched the selection".to_string()));
    }
    if stored {
        let records = ids
            .iter()
            .map(|id| SessionStore::from(store).get(id))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| fail(e.to_string()))?;
        return Ok(Originals::Stored(records));
    }
    let registry = source::driver_registry();
    let (driver, spec) = source::resolve_source(&registry, source).map_err(fail)?;
    let opened = driver
        .open_source(&SourceOptions::new(&spec))
        .map_err(|e| fail(format!("{spec}: {e}")))?;
    let natives = ids
        .iter()
        .map(|id| {
            opened
                .open_native(id)
                .map_err(|e| fail(format!("{id}: {e}")))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Originals::Native {
        driver,
        spec,
        natives,
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
        let rendered = match parse_filter("gage publish init", "--filter", "secrets", None) {
            Err(CliError::Usage(e)) => {
                assert_eq!(e.exit_code(), 2);
                e.render().to_string()
            }
            Err(_) => panic!("not a usage error"),
            Ok(f) => panic!("{f} parsed"),
        };
        assert!(
            rendered.contains("invalid value 'secrets' for '--filter <FILTER>'"),
            "{rendered}"
        );
        assert!(
            rendered.contains(
                "[possible values: pii, language, secret, system, harness-context, harness-records, custom]"
            ),
            "{rendered}"
        );
        assert!(
            rendered.contains("a similar value exists: 'secret'"),
            "{rendered}"
        );
        assert!(parse_filter("gage publish init", "--filter", "pii", None).is_ok());
        let custom = parse_filter("gage publish init", "--filter", "custom", None);
        assert!(
            matches!(custom, Err(CliError::Exit { message: Some(ref m), .. }) if m.contains("requires a prompt")),
            "custom without a prompt is a plain error"
        );
    }

    /// The publish commands live under `gage publish`, not `gage
    /// session`. `init` takes its own --source and --stored and the
    /// session selection; the other commands take a pending session.
    #[test]
    fn publish_is_a_top_level_command() {
        Cli::try_parse_from(["gage", "publish", "list"]).unwrap();
        Cli::try_parse_from(["gage", "publish", "init", "--stored", "abc"]).unwrap();
        let init_args = |argv: &[&str]| {
            let cli = Cli::try_parse_from([&["gage", "publish", "init"], argv].concat()).unwrap();
            let Command::Publish(PublishArgs {
                command: Some(PublishCommand::Init(args)),
                ..
            }) = cli.command
            else {
                panic!("expected publish init");
            };
            *args
        };
        let args = init_args(&[
            "abc",
            "def",
            "-g",
            "The question",
            "--segment",
            "The reply",
            "-N",
            "For the blog",
            "-A",
            "public",
            "-U",
            "reading",
            "-y",
        ]);
        assert_eq!(args.sessions, ["abc", "def"]);
        assert_eq!(args.segment, ["The question", "The reply"]);
        assert_eq!(args.notes.as_deref(), Some("For the blog"));
        assert_eq!(args.audience.as_deref(), Some("public"));
        assert_eq!(args.r#use.as_deref(), Some("reading"));
        assert!(args.yes && !args.stored);
        assert_eq!(check_init_args(&args), Ok(()));
        let args = init_args(&["-S", "--days", "7", "-f", "secret"]);
        assert!(args.stored);
        assert_eq!(args.select.days, Some(7));
        assert_eq!(args.filter, ["secret"]);
        assert_eq!(check_init_args(&args), Ok(()));
        let args = init_args(&["-s", "claude:/tmp/claude", "--all"]);
        assert_eq!(args.source.as_deref(), Some("claude:/tmp/claude"));
        assert!(args.select.all);
        let args = init_args(&["-A", "public"]);
        assert_eq!(
            check_init_args(&args),
            Err("SESSION or a selection option is required".to_string())
        );
        // A list flag stands alone
        Cli::try_parse_from(["gage", "publish", "init", "--list-filters"]).unwrap();
        for argv in [
            vec!["init", "abc", "--days", "7"],
            vec!["init", "-s", "claude:/tmp/claude", "-S", "abc"],
            vec!["init", "abc", "--list-filters"],
        ] {
            assert!(
                Cli::try_parse_from([&["gage", "publish"], &argv[..]].concat()).is_err(),
                "{argv:?} parsed"
            );
        }
        for argv in [
            vec!["show", "abc"],
            vec!["show", "abc", "--instructions"],
            vec!["show", "abc", "--check"],
            vec!["edit", "abc"],
            vec!["refresh", "abc", "-y"],
            vec!["candidate", "abc", "--split-files"],
            vec![
                "candidate",
                "abc",
                "--split",
                "1000",
                "--split-files-dir",
                "/tmp/c",
            ],
            vec!["apply", "abc", "--tag", "x"],
            vec!["view"],
            vec!["query"],
            vec!["query", "SELECT 1", "-f", "json", "--system"],
        ] {
            Cli::try_parse_from([&["gage", "publish"], &argv[..]].concat())
                .unwrap_or_else(|e| panic!("{argv:?}: {e}"));
        }
        // The view and query wrappers take the view and query options
        // and no scope option of their own
        let cli =
            Cli::try_parse_from(["gage", "publish", "view", "abc:12", "-v", "tools"]).unwrap();
        let Command::Publish(PublishArgs {
            command: Some(PublishCommand::View(view)),
            ..
        }) = cli.command
        else {
            panic!("expected publish view");
        };
        assert_eq!(view.session.as_deref(), Some("abc:12"));
        assert_eq!(view.view.options.as_deref(), Some("tools"));
        for argv in [
            vec!["view", "-P"],
            vec!["view", "--stored"],
            vec!["query", "--scan", "abc"],
            vec!["query", "-P"],
        ] {
            assert!(
                Cli::try_parse_from([&["gage", "publish"], &argv[..]].concat()).is_err(),
                "{argv:?} parsed"
            );
        }
        assert!(Cli::try_parse_from(["gage", "query", "-P"]).is_err());
        assert!(Cli::try_parse_from(["gage", "session", "view", "-P"]).is_err());
        assert!(Cli::try_parse_from(["gage", "session", "publish", "list"]).is_err());
        assert!(Cli::try_parse_from(["gage", "publish", "show", "--stored", "abc"]).is_err());
        assert!(
            Cli::try_parse_from([
                "gage",
                "publish",
                "show",
                "abc",
                "--check",
                "--instructions"
            ])
            .is_err()
        );
        assert!(Cli::try_parse_from(["gage", "publish", "edit"]).is_err());
        // The former subcommands are a run's positional sessions
        let cli = Cli::try_parse_from(["gage", "publish", "status", "abc"]).unwrap();
        let Command::Publish(PublishArgs {
            command: None,
            run_args,
        }) = cli.command
        else {
            panic!("expected a publish run");
        };
        assert_eq!(run_args.sessions, ["status", "abc"]);
        let cli = Cli::try_parse_from(["gage", "publish", "abort", "abc", "def", "-y"]).unwrap();
        let Command::Publish(PublishArgs {
            command: Some(PublishCommand::Abort(args)),
            ..
        }) = cli.command
        else {
            panic!("expected publish abort");
        };
        assert_eq!(args.session, ["abc", "def"]);
        assert!(Cli::try_parse_from(["gage", "publish", "abort"]).is_err());
    }

    /// A problem renders as a compiler does, with the path.
    #[test]
    fn diagnostic_is_path_line_column_and_message() {
        let problem = Problem {
            line: 4,
            column: 14,
            message: "unknown reason \"bogus\"".into(),
        };
        assert_eq!(
            diagnostic(Path::new("/p/abc.publish.jsonc"), &problem),
            "/p/abc.publish.jsonc:4:14: error: unknown reason \"bogus\""
        );
        let e = PublishError::Definition(vec![problem.clone(), problem]);
        assert_eq!(definition_failure(Path::new("x"), &e).lines().count(), 2);
        let e = PublishError::Unchanged;
        assert_eq!(definition_failure(Path::new("x"), &e), e.to_string());
    }

    /// `gage publish` without a subcommand is a run: its positional
    /// sessions, scope, selection axes, and publish configuration
    /// parse into the run args, and the subcommands still parse. The
    /// positional sessions exclude the axes, `--source` excludes
    /// `--stored`, and `--list-filters` stands alone.
    #[test]
    fn publish_without_a_subcommand_is_a_run() {
        let run_args = |argv: &[&str]| {
            let cli = Cli::try_parse_from([&["gage", "publish"], argv].concat()).unwrap();
            let Command::Publish(PublishArgs {
                command: None,
                run_args,
            }) = cli.command
            else {
                panic!("expected a publish run");
            };
            run_args
        };
        let args = run_args(&[
            "abc",
            "def",
            "-A",
            "public",
            "-U",
            "reading",
            "-f",
            "pii",
            "-g",
            "The question",
            "-N",
            "For the blog",
            "-m",
            "opus",
            "-b",
            "-y",
            "--",
            "--verbose",
        ]);
        assert_eq!(args.sessions, ["abc", "def"]);
        assert_eq!(args.audience.as_deref(), Some("public"));
        assert_eq!(args.r#use.as_deref(), Some("reading"));
        assert_eq!(args.filter, ["pii"]);
        assert_eq!(args.segment, ["The question"]);
        assert_eq!(args.notes.as_deref(), Some("For the blog"));
        assert_eq!(args.model.as_deref(), Some("opus"));
        assert_eq!(args.extra_args, ["--verbose"]);
        assert!(args.batch && args.yes && !args.stored);
        let args = run_args(&["-S", "--days", "7", "--filter", "secret"]);
        assert!(args.stored);
        assert_eq!(args.select.days, Some(7));
        let args = run_args(&["--source", "claude:/tmp/claude", "--all"]);
        assert_eq!(args.source.as_deref(), Some("claude:/tmp/claude"));
        assert!(args.select.all);
        run_args(&["--list-filters"]);
        run_args(&[]);
        Cli::try_parse_from([
            "gage", "publish", "init", "-A", "public", "-U", "reading", "abc",
        ])
        .unwrap();
        for argv in [
            vec!["abc", "--days", "7"],
            vec!["--source", "claude:/tmp/claude", "--stored", "abc"],
            vec!["--list-filters", "abc"],
            vec!["-a", "public", "abc"],
        ] {
            assert!(
                Cli::try_parse_from([&["gage", "publish"], &argv[..]].concat()).is_err(),
                "{argv:?} parsed"
            );
        }
    }
    /// The skill receives the settled inputs as `gage publish init`
    /// options, source first and ids last, with no selection option:
    /// the selection is already resolved to ids.
    #[test]
    fn skill_args_spell_the_init_invocation() {
        let run_args = |argv: &[&str]| {
            let cli = Cli::try_parse_from([&["gage", "publish"], argv].concat()).unwrap();
            let Command::Publish(PublishArgs { run_args, .. }) = cli.command else {
                panic!("not a publish command");
            };
            run_args
        };
        let args = run_args(&["--source", "claude:/tmp/c", "--days", "7", "-f", "pii"]);
        let inputs = InitInputs {
            audience: Some(Audience::Public),
            uses: Some(Use::Reading),
            filters: &args.filter,
            segments: &["The question".to_string()],
            notes: Some("For the blog"),
        };
        let ids = ["n1".to_string(), "n2".to_string()];
        assert_eq!(
            skill_args(&args, &inputs, &ids),
            [
                "--source",
                "claude:/tmp/c",
                "--audience",
                "public",
                "--use",
                "reading",
                "--filter",
                "pii",
                "--segment",
                "The question",
                "--notes",
                "For the blog",
                "n1",
                "n2",
            ]
        );
        let args = run_args(&["-S"]);
        let inputs = InitInputs {
            audience: None,
            uses: None,
            filters: &[],
            segments: &[],
            notes: None,
        };
        assert_eq!(skill_args(&args, &inputs, &[]), ["--stored"]);
    }

    /// The notice names an interactive session without `--batch` and a
    /// publish run with it.
    #[test]
    fn usage_notice_names_the_mode() {
        assert!(usage_notice(false).starts_with("You are about to start an interactive session"));
        assert!(usage_notice(true).starts_with("You are about to publish sessions"));
    }
    /// `--batch` needs a session selection and a filter or a segment;
    /// an interactive run needs neither.
    #[test]
    fn batch_needs_a_selection_and_a_filter_or_segment() {
        let check = |argv: &[&str]| {
            let cli = Cli::try_parse_from([&["gage", "publish"], argv].concat()).unwrap();
            let Command::Publish(PublishArgs { run_args, .. }) = cli.command else {
                panic!("expected publish");
            };
            check_run_args(&run_args)
        };
        assert_eq!(check(&["-b", "abc", "-f", "pii"]), Ok(()));
        assert_eq!(check(&["-b", "--days", "7", "-g", "The question"]), Ok(()));
        assert_eq!(check(&[]), Ok(()));
        assert_eq!(check(&["abc"]), Ok(()));
        assert_eq!(check(&["-f", "pii"]), Ok(()));
        assert_eq!(
            check(&["-b", "-f", "pii"]),
            Err("--batch requires a session selection".to_string())
        );
        assert_eq!(
            check(&["-b", "abc"]),
            Err("--batch requires a filter or segment".to_string())
        );
        assert_eq!(
            check(&["-b", "--days", "7"]),
            Err("--batch requires a filter or segment".to_string())
        );
    }
    /// `apply --force` stands alone; `--force-tag` still needs `--tag`.
    #[test]
    fn apply_force_is_separate_from_force_tag() {
        let cli = Cli::try_parse_from(["gage", "publish", "apply", "abc", "--force"]).unwrap();
        let Command::Publish(PublishArgs {
            command: Some(PublishCommand::Apply(args)),
            ..
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
        let (ids, origins) = list_highlighters(
            &[older.clone(), newer.clone()],
            vec![orig1.to_string(), orig2.to_string()],
        );

        let rows = pending_rows(vec![older.clone(), newer.clone()], &ids, &origins, |id| {
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
    /// A pending id's prefix is unique among the pending publishes
    /// alone, so a stored session sharing its leading characters does
    /// not lengthen it; an origin's prefix is unique among the stored
    /// sessions.
    #[test]
    fn list_highlights_pending_ids_among_pending_publishes_alone() {
        let now = now_ms();
        let one = pending(
            "aaaaaaaa-0000-4000-8000-000000000001",
            "session:aaaaaaaa-0000-4000-8000-000000000002@0123456789abcdef0123456789abcdef01234567",
            None,
            now,
        );
        let stored = vec![
            "aaaaaaaa-0000-4000-8000-000000000002".to_string(),
            "aaaaaaaa-0000-4000-8000-000000000003".to_string(),
        ];
        let (ids, origins) = list_highlighters(std::slice::from_ref(&one), stored);
        assert_eq!(ids.unique_prefix_len(&one.id), 1);
        assert_eq!(
            origins.unique_prefix_len("aaaaaaaa-0000-4000-8000-000000000002"),
            36
        );
    }

    #[test]
    fn origin_cell_shows_a_native_origin_as_stored() {
        let highlighter = IdHighlighter::new(vec![]);
        assert_eq!(
            console::strip_ansi_codes(&origin_cell("claude:/home/u/.claude/abc", &highlighter)),
            "claude:/home/u/.claude/abc"
        );
    }
    #[test]
    fn filter_rows_list_every_named_filter_with_its_values_and_a_description() {
        let rows = filter_rows();
        let mut names: Vec<&str> = rows.iter().map(|r| r[0].as_str()).collect();
        names.sort_unstable();
        let mut expected = NAMED_FILTERS.to_vec();
        expected.sort_unstable();
        assert_eq!(names, expected);
        assert!(rows.iter().all(|r| r.len() == 4 && !r[3].is_empty()));
        let row = |name: &str| rows.iter().find(|r| r[0] == name).unwrap();
        assert_eq!(row("secret")[1], "public, trusted");
        assert_eq!(row("secret")[2], "");
        assert_eq!(row("harness-context")[1], "");
        assert_eq!(row("harness-context")[2], "reading");
        assert_eq!(row("custom")[1], "");
        assert_eq!(row("custom")[2], "");
    }
    /// An init stages what the audience and use select, then the
    /// filters given, each once. The defaults select nothing.
    #[test]
    fn init_filters_are_the_selection_then_the_added_ones_once() {
        let spec = |name: &str| FilterSpec::new(name, None).unwrap();
        let names = |audience: Option<&str>, uses: Option<&str>, added| -> Vec<String> {
            let (Ok(audience), Ok(uses)) = (
                parse_audience("gage publish init", audience),
                parse_use("gage publish init", uses),
            ) else {
                panic!("{audience:?} {uses:?} parse");
            };
            selected_filters(audience, uses, added)
                .into_iter()
                .map(|f| f.name)
                .collect()
        };
        assert_eq!(names(None, None, vec![spec("pii")]), ["pii"]);
        assert_eq!(
            names(
                Some("trusted"),
                Some("reading"),
                vec![spec("secret"), spec("pii")]
            ),
            [
                "secret",
                "language",
                "harness-context",
                "harness-records",
                "pii"
            ]
        );
        let text = match parse_audience("gage publish init", Some("everyone")) {
            Err(CliError::Usage(e)) => e.render().to_string(),
            _ => panic!("not a usage error"),
        };
        assert!(
            text.contains("invalid value 'everyone' for '--audience <AUDIENCE>'"),
            "{text}"
        );
        assert!(
            text.contains("[possible values: self, trusted, public]"),
            "{text}"
        );
        let text = match parse_use("gage publish init", Some("read")) {
            Err(CliError::Usage(e)) => e.render().to_string(),
            _ => panic!("not a usage error"),
        };
        assert!(
            text.contains("[possible values: reading, provenance, other]"),
            "{text}"
        );
    }
}
