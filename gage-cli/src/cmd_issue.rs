use clap::{Args, Subcommand};
use cliclack as cli;
use gage_core::text_resolve::TextResolver;
use gage_core::uuid::short_uuid;
use gage_db::db;
use gage_db::issue::{self, Issue, IssueFilters, IssueStatus, IssueStatusFilter, StatusReason};
use gage_db::scan::{self, ScanError};
use gage_registry::scanner::ScannerRegistry;
use gage_registry::scheme::{ErrorScheme, ScannerScheme};

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

    /// Open a pending or closed issue
    Open(IssueOpenArgs),

    /// Comment on an issue
    Comment(IssueCommentArgs),
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
    #[arg(short, long)]
    description: Option<String>,
    /// Issue name (default: user-issue)
    #[arg(short, long, default_value = "user-issue")]
    name: String,

    /// Add as 'pending' instead of the default 'open'
    #[arg(short, long)]
    pending: bool,
}

#[derive(Args)]
pub struct IssueDeleteArgs {
    /// Issue IDs (or prefix)
    ids: Vec<String>,

    /// Skip confirmation prompt
    #[arg(short, long)]
    yes: bool,
}

#[derive(Args)]
pub struct IssueOpenArgs {
    /// Issue ID (or prefix)
    id: String,

    /// Message explaining issue open
    #[arg(short, long)]
    message: Option<String>,

    /// Skip confirmation prompt
    #[arg(short, long)]
    yes: bool,
}

#[derive(Args)]
pub struct IssueCommentArgs {
    /// Issue ID (or prefix)
    id: String,

    /// Comment text (prompted if omitted)
    #[arg(short, long)]
    message: Option<String>,

    /// Skip confirmation prompt
    #[arg(short, long)]
    yes: bool,
}

#[derive(Args)]
pub struct IssueCloseArgs {
    /// Issue ID (or prefix)
    id: String,

    /// Close as 'skipped' instead of the default 'completed'
    #[arg(short, long)]
    skipped: bool,

    /// Close as 'duplicate' instead of the default 'completed'
    #[arg(short, long, conflicts_with = "skipped")]
    duplicate: bool,

    /// Message explaining issue close
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
    #[arg(long)]
    name: Option<String>,

    /// Show closed issues
    #[arg(short, long)]
    closed: bool,
}

