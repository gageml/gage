//! `gage share`: the shares of published sessions.
//!
//! A share holds published sessions and the filters every member was
//! published with. `add`, `link`, and `edit --filters` apply the
//! membership rule before anything is written; `gage publish apply
//! --share` is the other path a session takes into a share.

use std::sync::{Arc, Mutex};

use anyhow::Context;
use clap::{ArgGroup, Args, Subcommand};
use cliclack as cli;
use datafusion::arrow::array::{Int64Array, StringArray, TimestampMillisecondArray};
use gage_core::datetime::ms_to_iso8601;
use gage_core::uuid::short_uuid;
use gage_query::ContextBuilder;
use gage_registry::filter::FilterRegistry;
use gage_store::{
    SHARE_TYPE, SessionOutcome, SessionRecord, SessionStore, ShareAttrs, ShareLinkOutcome,
    ShareStore, Store, StoreError, TagStore, parse_filter_specs,
};
use tabled::{
    Table,
    settings::{
        Alignment, Color, Style,
        object::{Columns, Object, Rows},
    },
};

use crate::cmd_note::count_rows;
use crate::cmd_session::{column, run_query};
use crate::cmd_tag::TAG_HELP;
use crate::dialog::{self, DialogError};
use crate::error::CliError;
use crate::filter_prompt::prompt_filter_multiselect;
use crate::human::{count, format_elapsed_ms};
use crate::session_filter::{SessionLine, preview_sessions};
use crate::style::{self, IdKind, styled_id};
use crate::text_arg::text_value;

/// The paragraph `--filters` options share on what a filter spec is.
const FILTERS_HELP: &str = "A comma-separated list of entries, each a filter name with optional \
version comparators separated by spaces, as \"secret >=1.2 <2, language\". Every member must \
have been published with every listed filter at an accepted version. Given whole; replaces \
share's filters.";

#[derive(Subcommand)]
pub enum ShareCommand {
    /// Add a share
    ///
    /// Without sessions, adds an empty share. With sessions, adds a
    /// share holding those published sessions, each checked against
    /// --filters before anything is written. Dialog prompts for each
    /// of --description, --filters, and --tag that is not given; every
    /// answer is optional.
    Add(ShareAddArgs),

    /// List shares
    List(ShareListArgs),

    /// Show a share and its members
    Show(ShareShowArgs),

    /// Edit share attributes
    ///
    /// A --filters spec that an existing member fails is refused;
    /// --force applies it and unlinks failing members.
    Edit(ShareEditArgs),

    /// Delete shares
    ///
    /// Member sessions are kept unless --sessions is given.
    Delete(ShareDeleteArgs),

    /// Add published sessions to a share
    ///
    /// Each session is checked against share's filters before anything
    /// is written.
    Link(ShareLinkArgs),

    /// Remove sessions from a share
    ///
    /// Sessions stay in store.
    Unlink(ShareUnlinkArgs),
}

#[derive(Args)]
pub struct ShareAddArgs {
    /// Published sessions to add (tag, ID, or prefix)
    #[arg(value_name = "SESSION")]
    sessions: Vec<String>,

    /// Description of share
    ///
    /// Read from a file as @PATH
    #[arg(short, long, value_name = "TEXT")]
    description: Option<String>,

    /// Filters every member was published with
    #[arg(
        short,
        long,
        value_name = "SPEC",
        long_help = format!("Filters every member was published with\n\n{FILTERS_HELP}")
    )]
    filters: Option<String>,

    /// Tag new share
    #[arg(short, long, value_name = "NAME", long_help = format!("Tag new share\n\n{TAG_HELP}"))]
    tag: Option<String>,

    /// Move tag if it exists
    #[arg(long, requires = "tag")]
    force_tag: bool,

    /// Skip confirmation prompt
    ///
    /// Without --description, --filters, or --tag, none is given.
    #[arg(short, long)]
    yes: bool,
}

#[derive(Args)]
pub struct ShareListArgs {
    #[command(flatten)]
    limit: crate::limit::LimitArgs,

    /// Show full share ID
    #[arg(long)]
    full_id: bool,
}

