//! `gage issue2`: issues in the Gage store.
//!
//! Listings and reads go through the query context (`issue`,
//! `issue_event`, `issue_evidence`); writes go through
//! [`IssueStore`]. An issue has no target; the sessions it concerns
//! are reached through the notes it cites.

use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use clap::{Args, Subcommand, ValueEnum};
use cliclack as cli;
use console::style;
use datafusion::arrow::array::{Array, StringArray, TimestampMillisecondArray};
use gage_core::uuid::short_uuid;
use gage_query2::ContextBuilder;
use gage_store::{
    ISSUE_TYPE, IssueFull, IssueInput, IssueStatus, IssueStore, NOTE_TYPE, StatusReason, Store,
};
use tabled::{
    Table,
    settings::{
        Color, Style, Width,
        object::{Columns, Object, Rows},
        peaker::PriorityMax,
    },
};

use crate::author::resolve_author;
use crate::cmd_note::{count_rows, target_cell, value_cell};
use crate::cmd_session::{column, run_query};
use crate::dialog::{self, DialogError};
use crate::style::{self, IdKind, styled_id};

#[derive(Subcommand)]
pub enum Issue2Command {
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

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum CloseReason {
    Completed,
    Wontfix,
    Duplicate,
}

impl From<CloseReason> for StatusReason {
    fn from(reason: CloseReason) -> Self {
        match reason {
            CloseReason::Completed => StatusReason::Completed,
            CloseReason::Wontfix => StatusReason::WontFix,
            CloseReason::Duplicate => StatusReason::Duplicate,
        }
    }
}

#[derive(Args)]
pub struct IssueListArgs {
    #[command(flatten)]
    limit: crate::limit::LimitArgs,

    /// Filter by issue name
    #[arg(long)]
    name: Option<String>,

    /// Show closed issues
    #[arg(short, long)]
    closed: bool,
}

#[derive(Args)]
pub struct IssueShowArgs {
    /// Issue ID (or prefix)
    id: String,
}

#[derive(Args)]
pub struct IssueAddArgs {
    /// Title (prompted if omitted)
    #[arg(short, long)]
    title: Option<String>,

    /// Description (prompted if omitted)
    #[arg(short, long, conflicts_with = "description_file")]
    description: Option<String>,

    /// Read the description from a file
    #[arg(long, value_name = "PATH")]
    description_file: Option<PathBuf>,

    /// Issue name (default: "user-issue")
    #[arg(short, long)]
    name: Option<String>,

    /// Note cited as evidence, by ID (or prefix); repeatable
    #[arg(short, long, value_name = "NOTE")]
    evidence: Vec<String>,

    /// Author username (default: $USER)
    #[arg(short, long)]
    user: Option<String>,

    /// Add as 'pending' instead of the default 'open'
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
    /// Issue IDs (or prefixes)
    #[arg(required = true)]
    ids: Vec<String>,

    /// Skip confirmation prompt
    #[arg(short, long)]
    yes: bool,
}

#[derive(Args)]
pub struct IssueCloseArgs {
    /// Issue IDs (or prefixes)
    #[arg(required = true)]
    ids: Vec<String>,

    /// Close reason (default: completed)
    #[arg(short, long, value_enum)]
    reason: Option<CloseReason>,

    #[command(flatten)]
    message: MessageArgs,

    /// Skip prompts
    #[arg(short, long)]
    yes: bool,
}

#[derive(Args)]
pub struct IssueOpenArgs {
    /// Issue IDs (or prefixes)
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
    /// Issue IDs (or prefixes)
    #[arg(required = true)]
    ids: Vec<String>,

    #[command(flatten)]
    message: MessageArgs,

    /// Skip confirmation prompt
    ///
    /// Requires --message or --message-file
    #[arg(short, long)]
    yes: bool,
}

/// A message body given on the command line or read from a file.
#[derive(Args)]
pub struct MessageArgs {
    /// Message text
    #[arg(short, long, conflicts_with = "message_file")]
    message: Option<String>,

    /// Read the message from a file
    #[arg(long, value_name = "PATH")]
    message_file: Option<PathBuf>,
}

impl MessageArgs {
    /// The message as given, or `None` when neither option was used.
    fn resolve(&self, command: &str) -> Option<String> {
        if let Some(text) = &self.message {
            return Some(text.clone());
        }
        let path = self.message_file.as_ref()?;
        Some(read_text_file(command, "--message-file", path))
    }

