//! `gage status` — one-shot health and inventory report.
//!
//! Gathers each driver's status (its harness, login, and Gage's
//! plugin installation), corpus item counts, and disk usage under Gage home
//! behind a spinner, then prints the whole report at once. Read-only:
//! no dialogs, no session spawns, no token cost.

use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Context;
use clap::Args;
use console::style;
use datafusion::prelude::SessionContext;

use gage_core::config::gage_home;
use gage_core::path::shorten_home;
use gage_query::ContextBuilder;
use gage_session::{AuthInfo, Driver, DriverStatus, Field, HarnessInfo, InstallInfo};
use gage_store::{Store, StoreOrigin};

use crate::cmd_note::count_rows;
use crate::error::CliError;
use crate::human::format_size;
use crate::progress;
use crate::source::driver_registry;

#[derive(Args)]
pub struct StatusArgs {
    /// Show per-subdirectory storage breakdown under Gage home
    #[arg(short, long)]
    pub verbose: bool,
}

pub async fn run(args: StatusArgs) -> Result<(), CliError> {
    let report = progress::busy_async("Reading status", report(args)).await?;
    print!("{report}");
    Ok(())
}

async fn report(args: StatusArgs) -> Result<String, CliError> {
    let mut out = String::new();
    let mut problems = Vec::new();
    w(&mut out, format!("{}", style("Drivers").bold()));
    let registry = driver_registry();
    let default = registry.default().map(|d| d.name());
    for driver in registry.drivers() {
        let is_default = default == Some(driver.name());
        write_driver(&mut out, &mut problems, driver.as_ref(), is_default);
    }
    out.push('\n');
    write_counts(&mut out, &mut problems).await?;
    out.push('\n');
    write_storage(&mut out, &gage_home(), args.verbose);
    if !problems.is_empty() {
        out.push('\n');
        write_problems(&mut out, &problems);
    }
    Ok(out)
}

/// One driver's block: its name, then its harness, login, and Gage
/// plugin each on a labeled line, with every problem collected
/// for the closing list. A harness that is not installed is shown as
/// such and is a problem only for the default driver, the one a
/// model with no scheme goes to.
fn write_driver(
    out: &mut String,
    problems: &mut Vec<String>,
    driver: &dyn Driver,
    is_default: bool,
) {
    w(out, format!("  {}", driver.name()));
    let status = match driver.status() {
        Ok(status) => status,
        Err(e) => {
            w(out, "    unavailable".to_string());
            problems.push(format!("Driver {} status unavailable: {e}", driver.name()));
            return;
        }
    };
    let DriverStatus {
        harness,
        auth,
        install,
    } = status;
    write_field(out, "Harness", &harness, harness_lines);
    write_field(out, "Login", &auth, auth_lines);
    write_field(out, "Plugin", &install, install_lines);
    for problem in [harness.problem(), auth.problem(), install.problem()]
        .into_iter()
        .flatten()
    {
        problems.push(problem_text(problem));
    }
    if is_default && let Some((message, remedy)) = harness.absent() {
        problems.push(problem_text((
            &format!("{message}, and {} is the default driver", driver.name()),
            remedy,
        )));
    }
}

/// Write `label` and the field's value: the rendered info, a
/// problem's message, or nothing for an unsupported field. The first
/// line carries the label; later lines are indented to the value
/// column.
fn write_field<T>(out: &mut String, label: &str, field: &Field<T>, render: fn(&T) -> Vec<String>) {
    let lines = match field {
        Field::Ok(info) => render(info),
        Field::Problem { message, .. } => vec![message.clone()],
        Field::Absent { message, .. } => vec![format!("{}", style(message).dim())],
        Field::Unsupported => return,
    };
    for (i, line) in lines.into_iter().enumerate() {
        if i == 0 {
            w(out, format!("    {label:<7}  {line}"));
        } else {
            w(out, format!("    {:<7}  {line}", ""));
        }
    }
}

fn harness_lines(h: &HarnessInfo) -> Vec<String> {
    let version = h.version.as_deref().unwrap_or("unknown version");
    vec![format!(
        "{} {version} {}",
        h.name,
        style(h.path.display()).dim()
    )]
}

fn auth_lines(a: &AuthInfo) -> Vec<String> {
    let mut lines = vec![a.account.clone().unwrap_or_else(|| "signed in".into())];
    for (label, value) in [
        ("plan", &a.plan),
        ("org", &a.org),
        ("provider", &a.provider),
        ("method", &a.method),
    ] {
        if let Some(value) = value {
            lines.push(format!("{}", style(format!("{label} {value}")).dim()));
        }
    }
    lines
}

fn install_lines(i: &InstallInfo) -> Vec<String> {
    let version = i.version.as_deref().unwrap_or("unknown version");
    let disabled = if i.enabled { "" } else { " (disabled)" };
    let mut lines = vec![format!("gage v{version}{disabled}")];
    if let Some(path) = &i.path {
        lines.push(format!("{}", style(path.display()).dim()));
    }
    lines
}

fn problem_text((message, remedy): (&str, &str)) -> String {
    if remedy.is_empty() {
        message.to_string()
    } else {
        format!("{message} - {remedy}")
    }
}

