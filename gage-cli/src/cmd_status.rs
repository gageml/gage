//! `gage status` — one-shot health and inventory report.
//!
//! Gathers each driver's status (its harness, login, and Gage's
//! installation), corpus item counts, and disk usage under `~/.gage`
//! behind a spinner, then prints the whole report at once. Read-only:
//! no dialogs, no session spawns, no token cost.

use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use clap::Args;
use console::style;
use datafusion::prelude::SessionContext;

use gage_claude::session::SessionListBuilder;
use gage_core::config::gage_home;
use gage_query::ContextBuilder;
use gage_session::{AuthInfo, Driver, DriverStatus, Field, HarnessInfo, InstallInfo};
use gage_store::Store;

use crate::cmd_note::count_rows;
use crate::human::format_size;
use crate::source::driver_registry;

#[derive(Args)]
pub struct StatusArgs {
    /// Show per-subdirectory storage breakdown under ~/.gage
    #[arg(short, long)]
    pub verbose: bool,
}

pub async fn run(args: StatusArgs) {
    let spinner = crate::style::spinner("Reading status...");
    let mut out = String::new();
    let mut problems = Vec::new();
    for driver in driver_registry().drivers() {
        write_driver(&mut out, &mut problems, driver.as_ref());
        out.push('\n');
    }
    write_counts(&mut out, &mut problems).await;
    out.push('\n');
    write_storage(&mut out, args.verbose);
    if !problems.is_empty() {
        out.push('\n');
        write_problems(&mut out, &problems);
    }
    spinner.finish_and_clear();
    print!("{out}");
}

/// One driver's block: its harness, login, and Gage installation,
/// each on a labeled line, with every problem collected for the
/// closing list.
fn write_driver(out: &mut String, problems: &mut Vec<String>, driver: &dyn Driver) {
    w(
        out,
        format!("{}", style(format!("Driver {}", driver.name())).bold()),
    );
    let status = match driver.status() {
        Ok(status) => status,
        Err(e) => {
            w(out, "  unavailable".to_string());
            problems.push(format!("Driver {} status unavailable: {e}", driver.name()));
            return;
        }
    };
    let DriverStatus {
        harness,
        auth,
        install,
    } = status;
    write_field(out, "Harness", &harness, harness_line);
    write_field(out, "Login", &auth, auth_line);
    write_field(out, "Install", &install, install_line);
    for problem in [harness.problem(), auth.problem(), install.problem()]
        .into_iter()
        .flatten()
    {
        problems.push(problem_text(problem));
    }
}

/// Write `label` and the field's value: the rendered info, a
/// problem's message, or nothing for an unsupported field.
fn write_field<T>(out: &mut String, label: &str, field: &Field<T>, render: fn(&T) -> String) {
    let value = match field {
        Field::Ok(info) => render(info),
        Field::Problem { message, .. } => message.clone(),
        Field::Unsupported => return,
    };
    w(out, format!("  {label:<8} {value}"));
}

fn harness_line(h: &HarnessInfo) -> String {
    let version = h.version.as_deref().unwrap_or("unknown version");
    format!("{} {version} {}", h.name, style(h.path.display()).dim())
}

fn auth_line(a: &AuthInfo) -> String {
    let mut parts = vec![
        a.account
            .clone()
            .unwrap_or_else(|| "unknown account".into()),
    ];
    for (label, value) in [
        ("plan", &a.plan),
        ("org", &a.org),
        ("provider", &a.provider),
        ("method", &a.method),
    ] {
        if let Some(value) = value {
            parts.push(format!("{}", style(format!("{label} {value}")).dim()));
        }
    }
    parts.join(" · ")
}

fn install_line(i: &InstallInfo) -> String {
    let version = i.version.as_deref().unwrap_or("unknown version");
    let disabled = if i.enabled { "" } else { " (disabled)" };
    match &i.path {
        Some(path) => format!("gage v{version}{disabled} {}", style(path.display()).dim()),
        None => format!("gage v{version}{disabled}"),
    }
}

fn problem_text((message, remedy): (&str, &str)) -> String {
    if remedy.is_empty() {
        message.to_string()
    } else {
        format!("{message} - {remedy}")
    }
}

async fn write_counts(out: &mut String, problems: &mut Vec<String>) {
    w(out, format!("{}", style("Data").bold()));

    let path = gage_store::store_path();
    match Store::open(&path) {
        Ok(store) => {
            let ctx = ContextBuilder::new(Some(Arc::new(Mutex::new(store))))
                .build()
                .await;
            write_store_counts(out, &ctx).await;
        }
        Err(e) => {
            w(out, "  store unavailable".to_string());
            problems.push(format!("Store at {} unavailable: {e}", path.display()));
        }
    }

    let sessions = SessionListBuilder::new().build().len();
    w(out, format!("  Sessions: {sessions}"));
}

async fn write_store_counts(out: &mut String, ctx: &SessionContext) {
    let open_ct = count_rows(ctx, "SELECT COUNT(*) FROM issue WHERE status = 'open'").await;
    let pending_ct = count_rows(ctx, "SELECT COUNT(*) FROM issue WHERE status = 'pending'").await;
    let closed_ct = count_rows(ctx, "SELECT COUNT(*) FROM issue WHERE status = 'closed'").await;
    w(
        out,
        format!("  Issues:   {open_ct} open, {pending_ct} pending, {closed_ct} closed"),
    );

    let notes = count_rows(ctx, "SELECT COUNT(*) FROM note").await;
    w(out, format!("  Notes:    {notes}"));

    let scans = count_rows(ctx, "SELECT COUNT(*) FROM scan").await;
    w(out, format!("  Scans:    {scans}"));
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

fn write_problems(out: &mut String, problems: &[String]) {
    for problem in problems {
        w(out, format!("- {}", style(problem).red()));
    }
}

/// Append `line` and a newline to `out`. Writing to a `String` cannot
/// fail; the tiny helper trims the noise from the writeln! + must_use
/// dance without discarding a fallible operation.
fn w(out: &mut String, line: String) {
    writeln!(out, "{line}").expect("String write is infallible");
}