#[derive(Args)]
pub struct ShareShowArgs {
    /// Share to show (tag, ID, or prefix)
    share: String,
}

#[derive(Args)]
#[command(group = ArgGroup::new("share_edit_attrs").required(true).multiple(true).args(["description", "filters"]))]
pub struct ShareEditArgs {
    /// Share to edit (tag, ID, or prefix)
    share: String,

    /// Description of share
    ///
    /// Read from a file as @PATH
    #[arg(short, long, value_name = "TEXT")]
    description: Option<String>,

    /// Filters every member was published with
    #[arg(
        short,
        long,
        value_name = "SPEC",
        long_help = format!("Filters every member was published with\n\n{FILTERS_HELP}")
    )]
    filters: Option<String>,

    /// Unlink members that fail new filters
    #[arg(long, requires = "filters")]
    force: bool,

    /// Skip confirmation prompt
    #[arg(short, long)]
    yes: bool,
}

#[derive(Args)]
pub struct ShareDeleteArgs {
    /// Shares to delete (tag, ID, or prefix)
    #[arg(required = true)]
    shares: Vec<String>,

    /// Also remove member sessions from store
    ///
    /// A session that another share or dataset holds is kept.
    #[arg(long)]
    sessions: bool,

    /// Skip confirmation prompt
    #[arg(short, long)]
    yes: bool,
}

#[derive(Args)]
pub struct ShareLinkArgs {
    /// Share to add sessions to (tag, ID, or prefix)
    share: String,

    /// Published sessions to add (tag, ID, or prefix)
    #[arg(required = true, value_name = "SESSION")]
    sessions: Vec<String>,

    /// Skip confirmation prompt
    #[arg(short, long)]
    yes: bool,
}

#[derive(Args)]
pub struct ShareUnlinkArgs {
    /// Share to remove sessions from (tag, ID, or prefix)
    share: String,

    /// Sessions to remove (tag, ID, or prefix)
    #[arg(required = true, value_name = "SESSION")]
    sessions: Vec<String>,

    /// Skip confirmation prompt
    #[arg(short, long)]
    yes: bool,
}

pub fn add(args: ShareAddArgs) -> Result<(), CliError> {
    let command = "gage share add";
    let store = open_store(command)?;
    let given = attrs_from_args(
        command,
        args.description.as_deref(),
        args.filters.as_deref(),
    )?;
    let records = resolve_sessions(command, &store, &args.sessions)?;
    dialog::run("Add share", || {
        let tags = TagStore::from(&store);
        let attrs = ShareAttrs {
            description: description_step(given.description, args.yes)?,
            filters: filters_step(given.filters, args.yes)?,
        };
        let tag = tag_step(args.tag.clone(), args.yes)?;
        // A tag that cannot be added is reported before the share
        // exists, so the failure leaves the store untouched
        if let Some(name) = &tag {
            tags.check_available(name, args.force_tag)
                .map_err(|e| match e {
                    StoreError::TagExists { .. } => {
                        DialogError::Failed(format!("{e} (use --force-tag to move it)"))
                    }
                    e => DialogError::Failed(e.to_string()),
                })?;
        }
        if !records.is_empty() {
            let lines: Vec<SessionLine> = records.iter().map(SessionLine::from).collect();
            preview_sessions(&lines, IdKind::Gage, None)?;
        }
        dialog::confirm(args.yes, true)?;

        let ids: Vec<String> = records.iter().map(|r| r.id.clone()).collect();
        let share_id = ShareStore::from(&store)
            .create(&attrs, &ids)
            .map_err(|e| DialogError::Failed(e.to_string()))?;
        if let Some(name) = &tag {
            let added = tags
                .add(name, &share_id, args.force_tag)
                .map_err(|e| DialogError::Failed(e.to_string()))?;
            if let Some(previous) = added.previous_id {
                cli::log::info(format!("Moved tag {name} from {}", short_uuid(&previous)))?;
            }
        }
        let shown = style::outro_id(&store, SHARE_TYPE, &share_id);
        Ok(if ids.is_empty() {
            format!("Added share {shown}")
        } else {
            format!("Added share {shown} with {}", count(ids.len(), "session"))
        }
        .into())
    })
}