async fn write_counts(out: &mut String, problems: &mut Vec<String>) -> Result<(), CliError> {
    w(out, format!("{}", style("Data").bold()));

    let path = gage_store::store_path();
    let origin = match gage_store::store_origin() {
        StoreOrigin::Variable => " (GAGE_STORE)",
        StoreOrigin::Project => " (project config)",
        StoreOrigin::Home => "",
    };
    w(out, format!("  Store:    {}{origin}", shorten_home(&path)));
    match Store::open(&path) {
        Ok(store) => {
            let ctx = ContextBuilder::new(Some(Arc::new(Mutex::new(store))))
                .build()
                .await;
            write_store_counts(out, &ctx).await.context("gage status")?;
        }
        Err(e) => {
            w(out, "  store unavailable".to_string());
            problems.push(format!("Store at {} unavailable: {e}", path.display()));
        }
    }
    Ok(())
}

async fn write_store_counts(
    out: &mut String,
    ctx: &SessionContext,
) -> datafusion::error::Result<()> {
    let open_ct = count_rows(ctx, "SELECT COUNT(*) FROM issue WHERE status = 'open'").await?;
    let pending_ct = count_rows(ctx, "SELECT COUNT(*) FROM issue WHERE status = 'pending'").await?;
    let closed_ct = count_rows(ctx, "SELECT COUNT(*) FROM issue WHERE status = 'closed'").await?;
    w(
        out,
        format!("  Issues:   {open_ct} open, {pending_ct} pending, {closed_ct} closed"),
    );

    let notes = count_rows(ctx, "SELECT COUNT(*) FROM note").await?;
    w(out, format!("  Notes:    {notes}"));

    let scans = count_rows(ctx, "SELECT COUNT(*) FROM scan").await?;
    w(out, format!("  Scans:    {scans}"));

    let sessions = count_rows(ctx, "SELECT COUNT(*) FROM session").await?;
    w(out, format!("  Sessions: {sessions}"));
    Ok(())
}

fn write_storage(out: &mut String, root: &Path, verbose: bool) {
    w(out, format!("{}", style("Storage").bold()));
    let total = dir_size(root);
    let hint = if verbose {
        String::new()
    } else {
        format!(" {}", style("(use -v for details)").dim())
    };
    w(
        out,
        format!(
            "  {}:  {}{hint}",
            shorten_home(root),
            format_size(total as i64)
        ),
    );
    if !verbose {
        return;
    }
    let entries = match fs::read_dir(root) {
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

#[cfg(test)]
mod tests {
    use gage_session::{
        ContentSink, ContentSource, DriverError, NativeSession, Source, SourceOptions,
        StoredSession, WrittenContent,
    };

    use super::*;

    /// A driver that reports `status` and does nothing else
    struct Reports(DriverStatus);

    impl Driver for Reports {
        fn name(&self) -> &'static str {
            "reports"
        }
        fn version(&self) -> &'static str {
            "0"
        }
        fn schemes(&self) -> &'static [&'static str] {
            &["reports"]
        }
        fn open_source(&self, _options: &SourceOptions) -> Result<Box<dyn Source>, DriverError> {
            Err(DriverError::Other("not used".into()))
        }
        fn split_session_url(&self, _session_url: &str) -> Result<(String, String), DriverError> {
            Err(DriverError::Other("not used".into()))
        }
        fn write_native(
            &self,
            _session: &mut dyn NativeSession,
            _sink: &mut dyn ContentSink,
        ) -> Result<WrittenContent, DriverError> {
            Err(DriverError::Other("not used".into()))
        }
        fn read_stored(
            &self,
            _native_id: String,
            _content_format: &str,
            _source: Box<dyn ContentSource>,
        ) -> Result<Box<dyn StoredSession>, DriverError> {
            Err(DriverError::Other("not used".into()))
        }
        fn status(&self) -> Result<DriverStatus, DriverError> {
            Ok(self.0.clone())
        }
    }

    fn not_installed() -> Reports {
        Reports(DriverStatus {
            harness: Field::Absent {
                message: "Agent not installed".to_string(),
                remedy: "Install Agent".to_string(),
            },
            auth: Field::Unsupported,
            install: Field::Unsupported,
        })
    }

    #[test]
    fn a_harness_that_is_not_installed_is_shown_and_is_no_problem() {
        let mut out = String::new();
        let mut problems = Vec::new();
        write_driver(&mut out, &mut problems, &not_installed(), false);
        assert!(out.contains("Agent not installed"), "{out}");
        assert!(problems.is_empty(), "{problems:?}");
    }

    #[test]
    fn a_default_driver_whose_harness_is_not_installed_is_a_problem() {
        let mut out = String::new();
        let mut problems = Vec::new();
        write_driver(&mut out, &mut problems, &not_installed(), true);
        assert_eq!(
            problems,
            ["Agent not installed, and reports is the default driver - Install Agent"]
        );
    }

    #[test]
    fn storage_line_names_the_given_root() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a"), b"12345").unwrap();
        let mut out = String::new();
        write_storage(&mut out, dir.path(), false);
        let line = console::strip_ansi_codes(out.lines().nth(1).unwrap()).into_owned();
        assert_eq!(
            line,
            format!("  {}:  5 B (use -v for details)", shorten_home(dir.path()))
        );
        assert!(!out.contains("~/.gage"));
    }
}
