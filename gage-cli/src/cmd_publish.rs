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
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::slice;
use std::sync::Arc;

use clap::{Args, Subcommand};
use cliclack as cli;
use console::style;
use gage_core::config::{Config, FilterConfig, gage_home};
use gage_core::style::IdHighlighter;
use gage_core::uuid::short_uuid;
use gage_publish::{
    FilterEntry, InitArgs as InitRecord, Problem, PublishError, Publisher, Staging, TextEntry,
    candidate,
};
use gage_registry::filter::{Filter, FilterDef, FilterRegistry, filter_source};
use gage_registry::scanner::split_scanner_spec;
use gage_session::{Driver, NativeSession, SkillMode, SkillSession, SourceOptions};
use gage_store::url::parse_session_origin;
use gage_store::{
    ExternalSessionRecord, SESSION_TYPE, SessionRecord, SessionStore, ShareStore, Store,
    StoreError, TagStore,
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
use crate::progress;
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

    /// Filter group to apply (repeatable)
    #[arg(
        short,
        long,
        value_name = "GROUP",
        long_help = format!("Filter group to apply (repeatable)\n\n{GROUP_HELP}")
    )]
    group: Vec<String>,

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
    #[arg(short = 't', long, value_name = "TEXT")]
    segment: Vec<String>,

    /// Description of publish for model
    ///
    /// Read from a file as @PATH. If omitted, dialog prompts for an
    /// optional description.
    #[arg(short, long, value_name = "TEXT")]
    description: Option<String>,

    /// Add to share (tag, ID, or prefix; repeatable)
    #[arg(
        long,
        value_name = "SHARE",
        long_help = format!(
            "Add to share (tag, ID, or prefix; repeatable)\n\n{SHARE_HELP} \
             Passed to gage publish apply for each session."
        )
    )]
    share: Vec<String>,

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
    /// Without --model, model takes its default. Without
    /// --description, none is given.
    #[arg(short, long)]
    yes: bool,

    /// Show available filters and exit
    #[arg(long, exclusive = true)]
    list_filters: bool,

    /// Additional arguments passed through to harness
    #[arg(last = true, value_name = "ARGS")]
    extra_args: Vec<String>,
}

/// The paragraph the `--group` options share.
const GROUP_HELP: &str = "Selects every filter that lists group; see --list-filters for \
groups each filter lists. Groups are independent: one implies nothing about another, and a \
group not given selects nothing. A group no filter lists is an error.";

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
    /// Tables session, entry, message, tool_use, tool_use_file, and
    /// session_change hold staged sessions and nothing else; no other
    /// table is present.
    Query(QueryRunArgs),

    /// Show definition of a pending publish
    ///
    /// Show definition of a pending publish. Prints definition file.
    Show(ShowArgs),

    /// Edit definition of a pending publish
    ///
    /// Edit definition of a pending publish. Opens definition file in
    /// $VISUAL or $EDITOR and rebuilds staged session when editor
    /// exits, as refresh does. An invalid definition is reported and
    /// nothing changes; file is kept as saved. With --check, reports
    /// each problem of definition file by line and makes no change
    /// instead. With --path, prints definition file path and makes no
    /// change. With --title, sets published session title and
    /// rebuilds without opening editor. With --redact, adds text
    /// redactions from a file and rebuilds.
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
    /// --split-files. This is what a redaction agent reads. With
    /// --instructions, prints redaction instructions of each filter as
    /// markdown instead: a heading naming filter, then its
    /// instructions with prompt given for it in place of its
    /// placeholder.
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

    /// Filter group to apply (repeatable)
    #[arg(
        short,
        long,
        value_name = "GROUP",
        long_help = format!("Filter group to apply (repeatable)\n\n{GROUP_HELP}")
    )]
    group: Vec<String>,

    /// Filter to apply (repeatable)
    #[arg(
        short,
        long,
        value_name = "FILTER",
        long_help = format!(
            "Filter to apply (repeatable)\n\n{FILTER_HELP} A group or filter is required."
        )
    )]
    filter: Vec<String>,

    /// Prepare session for a share (tag, ID, or prefix)
    ///
    /// Every filter share lists is selected at version share requires;
    /// a filter whose definition does not satisfy that version is an
    /// error. --filter adds to selection, and naming a selected filter
    /// again changes nothing
    #[arg(long = "for-share", value_name = "SHARE")]
    for_share: Option<String>,

    /// Segment to add (repeatable)
    ///
    /// A segment starts without lines and drops nothing until
    /// definition gives it lines.
    #[arg(short = 't', long, value_name = "TEXT")]
    segment: Vec<String>,

    /// Skip confirmation prompt
    #[arg(short, long)]
    yes: bool,

    /// Show available filters and exit
    #[arg(long, exclusive = true)]
    list_filters: bool,
}

/// The paragraph `--share` options share on what adding to a share
/// means.
const SHARE_HELP: &str = "A share holds published sessions. Each named share's filter \
requirements are checked against publish definition before anything is written, and published \
session is linked to each share after it is written.";

/// The paragraph `--filter` options share on what a filter argument
/// is.
const FILTER_HELP: &str = "A filter name; see --list-filters. A trailing #{..} overrides filter \
params, as pii#{ prompt: \"Watch for hosts.\" }. A filter given here is added to those --group \
selects and replaces a selected filter of the same name.";

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
}

#[derive(Args)]
pub struct EditArgs {
    /// Pending publish session to edit (ID or prefix)
    #[arg(value_name = "SESSION")]
    session: String,

    /// Check definition file and report each problem by line
    ///
    /// Check definition file and report each problem by line. A
    /// problem is one line on standard error, as "PATH:LINE:COLUMN:
    /// error: MESSAGE". A file without problems prints nothing. Exit
    /// status is 1 when there is a problem.
    #[arg(long)]
    check: bool,

    /// Print definition file path
    ///
    /// Print definition file path and make no other change. Prints
    /// the absolute path of the definition file on standard output.
    #[arg(long, conflicts_with = "check")]
    path: bool,