pub fn list(args: IssueListArgs) {
    let conn = db::open_db().unwrap();
    let filter_only = IssueFilters {
        status: if args.closed {
            IssueStatusFilter::Closed
        } else {
            IssueStatusFilter::Unresolved
        },
        name: args.name,
        limit: None,
        offset: None,
    };
    let total = match issue::count(&conn, &filter_only) {
        Ok(n) => n as usize,
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    };
    if total == 0 {
        println!("No issues found");
        return;
    }

    let show = args.limit.show_count(total);
    let issues = match issue::find(
        &conn,
        &IssueFilters {
            limit: Some(show as u32),
            ..filter_only
        },
    ) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    };

    let highlighter = style::IdHighlighter::new(match issue::all_ids(&conn) {
        Ok(ids) => ids,
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    });

    let header: Vec<String> = ["Id", "Name", "Title", "Scan", "Status", "Created"]
        .iter()
        .map(|s| s.to_string())
        .collect();

    let rows: Vec<Vec<String>> = issues
        .iter()
        .map(|t| {
            vec![
                highlighter.short(&t.id),
                t.name.clone(),
                t.title.clone(),
                t.scan
                    .as_deref()
                    .map(short_uuid)
                    .unwrap_or_default()
                    .to_string(),
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
        .modify(Rows::first(), style::tty(Color::FG_BRIGHT_YELLOW))
        .modify(Columns::new(3..6).not(Rows::first()), style::dim())
        .to_string();
    println!("{table}");

    args.limit.print_summary(show, total, "issue");
}

pub fn show(args: IssueShowArgs) {
    let conn = db::open_db().unwrap();
    let issue = match issue::get(&conn, &args.id) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    };

    let description_display = resolve_description(&issue);

    let session_ids = match issue::issue_sessions(&conn, &issue.id) {
        Ok(ids) => ids,
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    };
    let mut displays = crate::cmd_scan::SessionDisplays::default();
    let session_displays: Vec<_> = session_ids
        .iter()
        .map(|id| (id.as_str(), displays.resolve(id)))
        .collect();

    let scan_display = match issue.scan.as_deref() {
        Some(scan_id) => {
            let created = match scan::get_scan(&conn, scan_id) {
                Ok(s) => crate::human::format_elapsed_ms(s.created),
                Err(ScanError::NotFound(_)) => "(unavailable)".to_string(),
                Err(e) => {
                    eprintln!("Error: {e}");
                    std::process::exit(1);
                }
            };
            format!("{} · {}", short_uuid(scan_id), created)
        }
        None => String::new(),
    };

    let attrs = vec![
        ("id", issue.id.clone()),
        ("name", issue.name.clone()),
        ("title", issue.title.clone()),
        (
            "status",
            match issue.status_reason {
                Some(r) => format!("{} ({})", issue.status.as_str(), r.as_str()),
                None => issue.status.as_str().to_string(),
            },
        ),
        ("description", description_display),
        (
            "target",
            issue
                .target
                .as_ref()
                .map(|t| t.to_uri())
                .unwrap_or_default(),
        ),
        // Placeholder — styled after value_width is known, like the
        // evidence and events sections.
        ("sessions", String::new()),
        ("scan", scan_display),
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

    let evidence = match issue::related_notes(&conn, &issue.id) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    };

    let events = match issue::issue_events_for(&conn, &issue.id) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    };

    let config = match gage_core::config::Config::load_user() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    };
    let related =
        match gage_db::related::related_issues(&conn, &issue.id, config.issues.related_threshold) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        };

    let related_label = "related";
    let evidence_label = "evidence";
    let events_label = "events";
    let label_width = attrs
        .iter()
        .map(|(k, _)| k.len())
        .chain([
            related_label.len(),
            evidence_label.len(),
            events_label.len(),
        ])
        .max()
        .unwrap_or(0);
    let (_, term_width) = console::Term::stdout().size();
    // Borders + padding: "│ " + " │ " + " │" = 8 chars
    let value_width = (term_width as usize)
        .saturating_sub(label_width + 8)
        .max(20);

    let sessions_display = session_displays
        .iter()
        .map(|(id, d)| {
            let header = console::style(textwrap::fill(
                &format!("{} · {}", short_uuid(id), d.project),
                value_width,
            ))
            .dim();
            let title = console::style(textwrap::fill(&d.title, value_width))
                .cyan()
                .bright();
            format!("{header}\n{title}")
        })
        .collect::<Vec<_>>()
        .join("\n\n");

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

    if !evidence.is_empty() {
        let entries: Vec<String> = evidence
            .iter()
            .map(|n| {
                let header = console::style(textwrap::fill(
                    &format!("{} · {} · {}", short_uuid(&n.id), n.name, n.target.to_uri()),
                    value_width,
                ))
                .dim();
                let value_str = crate::cmd_note::format_value(&n.value);
                let value = console::style(textwrap::fill(&value_str, value_width))
                    .cyan()
                    .bright();
                format!("{header}\n{value}")
            })
            .collect();
        rows.push(vec![evidence_label.to_string(), entries.join("\n\n")]);
    }

    if !related.is_empty() {
        let entries: Vec<String> = related
            .iter()
            .map(|r| {
                let rel = match issue::get(&conn, &r.id) {
                    Ok(i) => i,
                    Err(e) => {
                        eprintln!("Error: {e}");
                        std::process::exit(1);
                    }
                };
                let prefix = format!("{} · {} · ", short_uuid(&rel.id), rel.status.as_str());
                let wrapped = textwrap::fill(&format!("{prefix}{}", rel.title), value_width);
                let dimmed = console::style(&prefix).dim().to_string();
                wrapped.replacen(&prefix, &dimmed, 1)
            })
            .collect();
        rows.push(vec![related_label.to_string(), entries.join("\n")]);
    }

    if !events.is_empty() {
        let entries: Vec<String> = events
            .iter()
            .map(|ev| {
                let header = console::style(textwrap::fill(
                    &format!(
                        "{} · {} · {}",
                        ev.event.to_label(),
                        ev.author,
                        gage_core::datetime::ms_to_iso8601(ev.timestamp),
                    ),
                    value_width,
                ))
                .dim();
                match ev.event.message() {
                    Some(m) => format!("{}\n{}", header, textwrap::fill(m, value_width)),
                    None => header.to_string(),
                }
            })
            .collect();
        rows.push(vec![events_label.to_string(), entries.join("\n\n")]);
    }

    let table = Table::from_iter(rows)
        .with(Style::rounded())
        .modify(Columns::first(), style::tty(Color::FG_BRIGHT_YELLOW))
        .to_string();
    println!("{table}");
}

