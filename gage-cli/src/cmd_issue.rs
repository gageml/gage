//! `gage issue`: issues in the Gage store.
//!
//! Listings and reads go through the query context (`issue`,
//! `issue_event`, `issue_evidence`); writes go through
//! [`IssueStore`]. An issue has no target; the sessions it concerns
//! are reached through the notes it cites.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Context;
use clap::{ArgGroup, Args, Subcommand};
use cliclack as cli;
use console::style;
use datafusion::arrow::array::{Array, StringArray, TimestampMillisecondArray};
use gage_core::config::Config;
use gage_core::uuid::short_uuid;
use gage_query::ContextBuilder;
use gage_registry::driver::DriverRegistry;
use gage_store::{
    ISSUE_TYPE, IssueFull, IssueInput, IssueStatus, IssueStore, NOTE_TYPE, StatusReason, Store,
};
use tabled::{
    Table,
    settings::{
        Color, Style, Width,
        object::{Columns, Object, Rows},
    },
};

use crate::author::resolve_author;
use crate::cmd_note::{count_rows, target_cell, value_cell};
use crate::cmd_session::{column, run_query};
use crate::dialog::{self, DialogError};
use crate::error::CliError;
use crate::human::count;
use crate::style::{self, IdKind, styled_id};
use crate::text_arg::text_value;

#[derive(Subcommand)]
pub enum IssueCommand {
    /// List issues
    List(IssueListArgs),

    /// Show an issue
    Show(IssueShowArgs),

    /// Add an issue
    Add(IssueAddArgs),

    /// Delete issues
    Delete(IssueDeleteArgs),

    /// Close issues
    Close(IssueCloseArgs),

    /// Open pending or closed issues
    Open(IssueOpenArgs),

    /// Comment on issues
    Comment(IssueCommentArgs),
}

#[derive(Args)]
pub struct IssueListArgs {
    /// Filter by issue name
    #[arg(long)]
    name: Option<String>,

    /// Show closed issues
    #[arg(short, long)]
    closed: bool,

    #[command(flatten)]
    limit: crate::limit::LimitArgs,

    /// Show full issue ID
    #[arg(long)]
    full_id: bool,
}

#[derive(Args)]
pub struct IssueShowArgs {
    /// Issue to show (ID or prefix)
    id: String,
}

#[derive(Args)]
pub struct IssueAddArgs {
    /// Issue title
    #[arg(short, long)]
    title: Option<String>,

    /// Issue description or @PATH to read a file
    ///
    /// Text cannot begin with a literal @
    #[arg(short, long, value_name = "TEXT")]
    description: Option<String>,

    /// Issue name (default "user-issue")
    #[arg(short, long)]
    name: Option<String>,

    /// Note cited as evidence (ID or prefix; repeatable)
    #[arg(short, long, value_name = "NOTE")]
    evidence: Vec<String>,

    /// Author username (default $USER)
    #[arg(short, long)]
    user: Option<String>,

    /// Add as pending instead of open
    #[arg(short, long)]
    pending: bool,

    /// Skip prompts
    ///
    /// Requires --title; other values take their defaults
    #[arg(short, long)]
    yes: bool,
}

#[derive(Args)]
pub struct IssueDeleteArgs {
    /// Issues to delete (ID or prefix)
    #[arg(required = true)]
    ids: Vec<String>,

    /// Skip confirmation prompt
    #[arg(short, long)]
    yes: bool,
}

#[derive(Args)]
#[command(group = ArgGroup::new("reason").args(["completed", "wontfix", "duplicate"]))]
pub struct IssueCloseArgs {
    /// Issues to close (ID or prefix)
    #[arg(required = true)]
    ids: Vec<String>,

    /// Close issue as "completed"
    #[arg(short, long)]
    completed: bool,

    /// Close issue as "wontfix"
    #[arg(short, long)]
    wontfix: bool,

    /// Close issue as "duplicate" of issue (ID or prefix)
    #[arg(short, long, value_name = "ISSUE")]
    duplicate: Option<String>,

    #[command(flatten)]
    message: MessageArgs,

    /// Skip prompts
    #[arg(short, long)]
    yes: bool,
}

#[derive(Args)]
pub struct IssueOpenArgs {
    /// Issues to open (ID or prefix)
    #[arg(required = true)]
    ids: Vec<String>,

    #[command(flatten)]
    message: MessageArgs,

