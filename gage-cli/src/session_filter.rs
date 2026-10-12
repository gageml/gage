//! Shared session-filter surface for commands that operate on more
//! than one native session at a time. `SessionFilterArgs` flattens
//! into a subcommand's clap struct next to that command's own
//! positional session list; `select_sessions` turns the two into a
//! concrete list of native session records.
//!
//! The same struct is used by `session add`, `dataset add`, and
//! `scan`.

use std::collections::HashSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::Args;
use gage_claude::session::{self, SessionInfo, SessionListBuilder};
use gage_query::ContextBuilder;
use rand::seq::SliceRandom;

/// Names of the arg fields on this struct, in the order the shared
/// help layout shows them. A wrapping args struct uses this list to
/// make its positional session list conflict with every filter, and
/// to build an `ArgGroup` when at least one selection is required.
pub const FILTER_ARG_NAMES: &[&str] = &["limit", "sample", "days", "today", "all", "project"];

#[derive(Args)]
pub struct SessionFilterArgs {
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

    /// Select sessions modified in past N days
    #[arg(long, value_name = "N", conflicts_with = "all")]
    pub days: Option<u32>,

    /// Select sessions modified since 00:00 today
    #[arg(long, conflicts_with_all = ["days", "all"])]
    pub today: bool,

    /// Select every available session
    #[arg(short, long)]
    pub all: bool,

    /// Limit sessions to project (path or name)
    ///
    /// PROJECT is a project directory path (absolute, relative, or
    /// ~-prefixed) or a project slug as shown by 'gage session list'.
    #[arg(short, long, value_name = "PROJECT", allow_hyphen_values = true)]
    pub project: Option<String>,
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
/// native session records.
///
/// A non-empty `sessions` list is resolved through
/// `session::one_session` and returned as-is. Otherwise the filter
/// axes populate a `SessionListBuilder`; a missing window defaults to
/// the past 30 days, and a missing limit defaults to 20 unless a
/// window or `--project` was pinned (in which case the full filtered
/// set is returned).
pub async fn select_sessions(
    sessions: &[String],
    filter: &SessionFilterArgs,
) -> Result<Vec<SessionInfo>, SessionSelectError> {
    if !sessions.is_empty() {
        let mut out = Vec::with_capacity(sessions.len());
        let mut errors = Vec::new();
        for prefix in sessions {
            match session::one_session(prefix) {
                Ok(s) => out.push(s),
                Err(e) => errors.push(e.to_string()),
            }
        }
        if !errors.is_empty() {
            return Err(SessionSelectError::Lookup(errors));
        }
        return Ok(out);
    }

    {
        let project_slug = match &filter.project {
            Some(p) => {
                let projects = known_projects()
                    .await
                    .map_err(|e| SessionSelectError::Setup(e.to_string()))?;
                let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
                let cwd = std::env::current_dir()
                    .map_err(|e| SessionSelectError::Setup(e.to_string()))?;
                match resolve_project(p, &home, &cwd, &projects) {
                    Some(slug) => Some(slug),
                    None => return Err(SessionSelectError::NoProject(p.clone())),
                }
            }
            None => None,
        };

        let since = if filter.all {
            None
        } else if filter.today {
            Some(since_local_midnight())
        } else if let Some(days) = filter.days {
            Some(Duration::from_secs(u64::from(days) * 86_400))
        } else {
            Some(Duration::from_secs(30 * 86_400))
        };

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

        let mut builder = SessionListBuilder::new();
        if let Some(slug) = project_slug {
            builder = builder.project_slug(slug);
        }
        if let Some(d) = since {
            builder = builder.since(d);
        }
        if let Some(n) = limit {
            builder = builder.limit(n);
        }
        let mut sessions: Vec<SessionInfo> = builder.build().into_iter().collect();
        if let Some(n) = filter.sample {
            sessions.shuffle(&mut rand::rng());
            sessions.truncate(n);
        }
        Ok(sessions)
    }
}

/// Every distinct `project` slug recorded in the session corpus.
/// The empty source is used, matching how `scan` and `session list`
/// discover the default source.
pub(crate) async fn known_projects() -> anyhow::Result<HashSet<String>> {
    use datafusion::arrow::array::{Array, StringArray};

    let ctx = ContextBuilder::new(None).build().await;
    let batches = ctx
        .sql("SELECT DISTINCT project FROM native_session('')")
        .await?
        .collect()
        .await?;
    let mut out = HashSet::new();
    for batch in &batches {
        let col = batch
            .column(0)
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("session.project should be a Utf8 column");
        for i in 0..batch.num_rows() {
            if !col.is_null(i) {
                out.insert(col.value(i).to_string());
            }
        }
    }
    Ok(out)
}

/// Resolve a `--project` value to an encoded project slug recorded
/// in the session corpus. A leading `-` names a slug exactly. A
/// value with no path separator is tried as a slug first: as the
/// home-relative abbreviation `gage session list` shows, then with a
/// leading `-` prepended. Anything else — or a slug miss — resolves
/// as a path (`~` against `home`, relative against `cwd`),
/// canonicalized and encoded. `None` when no candidate matches a
/// recorded project.
pub(crate) fn resolve_project(
    value: &str,
    home: &Path,
    cwd: &Path,
    projects: &HashSet<String>,
) -> Option<String> {
    if value.starts_with('-') {
        return projects.contains(value).then(|| value.to_string());
    }
    if !value.contains('/') && !value.starts_with('~') {
        let abbreviated = format!("{}-{value}", session::encode_project_dir(home));
        if projects.contains(&abbreviated) {
            return Some(abbreviated);
        }
        let full = format!("-{value}");
        if projects.contains(&full) {
            return Some(full);
        }
    }
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
    let canonical = resolved.canonicalize().unwrap_or(resolved);
    let slug = session::encode_project_dir(&canonical);
    projects.contains(&slug).then_some(slug)
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
    use std::path::Path;

    use super::resolve_project;

    const HOME: &str = "/home/tester";
    const CWD: &str = "/work";

    fn resolve(value: &str, known: &[&str]) -> Option<String> {
        let projects: HashSet<String> = known.iter().map(|s| s.to_string()).collect();
        resolve_project(value, Path::new(HOME), Path::new(CWD), &projects)
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
    fn tilde_path_resolves_against_home() {
        assert_eq!(
            resolve("~/proj", &["-home-tester-proj"]).as_deref(),
            Some("-home-tester-proj")
        );
    }

    #[test]
    fn relative_path_resolves_against_cwd() {
        assert_eq!(
            resolve("proj", &["-work-proj"]).as_deref(),
            Some("-work-proj")
        );
    }

    #[test]
    fn abbreviation_wins_over_path() {
        // "proj" matches both the abbreviation and a cwd-relative path;
        // the slug interpretation is checked first
        assert_eq!(
            resolve("proj", &["-home-tester-proj", "-work-proj"]).as_deref(),
            Some("-home-tester-proj")
        );
    }

    #[test]
    fn no_match_is_none() {
        assert_eq!(resolve("nonexistent", &["-home-tester-Code-gage"]), None);
    }
}