pub fn add(args: IssueAddArgs) {
    dialog::run("Add issue", || {
        let title: String = match args.title {
            Some(ref t) => t.clone(),
            None => cli::input("Title").placeholder("Issue title").interact()?,
        };
        let description: String = match args.description {
            Some(ref d) => d.clone(),
            None => cli::input("Description")
                .placeholder("Type a description")
                .interact()?,
        };
        let description = if description.is_empty() {
            None
        } else {
            Some(description)
        };

        let id = gage_core::uuid::new_uuid();
        // Each created issue is its own writing event: the author
        // carries a per-call instance so `(name, author)` never
        // collides across creations by the same user.
        let author = format!(
            "{}?call={}",
            crate::author::resolve_author(None),
            short_uuid(&id)
        );
        let issue = Issue {
            name: "general".to_string(),
            id,
            title,
            description,
            target: None,
            metadata: None,
            status: if args.pending {
                IssueStatus::Pending
            } else {
                IssueStatus::Open
            },
            status_reason: None,
            created: gage_core::datetime::now_ms(),
            modified: None,
            author,
            scan: None,
        };
        let conn = db::open_db().unwrap();
        issue::insert(&conn, &issue)
            .map_err(|e| DialogError::Other(anyhow::Error::msg(e.to_string())))?;

        cli::log::remark(format!("id: {}", issue.id))?;
        Ok("Issue added".into())
    });
}

pub fn delete(args: IssueDeleteArgs) {
    if args.ids.is_empty() {
        eprintln!(
            "gage issue delete: provide one or more issue IDs\n\n\
             Use 'gage issue list' to show issues"
        );
        std::process::exit(1);
    }

    let conn = db::open_db().unwrap();

    let mut issues: Vec<Issue> = Vec::new();
    let mut errors = 0;
    for prefix in &args.ids {
        match issue::get(&conn, prefix) {
            Ok(i) => issues.push(i),
            Err(e) => {
                eprintln!("{e}");
                errors += 1;
            }
        }
    }
    if errors > 0 {
        std::process::exit(1);
    }

    let count = issues.len();

    dialog::run("Delete issues", || {
        let listing = issues
            .iter()
            .map(|i| format!("{} {}", console::style(short_uuid(&i.id)).dim(), i.title))
            .collect::<Vec<_>>()
            .join("\n");
        let label = if count == 1 { "Issue" } else { "Issues" };
        cli::log::step(format!("{label}\n{listing}"))?;

        if !args.yes {
            let plural = if count == 1 { "issue" } else { "issues" };
            let prompt = format!("Permanently delete {count} {plural}? This cannot be undone.");
            let confirmed = cli::confirm(prompt).initial_value(false).interact()?;
            if !confirmed {
                return Err(DialogError::Canceled);
            }
        }

        let mut deleted = 0;
        for issue in &issues {
            if let Err(e) = issue::delete(&conn, &issue.id) {
                eprintln!("warning: failed to delete {}: {e}", short_uuid(&issue.id));
            } else {
                deleted += 1;
            }
        }

        let plural = if deleted == 1 { "issue" } else { "issues" };
        Ok(format!("Deleted {deleted} {plural}").into())
    });
}