    /// Set published session title
    ///
    /// Set published session title and rebuild without opening
    /// editor. Read from a file as @PATH.
    #[arg(long, value_name = "TEXT", conflicts_with_all = ["check", "path"])]
    title: Option<String>,

    /// Add text redactions from a file
    ///
    /// Add text redactions read as JSON from @PATH, or from standard
    /// input as -. Each row is [text, reason], [text, reason, mark],
    /// or [text, reason, author, mark]. A missing author takes edit
    /// author; a missing mark keeps the marker. An exact duplicate of
    /// a recorded row is skipped.
    #[arg(long, value_name = "FILE", conflicts_with_all = ["check", "path"])]
    redact: Option<String>,

    /// Author for rows this edit writes
    ///
    /// Author for rows this edit writes, as given. Without it the
    /// session or user is used. A row that already names an author
    /// keeps it.
    #[arg(long, value_name = "AUTHOR")]
    author: Option<String>,

    /// Skip confirmation prompt
    #[arg(short, long)]
    yes: bool,
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

    /// Print redaction instructions of each filter
    ///
    /// Print redaction instructions of each filter as markdown and
    /// exit. Instructions are what a redaction agent applies to
    /// candidate text.
    #[arg(long, conflicts_with_all = ["split", "split_files", "split_files_dir"])]
    instructions: bool,
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

    /// Add session to share (tag, ID, or prefix; repeatable)
    #[arg(
        long,
        value_name = "SHARE",
        long_help = format!(
            "Add session to share (tag, ID, or prefix; repeatable)\n\n{SHARE_HELP}"
        )
    )]
    share: Vec<String>,

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

    /// Publish session even when definition changed nothing
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
        PublishCommand::Edit(args) => edit(args).await,
        PublishCommand::Refresh(args) => refresh(args).await,
        PublishCommand::Candidate(args) => candidate(args).await,
        PublishCommand::Abort(args) => abort(args),
        PublishCommand::Apply(args) => apply(args).await,
    }
}