    /// Skip prompts
    #[arg(short, long)]
    yes: bool,
}

#[derive(Args)]
pub struct IssueCommentArgs {
    /// Issues to comment on (ID or prefix)
    #[arg(required = true)]
    ids: Vec<String>,

    #[command(flatten)]
    message: MessageArgs,

    /// Skip confirmation prompt
    ///
    /// Requires --message
    #[arg(short, long)]
    yes: bool,
}

/// A message body given on the command line or read from a file.
#[derive(Args)]
pub struct MessageArgs {
    /// Message text or @PATH to read a file
    ///
    /// Text cannot begin with a literal @
    #[arg(short, long, value_name = "TEXT")]
    message: Option<String>,
}

impl MessageArgs {
    /// The message as given, or `None` when the option was not used.
    fn resolve(&self, command: &str) -> anyhow::Result<Option<String>> {
        self.message
            .as_deref()
            .map(|value| text_value(command, "--message", value))
            .transpose()
    }

    fn given(&self) -> bool {
        self.message.is_some()
    }
}

fn open_store(command: &str) -> anyhow::Result<Store> {
    Store::open(&gage_store::store_path()).context(command.to_string())
}

pub async fn list(args: IssueListArgs) -> Result<(), CliError> {
    let store = open_store("gage issue list")?;
    let ctx = ContextBuilder::new(Some(Arc::new(Mutex::new(store))))
        .build()
        .await;
    let mut clauses: Vec<String> = Vec::new();
    if args.closed {
        clauses.push("status = 'closed'".to_string());
    } else {
        clauses.push("status IN ('pending', 'open')".to_string());
    }
    if let Some(name) = &args.name {
        clauses.push(format!("name = '{}'", sql_str(name)));
    }
    let where_clause = format!(" WHERE {}", clauses.join(" AND "));
    let limit_clause = match args.limit.fetch_limit() {
        Some(n) => format!(" LIMIT {n}"),
        None => String::new(),
    };
    let sql = format!(
        "SELECT id, id_prefix, title, status, status_reason, name, scan, created \
         FROM issue{where_clause} ORDER BY created DESC{limit_clause}"
    );
    let batches = run_query(&ctx, &sql).await.context("gage issue list")?;
    let total = count_rows(&ctx, &format!("SELECT COUNT(*) FROM issue{where_clause}"))
        .await
        .context("gage issue list")?;
    if total == 0 {
        println!("No issues found");
        return Ok(());
    }

    let header: Vec<String> = ["Id", "Name", "Title", "Scan", "Status", "Created"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let mut rows: Vec<Vec<String>> = Vec::new();
    for batch in &batches {
        let ids = column::<StringArray>(batch, 0);
        let prefixes = column::<StringArray>(batch, 1);
        let titles = column::<StringArray>(batch, 2);
        let statuses = column::<StringArray>(batch, 3);
        let reasons = column::<StringArray>(batch, 4);
        let names = column::<StringArray>(batch, 5);
        let scans = column::<StringArray>(batch, 6);
        let createds = column::<TimestampMillisecondArray>(batch, 7);
        for i in 0..batch.num_rows() {
            let id = ids.value(i);
            let shown = if args.full_id { id } else { short_uuid(id) };
            let status = if reasons.is_valid(i) {
                reasons.value(i).to_string()
            } else {
                statuses.value(i).to_string()
            };
            let scan = if scans.is_valid(i) {
                short_uuid(scans.value(i)).to_string()
            } else {
                String::new()
            };
            let created = if createds.is_valid(i) {
                crate::human::format_elapsed_ms(createds.value(i))
            } else {
                String::new()
            };
            rows.push(vec![
                styled_id(shown, prefixes.value(i), IdKind::Gage),
                names.value(i).to_string(),
                titles.value(i).to_string(),
                scan,
                status,
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
                .priority(style::IdAwarePriority::new(args.full_id)),
        )
        .modify(Rows::first(), style::tty(Color::FG_BRIGHT_YELLOW))
        .modify(
            Columns::one(2).not(Rows::first()),
            style::tty(Color::FG_BRIGHT_CYAN),
        )
        .modify(Columns::new(3..6).not(Rows::first()), style::dim())
        .to_string();
    println!("{table}");

    args.limit.print_summary(shown, total, "issue");
    Ok(())
}

fn sql_str(s: &str) -> String {
    s.replace('\'', "''")
}

pub async fn show(args: IssueShowArgs) -> Result<(), CliError> {
    let store = open_store("gage issue show")?;
    // The prefix resolves through the store; the rows come from SQL
    let found = store
        .resolve_in(&args.id, Some(ISSUE_TYPE))
        .context("gage issue show")?;
    if found.deleted {
        return Err(CliError::message(format!(
            "gage issue show: issue {} is deleted",
            short_uuid(&found.id)
        )));
    }
    let id = found.id;
    let ctx = ContextBuilder::new(Some(Arc::new(Mutex::new(store))))
        .build()
        .await;
    let sql = format!(
        "SELECT name, title, status, status_reason, description, author, scan, \
         created, modified FROM issue WHERE id = '{id}'"
    );
    let batches = run_query(&ctx, &sql).await.context("gage issue show")?;
    let Some(batch) = batches.iter().find(|b| b.num_rows() > 0) else {
        return Err(CliError::message(format!(
            "gage issue show: issue {id} not found"
        )));
    };
    let text = |idx: usize| column::<StringArray>(batch, idx).value(0).to_string();
    let opt_text = |idx: usize| {
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
    let status = match opt_text(3) {
        Some(reason) => format!("{} ({reason})", text(2)),
        None => text(2),
    };
    let attrs = vec![
        ("id", id.clone()),
        ("name", text(0)),
        ("title", text(1)),
        ("status", status),
        ("description", opt_text(4).unwrap_or_default()),
        // Styled after value_width is known, like the evidence and
        // events sections
        ("sessions", String::new()),
        ("author", text(5)),
        (
            "scan",
            opt_text(6)
                .map(|s| short_uuid(&s).to_string())
                .unwrap_or_default(),
        ),
        ("created", iso(7)),
        ("modified", iso(8)),
    ];

    let evidence_sql = format!(
        "SELECT n.id, n.name, n.target, n.text, n.value \
         FROM issue_evidence e JOIN note n ON n.id = e.note_id \
         WHERE e.issue_id = '{id}' ORDER BY n.created"
    );
    let evidence = run_query(&ctx, &evidence_sql)
        .await
        .context("gage issue show")?;
    let events_sql = format!(
        "SELECT event, author, timestamp, from_status, to_status, reason, \"of\", message \
         FROM issue_event WHERE issue_id = '{id}' ORDER BY event_id"
    );
    let events = run_query(&ctx, &events_sql)
        .await
        .context("gage issue show")?;
    let sessions_sql = format!(
        "SELECT s.id, s.project, s.driver, s.title \
         FROM session_issue si JOIN session s ON s.id = si.session_id \
         WHERE si.issue_id = '{id}' ORDER BY s.id"
    );
    let sessions = run_query(&ctx, &sessions_sql)
        .await
        .context("gage issue show")?;
    let threshold = Config::load_user()
        .context("gage issue show")?
        .issues
        .related_threshold;
    let related_sql = format!(
        "SELECT r.related_id, i.status, i.title \
         FROM related_issue('{id}') r JOIN issue i ON i.id = r.related_id \
         WHERE r.score >= {threshold} ORDER BY r.score DESC"
    );
    let related = run_query(&ctx, &related_sql)
        .await
        .context("gage issue show")?;

    let evidence_label = "evidence";
    let related_label = "related";
    let events_label = "events";
    let label_width = attrs
        .iter()
        .map(|(k, _)| k.len())
        .chain([
            evidence_label.len(),
            related_label.len(),
            events_label.len(),
        ])
        .max()
        .unwrap_or(0);
    let (_, term_width) = console::Term::stdout().size();
    // Borders + padding: "│ " + " │ " + " │" = 8 chars
    let value_width = (term_width as usize)
        .saturating_sub(label_width + 8)
        .max(20);

    let registry = DriverRegistry::builtin();
    let mut session_entries: Vec<String> = Vec::new();
    for batch in &sessions {
        let ids = column::<StringArray>(batch, 0);
        let projects = column::<StringArray>(batch, 1);
        let drivers = column::<StringArray>(batch, 2);
        let titles = column::<StringArray>(batch, 3);
        for i in 0..batch.num_rows() {
            let project = project_display(&registry, drivers.value(i), projects, i);
            let header = style(textwrap::fill(
                &format!("{} · {project}", short_uuid(ids.value(i))),
                value_width,
            ))
            .dim();
            let title = if titles.is_valid(i) {
                titles.value(i)
            } else {
                ""
            };
            let title = style(textwrap::fill(title, value_width)).cyan().bright();
            session_entries.push(format!("{header}\n{title}"));
        }
    }
    let sessions_display = session_entries.join("\n\n");

    let mut rows: Vec<Vec<String>> = attrs
        .into_iter()
        .map(|(k, v)| {
            let value = if k == "description" {
                crate::markdown::render(&v, value_width)
            } else if k == "sessions" {
                sessions_display.clone()
            } else {
                textwrap::fill(&v, value_width)
            };
            vec![k.to_string(), value]
        })
        .collect();

    let mut evidence_entries: Vec<String> = Vec::new();
    for batch in &evidence {
        let ids = column::<StringArray>(batch, 0);
        let names = column::<StringArray>(batch, 1);
        let targets = column::<StringArray>(batch, 2);
        let texts = column::<StringArray>(batch, 3);
        let values = column::<StringArray>(batch, 4);
        for i in 0..batch.num_rows() {
            let mut header = format!("{} · {}", short_uuid(ids.value(i)), names.value(i));
            if targets.is_valid(i) {
                header.push_str(" · ");
                header.push_str(&target_cell(targets.value(i)));
            }
            let raw = if texts.is_valid(i) {
                texts.value(i)
            } else {
                values.value(i)
            };
            let header = style(textwrap::fill(&header, value_width)).dim();
            let value = style(textwrap::fill(&value_cell(raw), value_width))
                .cyan()
                .bright();
            evidence_entries.push(format!("{header}\n{value}"));
        }
    }
    if !evidence_entries.is_empty() {
        rows.push(vec![
            evidence_label.to_string(),
            evidence_entries.join("\n\n"),
        ]);
    }

    let mut related_entries: Vec<String> = Vec::new();
    for batch in &related {
        let ids = column::<StringArray>(batch, 0);
        let statuses = column::<StringArray>(batch, 1);
        let titles = column::<StringArray>(batch, 2);
        for i in 0..batch.num_rows() {
            let prefix = format!("{} · {} · ", short_uuid(ids.value(i)), statuses.value(i));
            let wrapped = textwrap::fill(&format!("{prefix}{}", titles.value(i)), value_width);
            let dimmed = style(&prefix).dim().to_string();
            related_entries.push(wrapped.replacen(&prefix, &dimmed, 1));
        }
    }
    if !related_entries.is_empty() {
        rows.push(vec![related_label.to_string(), related_entries.join("\n")]);
    }

    let mut event_entries: Vec<String> = Vec::new();
    for batch in &events {
        let kinds = column::<StringArray>(batch, 0);
        let authors = column::<StringArray>(batch, 1);
        let timestamps = column::<TimestampMillisecondArray>(batch, 2);
        let froms = column::<StringArray>(batch, 3);
        let tos = column::<StringArray>(batch, 4);
        let reasons = column::<StringArray>(batch, 5);
        let ofs = column::<StringArray>(batch, 6);
        let messages = column::<StringArray>(batch, 7);
        for i in 0..batch.num_rows() {
            let opt = |col: &StringArray| col.is_valid(i).then(|| col.value(i).to_string());
            let label = event_label(
                kinds.value(i),
                opt(froms).as_deref(),
                opt(tos).as_deref(),
                opt(reasons).as_deref(),
                opt(ofs).as_deref(),
            );
            let header = style(textwrap::fill(
                &format!(
                    "{label} · {} · {}",
                    authors.value(i),
                    gage_core::datetime::ms_to_iso8601(timestamps.value(i))
                ),
                value_width,
            ))
            .dim();
            event_entries.push(match opt(messages) {
                Some(m) => format!("{header}\n{}", textwrap::fill(&m, value_width)),
                None => header.to_string(),
            });
        }
    }
    if !event_entries.is_empty() {
        rows.push(vec![events_label.to_string(), event_entries.join("\n\n")]);
    }

    let table = Table::from_iter(rows)
        .with(Style::rounded())
        .modify(Columns::first(), style::tty(Color::FG_BRIGHT_YELLOW))
        .to_string();
    println!("{table}");
    Ok(())
}

/// The project of a session row in the driver's display form; the
/// raw name when the driver is unknown.
fn project_display(
    registry: &DriverRegistry,
    driver: &str,
    projects: &StringArray,
    i: usize,
) -> String {
    if !projects.is_valid(i) {
        return String::new();
    }
    let name = driver.split_once(' ').map_or(driver, |(name, _)| name);
    match registry.for_name(name) {
        Some(driver) => driver.format_project(projects.value(i), usize::MAX),
        None => projects.value(i).to_string(),
    }
}

/// One line naming an event: `created pending`, `pending → open`,
/// `open → closed (wontfix)`, `pending → closed (duplicate of
/// 2jc5yz17)`, `comment`.
pub(crate) fn event_label(
    event: &str,
    from: Option<&str>,
    to: Option<&str>,
    reason: Option<&str>,
    of: Option<&str>,
) -> String {
    let mut label = match (event, from, to) {
        ("create", _, Some(to)) => format!("created {to}"),
        ("status", Some(from), Some(to)) => format!("{from} → {to}"),
        (other, _, _) => other.to_string(),
    };
    match (reason, of) {
        (Some(reason), Some(of)) => label.push_str(&format!(" ({reason} of {})", short_uuid(of))),
        (Some(reason), None) => label.push_str(&format!(" ({reason})")),
        (None, _) => {}
    }
    label
}

pub fn add(args: IssueAddArgs) -> Result<(), CliError> {
    if args.yes && args.title.is_none() {
        return Err(CliError::message("gage issue add: --yes requires --title"));
    }
    let store = open_store("gage issue add")?;
    let issues = IssueStore::from(&store);
    let description = match args.description.as_deref() {
        Some(value) => {
            let path = value.strip_prefix('@').map(PathBuf::from);
            Some((path, text_value("gage issue add", "--description", value)?))
        }
        None => None,
    };

    // Resolve every cited note before any prompt, so a bad argument
    // fails first
    let mut evidence: Vec<String> = Vec::with_capacity(args.evidence.len());
    let mut errors = 0;
    for prefix in &args.evidence {
        match store.resolve_in(prefix, Some(NOTE_TYPE)) {
            Ok(found) if found.deleted => {
                eprintln!(
                    "gage issue add: evidence {prefix}: note {} is deleted",
                    short_uuid(&found.id)
                );
                errors += 1;
            }
            Ok(found) => evidence.push(found.id),
            Err(e) => {
                eprintln!("gage issue add: evidence {prefix}: {e}");
                errors += 1;
            }
        }
    }
    if errors > 0 {
        return Err(CliError::exit(1));
    }

    dialog::run("Add issue", || {
        let title: String = match args.title {
            Some(ref t) => {
                cli::log::step(format!("Title\n{}", style(t).dim()))?;
                t.clone()
            }
            None => cli::input("Title").placeholder("Issue title").interact()?,
        };
        let description: Option<String> = match &description {
            Some((None, d)) => {
                cli::log::step(format!("Description\n{}", style(d).dim()))?;
                Some(d.clone())
            }
            Some((Some(path), d)) => {
                cli::log::step(format!(
                    "Description\n{}",
                    style(format!("{} ({} bytes)", path.display(), d.len())).dim()
                ))?;
                Some(d.clone())
            }
            None if args.yes => None,
            None => Some(
                cli::input("Description")
                    .placeholder("Type a description (optional)")
                    .required(false)
                    .interact()?,
            ),
        };
        let description = description.filter(|d| !d.trim().is_empty());
        let name: String = match args.name {
            Some(ref n) => n.clone(),
            None if args.yes => "user-issue".to_string(),
            None => cli::input("Name")
                .default_input("user-issue")
                .placeholder("user-issue")
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
        if !evidence.is_empty() {
            let listing = evidence
                .iter()
                .map(|id| short_uuid(id).to_string())
                .collect::<Vec<_>>()
                .join("\n");
            cli::log::step(format!("Evidence\n{}", style(listing).dim()))?;
        }
        let status = if args.pending {
            IssueStatus::Pending
        } else {
            IssueStatus::Open
        };
        cli::log::step(format!("Status\n{}", style(status).dim()))?;

        if !args.yes {
            let confirmed = cli::confirm("Continue?").initial_value(true).interact()?;
            if !confirmed {
                return Err(DialogError::Canceled);
            }
        }

        let author = resolve_author(Some(username));
        let id = issues
            .create(IssueInput {
                name: &name,
                title: &title,
                description: description.as_deref(),
                author: &author,
                status,
                evidence: &evidence,
                key: None,
            })
            .map_err(|e| DialogError::Failed(e.to_string()))?;
        Ok(format!("Added issue {}", short_uuid(&id)).into())
    })
}

/// `$USER` when set and non-empty
fn env_user() -> Option<String> {
    std::env::var_os("USER")
        .map(|u| u.to_string_lossy().into_owned())
        .filter(|u| !u.is_empty())
}

/// Resolve every argument to an issue before writing anything, so one
/// bad argument leaves the store untouched. `accept` rejects an issue
/// whose current status does not fit the command.
fn resolve_issues(
    command: &str,
    issues: &IssueStore,
    prefixes: &[String],
    accept: impl Fn(&IssueFull) -> Result<(), String>,
) -> Result<Vec<IssueFull>, CliError> {
    let mut out: Vec<IssueFull> = Vec::with_capacity(prefixes.len());
    let mut errors = 0;
    for prefix in prefixes {
        match issues.get(prefix) {
            Ok(issue) => match accept(&issue) {
                Ok(()) => out.push(issue),
                Err(reason) => {
                    eprintln!("{command}: issue {}: {reason}", short_uuid(&issue.id));
                    errors += 1;
                }
            },
            Err(e) => {
                eprintln!("{command}: {e}");
                errors += 1;
            }
        }
    }
    if errors > 0 {
        return Err(CliError::exit(1));
    }
    Ok(out)
}

fn issues_step(issues: &[IssueFull]) -> Result<(), std::io::Error> {
    let label = if issues.len() == 1 { "Issue" } else { "Issues" };
    dialog::step_list(
        label,
        issues
            .iter()
            .map(|i| format!("{} {}", short_uuid(&i.id), i.title)),
    )
}

pub fn delete(args: IssueDeleteArgs) -> Result<(), CliError> {
    let store = open_store("gage issue delete")?;
    let issues = IssueStore::from(&store);
    let targets = resolve_issues("gage issue delete", &issues, &args.ids, |_| Ok(()))?;
    let total = targets.len();

    dialog::run("Delete issues", || {
        issues_step(&targets)?;
        cli::log::remark("This cannot be undone")?;
        if !args.yes {
            let confirmed = cli::confirm("Continue?").initial_value(false).interact()?;
            if !confirmed {
                return Err(DialogError::Canceled);
            }
        }
        let mut deleted = 0;
        for issue in &targets {
            match issues.delete(&issue.id) {
                Ok(_) => deleted += 1,
                Err(e) => cli::log::warning(format!("{}: {e}", short_uuid(&issue.id)))?,
            }
        }
        if deleted < total {
            return Err(dialog::partial("Deleted", deleted, total, "issue"));
        }
        Ok(format!("Deleted {}", count(deleted, "issue")).into())
    })
}

pub fn close(args: IssueCloseArgs) -> Result<(), CliError> {
    let store = open_store("gage issue close")?;
    let issues = IssueStore::from(&store);
    let targets = resolve_issues("gage issue close", &issues, &args.ids, |i| {
        if i.status == IssueStatus::Closed {
            Err("already closed".to_string())
        } else {
            Ok(())
        }
    })?;
    let reason = if args.wontfix {
        StatusReason::WontFix
    } else if args.duplicate.is_some() {
        StatusReason::Duplicate
    } else {
        StatusReason::Completed
    };
    let survivor = match &args.duplicate {
        Some(duplicate) => Some(
            issues
                .get(duplicate)
                .with_context(|| format!("gage issue close: --duplicate {duplicate}"))?,
        ),
        None => None,
    };
    let given_message = args.message.resolve("gage issue close")?;
    let total = targets.len();

    dialog::run("Close issues", || {
        issues_step(&targets)?;
        cli::log::step(format!("Reason\n{}", style(reason).dim()))?;
        if let Some(survivor) = &survivor {
            cli::log::step(format!(
                "Duplicate of\n{}",
                style(format!("{} {}", short_uuid(&survivor.id), survivor.title)).dim()
            ))?;
        }
        let message = prompt_message(given_message.clone(), args.yes, "Type a message (optional)")?;
        if !args.yes {
            let confirmed = cli::confirm("Continue?").initial_value(true).interact()?;
            if !confirmed {
                return Err(DialogError::Canceled);
            }
        }
        let author = resolve_author(None);
        let mut closed = 0;
        for issue in &targets {
            let result = match &survivor {
                Some(survivor) => {
                    issues.close_duplicate(&issue.id, &survivor.id, &author, message.as_deref())
                }
                None => issues.set_status(
                    &issue.id,
                    IssueStatus::Closed,
                    Some(reason),
                    &author,
                    message.as_deref(),
                ),
            };
            match result {
                Ok(_) => closed += 1,
                Err(e) => cli::log::warning(format!("{}: {e}", short_uuid(&issue.id)))?,
            }
        }
        if closed < total {
            return Err(dialog::partial("Closed", closed, total, "issue"));
        }
        Ok(format!("Closed {} ({reason})", count(closed, "issue")).into())
    })
}

pub fn open(args: IssueOpenArgs) -> Result<(), CliError> {
    let store = open_store("gage issue open")?;
    let issues = IssueStore::from(&store);
    let targets = resolve_issues("gage issue open", &issues, &args.ids, |i| {
        if i.status == IssueStatus::Open {
            Err("already open".to_string())
        } else {
            Ok(())
        }
    })?;
    let given_message = args.message.resolve("gage issue open")?;
    let total = targets.len();

    dialog::run("Open issues", || {
        issues_step(&targets)?;
        let message = prompt_message(given_message.clone(), args.yes, "Type a message (optional)")?;
        if !args.yes {
            let confirmed = cli::confirm("Continue?").initial_value(true).interact()?;
            if !confirmed {
                return Err(DialogError::Canceled);
            }
        }
        let author = resolve_author(None);
        let mut opened = 0;
        for issue in &targets {
            match issues.set_status(
                &issue.id,
                IssueStatus::Open,
                None,
                &author,
                message.as_deref(),
            ) {
                Ok(_) => opened += 1,
                Err(e) => cli::log::warning(format!("{}: {e}", short_uuid(&issue.id)))?,
            }
        }
        if opened < total {
            return Err(dialog::partial("Opened", opened, total, "issue"));
        }
        Ok(format!("Opened {}", count(opened, "issue")).into())
    })
}

pub fn comment(args: IssueCommentArgs) -> Result<(), CliError> {
    if args.yes && !args.message.given() {
        return Err(CliError::message(
            "gage issue comment: --yes requires --message",
        ));
    }
    let store = open_store("gage issue comment")?;
    let issues = IssueStore::from(&store);
    let targets = resolve_issues("gage issue comment", &issues, &args.ids, |_| Ok(()))?;
    let given_message = args.message.resolve("gage issue comment")?;
    let total = targets.len();

    dialog::run("Comment on issues", || {
        issues_step(&targets)?;
        let message: String = match given_message {
            Some(ref m) => {
                cli::log::step(format!("Message\n{}", style(m.trim_end()).dim()))?;
                m.clone()
            }
            None => cli::input("Message")
                .placeholder("Type a comment")
                .interact()?,
        };
        if message.trim().is_empty() {
            return Err(DialogError::Failed("comment is empty".into()));
        }
        if !args.yes {
            let confirmed = cli::confirm("Continue?").initial_value(true).interact()?;
            if !confirmed {
                return Err(DialogError::Canceled);
            }
        }
        let author = resolve_author(None);
        let mut commented = 0;
        for issue in &targets {
            match issues.comment(&issue.id, &author, &message) {
                Ok(_) => commented += 1,
                Err(e) => cli::log::warning(format!("{}: {e}", short_uuid(&issue.id)))?,
            }
        }
        if commented < total {
            return Err(dialog::partial("Commented on", commented, total, "issue"));
        }
        Ok(format!("Commented on {}", count(commented, "issue")).into())
    })
}

/// An optional message: the given one, shown; or a prompt unless
/// `--yes`, in which case none.
fn prompt_message(
    given: Option<String>,
    yes: bool,
    placeholder: &str,
) -> Result<Option<String>, DialogError> {
    match given {
        Some(m) => {
            cli::log::step(format!("Message\n{}", style(m.trim_end()).dim()))?;
            Ok(Some(m))
        }
        None if yes => Ok(None),
        None => {
            let m: String = cli::input("Message")
                .placeholder(placeholder)
                .required(false)
                .interact()?;
            Ok(if m.trim().is_empty() { None } else { Some(m) })
        }
    }
}
