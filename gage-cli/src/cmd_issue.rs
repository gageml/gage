use std::io;

use clap::{Args, Subcommand};
use cliclack as cli;
use gage_core::text_resolve::TextResolver;
use gage_core::uuid::short_uuid;
use gage_db::issue::{self, ClosedReason, Issue, IssueFilters, IssueStatus, IssueStatusFilter};
use gage_db::{db, target::NoteTarget};
use gage_scan::scanner::ScannerRegistry;
use gage_scan::scanner_scheme::{ErrorScheme, ScannerScheme};

use crate::dialog::{self, DialogError};
use tabled::{
    Table,
    settings::{
        Color, Style, Width,
        object::{Columns, Object, Rows},
        peaker::PriorityMax,
    },
};

use crate::style;

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
    /// Close an issue
    Close(IssueCloseArgs),
    /// Reopen a closed issue
    Reopen(IssueReopenArgs),
}

#[derive(Args)]
pub struct IssueShowArgs {
    /// Issue UUID (or prefix)
    id: String,
}

#[derive(Args)]
pub struct IssueAddArgs {
    /// Title (prompted if omitted)
    #[arg(short, long)]
    title: Option<String>,
    /// Description (prompted if omitted; leave blank to skip)
    #[arg(short, long)]
    description: Option<String>,
}

#[derive(Args)]
pub struct IssueDeleteArgs {
    /// Issue UUID (or prefix)
    id: String,
    /// Skip confirmation prompt
    #[arg(short, long)]
    yes: bool,
}

#[derive(Args)]
pub struct IssueReopenArgs {
    /// Issue UUID (or prefix)
    id: String,
    /// Message recorded with the reopen event
    #[arg(short, long)]
    message: Option<String>,
    /// Skip confirmation prompt
    #[arg(short, long)]
    yes: bool,
}

#[derive(Args)]
pub struct IssueCloseArgs {
    /// Issue UUID (or prefix)
    id: String,
    /// Close as `skipped` instead of the default `completed`
    #[arg(short, long)]
    skipped: bool,
    /// Message recorded with the close event
    #[arg(short, long)]
    message: Option<String>,
    /// Skip confirmation prompt
    #[arg(short, long)]
    yes: bool,
}

#[derive(Args)]
pub struct IssueListArgs {
    #[command(flatten)]
    limit: crate::limit::LimitArgs,

    /// Filter by issue name
    #[arg(short, long)]
    name: Option<String>,

    /// Include closed issues
    #[arg(short, long)]
    closed: bool,
}

