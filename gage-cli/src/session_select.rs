//! Shared session-selection surface for commands that operate on
//! more than one native session at a time. `SessionSelectArgs`
//! flattens into a subcommand's clap struct; `resolve` turns the
//! parsed values into a concrete list of native session records.
//!
//! The same struct is used by `session add` and, in follow-up work,
//! by `dataset add` and `scan2`. `scan` keeps its own args struct
//! while its interactive dialog remains, and reuses the helpers in
//! this module for project resolution and the today-window cutoff.

use std::collections::HashSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::Args;
use gage_claude::session::{self, SessionInfo, SessionListBuilder};
use rand::seq::SliceRandom;

use crate::source;

/// Names of the arg fields on this struct, in the order the shared
/// help layout shows them. Callers that need to enforce "at least
/// one selection" pass this list into an `ArgGroup` on the wrapping
/// args struct.
pub const SELECT_ARG_NAMES: &[&str] = &[
    "sessions", "limit", "sample", "days", "today", "all", "project",
];

#[derive(Args)]
pub struct SessionSelectArgs {
    /// Session IDs (or prefixes)
    #[arg(
        value_name = "SESSION",
        conflicts_with_all = ["project", "limit", "sample", "days", "today", "all"]
    )]
    pub sessions: Vec<String>,

    /// Select the latest N sessions
    ///
    /// Combines with --days or --today to cap the sessions selected
    /// from the window.
    #[arg(
        short = 'n',
        long,
        value_name = "N",
        conflicts_with_all = ["all", "sample"],
        display_order = 6,
    )]
    pub limit: Option<usize>,

    /// Select N sessions at random
    ///
    /// Samples from sessions modified in the past 30 days, or the
    /// window given with --days or --today.
    #[arg(
        short = 'r',
        long,
        value_name = "N",
        conflicts_with = "all",
        display_order = 7
    )]
    pub sample: Option<usize>,

    /// Select sessions modified in the past N days
    #[arg(long, value_name = "N", conflicts_with = "all", display_order = 8)]
    pub days: Option<u32>,

    /// Select sessions modified since midnight local time
    #[arg(long, conflicts_with_all = ["days", "all"], display_order = 9)]
    pub today: bool,

    /// Select every available session
    #[arg(short, long, display_order = 10)]
    pub all: bool,

    /// Limit sessions to a project
    ///
    /// PROJECT is a project directory path (absolute, relative, or
    /// ~-prefixed) or a project slug as shown by 'gage session list'.
    #[arg(
        short,
        long,
        value_name = "PROJECT",
        allow_hyphen_values = true,
        display_order = 11
    )]
    pub project: Option<String>,
}

/// The failure modes of `SessionSelectArgs::resolve`.
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

impl SessionSelectArgs {
    /// True when the caller supplied no positional session and no
    /// filter axis — every field holds its clap default.
    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
            && self.project.is_none()
            && self.limit.is_none()
            && self.sample.is_none()
            && self.days.is_none()
            && !self.today
            && !self.all
    }

    /// Resolve the parsed options to native session records.
    ///
    /// A non-empty positional `sessions` list is resolved through
    /// `session::one_session` and returned as-is. Otherwise the
    /// filter axes populate a `SessionListBuilder`; a missing window
    /// defaults to the past 30 days, and a missing limit defaults to
    /// 20 unless a window or `--project` was pinned (in which case
    /// the full filtered set is returned). `cmd_label` is used only
    /// for error prefixes on any project-corpus query that runs to
    /// resolve `--project`.
    pub async fn resolve(&self, cmd_label: &str) -> Result<Vec<SessionInfo>, SessionSelectError> {
        if !self.sessions.is_empty() {
            let mut out = Vec::with_capacity(self.sessions.len());
            let mut errors = Vec::new();
            for prefix in &self.sessions {
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

        let project_slug = match &self.project {
            Some(p) => {
                let projects = known_projects(cmd_label)
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

        let since = if self.all {
            None
        } else if self.today {
            Some(since_local_midnight())
        } else if let Some(days) = self.days {
            Some(Duration::from_secs(u64::from(days) * 86_400))
        } else {
            Some(Duration::from_secs(30 * 86_400))
        };

        let window_pinned = self.all || self.today || self.days.is_some();
        let limit = if self.all || self.sample.is_some() {
            None
        } else if let Some(n) = self.limit {
            Some(n)
        } else if window_pinned || self.project.is_some() {
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
        if let Some(n) = self.sample {
            sessions.shuffle(&mut rand::rng());
            sessions.truncate(n);
        }
        Ok(sessions)
    }
}

/// Every distinct `project` slug recorded in the session corpus.
/// The empty source is used, matching how `scan` and `session list`
/// discover the default source.
pub(crate) async fn known_projects(cmd_label: &str) -> anyhow::Result<HashSet<String>> {
    use datafusion::arrow::array::{Array, StringArray};

    let source = source::open_source_or_exit(cmd_label, "");
    let ctx = gage_query::create_context(source.as_ref()).await?;
    let batches = ctx
        .sql("SELECT DISTINCT project FROM session")
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

/// The slug with the home prefix stripped — the form that
/// `gage session list` shows.
pub(crate) fn project_slug_display(slug: &str) -> String {
    let home = std::env::var_os("HOME").unwrap_or_default();
    let prefix = format!("{}-", session::encode_project_dir(Path::new(&home)));
    slug.strip_prefix(&prefix).unwrap_or(slug).to_string()
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
