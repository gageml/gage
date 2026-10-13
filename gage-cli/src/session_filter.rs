//! Shared session-filter surface for commands that operate on more
//! than one native session at a time. `SessionFilterArgs` flattens
//! into a subcommand's clap struct next to that command's own
//! positional session list; `select_sessions` turns the two into a
//! concrete list of native session ids.
//!
//! The same struct is used by `session add`, `dataset add`, and
//! `scan`.

use std::collections::HashSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::Args;
use datafusion::prelude::SessionContext;
use gage_query::ContextBuilder;
use rand::seq::SliceRandom;

use crate::source;

/// Names of the arg fields on this struct, in the order the shared
/// help layout shows them. A wrapping args struct uses this list to
/// make its positional session list conflict with every filter, and
/// to build an `ArgGroup` when at least one selection is required.
pub const FILTER_ARG_NAMES: &[&str] = &["project", "days", "today", "all", "limit", "sample"];

#[derive(Args)]
pub struct SessionFilterArgs {
    /// Limit sessions to project (path or name)
    ///
    /// PROJECT is a project directory path (absolute, relative, or
    /// ~-prefixed) or a project slug as shown by 'gage session list'.
    #[arg(short, long, value_name = "PROJECT", allow_hyphen_values = true)]
    pub project: Option<String>,

    /// Select sessions modified in past N days
    #[arg(long, value_name = "N", conflicts_with = "all")]
    pub days: Option<u32>,

    /// Select sessions modified since 00:00 today
    #[arg(long, conflicts_with_all = ["days", "all"])]
    pub today: bool,

    /// Select every available session
    #[arg(short, long)]
    pub all: bool,

    /// Select latest N sessions
    ///
    /// Combines with --days or --today to cap the sessions selected
    /// from the window.
    #[arg(
        short = 'n',
        long,
        value_name = "N",
        conflicts_with_all = ["all", "sample"],
    )]
    pub limit: Option<usize>,

    /// Select N sessions at random
    ///
    /// Samples from sessions modified in the past 30 days, or the
    /// window given with --days or --today.
    #[arg(short = 'r', long, value_name = "N", conflicts_with = "all")]
    pub sample: Option<usize>,
}

/// The failure modes of `select_sessions`.
#[derive(Debug)]
pub enum SessionSelectError {
    /// One or more positional `sessions` prefixes did not resolve.
    /// The vector holds the per-prefix error text, ready to print
    /// one per line.
    Lookup(Vec<String>),
    /// The `--project` value did not name any recorded project.
    NoProject(String),
    /// Setup failure — the project-corpus query failed, or a required
    /// piece of the environment (cwd) could not be read.
    Setup(String),
}