/// Publish the selected sessions through Claude Code. The dialog
/// shows the inputs given, asks for a description and a model, and
/// spells the inputs as skill arguments; the skill session runs after
/// it, since the harness owns the terminal from then on.
async fn publish_sessions(args: PublishRunArgs) -> Result<(), CliError> {
    let fail = |e: String| CliError::message(format!("gage publish: {e}"));
    if args.list_filters {
        list_filters(&FilterRegistry::load());
        return Ok(());
    }
    check_run_args(&args).map_err(fail)?;
    let registry = FilterRegistry::load();
    let config = Config::load_user().map_err(|e| fail(format!("reading config: {e}")))?;
    check_groups("gage publish", &registry, &args.group)?;
    let specs = resolve_filter_specs("gage publish", &registry, &args.filter)?;
    let segments = args
        .segment
        .iter()
        .map(|text| {
            text_value("gage publish", "--segment", text)
                .map(|t| t.trim_end().to_string())
                .map_err(|e| fail(e.to_string()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let description = args
        .description
        .as_deref()
        .map(|text| {
            text_value("gage publish", "--description", text)
                .map(|t| t.trim().to_string())
                .map_err(|e| fail(e.to_string()))
        })
        .transpose()?;
    let store = Store::open(&gage_store::store_path()).map_err(|e| fail(e.to_string()))?;
    let shares = resolve_shares("gage publish", &store, &args.share)?;

    let mut setup: Option<PublishSetup> = None;
    dialog::run_async("Publish sessions", || async {
        if !shares.is_empty() {
            dialog::step_list("Shares", shares.iter().map(|s| short_uuid(s).to_string()))?;
        }
        let ids = if selects_sessions(&args) {
            preview_selection(&args, &store).await?
        } else {
            Vec::new()
        };
        if !args.group.is_empty() || !specs.is_empty() {
            select_filters(&registry, &config.filters, &args.group, &specs)?;
        }
        if !segments.is_empty() {
            dialog::step_list("Segments", segments.iter().map(|s| value_cell(s)))?;
        }
        let description = description_step(description, args.yes)?;
        let model = model_prompt::model_step(args.model.as_deref(), args.yes)?;
        if !args.yes {
            dialog::usage_notice(usage_notice(args.batch))?;
            dialog::confirm(false, true)?;
        }
        let inputs = SkillInputs {
            description: description.as_deref(),
            segments: &segments,
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

/// The "Description" step: the given description is shown, `yes`
/// without one gives none, and otherwise an optional prompt asks.
fn description_step(given: Option<String>, yes: bool) -> Result<Option<String>, DialogError> {
    match given {
        Some(description) => {
            dialog::step_value("Description", value_cell(&description))?;
            Ok(Some(description))
        }
        None if yes => Ok(None),
        None => {
            let text: String = cli::input("Description")
                .placeholder("What to publish and for whom (optional)")
                .required(false)
                .interact()?;
            let text = text.trim();
            Ok((!text.is_empty()).then(|| text.to_string()))
        }
    }
}

/// The inputs the dialog settled that `PublishRunArgs` does not hold
/// in the form the skill receives.
struct SkillInputs<'a> {
    description: Option<&'a str>,
    /// The `--segment` values with `@PATH` read
    segments: &'a [String],
}

/// The skill's arguments: the `--description` option the skill reads
/// itself, the `gage publish init` options that spell the inputs
/// given, the `--share` options the skill passes to `gage publish
/// apply`, then the selected session ids. The source comes ahead of
/// the ids, so they read against it; a session selection is already
/// resolved to ids, so no selection option is passed.
fn skill_args(args: &PublishRunArgs, inputs: &SkillInputs<'_>, ids: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if let Some(description) = inputs.description {
        out.extend(["--description".to_string(), description.to_string()]);
    }
    if args.stored {
        out.push("--stored".to_string());
    } else if let Some(source) = &args.source {
        out.extend(["--source".to_string(), source.clone()]);
    }
    for group in &args.group {
        out.extend(["--group".to_string(), group.clone()]);
    }
    for spec in &args.filter {
        out.extend(["--filter".to_string(), spec.clone()]);
    }
    for text in inputs.segments {
        out.extend(["--segment".to_string(), text.clone()]);
    }
    for share in &args.share {
        out.extend(["--share".to_string(), share.clone()]);
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

/// The filters the groups and `--filter` values select, shown as the
/// `Groups` and `Filters` steps. With neither given, the selection
/// fails.
fn select_filters<'a>(
    registry: &'a FilterRegistry,
    config: &FilterConfig,
    groups: &[String],
    specs: &'a [ResolvedSpec<'a>],
) -> Result<Vec<Filter<'a>>, DialogError> {
    if groups.is_empty() && specs.is_empty() {
        return Err(DialogError::Failed(
            "A filter or group is required; see --list-filters".to_string(),
        ));
    }
    if !groups.is_empty() {
        dialog::step_list("Groups", groups.iter().cloned())?;
    }
    let filters = selected_filters(registry, config, groups, specs).map_err(DialogError::Failed)?;
    dialog::step_list("Filters", filters.iter().map(|f| f.def.name.clone()))?;
    Ok(filters)
}

/// The filters `groups` select in listing order, each once, then each
/// of `specs`, replacing a selected filter of the same name or
/// following the rest.
fn selected_filters<'a>(
    registry: &'a FilterRegistry,
    config: &FilterConfig,
    groups: &[String],
    specs: &'a [ResolvedSpec<'a>],
) -> Result<Vec<Filter<'a>>, String> {
    let mut filters: Vec<Filter<'a>> = Vec::new();
    for group in groups {
        for def in registry.group_members(group) {
            if filters.iter().any(|f| f.def.name == def.name) {
                continue;
            }
            let filter =
                Filter::from_spec(def, config, None, &def.name).map_err(|e| e.to_string())?;
            filters.push(filter);
        }
    }
    for spec in specs {
        let filter = spec.filter(config)?;
        match filters.iter_mut().find(|f| f.def.name == filter.def.name) {
            Some(selected) => *selected = filter,
            None => filters.push(filter),
        }
    }
    Ok(filters)
}

/// The rule clap does not express: `--batch` runs without a session
/// to ask in, so it needs a session selection and a group, a filter,
/// or a segment. An interactive run takes whatever is given.
fn check_run_args(args: &PublishRunArgs) -> Result<(), String> {
    if !args.batch {
        return Ok(());
    }
    if !selects_sessions(args) {
        return Err("--batch requires a session selection".to_string());
    }
    if args.group.is_empty() && args.filter.is_empty() && args.segment.is_empty() {
        return Err("--batch requires a group, filter, or segment".to_string());
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
    let pending = Staging::from(&store)
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
    let publisher = Publisher::new(&store, FilterRegistry::load());
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
    pending: &[ExternalSessionRecord],
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
    mut pending: Vec<ExternalSessionRecord>,
    ids: &IdHighlighter,
    origins: &IdHighlighter,
    redactions: impl Fn(&str) -> Result<String, PublishError>,
) -> Result<Vec<Vec<String>>, PublishError> {
    pending.sort_by_key(|p| Reverse(p.attrs_modified_ms));
    pending
        .iter()
        .map(|p| {
            Ok(vec![
                ids.short(&p.id),
                origin_cell(&p.attrs.origin, origins),
                p.attrs.summary.title.clone().unwrap_or_default(),
                redactions(&p.id)?,
                format_elapsed_ms(p.attrs_modified_ms),
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
/// sessions, groups, and filters are resolved before the dialog opens,
/// so a bad argument fails without a prompt, and a native session is
/// added to the store only after the user confirms. The dialog
/// previews and confirms; it prompts for nothing, so a run without a
/// group or filter fails.
async fn init(args: InitArgs) -> Result<(), CliError> {
    let fail = |e: String| CliError::message(format!("gage publish init: {e}"));
    if args.list_filters {
        list_filters(&FilterRegistry::load());
        return Ok(());
    }
    check_init_args(&args).map_err(fail)?;
    let store = Store::open(&gage_store::store_path()).map_err(|e| fail(e.to_string()))?;
    let publisher = Publisher::new(&store, FilterRegistry::load());
    let registry = publisher.filters();
    let config = Config::load_user().map_err(|e| fail(format!("reading config: {e}")))?;
    check_groups("gage publish init", registry, &args.group)?;
    let mut filter_args = args.filter.clone();
    if let Some(share) = &args.for_share {
        for name in filters_for_share("gage publish init", &store, registry, share)? {
            let named = filter_args
                .iter()
                .any(|spec| split_scanner_spec(spec).0 == name);
            if !named {
                filter_args.push(name);
            }
        }
    }
    let specs = resolve_filter_specs("gage publish init", registry, &filter_args)?;
    let segments = args
        .segment
        .iter()
        .map(|text| {
            text_value("gage publish init", "--segment", text)
                .map(|t| t.trim_end().to_string())
                .map_err(|e| fail(e.to_string()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let record = InitRecord {
        groups: args.group.clone(),
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
    dialog::run_async(title, || async {
        originals.preview()?;
        let filters: Vec<FilterEntry> =
            select_filters(registry, &config.filters, &args.group, &specs)?
                .iter()
                .map(FilterEntry::from_filter)
                .collect();
        if !segments.is_empty() {
            dialog::step_list("Segments", segments.iter().map(|s| value_cell(s)))?;
        }
        dialog::confirm(args.yes, true)?;
        let total = originals.len();
        let mut staged: Vec<String> = Vec::new();
        for original in originals.into_stored(&store) {
            let stage = async {
                let record_of = original?;
                let outcome = publisher
                    .init(&record_of, &filters, &segments, &author, &record)
                    .await
                    .map_err(|e| e.to_string())?;
                cli::log::info(format!(
                    "Staged publish {} of session {}",
                    short_uuid(&outcome.id),
                    short_uuid(&record_of.id)
                ))
                .map_err(|e| e.to_string())?;
                report_filter_redactions(&publisher, &outcome.id)
                    .await
                    .map_err(|e| e.to_string())?;
                Ok::<String, String>(outcome.id)
            };
            match stage.await {
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
    .await
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

/// The author a publish edit stamps on the rows it writes: the
/// `--author` value as given, else the session or user from
/// [`default_author`].
fn edit_author(explicit: Option<&str>) -> String {
    match explicit {
        Some(author) => author.to_string(),
        None => default_author(),
    }
}

/// Print the filters as a table, the way `gage scan --list-scanners`
/// prints scanners, then the files that failed to parse and the
/// declarations with problems.
fn list_filters(registry: &FilterRegistry) {
    let header: Vec<String> = ["Filter", "Source", "Groups", "Description"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let rows = filter_rows(registry);
    let mut table = Table::from_iter(std::iter::once(header).chain(rows));
    table.with(Style::rounded());
    if let Some(width) = style::stdout_width() {
        table.with(
            Width::wrap(width)
                .keep_words(true)
                .priority(Priority::max(true)),
        );
    }
    table
        .modify(Rows::first(), style::tty(Color::FG_BRIGHT_YELLOW))
        .modify(
            Columns::one(0).not(Rows::first()),
            style::tty(Color::FG_BRIGHT_YELLOW),
        )
        .modify(Columns::new(1..).not(Rows::first()), style::dim());
    println!("{table}");

    let errors: Vec<_> = registry.parse_errors().collect();
    if !errors.is_empty() {
        println!();
        eprintln!(
            "{}",
            style(format!("{} filter file(s) failed to parse:", errors.len()))
                .red()
                .bold()
        );
        for (path, rendered) in errors {
            eprintln!();
            eprintln!("{}", style(path.display()).yellow());
            eprintln!("{rendered}");
        }
    }

    let invalid: Vec<&FilterDef> = registry
        .list()
        .into_iter()
        .filter(|d| !d.problems.is_empty())
        .collect();
    if !invalid.is_empty() {
        println!();
        eprintln!(
            "{}",
            style(format!(
                "{} filter(s) have an invalid FILTER declaration:",
                invalid.len()
            ))
            .red()
            .bold()
        );
        for def in invalid {
            eprintln!();
            eprintln!("{}", style(def.path.display()).yellow());
            eprintln!("{}", def.render_problems());
        }
    }
}

/// One row per filter, by name: name, where its file lives, the
/// groups it lists (empty when it lists none), and description.
fn filter_rows(registry: &FilterRegistry) -> Vec<Vec<String>> {
    registry
        .list()
        .into_iter()
        .map(|d| {
            vec![
                d.name.clone(),
                filter_source(&d.path).to_string(),
                d.groups.join(", "),
                d.description.clone(),
            ]
        })
        .collect()
}

/// One `--filter` value resolved against the registry.
struct ResolvedSpec<'a> {
    def: &'a FilterDef,
    /// The `#{...}` params override, when given
    params: Option<&'a str>,
    /// The value as the user gave it, for diagnostics
    spec: &'a str,
}

impl ResolvedSpec<'_> {
    /// The filter with the config's params and its params override
    /// applied.
    fn filter(&self, config: &FilterConfig) -> Result<Filter<'_>, String> {
        Filter::from_spec(self.def, config, self.params, self.spec).map_err(|e| e.to_string())
    }
}

/// Resolve every `--filter` value, in order. A value is a filter name
/// with an optional `#{...}` params override. A name outside the
/// registry is a usage error in clap's layout, since the argument
/// accepts any text and the names are the registry's to decide; a
/// filter named twice is a plain error.
fn resolve_filter_specs<'a>(
    command: &str,
    registry: &'a FilterRegistry,
    specs: &'a [String],
) -> Result<Vec<ResolvedSpec<'a>>, CliError> {
    let mut out: Vec<ResolvedSpec<'a>> = Vec::with_capacity(specs.len());
    for spec in specs {
        let (name, params) = split_scanner_spec(spec);
        let Some(def) = registry.get_def(name) else {
            return Err(CliError::invalid_value(
                command,
                "--filter",
                name,
                &registry.names(),
            ));
        };
        if out.iter().any(|r| r.def.name == def.name) {
            return Err(CliError::message(format!(
                "{command}: filter {name:?} given more than once"
            )));
        }
        out.push(ResolvedSpec { def, params, spec });
    }
    Ok(out)
}

/// The filter names a share requires, for `--for`: each entry
/// of the share's spec, after checking that the registry has the
/// filter and that its declared version satisfies the share's
/// requirement.
fn filters_for_share(
    command: &str,
    store: &Store,
    registry: &FilterRegistry,
    share: &str,
) -> Result<Vec<String>, CliError> {
    let record = ShareStore::from(store)
        .get(share)
        .map_err(|e| CliError::message(format!("{command}: --for-share {share}: {e}")))?;
    let specs = record.attrs.filter_specs().map_err(|e| {
        CliError::message(format!("{command}: share {}: {e}", short_uuid(&record.id)))
    })?;
    let mut names = Vec::with_capacity(specs.len());
    for spec in specs {
        let Some(def) = registry.get_def(&spec.name) else {
            return Err(CliError::message(format!(
                "{command}: share {} requires filter {:?}, which is not a known filter",
                short_uuid(&record.id),
                spec.name
            )));
        };
        let version = Some(def.version.as_str()).filter(|v| !v.is_empty());
        if !spec.accepts(version) {
            return Err(CliError::message(format!(
                "{command}: share {} requires {spec}; filter {} is at version {}",
                short_uuid(&record.id),
                spec.name,
                version.unwrap_or("none")
            )));
        }
        names.push(spec.name);
    }
    Ok(names)
}

/// Resolve every `--share` value to a share id before anything runs,
/// so one bad argument leaves nothing half done.
fn resolve_shares(
    command: &str,
    store: &Store,
    shares: &[String],
) -> Result<Vec<String>, CliError> {
    let share_store = ShareStore::from(store);
    let mut ids = Vec::with_capacity(shares.len());
    for share in shares {
        let id = share_store
            .resolve_id(share)
            .map_err(|e| CliError::message(format!("{command}: --share {share}: {e}")))?;
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    Ok(ids)
}

/// Every group in `groups` is one some filter lists. A group outside
/// them is a usage error in clap's layout, since the argument accepts
/// any text and the groups are the registry's to decide.
fn check_groups(
    command: &str,
    registry: &FilterRegistry,
    groups: &[String],
) -> Result<(), CliError> {
    let known = registry.groups();
    for group in groups {
        if !known.contains(&group.as_str()) {
            return Err(CliError::invalid_value(command, "--group", group, &known));
        }
    }
    Ok(())
}

/// Report what the filters found: the text redactions each recorded
/// and the spans they produce.
async fn report_filter_redactions(publisher: &Publisher<'_>, id: &str) -> Result<(), PublishError> {
    let definition = publisher.definition(id)?;
    let counts = publisher.instances(id).await?;
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

/// Print the definition file of a pending publish or the problems the
/// file has.
fn show(args: ShowArgs) -> Result<(), CliError> {
    let fail = |e: String| CliError::message(format!("gage publish show: {e}"));
    let store = Store::open(&gage_store::store_path()).map_err(|e| fail(e.to_string()))?;
    let publisher = Publisher::new(&store, FilterRegistry::load());
    let pending = publisher
        .staging()
        .get(&args.session)
        .map_err(|e| fail(e.to_string()))?;
    let path = publisher.staging().definition_file(&pending.id);
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

/// Open the definition file in the editor and rebuild when the editor
/// exits. The editor save is the confirmation. A file the editor left
/// as it was rebuilds nothing.
async fn edit(args: EditArgs) -> Result<(), CliError> {
    let fail = |e: String| CliError::message(format!("gage publish edit: {e}"));
    let store = Store::open(&gage_store::store_path()).map_err(|e| fail(e.to_string()))?;
    let publisher = Publisher::new(&store, FilterRegistry::load());
    let pending = publisher
        .staging()
        .get(&args.session)
        .map_err(|e| fail(e.to_string()))?;
    let path = publisher.staging().definition_file(&pending.id);
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
    if args.path {
        println!("{}", path.display());
        return Ok(());
    }
    if args.title.is_some() || args.redact.is_some() {
        let author = edit_author(args.author.as_deref());
        let title = match &args.title {
            Some(t) => Some(
                text_value("gage publish edit", "--title", t)
                    .map(|t| t.trim().to_string())
                    .map_err(|e| fail(e.to_string()))?,
            ),
            None => None,
        };
        let redactions = match &args.redact {
            Some(src) => parse_redactions("gage publish edit", src, &author).map_err(fail)?,
            None => Vec::new(),
        };
        return refresh_dialog(
            "Edit publish",
            &publisher,
            &pending,
            &author,
            title.as_deref(),
            &redactions,
            !args.yes,
        )
        .await;
    }
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
    refresh_dialog(
        "Edit publish",
        &publisher,
        &pending,
        &default_author(),
        None,
        &[],
        false,
    )
    .await
}

/// The redaction rows a `--redact` value carries. The value is `-`
/// for standard input or `@PATH` for a file. A row without an author
/// takes `author`, and a row without a mark keeps the marker.
fn parse_redactions(command: &str, value: &str, author: &str) -> Result<Vec<TextEntry>, String> {
    let text = read_redact_input(command, value)?;
    parse_redaction_rows(command, &text, author)
}

/// The redaction rows of `text`, a JSON array of rows. Each row is
/// `[text, reason]`, `[text, reason, mark]`, or
/// `[text, reason, author, mark]`; the shorter forms take `author`
/// and a kept marker.
fn parse_redaction_rows(command: &str, text: &str, author: &str) -> Result<Vec<TextEntry>, String> {
    let rows: Vec<Vec<serde_json::Value>> =
        serde_json::from_str(text).map_err(|e| format!("{command}: --redact: {e}"))?;
    let mut out = Vec::with_capacity(rows.len());
    for (i, row) in rows.iter().enumerate() {
        let at = |m: &str| format!("{command}: --redact row {}: {m}", i + 1);
        if row.len() < 2 || row.len() > 4 {
            return Err(at(
                "row is [text, reason], [text, reason, mark], or [text, reason, author, mark]",
            ));
        }
        let literal = row
            .first()
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| at("text is a string"))?;
        let reason = row
            .get(1)
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| at("reason is a string"))?;
        let (row_author, mark) = match row.len() {
            2 => (author.to_string(), true),
            3 => (
                author.to_string(),
                row.get(2)
                    .and_then(serde_json::Value::as_bool)
                    .ok_or_else(|| at("mark is a bool"))?,
            ),
            _ => (
                row.get(2)
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| at("author is a string"))?
                    .to_string(),
                row.get(3)
                    .and_then(serde_json::Value::as_bool)
                    .ok_or_else(|| at("mark is a bool"))?,
            ),
        };
        out.push(TextEntry {
            literal: literal.to_string(),
            reason: reason.to_string(),
            author: row_author,
            mark,
        });
    }
    Ok(out)
}

/// The text a `--redact` value names: standard input for `-`, or the
/// file named after `@`.
fn read_redact_input(command: &str, value: &str) -> Result<String, String> {
    use std::io::Read;
    if value == "-" {
        let mut buf = String::new();
        io::stdin()
            .read_to_string(&mut buf)
            .map_err(|e| format!("{command}: --redact -: {e}"))?;
        return Ok(buf);
    }
    match value.strip_prefix('@') {
        Some(path) => {
            fs::read_to_string(path).map_err(|e| format!("{command}: --redact {path}: {e}"))
        }
        None => Err(format!("{command}: --redact takes - or @PATH")),
    }
}

/// Rebuild a pending publish from its definition after confirmation.
async fn refresh(args: RefreshArgs) -> Result<(), CliError> {
    let fail = |e: String| CliError::message(format!("gage publish refresh: {e}"));
    let store = Store::open(&gage_store::store_path()).map_err(|e| fail(e.to_string()))?;
    let publisher = Publisher::new(&store, FilterRegistry::load());
    let pending = publisher
        .staging()
        .get(&args.session)
        .map_err(|e| fail(e.to_string()))?;
    refresh_dialog(
        "Refresh publish",
        &publisher,
        &pending,
        &default_author(),
        None,
        &[],
        !args.yes,
    )
    .await
}

/// The dialog that rebuilds `pending` from its definition: the
/// definition is read and checked, its counts shown, the rebuild
/// confirmed when `confirm` is set and run under a spinner, and every
/// redaction that matches nothing reported by its position. A
/// definition with problems ends the dialog, and the problems follow
/// it one per line as `edit --check` prints them.
async fn refresh_dialog(
    heading: &str,
    publisher: &Publisher<'_>,
    pending: &ExternalSessionRecord,
    author: &str,
    new_title: Option<&str>,
    redactions: &[TextEntry],
    confirm: bool,
) -> Result<(), CliError> {
    let path = publisher.staging().definition_file(&pending.id);
    let mut problems = Vec::new();
    let result = dialog::run_async(heading, || async {
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
        if let Some(text) = new_title {
            definition.set_title(text, author);
        }
        definition.add_redactions(redactions);
        definition.fill_authors(author);
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
        dialog::confirm(!confirm, true)?;
        let refreshed = progress::busy_async(
            "Refreshing session",
            publisher.edit(&pending.id, author, new_title, redactions),
        )
        .await
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
    })
    .await;
    for problem in &problems {
        eprintln!("{}", diagnostic(&path, problem));
    }
    result
}

/// List the text values open to redaction, one per line where the
/// value fits, else as a header with the text indented below it, or
/// the redaction instructions of the definition's filters.
async fn candidate(args: CandidateArgs) -> Result<(), CliError> {
    let fail = |e: String| CliError::message(format!("gage publish candidate: {e}"));
    let store = Store::open(&gage_store::store_path()).map_err(|e| fail(e.to_string()))?;
    let publisher = Publisher::new(&store, FilterRegistry::load());
    let pending = publisher
        .staging()
        .get(&args.session)
        .map_err(|e| fail(e.to_string()))?;
    let path = publisher.staging().definition_file(&pending.id);
    if args.instructions {
        let markdown = publisher
            .instructions(&pending.id)
            .await
            .map_err(|e| fail(definition_failure(&path, &e)))?;
        print!("{markdown}");
        return Ok(());
    }
    let texts = publisher
        .candidate_texts(&pending.id)
        .await
        .map_err(|e| fail(definition_failure(&path, &e)))?;
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

/// Delete a pending publish after confirmation. The id is resolved
/// before the dialog opens, so a bad argument fails without a prompt.
fn abort(args: AbortArgs) -> Result<(), CliError> {
    let fail = |e: String| CliError::message(format!("gage publish abort: {e}"));
    let store = Store::open(&gage_store::store_path()).map_err(|e| fail(e.to_string()))?;
    let publish = Staging::from(&store);
    let mut pending: Vec<ExternalSessionRecord> = Vec::new();
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
        dialog::confirm(args.yes, false)?;
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
async fn apply(args: ApplyArgs) -> Result<(), CliError> {
    let fail = |e: String| CliError::message(format!("gage publish apply: {e}"));
    let store = Store::open(&gage_store::store_path()).map_err(|e| fail(e.to_string()))?;
    let publisher = Publisher::new(&store, FilterRegistry::load());
    let pending = publisher
        .staging()
        .get(&args.session)
        .map_err(|e| fail(e.to_string()))?;
    let path = publisher.staging().definition_file(&pending.id);
    let definition = publisher
        .definition(&pending.id)
        .map_err(|e| fail(definition_failure(&path, &e)))?;
    publisher
        .verify(&pending.id, args.force)
        .await
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
    // Every share admits the publish as defined, before anything is
    // written; a refusal names the session and the rule it fails
    let share_ids = resolve_shares("gage publish apply", &store, &args.share)?;
    let shares = ShareStore::from(&store);
    let publish_attrs = publisher.publish_attrs(&definition);
    for share in &share_ids {
        shares
            .accepts(share, &pending.id, &publish_attrs)
            .map_err(|e| fail(e.to_string()))?;
    }
    let instances: usize = publisher
        .instances(&pending.id)
        .await
        .map_err(|e| fail(e.to_string()))?
        .values()
        .sum();

    dialog::run_async("Apply publish", || async {
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
        if !share_ids.is_empty() {
            dialog::step_list(
                "Shares",
                share_ids.iter().map(|s| short_uuid(s).to_string()),
            )?;
        }
        dialog::confirm(args.yes, true)?;
        publisher
            .apply(&pending.id, args.force)
            .await
            .map_err(|e| DialogError::Failed(e.to_string()))?;
        for share in &share_ids {
            shares
                .link(share, slice::from_ref(&pending.id))
                .map_err(|e| DialogError::Failed(e.to_string()))?;
            cli::log::info(format!(
                "Added session {} to share {}",
                short_uuid(&pending.id),
                short_uuid(share)
            ))?;
        }
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
    .await
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

    /// The registry over the filters this repository ships.
    fn registry() -> FilterRegistry {
        let builtin = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../filters"));
        FilterRegistry::load_from_roots(&builtin, vec![builtin.clone()])
    }

    /// An unknown filter name reports as clap reports a value outside
    /// a known set, naming the argument as its help does, the names,
    /// and the nearest one; a filter given twice is a plain error.
    #[test]
    fn unknown_filter_name_is_a_usage_error_in_claps_layout() {
        let registry = registry();
        let specs = ["secrets".to_string()];
        let rendered = match resolve_filter_specs("gage publish init", &registry, &specs) {
            Err(CliError::Usage(e)) => {
                assert_eq!(e.exit_code(), 2);
                e.render().to_string()
            }
            Err(_) => panic!("not a usage error"),
            Ok(_) => panic!("secrets resolved"),
        };
        assert!(
            rendered.contains("invalid value 'secrets' for '--filter <FILTER>'"),
            "{rendered}"
        );
        assert!(
            rendered.contains("[possible values: harness, language, pii, secret, system]"),
            "{rendered}"
        );
        assert!(
            rendered.contains("a similar value exists: 'secret'"),
            "{rendered}"
        );
        let specs = ["secret".to_string(), "secret#{ prompt: \"x\" }".to_string()];
        let message = match resolve_filter_specs("gage publish init", &registry, &specs) {
            Err(CliError::Exit {
                message: Some(m), ..
            }) => m,
            Err(_) => panic!("not a plain error"),
            Ok(_) => panic!("a duplicate resolved"),
        };
        assert!(message.contains("given more than once"), "{message}");
    }

    /// A group no filter lists reports as clap reports a value outside
    /// a known set, naming the argument as its help does and the
    /// groups the filters list.
    #[test]
    fn unknown_group_is_a_usage_error_in_claps_layout() {
        let registry = registry();
        let rendered = match check_groups("gage publish init", &registry, &["everyone".into()]) {
            Err(CliError::Usage(e)) => {
                assert_eq!(e.exit_code(), 2);
                e.render().to_string()
            }
            Err(_) => panic!("not a usage error"),
            Ok(()) => panic!("everyone passed"),
        };
        assert!(
            rendered.contains("invalid value 'everyone' for '--group <GROUP>'"),
            "{rendered}"
        );
        assert!(
            rendered.contains("[possible values: public, reading, trusted]"),
            "{rendered}"
        );
        assert!(matches!(
            check_groups(
                "gage publish init",
                &registry,
                &["public".into(), "reading".into()],
            ),
            Ok(())
        ));
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
            "-t",
            "The question",
            "--segment",
            "The reply",
            "-g",
            "public",
            "--group",
            "reading",
            "-y",
        ]);
        assert_eq!(args.sessions, ["abc", "def"]);
        assert_eq!(args.segment, ["The question", "The reply"]);
        assert_eq!(args.group, ["public", "reading"]);
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
        let args = init_args(&["-g", "public"]);
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
            vec!["edit", "abc", "--check"],
            vec!["edit", "abc", "--path"],
            vec!["edit", "abc", "--title", "New"],
            vec!["edit", "abc", "--title", "New", "-y"],
            vec!["edit", "abc", "--redact", "@/tmp/r.json"],
            vec!["edit", "abc", "--redact", "-"],
            vec!["edit", "abc", "--title", "New", "--redact", "-", "-y"],
            vec!["edit", "abc"],
            vec!["refresh", "abc", "-y"],
            vec!["candidate", "abc", "--split-files"],
            vec!["candidate", "abc", "--instructions"],
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
        // Instructions are a mode of candidate, apart from its splits
        assert!(Cli::try_parse_from(["gage", "publish", "show", "abc", "--instructions"]).is_err());
        assert!(Cli::try_parse_from(["gage", "publish", "show", "abc", "--check"]).is_err());
        assert!(
            Cli::try_parse_from(["gage", "publish", "edit", "abc", "--title", "X", "--check"])
                .is_err()
        );
        assert!(
            Cli::try_parse_from(["gage", "publish", "edit", "abc", "--redact", "-", "--check"])
                .is_err()
        );
        assert!(
            Cli::try_parse_from(["gage", "publish", "edit", "abc", "--check", "--path"]).is_err()
        );
        for argv in [
            vec!["candidate", "abc", "--instructions", "--split", "10"],
            vec!["candidate", "abc", "--instructions", "--split-files"],
            vec![
                "candidate",
                "abc",
                "--instructions",
                "--split-files-dir",
                "/tmp/c",
            ],
        ] {
            assert!(
                Cli::try_parse_from([&["gage", "publish"], &argv[..]].concat()).is_err(),
                "{argv:?} parsed"
            );
        }
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
            "-g",
            "public",
            "-g",
            "reading",
            "-f",
            "pii",
            "-t",
            "The question",
            "-d",
            "For the blog",
            "-m",
            "opus",
            "-b",
            "-y",
            "--",
            "--verbose",
        ]);
        assert_eq!(args.sessions, ["abc", "def"]);
        assert_eq!(args.group, ["public", "reading"]);
        assert_eq!(args.filter, ["pii"]);
        assert_eq!(args.segment, ["The question"]);
        assert_eq!(args.description.as_deref(), Some("For the blog"));
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
        Cli::try_parse_from(["gage", "publish", "init", "-g", "public", "abc"]).unwrap();
        for argv in [
            vec!["abc", "--days", "7"],
            vec!["--source", "claude:/tmp/claude", "--stored", "abc"],
            vec!["--list-filters", "abc"],
            vec!["-a", "public", "abc"],
            vec!["-A", "public", "abc"],
            vec!["-U", "reading", "abc"],
        ] {
            assert!(
                Cli::try_parse_from([&["gage", "publish"], &argv[..]].concat()).is_err(),
                "{argv:?} parsed"
            );
        }
    }
    /// The skill receives the description as `--description` first,
    /// then the inputs given as `gage publish init` options, source
    /// ahead of the ids and ids last, with no selection option: the
    /// selection is already resolved to ids. Without a description,
    /// no `--description` is passed.
    #[test]
    fn skill_args_spell_the_init_invocation() {
        let run_args = |argv: &[&str]| {
            let cli = Cli::try_parse_from([&["gage", "publish"], argv].concat()).unwrap();
            let Command::Publish(PublishArgs { run_args, .. }) = cli.command else {
                panic!("not a publish command");
            };
            run_args
        };
        let args = run_args(&[
            "--source",
            "claude:/tmp/c",
            "--days",
            "7",
            "-g",
            "public",
            "-f",
            "pii#{ prompt: \"Watch for hosts.\" }",
        ]);
        let inputs = SkillInputs {
            description: Some("For the blog"),
            segments: &["The question".to_string()],
        };
        let ids = ["n1".to_string(), "n2".to_string()];
        assert_eq!(
            skill_args(&args, &inputs, &ids),
            [
                "--description",
                "For the blog",
                "--source",
                "claude:/tmp/c",
                "--group",
                "public",
                "--filter",
                "pii#{ prompt: \"Watch for hosts.\" }",
                "--segment",
                "The question",
                "n1",
                "n2",
            ]
        );
        let args = run_args(&["-S"]);
        let inputs = SkillInputs {
            description: None,
            segments: &[],
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
    /// `--batch` needs a session selection and a group, a filter, or
    /// a segment; an interactive run needs neither.
    #[test]
    fn batch_needs_a_selection_and_a_group_filter_or_segment() {
        let check = |argv: &[&str]| {
            let cli = Cli::try_parse_from([&["gage", "publish"], argv].concat()).unwrap();
            let Command::Publish(PublishArgs { run_args, .. }) = cli.command else {
                panic!("expected publish");
            };
            check_run_args(&run_args)
        };
        assert_eq!(check(&["-b", "abc", "-f", "pii"]), Ok(()));
        assert_eq!(check(&["-b", "abc", "-g", "public"]), Ok(()));
        assert_eq!(check(&["-b", "--days", "7", "-t", "The question"]), Ok(()));
        assert_eq!(check(&[]), Ok(()));
        assert_eq!(check(&["abc"]), Ok(()));
        assert_eq!(check(&["-f", "pii"]), Ok(()));
        assert_eq!(
            check(&["-b", "-f", "pii"]),
            Err("--batch requires a session selection".to_string())
        );
        assert_eq!(
            check(&["-b", "abc"]),
            Err("--batch requires a group, filter, or segment".to_string())
        );
        assert_eq!(
            check(&["-b", "--days", "7"]),
            Err("--batch requires a group, filter, or segment".to_string())
        );
    }
    #[test]
    fn redact_rows_fill_author_and_mark_by_arity() {
        let json = r#"[
            ["a", "pii"],
            ["b", "secret", false],
            ["c", "language", "user:x", false]
        ]"#;
        let rows = parse_redaction_rows("cmd", json, "session:s").unwrap();
        assert_eq!(
            rows,
            vec![
                TextEntry {
                    literal: "a".into(),
                    reason: "pii".into(),
                    author: "session:s".into(),
                    mark: true,
                },
                TextEntry {
                    literal: "b".into(),
                    reason: "secret".into(),
                    author: "session:s".into(),
                    mark: false,
                },
                TextEntry {
                    literal: "c".into(),
                    reason: "language".into(),
                    author: "user:x".into(),
                    mark: false,
                },
            ]
        );
    }

    #[test]
    fn redact_rows_reject_bad_arity_and_types() {
        assert!(parse_redaction_rows("cmd", r#"[["only"]]"#, "a").is_err());
        assert!(parse_redaction_rows("cmd", r#"[["a", "b", "c", "d", "e"]]"#, "a").is_err());
        assert!(parse_redaction_rows("cmd", r#"[["a", 5]]"#, "a").is_err());
        assert!(parse_redaction_rows("cmd", r#"[["a", "b", "x"]]"#, "a").is_err());
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
    fn pending(
        id: &str,
        origin: &str,
        title: Option<&str>,
        modified_ms: i64,
    ) -> ExternalSessionRecord {
        ExternalSessionRecord {
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
                publish: None,
            },
            dir: PathBuf::from("/tmp/publish").join(id),
            attrs_modified_ms: modified_ms,
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
    /// The listing has one row per filter by name, with its source,
    /// the groups it lists, and a description.
    #[test]
    fn filter_rows_list_every_filter_with_its_source_groups_and_description() {
        let registry = registry();
        let rows = filter_rows(&registry);
        let names: Vec<&str> = rows.iter().map(|r| r[0].as_str()).collect();
        assert_eq!(names, registry.names());
        assert!(rows.iter().all(|r| r.len() == 4 && !r[3].is_empty()));
        let row = |name: &str| rows.iter().find(|r| r[0] == name).unwrap();
        // The repository's filters directory is outside Gage home, so
        // the source is a project's
        assert_eq!(row("secret")[1], "project");
        assert_eq!(row("secret")[2], "public, trusted");
        assert_eq!(row("harness")[2], "reading");
        assert_eq!(row("pii")[2], "public");
    }

    /// With neither a group nor a filter, the selection fails naming
    /// the requirement.
    #[test]
    fn selection_without_a_group_or_filter_fails() {
        let registry = registry();
        let config = FilterConfig::default();
        match select_filters(&registry, &config, &[], &[]) {
            Err(DialogError::Failed(msg)) => {
                assert_eq!(msg, "A filter or group is required; see --list-filters");
            }
            _ => panic!("expected the missing-filter failure"),
        }
    }

    /// An init stages what the groups select in listing order, then
    /// the filters given, each once; a filter given by `--filter`
    /// replaces the group's copy and carries its params.
    #[test]
    fn init_filters_are_the_selection_then_the_added_ones_once() {
        let registry = registry();
        let config = FilterConfig::default();
        let names = |groups: &[&str], specs: &[String]| -> Vec<String> {
            let groups: Vec<String> = groups.iter().map(|g| g.to_string()).collect();
            let Ok(specs) = resolve_filter_specs("gage publish init", &registry, specs) else {
                panic!("{specs:?} resolve");
            };
            selected_filters(&registry, &config, &groups, &specs)
                .unwrap()
                .into_iter()
                .map(|f| f.def.name.clone())
                .collect()
        };
        assert_eq!(names(&[], &["pii".to_string()]), ["pii"]);
        assert_eq!(
            names(
                &["trusted", "reading"],
                &["secret".to_string(), "pii".to_string()]
            ),
            ["language", "secret", "harness", "pii"]
        );
        assert_eq!(
            names(&["public", "trusted"], &[]),
            ["language", "pii", "secret", "system"]
        );
        let specs = vec!["secret#{ prompt: \"Watch for hosts.\" }".to_string()];
        let Ok(specs) = resolve_filter_specs("gage publish init", &registry, &specs) else {
            panic!("{specs:?} resolve");
        };
        let filters =
            selected_filters(&registry, &config, &["trusted".to_string()], &specs).unwrap();
        let secret = filters.iter().find(|f| f.def.name == "secret").unwrap();
        assert_eq!(
            secret.params.as_ref().unwrap()["prompt"],
            "Watch for hosts.",
            "{secret:?}"
        );
        let entry = FilterEntry::from_filter(secret);
        assert_eq!(entry.params.unwrap()["prompt"], "Watch for hosts.");
    }
}