/// The "Description" step.
fn description_step(given: Option<String>, yes: bool) -> Result<Option<String>, DialogError> {
    match InputStep::new(given, yes) {
        InputStep::Show(description) => {
            dialog::step_value("Description", &description)?;
            Ok(Some(description))
        }
        InputStep::Skip => Ok(None),
        InputStep::Prompt => prompt_description(),
    }
}

/// The "Filters" step. No `--filters` entries is the option not given.
fn filters_step(given: Vec<String>, yes: bool) -> Result<Vec<String>, DialogError> {
    match InputStep::new(Some(given).filter(|f| !f.is_empty()), yes) {
        InputStep::Show(filters) => {
            dialog::step_list("Filters", filters.iter().cloned())?;
            Ok(filters)
        }
        InputStep::Skip => Ok(Vec::new()),
        InputStep::Prompt => prompt_filter_multiselect(&FilterRegistry::load(), false),
    }
}

/// The "Tag" step.
fn tag_step(given: Option<String>, yes: bool) -> Result<Option<String>, DialogError> {
    match InputStep::new(given, yes) {
        InputStep::Show(name) => {
            dialog::step_value("Tag", &name)?;
            Ok(Some(name))
        }
        InputStep::Skip => Ok(None),
        InputStep::Prompt => prompt_tag(),
    }
}

/// How the dialog settles one optional input. Each input is settled
/// on its own: a value given for one leaves the others prompted.
#[derive(Debug, PartialEq)]
enum InputStep<T> {
    /// The value was given: it is shown and not asked for
    Show(T),
    /// `--yes` without a value: nothing is asked and none is given
    Skip,
    Prompt,
}

impl<T> InputStep<T> {
    fn new(given: Option<T>, yes: bool) -> Self {
        match given {
            Some(value) => InputStep::Show(value),
            None if yes => InputStep::Skip,
            None => InputStep::Prompt,
        }
    }
}

/// The description prompt; `None` when nothing was entered.
fn prompt_description() -> Result<Option<String>, DialogError> {
    let text: String = cli::input("Description")
        .placeholder("What share is for (optional)")
        .required(false)
        .interact()?;
    let text = text.trim();
    Ok((!text.is_empty()).then(|| text.to_string()))
}

/// The tag prompt; `None` when nothing was entered.
fn prompt_tag() -> Result<Option<String>, DialogError> {
    let text: String = cli::input("Tag")
        .placeholder("Tag for new share (optional)")
        .required(false)
        .interact()?;
    let text = text.trim();
    Ok((!text.is_empty()).then(|| text.to_string()))
}

/// The attrs the `--description` and `--filters` options spell. A
/// filters spec is parsed here so a bad entry fails before the dialog.
fn attrs_from_args(
    command: &str,
    description: Option<&str>,
    filters: Option<&str>,
) -> Result<ShareAttrs, CliError> {
    let description = description
        .map(|text| {
            text_value(command, "--description", text)
                .map(|t| t.trim().to_string())
                .map_err(|e| CliError::message(e.to_string()))
        })
        .transpose()?
        .filter(|t| !t.is_empty());
    let filters = match filters {
        Some(spec) => parse_filter_specs(spec)
            .map_err(|e| CliError::message(format!("{command}: --filters: {e}")))?
            .iter()
            .map(ToString::to_string)
            .collect(),
        None => Vec::new(),
    };
    Ok(ShareAttrs {
        description,
        filters,
    })
}

/// The dialog steps that show a share's attrs.
fn attrs_steps(attrs: &ShareAttrs) -> std::io::Result<()> {
    if let Some(description) = &attrs.description {
        dialog::step_value("Description", description)?;
    }
    if !attrs.filters.is_empty() {
        dialog::step_list("Filters", attrs.filters.iter().cloned())?;
    }
    Ok(())
}

