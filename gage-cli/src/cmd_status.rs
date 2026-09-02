//! `gage status` — one-shot health and inventory report.
//!
//! Prints login state, plugin install/version state, corpus item
//! counts, and disk usage under `~/.gage`. Read-only: no dialogs, no
//! spinners, no session spawns, no token cost.

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
pub struct StatusArgs {}

pub fn run(_args: StatusArgs) {
    print_login();
    println!();
    print_plugin();
    println!();
    print_counts();
    println!();
    print_storage();
}

fn print_login() {
    println!("{}", style("Login").bold());
    match preflight::auth_status() {
        Ok(s) if s.logged_in => print_login_ok(&s),
        Ok(_) => println!(
            "  {} not logged in — run `claude` and complete /login",
            cross()
        ),
        Err(e) => println!("  {} {}", cross(), pretty_error(&e)),
    }
}

fn print_login_ok(s: &AuthStatus) {
    let identity = s.email.as_deref().unwrap_or("unknown account");
    println!("  {} logged in as {}", check(), identity);
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
        println!("    {}", style(detail.join(" · ")).dim());
    }
}

fn print_plugin() {
    println!("{}", style("Plugin").bold());
    match preflight::installed_plugin() {
        Ok(None) => println!("  {} gage plugin not installed — run `gage init`", cross(),),
        Ok(Some(p)) => print_plugin_entry(&p),
        Err(e) => println!("  {} {}", cross(), pretty_error(&e)),
    }
}

fn print_plugin_entry(p: &InstalledPlugin) {
    let expected = preflight::EXPECTED_VERSION;
    if p.version == expected {
        let disabled = if p.enabled { "" } else { " (disabled)" };
        println!("  {} {} v{}{}", check(), p.id, p.version, disabled,);
    } else {
        println!(
            "  {} {} v{} installed, this gage expects v{} — run `gage init`",
            cross(),
            p.id,
            p.version,
            expected,
        );
    }
    if let Some(path) = &p.install_path {
        println!("    {}", style(path).dim());
    }
}

fn print_counts() {
    println!("{}", style("Data").bold());

    match open_db() {
        Ok(conn) => {
            let open_ct = issue::count(
                &conn,
                &IssueFilters {
                    status: IssueStatusFilter::Open,
                    ..IssueFilters::default()
                },
            )
            .unwrap_or(0);
            let pending_ct = issue::count(
                &conn,
                &IssueFilters {
                    status: IssueStatusFilter::Pending,
                    ..IssueFilters::default()
                },
            )
            .unwrap_or(0);
            let closed_ct = issue::count(
                &conn,
                &IssueFilters {
                    status: IssueStatusFilter::Closed,
                    ..IssueFilters::default()
                },
            )
            .unwrap_or(0);
            println!(
                "  Issues:   {} open, {} pending, {} closed",
                open_ct, pending_ct, closed_ct,
            );

            let notes = note::count(&conn).unwrap_or(0);
            println!("  Notes:    {notes}");

            let scans = scan::all(&conn).map(|v| v.len()).unwrap_or(0);
            println!("  Scans:    {scans}");
        }
        Err(e) => {
            println!("  {} database open failed: {e}", cross());
        }
    }

    let sessions = SessionListBuilder::new().build().len();
    let agent_sessions = count_agent_sessions();
    println!("  Sessions: {sessions} Claude Code, {agent_sessions} agent");
}

fn count_agent_sessions() -> usize {
    let root = gage_home().join("claude");
    let Ok(entries) = fs::read_dir(&root) else {
        return 0;
    };
    let mut n = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Ok(files) = fs::read_dir(&path) else {
            continue;
        };
        for file in files.flatten() {
            if file
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

fn print_storage() {
    println!("{}", style("Storage").bold());
    let root = gage_home();
    match dir_size(&root) {
        Ok(total) => {
            println!("  {}  ({})", root.display(), format_size(total as i64));
            let entries = match fs::read_dir(&root) {
                Ok(it) => it,
                Err(e) => {
                    println!("  {} could not list {}: {e}", cross(), root.display());
                    return;
                }
            };
            let mut children: Vec<(String, u64)> = Vec::new();
            for entry in entries.flatten() {
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().into_owned();
                let size = if path.is_dir() {
                    dir_size(&path).unwrap_or(0)
                } else {
                    fs::metadata(&path).map(|m| m.len()).unwrap_or(0)
                };
                children.push((name, size));
            }
            children.sort_by_key(|(_, size)| std::cmp::Reverse(*size));
            for (name, size) in children {
                println!(
                    "    {:<24}  {}",
                    name,
                    style(format_size(size as i64)).dim(),
                );
            }
        }
        Err(e) => println!("  {} could not size {}: {e}", cross(), root.display()),
    }
}

fn dir_size(path: &Path) -> io::Result<u64> {
    let mut total = 0u64;
    let mut stack: Vec<PathBuf> = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(it) => it,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        for entry in entries.flatten() {
            let metadata = match entry.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            if metadata.is_dir() {
                stack.push(entry.path());
            } else {
                total = total.saturating_add(metadata.len());
            }
        }
    }
    Ok(total)
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