    fn given(&self) -> bool {
        self.message.is_some() || self.message_file.is_some()
    }
}

fn read_text_file(command: &str, option: &str, path: &PathBuf) -> String {
    match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) => {
            eprintln!("{command}: {option} {}: {e}", path.display());
            std::process::exit(1);
        }
    }
}

fn open_store(command: &str) -> Store {
    match Store::open(&gage_store::store_path()) {
        Ok(store) => store,
        Err(e) => {
            eprintln!("{command}: {e}");
            std::process::exit(1);
        }
    }
}

pub async fn list(args: IssueListArgs) {
    let store = open_store("gage issue2 list");
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
    let batches = run_query(&ctx, &sql).await;
    let total = count_rows(&ctx, &format!("SELECT COUNT(*) FROM issue{where_clause}")).await;
    if total == 0 {
        println!("No issues found");
        return;
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
            let status = if reasons.is_valid(i) {
                format!("{} ({})", statuses.value(i), reasons.value(i))
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
                styled_id(short_uuid(ids.value(i)), prefixes.value(i), IdKind::Gage),
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
                .priority(PriorityMax::left()),
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
}

fn sql_str(s: &str) -> String {
    s.replace('\'', "''")
}

pub async fn show(args: IssueShowArgs) {
    let store = open_store("gage issue2 show");
    // The prefix resolves through the store; the rows come from SQL
    let id = match store.resolve_in(&args.id, Some(ISSUE_TYPE)) {
        Ok(found) if found.deleted => {
            eprintln!(
                "gage issue2 show: issue {} is deleted",
                short_uuid(&found.id)
            );
            std::process::exit(1);
        }
        Ok(found) => found.id,
        Err(e) => {
            eprintln!("gage issue2 show: {e}");
            std::process::exit(1);
        }
    };
    let ctx = ContextBuilder::new(Some(Arc::new(Mutex::new(store))))
        .build()
        .await;
    let sql = format!(
        "SELECT name, title, status, status_reason, description, author, scan, \
         created, modified FROM issue WHERE id = '{id}'"
    );
    let batches = run_query(&ctx, &sql).await;
    let Some(batch) = batches.iter().find(|b| b.num_rows() > 0) else {
        eprintln!("gage issue2 show: issue {id} not found");
        std::process::exit(1);
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
    let evidence = run_query(&ctx, &evidence_sql).await;
    let events_sql = format!(
        "SELECT event, author, timestamp, from_status, to_status, reason, message \
         FROM issue_event WHERE issue_id = '{id}' ORDER BY event_id"
    );
    let events = run_query(&ctx, &events_sql).await;

    let evidence_label = "evidence";
    let events_label = "events";
    let label_width = attrs
        .iter()
        .map(|(k, _)| k.len())
        .chain([evidence_label.len(), events_label.len()])
        .max()
        .unwrap_or(0);
    let (_, term_width) = console::Term::stdout().size();
    // Borders + padding: "│ " + " │ " + " │" = 8 chars
    let value_width = (term_width as usize)
        .saturating_sub(label_width + 8)
        .max(20);

    let mut rows: Vec<Vec<String>> = attrs
        .into_iter()
        .map(|(k, v)| {
            let value = if k == "description" {
                crate::markdown::render(&v, value_width)
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

    let mut event_entries: Vec<String> = Vec::new();
    for batch in &events {
        let kinds = column::<StringArray>(batch, 0);
        let authors = column::<StringArray>(batch, 1);
        let timestamps = column::<TimestampMillisecondArray>(batch, 2);
        let froms = column::<StringArray>(batch, 3);
        let tos = column::<StringArray>(batch, 4);
        let reasons = column::<StringArray>(batch, 5);
        let messages = column::<StringArray>(batch, 6);
        for i in 0..batch.num_rows() {
            let opt = |col: &StringArray| col.is_valid(i).then(|| col.value(i).to_string());
            let label = event_label(
                kinds.value(i),
                opt(froms).as_deref(),
                opt(tos).as_deref(),
                opt(reasons).as_deref(),
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
}

/// One line naming an event: `created pending`, `pending → open`,
/// `open → closed (wontfix)`, `comment`.
pub(crate) fn event_label(
    event: &str,
    from: Option<&str>,
    to: Option<&str>,
    reason: Option<&str>,
) -> String {
    let mut label = match (event, from, to) {
        ("create", _, Some(to)) => format!("created {to}"),
        ("status", Some(from), Some(to)) => format!("{from} → {to}"),
        (other, _, _) => other.to_string(),
    };
    if let Some(reason) = reason {
        label.push_str(&format!(" ({reason})"));
    }
    label
}

pub fn add(args: IssueAddArgs) {
    if args.yes && args.title.is_none() {
        eprintln!("gage issue2 add: --yes requires --title");
        std::process::exit(1);
    }
    let store = open_store("gage issue2 add");
    let issues = IssueStore::from(&store);
    let file_description = args
        .description_file
        .as_ref()
        .map(|path| read_text_file("gage issue2 add", "--description-file", path));

    // Resolve every cited note before any prompt, so a bad argument
    // fails first
    let mut evidence: Vec<String> = Vec::with_capacity(args.evidence.len());
    let mut errors = 0;
    for prefix in &args.evidence {
        match store.resolve_in(prefix, Some(NOTE_TYPE)) {
            Ok(found) if found.deleted => {
                eprintln!(
                    "gage issue2 add: evidence {prefix}: note {} is deleted",
                    short_uuid(&found.id)
                );
                errors += 1;
            }
            Ok(found) => evidence.push(found.id),
            Err(e) => {
                eprintln!("gage issue2 add: evidence {prefix}: {e}");
                errors += 1;
            }
        }
    }
    if errors > 0 {
        std::process::exit(1);
    }

    dialog::run("Add issue", || {
        let title: String = match args.title {
            Some(ref t) => {
                cli::log::step(format!("Title\n{}", style(t).dim()))?;
                t.clone()
            }
            None => cli::input("Title").placeholder("Issue title").interact()?,
        };
        let description: Option<String> = match (&args.description, &file_description) {
            (Some(d), _) => {
                cli::log::step(format!("Description\n{}", style(d).dim()))?;
                Some(d.clone())
            }
            (None, Some(d)) => {
                cli::log::step(format!(
                    "Description\n{}",
                    style(format!(
                        "{} ({} bytes)",
                        args.description_file
                            .as_ref()
                            .map(|p| p.display().to_string())
                            .unwrap_or_default(),
                        d.len()
                    ))
                    .dim()
                ))?;
                Some(d.clone())
            }
            (None, None) if args.yes => None,
            (None, None) => Some(
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
            let confirmed = cli::confirm("Add this issue?")
                .initial_value(true)
                .interact()?;
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
        Ok(format!("Issue {} added", short_uuid(&id)).into())
    });
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
) -> Vec<IssueFull> {
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
        std::process::exit(1);
    }
    out
}

fn issues_step(issues: &[IssueFull]) -> Result<(), std::io::Error> {
    let listing = issues
        .iter()
        .map(|i| format!("{} {}", style(short_uuid(&i.id)).dim(), i.title))
        .collect::<Vec<_>>()
        .join("\n");
    let label = if issues.len() == 1 { "Issue" } else { "Issues" };
    cli::log::step(format!("{label}\n{listing}"))
}

fn plural(count: usize) -> &'static str {
    if count == 1 { "issue" } else { "issues" }
}

pub fn delete(args: IssueDeleteArgs) {
    let store = open_store("gage issue2 delete");
    let issues = IssueStore::from(&store);
    let targets = resolve_issues("gage issue2 delete", &issues, &args.ids, |_| Ok(()));
    let count = targets.len();

    dialog::run("Delete issues", || {
        issues_step(&targets)?;
        if !args.yes {
            let prompt = format!(
                "Permanently delete {count} {}? This cannot be undone.",
                plural(count)
            );
            let confirmed = cli::confirm(prompt).initial_value(false).interact()?;
            if !confirmed {
                return Err(DialogError::Canceled);
            }
        }
        let mut deleted = 0;
        for issue in &targets {
            if let Err(e) = issues.delete(&issue.id) {
                eprintln!("warning: failed to delete {}: {e}", short_uuid(&issue.id));
            } else {
                deleted += 1;
            }
        }
        Ok(format!("Deleted {deleted} {}", plural(deleted)).into())
    });
}

pub fn close(args: IssueCloseArgs) {
    let store = open_store("gage issue2 close");
    let issues = IssueStore::from(&store);
    let targets = resolve_issues("gage issue2 close", &issues, &args.ids, |i| {
        if i.status == IssueStatus::Closed {
            Err("already closed".to_string())
        } else {
            Ok(())
        }
    });
    let reason: StatusReason = args.reason.unwrap_or(CloseReason::Completed).into();
    let given_message = args.message.resolve("gage issue2 close");
    let count = targets.len();

    dialog::run("Close issues", || {
        issues_step(&targets)?;
        cli::log::step(format!("Reason\n{}", style(reason).dim()))?;
        let message = prompt_message(given_message.clone(), args.yes, "Type a message (optional)")?;
        if !args.yes {
            let prompt = if count == 1 {
                "Close this issue?".to_string()
            } else {
                format!("Close {count} issues?")
            };
            let confirmed = cli::confirm(prompt).initial_value(true).interact()?;
            if !confirmed {
                return Err(DialogError::Canceled);
            }
        }
        let author = resolve_author(None);
        let mut closed = 0;
        for issue in &targets {
            if let Err(e) = issues.set_status(
                &issue.id,
                IssueStatus::Closed,
                Some(reason),
                &author,
                message.as_deref(),
            ) {
                eprintln!("warning: failed to close {}: {e}", short_uuid(&issue.id));
            } else {
                closed += 1;
            }
        }
        Ok(format!("Closed {closed} {} ({reason})", plural(closed)).into())
    });
}

pub fn open(args: IssueOpenArgs) {
    let store = open_store("gage issue2 open");
    let issues = IssueStore::from(&store);
    let targets = resolve_issues("gage issue2 open", &issues, &args.ids, |i| {
        if i.status == IssueStatus::Open {
            Err("already open".to_string())
        } else {
            Ok(())
        }
    });
    let given_message = args.message.resolve("gage issue2 open");
    let count = targets.len();

    dialog::run("Open issues", || {
        issues_step(&targets)?;
        let message = prompt_message(given_message.clone(), args.yes, "Type a message (optional)")?;
        if !args.yes {
            let prompt = if count == 1 {
                "Open this issue?".to_string()
            } else {
                format!("Open {count} issues?")
            };
            let confirmed = cli::confirm(prompt).initial_value(true).interact()?;
            if !confirmed {
                return Err(DialogError::Canceled);
            }
        }
        let author = resolve_author(None);
        let mut opened = 0;
        for issue in &targets {
            if let Err(e) = issues.set_status(
                &issue.id,
                IssueStatus::Open,
                None,
                &author,
                message.as_deref(),
            ) {
                eprintln!("warning: failed to open {}: {e}", short_uuid(&issue.id));
            } else {
                opened += 1;
            }
        }
        Ok(format!("Opened {opened} {}", plural(opened)).into())
    });
}

pub fn comment(args: IssueCommentArgs) {
    if args.yes && !args.message.given() {
        eprintln!("gage issue2 comment: --yes requires --message or --message-file");
        std::process::exit(1);
    }
    let store = open_store("gage issue2 comment");
    let issues = IssueStore::from(&store);
    let targets = resolve_issues("gage issue2 comment", &issues, &args.ids, |_| Ok(()));
    let given_message = args.message.resolve("gage issue2 comment");
    let count = targets.len();

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
            let prompt = if count == 1 {
                "Add this comment?".to_string()
            } else {
                format!("Add this comment to {count} issues?")
            };
            let confirmed = cli::confirm(prompt).initial_value(true).interact()?;
            if !confirmed {
                return Err(DialogError::Canceled);
            }
        }
        let author = resolve_author(None);
        let mut commented = 0;
        for issue in &targets {
            if let Err(e) = issues.comment(&issue.id, &author, &message) {
                eprintln!(
                    "warning: failed to comment on {}: {e}",
                    short_uuid(&issue.id)
                );
            } else {
                commented += 1;
            }
        }
        Ok(format!("Commented on {commented} {}", plural(commented)).into())
    });
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