/// Resolve every session argument to its live record before anything
/// is written, so one bad argument leaves the store untouched.
fn resolve_sessions(
    command: &str,
    store: &Store,
    sessions: &[String],
) -> Result<Vec<SessionRecord>, CliError> {
    let session_store = SessionStore::from(store);
    let mut records: Vec<SessionRecord> = Vec::with_capacity(sessions.len());
    for arg in sessions {
        let record = session_store
            .get(arg)
            .with_context(|| format!("{command}: {arg}"))?;
        if !records.iter().any(|r| r.id == record.id) {
            records.push(record);
        }
    }
    Ok(records)
}

pub async fn list(args: ShareListArgs) -> Result<(), CliError> {
    let store = open_store("gage share list")?;
    let ctx = ContextBuilder::new(Some(Arc::new(Mutex::new(store))))
        .build()
        .await;
    let total = count_rows(&ctx, "SELECT COUNT(*) FROM share")
        .await
        .context("gage share list")?;
    if total == 0 {
        println!("No shares found");
        return Ok(());
    }
    let show = args.limit.show_count(total);
    let sql = format!(
        "SELECT s.id, s.id_prefix, s.created, COALESCE(m.n, 0) AS sessions, \
         arrow_cast(COALESCE(t.names, ''), 'Utf8') AS tags, \
         COALESCE(s.description, '') AS description \
         FROM share s \
         LEFT JOIN (SELECT share_id, COUNT(*) AS n FROM share_session \
                    GROUP BY share_id) m ON m.share_id = s.id \
         LEFT JOIN (SELECT id, string_agg(name, ', ' ORDER BY name) AS names FROM tag \
                    GROUP BY id) t ON t.id = s.id \
         ORDER BY s.modified DESC LIMIT {show}"
    );
    let batches = run_query(&ctx, &sql).await.context("gage share list")?;

    let header: Vec<String> = ["Id", "Sessions", "Tags", "Description", "Created"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let mut rows: Vec<Vec<String>> = Vec::new();
    for batch in &batches {
        let ids = column::<StringArray>(batch, 0);
        let prefixes = column::<StringArray>(batch, 1);
        let createds = column::<TimestampMillisecondArray>(batch, 2);
        let sessions = column::<Int64Array>(batch, 3);
        let tags = column::<StringArray>(batch, 4);
        let descriptions = column::<StringArray>(batch, 5);
        for i in 0..batch.num_rows() {
            let id = ids.value(i);
            let shown = if args.full_id { id } else { short_uuid(id) };
            rows.push(vec![
                styled_id(shown, prefixes.value(i), IdKind::Gage),
                sessions.value(i).to_string(),
                tags.value(i).to_string(),
                descriptions.value(i).to_string(),
                format_elapsed_ms(createds.value(i)),
            ]);
        }
    }
    let shown = rows.len();
    let mut table = Table::from_iter(std::iter::once(header).chain(rows));
    table
        .with(Style::rounded())
        .modify(Rows::first(), style::tty(Color::FG_BRIGHT_YELLOW))
        .modify(Columns::one(1), Alignment::right())
        .modify(Columns::one(1).not(Rows::first()), style::dim())
        .modify(
            Columns::one(2).not(Rows::first()),
            style::tty(Color::FG_BRIGHT_CYAN),
        )
        .modify(Columns::one(4).not(Rows::first()), style::dim());
    style::truncate_to(
        &mut table,
        style::stdout_width(),
        style::IdAwarePriority::new(args.full_id),
    );
    println!("{table}");
    args.limit.print_summary(shown, total, "share");
    Ok(())
}

pub fn show(args: ShareShowArgs) -> Result<(), CliError> {
    let store = open_store("gage share show")?;
    let shares = ShareStore::from(&store);
    let record = shares
        .get(&args.share)
        .with_context(|| format!("gage share show: {}", args.share))?;
    let sessions = shares.sessions(&record.id).context("gage share show")?;
    let tags = TagStore::from(&store)
        .targets()
        .context("gage share show")?;
    let iso = |ms: Option<i64>| ms.map(ms_to_iso8601).unwrap_or_default();

    let sessions_cell = sessions
        .iter()
        .map(|s| match &s.attrs.summary.title {
            Some(title) => format!("{} {title}", s.id),
            None => s.id.clone(),
        })
        .collect::<Vec<_>>()
        .join("\n");
    let tags_cell = tags
        .iter()
        .filter(|t| t.id == record.id)
        .map(|t| t.name.clone())
        .collect::<Vec<_>>()
        .join("\n");
    let rows = [
        ("id", record.id.clone()),
        (
            "description",
            record.attrs.description.clone().unwrap_or_default(),
        ),
        ("filters", record.attrs.filters.join("\n")),
        ("sessions", sessions_cell),
        ("tags", tags_cell),
        ("created", iso(Some(record.created_ms))),
        ("modified", iso(record.modified_ms)),
    ];
    let table = Table::from_iter(rows.iter().map(|(k, v)| [k.to_string(), v.clone()]))
        .with(Style::rounded())
        .modify(Columns::first(), style::tty(Color::FG_BRIGHT_YELLOW))
        .to_string();
    println!("{table}");
    Ok(())
}

pub fn edit(args: ShareEditArgs) -> Result<(), CliError> {
    let command = "gage share edit";
    let store = open_store(command)?;
    let shares = ShareStore::from(&store);
    let record = shares
        .get(&args.share)
        .with_context(|| format!("{command}: {}", args.share))?;
    let given = attrs_from_args(
        command,
        args.description.as_deref(),
        args.filters.as_deref(),
    )?;
    // An option not given keeps the share's value
    let attrs = ShareAttrs {
        description: match args.description {
            Some(_) => given.description,
            None => record.attrs.description.clone(),
        },
        filters: match args.filters {
            Some(_) => given.filters,
            None => record.attrs.filters.clone(),
        },
    };
    dialog::run("Edit share", || {
        dialog::step_value("Share", short_uuid(&record.id))?;
        attrs_steps(&attrs)?;
        if args.force {
            cli::log::remark("Members that fail new filters are unlinked")?;
        }
        dialog::confirm(args.yes, true)?;
        let outcome = shares
            .edit(&record.id, &attrs, args.force)
            .map_err(|e| match e {
                StoreError::ShareMembership { .. } => {
                    DialogError::Failed(format!("{e} (use --force to unlink it)"))
                }
                e => DialogError::Failed(e.to_string()),
            })?;
        if !outcome.unlinked.is_empty() {
            dialog::step_list(
                "Unlinked",
                outcome.unlinked.iter().map(|id| short_uuid(id).to_string()),
            )?;
        }
        Ok(format!("Edited share {}", short_uuid(&record.id)).into())
    })
}

pub fn delete(args: ShareDeleteArgs) -> Result<(), CliError> {
    let store = open_store("gage share delete")?;
    let shares = ShareStore::from(&store);

    // Resolve every argument before writing anything, so one bad
    // argument leaves the store untouched
    let mut records = Vec::with_capacity(args.shares.len());
    let mut errors = 0;
    for arg in &args.shares {
        match shares.get(arg) {
            Ok(record) => records.push(record),
            Err(e) => {
                eprintln!("gage share delete: {e}");
                errors += 1;
            }
        }
    }
    if errors > 0 {
        return Err(CliError::exit(1));
    }

    let total = records.len();
    let members: usize = records.iter().map(|r| r.session_count).sum();
    dialog::run("Delete shares", || {
        dialog::step_list(
            "Shares",
            records.iter().map(|r| {
                format!(
                    "{} ({})",
                    short_uuid(&r.id),
                    count(r.session_count, "session")
                )
            }),
        )?;
        if args.sessions {
            cli::log::remark(format!("{} to remove", count(members, "session")))?;
        }

        dialog::confirm_irreversible(args.yes)?;

        let mut deleted = 0;
        let mut removed = 0;
        let mut kept = 0;
        for record in &records {
            let result = if args.sessions {
                shares.delete_cascade(&record.id).map(|d| {
                    removed += d.removed.len();
                    kept += d.kept.len();
                })
            } else {
                shares.delete(&record.id).map(|_| ())
            };
            match result {
                Ok(()) => deleted += 1,
                Err(e) => cli::log::warning(format!("{}: {e}", short_uuid(&record.id)))?,
            }
        }
        if deleted < total {
            return Err(dialog::partial("Deleted", deleted, total, "share"));
        }

        let mut outro = format!("Deleted {}", count(deleted, "share"));
        if args.sessions {
            outro.push_str(&format!(", removed {}", count(removed, "session")));
            if kept > 0 {
                outro.push_str(&format!(", kept {kept} held by other shares or datasets"));
            }
        }
        Ok(outro.into())
    })
}

pub fn link(args: ShareLinkArgs) -> Result<(), CliError> {
    let command = "gage share link";
    let store = open_store(command)?;
    let shares = ShareStore::from(&store);
    let share_id = shares
        .resolve_id(&args.share)
        .with_context(|| format!("{command}: {}", args.share))?;
    let records = resolve_sessions(command, &store, &args.sessions)?;
    dialog::run("Link sessions", || {
        dialog::step_value("Share", short_uuid(&share_id))?;
        let lines: Vec<SessionLine> = records.iter().map(SessionLine::from).collect();
        preview_sessions(&lines, IdKind::Gage, None)?;
        dialog::confirm(args.yes, true)?;
        let ids: Vec<String> = records.iter().map(|r| r.id.clone()).collect();
        let outcomes = shares
            .link(&share_id, &ids)
            .map_err(|e| DialogError::Failed(e.to_string()))?;
        Ok(link_outro(&outcomes, &share_id).into())
    })
}

/// The closing line of a link: what changed, and the unchanged count
/// when every slot was already current.
fn link_outro(outcomes: &[ShareLinkOutcome], share_id: &str) -> String {
    let added = outcomes
        .iter()
        .filter(|o| o.outcome == SessionOutcome::Added)
        .count();
    let updated = outcomes
        .iter()
        .filter(|o| o.outcome == SessionOutcome::Updated)
        .count();
    let share = short_uuid(share_id);
    match (added, updated) {
        (0, 0) => format!(
            "{} already in share {share}",
            count(outcomes.len(), "session")
        ),
        (added, 0) => format!("Added {} to share {share}", count(added, "session")),
        (0, updated) => format!("Updated {} in share {share}", count(updated, "session")),
        (added, updated) => format!(
            "Added {} to share {share}, updated {updated}",
            count(added, "session")
        ),
    }
}

pub fn unlink(args: ShareUnlinkArgs) -> Result<(), CliError> {
    let command = "gage share unlink";
    let store = open_store(command)?;
    let shares = ShareStore::from(&store);
    let share_id = shares
        .resolve_id(&args.share)
        .with_context(|| format!("{command}: {}", args.share))?;
    // Members are read at the commit the share links, so a removed
    // session still resolves for unlinking
    let members = shares.sessions(&share_id).context(command)?;
    let mut records: Vec<&SessionRecord> = Vec::with_capacity(args.sessions.len());
    for arg in &args.sessions {
        let (id, _) = store
            .resolve_id(arg)
            .with_context(|| format!("{command}: {arg}"))?;
        let record = members.iter().find(|r| r.id == id).ok_or_else(|| {
            CliError::message(format!(
                "{command}: session {} is not in share {}",
                short_uuid(&id),
                short_uuid(&share_id)
            ))
        })?;
        if !records.iter().any(|r| r.id == record.id) {
            records.push(record);
        }
    }
    dialog::run("Unlink sessions", || {
        dialog::step_value("Share", short_uuid(&share_id))?;
        let lines: Vec<SessionLine> = records.iter().map(|r| SessionLine::from(*r)).collect();
        preview_sessions(&lines, IdKind::Gage, None)?;
        cli::log::remark("Sessions stay in the store")?;
        dialog::confirm(args.yes, false)?;
        let ids: Vec<String> = records.iter().map(|r| r.id.clone()).collect();
        let outcomes = shares
            .unlink(&share_id, &ids)
            .map_err(|e| DialogError::Failed(e.to_string()))?;
        Ok(format!(
            "Removed {} from share {}",
            count(outcomes.len(), "session"),
            short_uuid(&share_id)
        )
        .into())
    })
}

/// Open the default store, or `command: <error>`
fn open_store(command: &str) -> Result<Store, CliError> {
    let store = Store::open(&gage_store::store_path()).context(command.to_string())?;
    Ok(store)
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use crate::{Cli, Command};

    use super::*;

    fn share_command(argv: &[&str]) -> ShareCommand {
        let cli = Cli::try_parse_from([&["gage", "share"], argv].concat()).unwrap();
        let Command::Share { command } = cli.command else {
            panic!("not a share command");
        };
        command
    }

    /// `--filters` is one value holding the whole spec, and `edit`
    /// needs at least one attribute option.
    #[test]
    fn filters_is_one_spec_and_edit_needs_an_attribute() {
        let ShareCommand::Add(args) = share_command(&[
            "add",
            "-d",
            "Work.",
            "-f",
            "secret >=1.2 <2, language",
            "-t",
            "work",
            "abc",
        ]) else {
            panic!("not add");
        };
        assert_eq!(args.filters.as_deref(), Some("secret >=1.2 <2, language"));
        assert_eq!(args.sessions, ["abc"]);
        let Ok(attrs) =
            attrs_from_args("test", args.description.as_deref(), args.filters.as_deref())
        else {
            panic!("a valid spec parsed");
        };
        assert_eq!(
            attrs,
            ShareAttrs {
                description: Some("Work.".into()),
                filters: vec!["secret >=1.2 <2".into(), "language".into()],
            }
        );

        assert!(
            Cli::try_parse_from(["gage", "share", "edit", "work"]).is_err(),
            "edit without an attribute parsed"
        );
        assert!(
            Cli::try_parse_from(["gage", "share", "edit", "work", "--force"]).is_err(),
            "--force without --filters parsed"
        );
        let ShareCommand::Edit(args) = share_command(&["edit", "work", "-f", "secret", "--force"])
        else {
            panic!("not edit");
        };
        assert!(args.force);
        assert_eq!(args.description, None);
    }

    /// A bad entry in the spec fails before any dialog opens.
    #[test]
    fn a_bad_filters_spec_is_a_usage_error() {
        let Err(CliError::Exit {
            code: 1,
            message: Some(message),
        }) = attrs_from_args("gage share add", None, Some("secret >="))
        else {
            panic!("a bad spec parsed");
        };
        assert!(message.contains("invalid filter spec"), "{message}");
    }

    /// `link` and `unlink` take the share first, then at least one
    /// session.
    #[test]
    fn link_and_unlink_take_a_share_and_sessions() {
        let ShareCommand::Link(args) = share_command(&["link", "work", "a", "b"]) else {
            panic!("not link");
        };
        assert_eq!(args.share, "work");
        assert_eq!(args.sessions, ["a", "b"]);
        assert!(Cli::try_parse_from(["gage", "share", "unlink", "work"]).is_err());
    }

    /// A value given for one input shows that input alone; the others
    /// are still prompted, and `--yes` skips only the absent ones.
    #[test]
    fn each_add_input_is_settled_on_its_own() {
        assert_eq!(InputStep::new(Some("x"), false), InputStep::Show("x"));
        assert_eq!(InputStep::new(Some("x"), true), InputStep::Show("x"));
        assert_eq!(InputStep::<&str>::new(None, false), InputStep::Prompt);
        assert_eq!(InputStep::<&str>::new(None, true), InputStep::Skip);
    }

    #[test]
    fn link_outro_counts_what_changed() {
        let outcome = |n: u32, outcome: SessionOutcome| ShareLinkOutcome {
            session_num: n,
            id: format!("s{n}"),
            outcome,
        };
        let share = "01k2h4t9aaaabbbbccccddddee";
        assert_eq!(
            link_outro(&[outcome(1, SessionOutcome::Unchanged)], share),
            "1 session already in share 01k2h4t9"
        );
        assert_eq!(
            link_outro(
                &[
                    outcome(1, SessionOutcome::Added),
                    outcome(2, SessionOutcome::Added)
                ],
                share
            ),
            "Added 2 sessions to share 01k2h4t9"
        );
        assert_eq!(
            link_outro(
                &[
                    outcome(1, SessionOutcome::Added),
                    outcome(2, SessionOutcome::Updated)
                ],
                share
            ),
            "Added 1 session to share 01k2h4t9, updated 1"
        );
    }
}
