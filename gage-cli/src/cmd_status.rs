//! `gage status` — one-shot health and inventory report.
//!
//! Gathers login state, plugin install/version state, corpus item
//! counts, and disk usage under `~/.gage` behind a spinner, then
//! prints the whole report at once. Read-only: no dialogs, no session
//! spawns, no token cost.

use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use clap::Args;
use console::style;

use gage_claude::preflight::{self, AuthStatus, InstalledPlugin, PreflightError};
use gage_claude::session::SessionListBuilder;
use gage_core::config::gage_home;
use gage_db::db::open_db;
use gage_db::issue::{self, IssueFilters, IssueStatusFilter};
use gage_db::note;
use gage_db::scan;

use crate::human::format_size;

#[derive(Args)]
pub struct StatusArgs {
    /// Show per-subdirectory storage breakdown under ~/.gage
    #[arg(short, long)]
    pub verbose: bool,
}

pub fn run(args: StatusArgs) {
    let spinner = crate::style::spinner("Reading status...");
    let mut out = String::new();
    write_login(&mut out);
    out.push('\n');
    write_plugin(&mut out);
    out.push('\n');
    write_counts(&mut out);
    out.push('\n');
    write_storage(&mut out, args.verbose);
    spinner.finish_and_clear();
    print!("{out}");
}

fn write_login(out: &mut String) {
    w(out, format!("{}", style("Login").bold()));
    match preflight::auth_status() {
        Ok(s) if s.logged_in => write_login_ok(out, &s),
        Ok(_) => w(
            out,
            format!(
                "  {} not logged in — run `claude` and complete /login",
                cross(),
            ),
        ),
        Err(e) => w(out, format!("  {} {}", cross(), pretty_error(&e))),
    }
}

fn write_login_ok(out: &mut String, s: &AuthStatus) {
    let identity = s.email.as_deref().unwrap_or("unknown account");
    w(out, format!("  {} logged in as {}", check(), identity));
    let mut detail = Vec::new();
    if let Some(sub) = &s.subscription_type {
        detail.push(format!("plan {sub}"));
    }
    if let Some(org) = &s.org_name {
        detail.push(format!("org {org}"));
    }
    if let Some(provider) = &s.api_provider {
        detail.push(format!("provider {provider}"));
    }
    if let Some(method) = &s.auth_method {
        detail.push(format!("method {method}"));
    }
    if !detail.is_empty() {
        w(out, format!("    {}", style(detail.join(" · ")).dim()));
    }
}

fn write_plugin(out: &mut String) {
    w(out, format!("{}", style("Plugin").bold()));
    match preflight::installed_plugin() {
        Ok(None) => w(
            out,
            format!("  {} gage plugin not installed — run `gage init`", cross(),),
        ),
        Ok(Some(p)) => write_plugin_entry(out, &p),
        Err(e) => w(out, format!("  {} {}", cross(), pretty_error(&e))),
    }
}

fn write_plugin_entry(out: &mut String, p: &InstalledPlugin) {
    let expected = preflight::EXPECTED_VERSION;
    if p.version == expected {
        let disabled = if p.enabled { "" } else { " (disabled)" };
        w(
            out,
            format!("  {} {} v{}{}", check(), p.id, p.version, disabled),
        );
    } else {
        w(
            out,
            format!(
                "  {} {} v{} installed, this gage expects v{} — run `gage init`",
                cross(),
                p.id,
                p.version,
                expected,
            ),
        );
    }
    if let Some(path) = &p.install_path {
        w(out, format!("    {}", style(path).dim()));
    }
}

fn write_counts(out: &mut String) {
    w(out, format!("{}", style("Data").bold()));

    let conn = open_db().unwrap();
    let open_ct = issue_count(&conn, IssueStatusFilter::Open);
    let pending_ct = issue_count(&conn, IssueStatusFilter::Pending);
    let closed_ct = issue_count(&conn, IssueStatusFilter::Closed);
    w(
        out,
        format!("  Issues:   {open_ct} open, {pending_ct} pending, {closed_ct} closed"),
    );

    let notes = note::count(&conn).unwrap();
    w(out, format!("  Notes:    {notes}"));

    let scans = scan::all(&conn).unwrap().len();
    w(out, format!("  Scans:    {scans}"));

    let sessions = SessionListBuilder::new().build().len();
    let agent_sessions = count_agent_sessions();
    w(
        out,
        format!("  Sessions: {sessions} Claude Code, {agent_sessions} agent"),
    );
}

fn issue_count(conn: &gage_db::rusqlite::Connection, status: IssueStatusFilter) -> u32 {
    issue::count(
        conn,
        &IssueFilters {
            status,
            ..IssueFilters::default()
        },
    )
    .unwrap()
}

fn count_agent_sessions() -> usize {
    let root = gage_home().join("claude");
    let entries = match fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return 0,
        Err(e) => panic!("failed to read {}: {e}", root.display()),
    };
    let mut n = 0;
    for entry in entries {
        let path = entry.unwrap().path();
        if !path.is_dir() {
            continue;
        }
        for file in fs::read_dir(&path).unwrap() {
            if file
                .unwrap()
                .path()
                .extension()
                .and_then(|s| s.to_str())
                .is_some_and(|ext| ext == "jsonl")
            {
                n += 1;
            }
        }
    }
    n
}

fn write_storage(out: &mut String, verbose: bool) {
    w(out, format!("{}", style("Storage").bold()));
    let root = gage_home();
    let total = dir_size(&root);
    w(out, format!("  ~/.gage:  {}", format_size(total as i64)));
    if !verbose {
        return;
    }
    let entries = match fs::read_dir(&root) {
        Ok(it) => it,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return,
        Err(e) => panic!("failed to read {}: {e}", root.display()),
    };
    let mut children: Vec<(String, u64)> = Vec::new();
    for entry in entries {
        let entry = entry.unwrap();
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let size = if path.is_dir() {
            dir_size(&path)
        } else {
            fs::metadata(&path).unwrap().len()
        };
        children.push((name, size));
    }
    children.sort_by_key(|(_, size)| std::cmp::Reverse(*size));
    for (name, size) in children {
        w(
            out,
            format!(
                "    {:<24}  {}",
                name,
                style(format_size(size as i64)).dim(),
            ),
        );
    }
}

fn dir_size(path: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack: Vec<PathBuf> = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(it) => it,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => panic!("failed to read {}: {e}", dir.display()),
        };
        for entry in entries {
            let entry = entry.unwrap();
            let metadata = entry.metadata().unwrap();
            if metadata.is_dir() {
                stack.push(entry.path());
            } else {
                total = total.saturating_add(metadata.len());
            }
        }
    }
    total
}

/// Append `line` and a newline to `out`. Writing to a `String` cannot
/// fail; the tiny helper trims the noise from the writeln! + must_use
/// dance without discarding a fallible operation.
fn w(out: &mut String, line: String) {
    writeln!(out, "{line}").expect("String write is infallible");
}

fn check() -> console::StyledObject<&'static str> {
    style("✓").green()
}

fn cross() -> console::StyledObject<&'static str> {
    style("✗").red()
}

fn pretty_error(e: &PreflightError) -> String {
    match e {
        PreflightError::Other(m) => m.clone(),
        other => other.to_string(),
    }
}