pub fn close(args: IssueCloseArgs) {
    let conn = db::open_db().unwrap();
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
        StatusReason::Skipped
    } else if args.duplicate {
        StatusReason::Duplicate
    } else {
        StatusReason::Completed
    };

    dialog::run("Close issue", || {
        cli::log::step(format!(
            "Issue\n{} {}",
            console::style(short_uuid(&target_issue.id)).dim(),
            target_issue.title,
        ))?;
        cli::log::step(format!("Reason\n{}", console::style(reason.as_str()).dim()))?;

        let message: Option<String> = match args.message {
            Some(ref m) => {
                cli::log::step(format!("Message\n{}", console::style(m).dim()))?;
                Some(m.clone())
            }
            None => {
                let m: String = cli::input("Message")
                    .placeholder("Type a message (optional)")
                    .required(false)
                    .interact()?;
                if m.is_empty() { None } else { Some(m) }
            }
        };

        if !args.yes {
            let confirmed = cli::confirm("Close this issue?")
                .initial_value(true)
                .interact()?;
            if !confirmed {
                return Err(DialogError::Canceled);
            }
        }

        let author = crate::author::resolve_author(None);
        issue::set_status(
            &conn,
            &target_issue.id,
            IssueStatus::Closed,
            Some(reason),
            &author,
            message.as_deref(),
        )
        .map_err(|e| DialogError::Other(anyhow::Error::msg(e.to_string())))?;

        Ok(format!(
            "Closed issue {} ({})",
            short_uuid(&target_issue.id),
            reason.as_str()
        )
        .into())
    });
}

pub fn open(args: IssueOpenArgs) {
    let conn = db::open_db().unwrap();
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

    dialog::run("Open issue", || {
        cli::log::step(format!(
            "Issue\n{} {}",
            console::style(short_uuid(&target_issue.id)).dim(),
            target_issue.title,
        ))?;

        if !args.yes {
            let confirmed = cli::confirm("Open this issue?")
                .initial_value(true)
                .interact()?;
            if !confirmed {
                return Err(DialogError::Canceled);
            }
        }

        let author = crate::author::resolve_author(None);
        issue::set_status(
            &conn,
            &target_issue.id,
            IssueStatus::Open,
            None,
            &author,
            args.message.as_deref(),
        )
        .map_err(|e| DialogError::Other(anyhow::Error::msg(e.to_string())))?;

        Ok(format!("Opened issue {}", short_uuid(&target_issue.id)).into())
    });
}

pub fn comment(args: IssueCommentArgs) {
    let conn = db::open_db().unwrap();
    let target_issue = match issue::get(&conn, &args.id) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    };

    dialog::run("Comment on issue", || {
        cli::log::step(format!(
            "Issue\n{} {}",
            console::style(short_uuid(&target_issue.id)).dim(),
            target_issue.title,
        ))?;

        let message: String = match args.message {
            Some(ref m) => m.clone(),
            None => cli::input("Message")
                .placeholder("Type a comment")
                .interact()?,
        };

        if !args.yes {
            let confirmed = cli::confirm("Add this comment?")
                .initial_value(true)
                .interact()?;
            if !confirmed {
                return Err(DialogError::Canceled);
            }
        }

        let author = crate::author::resolve_author(None);
        issue::comment(&conn, &target_issue.id, &author, &message)
            .map_err(|e| DialogError::Other(anyhow::Error::msg(e.to_string())))?;

        Ok(format!("Commented on issue {}", short_uuid(&target_issue.id)).into())
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
        Some(name) => match ScannerScheme::with_scanner_name(&registry, name) {
            Ok(s) => r.with_scheme("scanner", s),
            Err(e) => r.with_scheme("scanner", ErrorScheme::new(e.to_string())),
        },
        None => r.with_scheme("scanner", ScannerScheme::absolute_only()),
    }
}