pub fn list(args: IssueListArgs) {
    let conn = db::open_db();
    let filters = IssueFilters {
        status: if args.closed {
            IssueStatusFilter::Any
        } else {
            IssueStatusFilter::Open
        },
        name: args.name,
    };
    let issues = match issue::find(&conn, &filters) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    };

    let total = issues.len();
    if total == 0 {
        println!("No issues found");
        return;
    }

    let show = args.limit.show_count(total);

    let header: Vec<String> = ["Id", "Name", "Title", "Status", "Created"]
        .iter()
        .map(|s| s.to_string())
        .collect();

    let rows: Vec<Vec<String>> = issues
        .iter()
        .take(show)
        .map(|t| {
            vec![
                short_uuid(&t.id).to_string(),
                t.name.clone(),
                t.title.clone(),
                t.status.as_str().to_string(),
                crate::human::format_elapsed_ms(t.created),
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
        .modify(Rows::first(), Color::FG_BRIGHT_YELLOW)
        .modify(Columns::first().not(Rows::first()), Color::FG_BRIGHT_YELLOW)
        .modify(Columns::new(3..5).not(Rows::first()), style::dim())
        .to_string();
    println!("{table}");

    args.limit.print_summary(show, total, "issue");
}

pub fn show(args: IssueShowArgs) {
    let conn = db::open_db();
    let issue = match issue::get(&conn, &args.id) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    };

    let description_display = resolve_description(&issue);

    let attrs = vec![
        ("id", issue.id.clone()),
        ("name", issue.name.clone()),
        ("target", issue.target.clone()),
        ("title", issue.title.clone()),
        ("status", issue.status.as_str().to_string()),
        (
            "closed_reason",
            issue
                .closed_reason
                .map(|r| r.as_str().to_string())
                .unwrap_or_default(),
        ),
        ("description", description_display),
        ("author", issue.author.clone()),
        ("created", gage_core::datetime::ms_to_iso8601(issue.created)),
        (
            "modified",
            issue
                .modified
                .map(gage_core::datetime::ms_to_iso8601)
                .unwrap_or_default(),
        ),
    ];

    let related = match issue::related_notes(&conn, &issue.id) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    };

    let evidence_label = "evidence";
    let label_width = attrs
        .iter()
        .map(|(k, _)| k.len())
        .chain(std::iter::once(evidence_label.len()))
        .max()
        .unwrap_or(0);
    let (_, term_width) = console::Term::stdout().size();
    // Borders + padding: "│ " + " │ " + " │" = 8 chars
    let value_width = (term_width as usize)
        .saturating_sub(label_width + 8)
        .max(20);

    let mut rows: Vec<Vec<String>> = attrs
        .into_iter()
        .map(|(k, v)| vec![k.to_string(), textwrap::fill(&v, value_width)])
        .collect();

    if !related.is_empty() {
        let entries: Vec<String> = related
            .iter()
            .map(|n| {
                let line = format!(
                    "{} ({}) {} - {}",
                    n.name,
                    short_uuid(&n.id),
                    shorten_target(&n.target),
                    n.value.to_json(),
                );
                textwrap::fill(&line, value_width)
            })
            .collect();
        rows.push(vec![evidence_label.to_string(), entries.join("\n\n")]);
    }

    let table = Table::from_iter(rows)
        .with(Style::rounded())
        .modify(Columns::first(), Color::FG_BRIGHT_YELLOW)
        .to_string();
    println!("{table}");
}

pub fn add(args: IssueAddArgs) {
    dialog::run("Add issue", || {
        let title: String = match args.title {
            Some(ref t) => t.clone(),
            None => cli::input("Title").interact()?,
        };
        let description: String = match args.description {
            Some(ref d) => d.clone(),
            None => cli::input("Description")
                .placeholder("optional; leave blank to skip")
                .required(false)
                .interact()?,
        };
        let description = if description.is_empty() {
            None
        } else {
            Some(description)
        };

        let issue = Issue {
            id: gage_core::uuid::new_uuid(),
            name: String::new(),
            target: String::new(),
            title,
            description,
            status: IssueStatus::Open,
            closed_reason: None,
            created: gage_core::datetime::now_ms(),
            modified: None,
            author: crate::author::resolve_author(None),
        };
        let conn = db::open_db();
        issue::insert(&conn, &issue)
            .map_err(|e| DialogError::Other(io::Error::other(e.to_string())))?;

        cli::log::remark(format!("id: {}", issue.id))?;
        Ok("Issue added".into())
    });
}

pub fn delete(args: IssueDeleteArgs) {
    let conn = db::open_db();
    let target_issue = match issue::get(&conn, &args.id) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    };

    dialog::run("Delete issue", || {
        cli::log::step(format!(
            "Issue\n{} {}",
            console::style(short_uuid(&target_issue.id)).dim(),
            target_issue.title,
        ))?;

        if !args.yes {
            let confirmed = cli::confirm("Permanently delete this issue? This cannot be undone.")
                .initial_value(false)
                .interact()?;
            if !confirmed {
                return Err(DialogError::Canceled);
            }
        }

        issue::delete(&conn, &target_issue.id)
            .map_err(|e| DialogError::Other(io::Error::other(e.to_string())))?;

        Ok(format!("Deleted issue {}", short_uuid(&target_issue.id)).into())
    });
}