impl fmt::Display for SessionSelectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Lookup(errors) => {
                for (i, e) in errors.iter().enumerate() {
                    if i > 0 {
                        writeln!(f)?;
                    }
                    write!(f, "{e}")?;
                }
                Ok(())
            }
            Self::NoProject(p) => write!(f, "no sessions for project '{p}'"),
            Self::Setup(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for SessionSelectError {}

impl SessionFilterArgs {
    /// True when the caller supplied no filter axis — every field
    /// holds its clap default.
    pub fn is_empty(&self) -> bool {
        self.project.is_none()
            && self.limit.is_none()
            && self.sample.is_none()
            && self.days.is_none()
            && !self.today
            && !self.all
    }
}

/// Resolve a command's positional session list and filter options to
/// native session ids in `source`, the default source when `None`.
///
/// A non-empty `sessions` list is resolved prefix by prefix through
/// the source. Otherwise the filter axes become one query over the
/// source's session table, which the driver answers from its listing
/// without parsing a session: a missing window defaults to the past
/// 30 days, and a missing limit defaults to 20 unless a window or
/// `--project` was pinned (in which case the full filtered set is
/// returned).
pub async fn select_sessions(
    source: Option<&str>,
    sessions: &[String],
    filter: &SessionFilterArgs,
) -> Result<Vec<String>, SessionSelectError> {
    let registry = source::driver_registry();
    let (driver, spec) =
        source::resolve_source(&registry, source).map_err(SessionSelectError::Setup)?;

    if !sessions.is_empty() {
        let handle = driver
            .open_source(&spec)
            .map_err(|e| SessionSelectError::Setup(format!("{spec}: {e}")))?;
        let mut out = Vec::with_capacity(sessions.len());
        let mut errors = Vec::new();
        for prefix in sessions {
            match handle.find_native(prefix) {
                Ok(id) => out.push(id),
                Err(e) => errors.push(e.to_string()),
            }
        }
        handle
            .close()
            .map_err(|e| SessionSelectError::Setup(format!("{spec}: {e}")))?;
        if !errors.is_empty() {
            return Err(SessionSelectError::Lookup(errors));
        }
        return Ok(out);
    }

    let ctx = ContextBuilder::new(None).build().await;
    let from = format!("native_session('{}')", sql_str(&spec));
    let mut predicates = Vec::new();

    if let Some(value) = &filter.project {
        let slug = resolve_project_in(&ctx, &from, value).await?;
        predicates.push(format!("project = '{}'", sql_str(&slug)));
    }

    let since = if filter.all {
        None
    } else if filter.today {
        Some(since_local_midnight())
    } else if let Some(days) = filter.days {
        Some(Duration::from_secs(u64::from(days) * 86_400))
    } else {
        Some(Duration::from_secs(30 * 86_400))
    };
    if let Some(d) = since {
        let cutoff_ms = (SystemTime::now() - d)
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        predicates.push(format!("mtime >= to_timestamp_millis({cutoff_ms})"));
    }

    let window_pinned = filter.all || filter.today || filter.days.is_some();
    let limit = if filter.all || filter.sample.is_some() {
        None
    } else if let Some(n) = filter.limit {
        Some(n)
    } else if window_pinned || filter.project.is_some() {
        None
    } else {
        Some(20)
    };

    let mut sql = format!("SELECT id FROM {from}");
    if !predicates.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&predicates.join(" AND "));
    }
    sql.push_str(" ORDER BY mtime DESC");
    if let Some(n) = limit {
        sql.push_str(&format!(" LIMIT {n}"));
    }
    let mut ids = string_column(&ctx, &sql)
        .await
        .map_err(|e| SessionSelectError::Setup(e.to_string()))?;
    if let Some(n) = filter.sample {
        ids.shuffle(&mut rand::rng());
        ids.truncate(n);
    }
    Ok(ids)
}

/// Resolve a `--project` value against the projects recorded in the
/// source. Slug forms are matched first; anything else resolves as a
/// path through the driver's `project_for_path`.
async fn resolve_project_in(
    ctx: &SessionContext,
    from: &str,
    value: &str,
) -> Result<String, SessionSelectError> {
    let projects = known_projects(ctx, from)
        .await
        .map_err(|e| SessionSelectError::Setup(e.to_string()))?;
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    let home_slug = project_for_path(ctx, &home)
        .await
        .map_err(|e| SessionSelectError::Setup(e.to_string()))?;
    if let Some(slug) = resolve_project_slug(value, &home_slug, &projects) {
        return Ok(slug);
    }
    let cwd = std::env::current_dir().map_err(|e| SessionSelectError::Setup(e.to_string()))?;
    let path = project_path_candidate(value, &home, &cwd);
    let slug = project_for_path(ctx, &path)
        .await
        .map_err(|e| SessionSelectError::Setup(e.to_string()))?;
    if projects.contains(&slug) {
        Ok(slug)
    } else {
        Err(SessionSelectError::NoProject(value.to_string()))
    }
}

/// Every distinct `project` slug recorded in the source.
pub(crate) async fn known_projects(
    ctx: &SessionContext,
    from: &str,
) -> anyhow::Result<HashSet<String>> {
    Ok(
        string_column(ctx, &format!("SELECT DISTINCT project FROM {from}"))
            .await?
            .into_iter()
            .collect(),
    )
}

/// The driver's project name for `path`, through the `project_for_path`
/// UDF.
async fn project_for_path(ctx: &SessionContext, path: &Path) -> anyhow::Result<String> {
    let sql = format!(
        "SELECT project_for_path('{}')",
        sql_str(&path.to_string_lossy())
    );
    string_column(ctx, &sql)
        .await?
        .pop()
        .ok_or_else(|| anyhow::anyhow!("project_for_path returned no row"))
}

/// The non-null values of a query's first column, in row order.
async fn string_column(ctx: &SessionContext, sql: &str) -> anyhow::Result<Vec<String>> {
    use datafusion::arrow::array::{Array, StringArray};

    let batches = ctx.sql(sql).await?.collect().await?;
    let mut out = Vec::new();
    for batch in &batches {
        let col = batch
            .column(0)
            .as_any()
            .downcast_ref::<StringArray>()
            .ok_or_else(|| anyhow::anyhow!("expected a Utf8 column from: {sql}"))?;
        for i in 0..batch.num_rows() {
            if !col.is_null(i) {
                out.push(col.value(i).to_string());
            }
        }
    }
    Ok(out)
}