pub fn close(args: IssueCloseArgs) {
    let conn = db::open_db();
    let target_issue = match issue::get(&conn, &args.id) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    };

    if target_issue.status == IssueStatus::Closed {
        eprintln!("Issue {} is already closed", short_uuid(&target_issue.id));
        std::process::exit(1);
    }

    let reason = if args.skipped {
        ClosedReason::Skipped
    } else {
        ClosedReason::Completed
    };

    dialog::run("Close issue", || {
        cli::log::step(format!(
            "Issue\n{} {}",
            console::style(short_uuid(&target_issue.id)).dim(),
            target_issue.title,
        ))?;
        cli::log::step(format!("Reason\n{}", console::style(reason.as_str()).dim()))?;

        if !args.yes {
            let confirmed = cli::confirm("Close this issue?")
                .initial_value(true)
                .interact()?;
            if !confirmed {
                return Err(DialogError::Canceled);
            }
        }

        let now = gage_core::datetime::now_ms();
        let author = crate::author::resolve_author(None);
        issue::close(
            &conn,
            &target_issue.id,
            reason,
            &author,
            args.message.as_deref(),
            now,
        )
        .map_err(|e| DialogError::Other(io::Error::other(e.to_string())))?;

        Ok(format!(
            "Closed issue {} ({})",
            short_uuid(&target_issue.id),
            reason.as_str()
        )
        .into())
    });
}

pub fn reopen(args: IssueReopenArgs) {
    let conn = db::open_db();
    let target_issue = match issue::get(&conn, &args.id) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    };

    if target_issue.status == IssueStatus::Open {
        eprintln!("Issue {} is already open", short_uuid(&target_issue.id));
        std::process::exit(1);
    }

    dialog::run("Reopen issue", || {
        cli::log::step(format!(
            "Issue\n{} {}",
            console::style(short_uuid(&target_issue.id)).dim(),
            target_issue.title,
        ))?;

        if !args.yes {
            let confirmed = cli::confirm("Reopen this issue?")
                .initial_value(true)
                .interact()?;
            if !confirmed {
                return Err(DialogError::Canceled);
            }
        }

        let now = gage_core::datetime::now_ms();
        let author = crate::author::resolve_author(None);
        issue::reopen(
            &conn,
            &target_issue.id,
            &author,
            args.message.as_deref(),
            now,
        )
        .map_err(|e| DialogError::Other(io::Error::other(e.to_string())))?;

        Ok(format!("Reopened issue {}", short_uuid(&target_issue.id)).into())
    });
}

fn resolve_description(issue: &Issue) -> String {
    let Some(raw) = issue.description.as_deref() else {
        return String::new();
    };
    let resolver = issue_text_resolver(issue);
    match resolver.resolve(raw.to_string()) {
        Ok(text) => text,
        Err(e) => format!("(unresolved {raw}: {e})"),
    }
}

fn issue_text_resolver(issue: &Issue) -> TextResolver {
    let registry = ScannerRegistry::load();
    let r = TextResolver::new();
    match issue.author.strip_prefix("scanner:") {
        Some(name) => match ScannerScheme::for_scanner_name(&registry, name) {
            Ok(s) => r.with_scheme("scanner", s),
            Err(e) => r.with_scheme("scanner", ErrorScheme::new(e.to_string())),
        },
        None => r.with_scheme("scanner", ScannerScheme::absolute_only()),
    }
}

fn shorten_target(target: &NoteTarget) -> String {
    let (glyph, s) = match target {
        NoteTarget::Session(t) => ("▪", t.to_uri()),
        NoteTarget::Scan(t) => ("≡", t.scan_id.clone()),
        NoteTarget::Project(t) => ("⊡", t.project_path.clone()),
    };
    let shortened = if s.len() >= 36 {
        format!("{}{}", &s[..8], &s[36..])
    } else {
        s
    };
    format!("{glyph} {shortened}")
}