fn sql_str(s: &str) -> String {
    s.replace('\'', "''")
}

/// Resolve a `--project` value as a project slug recorded in the
/// corpus. A leading `-` names a slug exactly. A value with no path
/// separator is tried as the home-relative abbreviation `gage session
/// list` shows (`home_slug` plus `-` plus the value), then with a
/// leading `-` prepended. `None` when no slug form matches; the value
/// is then a path.
pub(crate) fn resolve_project_slug(
    value: &str,
    home_slug: &str,
    projects: &HashSet<String>,
) -> Option<String> {
    if value.starts_with('-') {
        return projects.contains(value).then(|| value.to_string());
    }
    if !value.contains('/') && !value.starts_with('~') {
        let abbreviated = format!("{home_slug}-{value}");
        if projects.contains(&abbreviated) {
            return Some(abbreviated);
        }
        let full = format!("-{value}");
        if projects.contains(&full) {
            return Some(full);
        }
    }
    None
}

/// The directory a `--project` value names when it is a path: `~`
/// against `home`, relative against `cwd`, canonicalized when it
/// exists.
pub(crate) fn project_path_candidate(value: &str, home: &Path, cwd: &Path) -> PathBuf {
    let expanded = if value == "~" {
        home.to_path_buf()
    } else if let Some(rest) = value.strip_prefix("~/") {
        home.join(rest)
    } else {
        PathBuf::from(value)
    };
    let resolved = if expanded.is_absolute() {
        expanded
    } else {
        cwd.join(expanded)
    };
    resolved.canonicalize().unwrap_or(resolved)
}

/// Elapsed time since midnight local time.
/// `SessionListBuilder::since` takes a duration back from now, so
/// the local-midnight cutoff is expressed as that offset.
pub(crate) fn since_local_midnight() -> Duration {
    use chrono::{Local, NaiveTime};
    let now = Local::now();
    let midnight = now
        .with_time(NaiveTime::MIN)
        .earliest()
        .expect("midnight should map to a local time");
    (now - midnight)
        .to_std()
        .expect("now should not precede midnight")
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::path::{Path, PathBuf};

    use super::{project_path_candidate, resolve_project_slug};

    const HOME: &str = "/home/tester";
    const HOME_SLUG: &str = "-home-tester";
    const CWD: &str = "/work";

    fn resolve(value: &str, known: &[&str]) -> Option<String> {
        let projects: HashSet<String> = known.iter().map(|s| s.to_string()).collect();
        resolve_project_slug(value, HOME_SLUG, &projects)
    }

    fn path(value: &str) -> PathBuf {
        project_path_candidate(value, Path::new(HOME), Path::new(CWD))
    }

    #[test]
    fn full_slug_matches_exactly() {
        assert_eq!(
            resolve("-home-tester-Code-gage", &["-home-tester-Code-gage"]).as_deref(),
            Some("-home-tester-Code-gage")
        );
    }

    #[test]
    fn unknown_full_slug_is_none() {
        assert_eq!(
            resolve("-home-tester-nope", &["-home-tester-Code-gage"]),
            None
        );
    }

    #[test]
    fn abbreviation_prepends_home_slug() {
        assert_eq!(
            resolve("Code-gage", &["-home-tester-Code-gage"]).as_deref(),
            Some("-home-tester-Code-gage")
        );
    }

    #[test]
    fn slug_missing_leading_dash_matches() {
        assert_eq!(
            resolve("home-tester-Code-gage", &["-home-tester-Code-gage"]).as_deref(),
            Some("-home-tester-Code-gage")
        );
    }

    #[test]
    fn abbreviation_wins_over_path() {
        // "proj" matches the abbreviation; the slug interpretation is
        // checked before the path
        assert_eq!(
            resolve("proj", &["-home-tester-proj", "-work-proj"]).as_deref(),
            Some("-home-tester-proj")
        );
    }

    #[test]
    fn path_forms_are_not_slugs() {
        assert_eq!(resolve("~/proj", &["-home-tester-proj"]), None);
        assert_eq!(resolve("./proj", &["-work-proj"]), None);
        assert_eq!(resolve("nonexistent", &["-home-tester-Code-gage"]), None);
    }

    #[test]
    fn tilde_path_resolves_against_home() {
        assert_eq!(path("~/proj"), Path::new("/home/tester/proj"));
        assert_eq!(path("~"), Path::new("/home/tester"));
    }

    #[test]
    fn relative_path_resolves_against_cwd() {
        assert_eq!(path("proj"), Path::new("/work/proj"));
    }
}
