//! The report of a run.

use std::collections::BTreeMap;
use std::fmt::Write;

use crate::check::SessionResult;

/// What a run found for one source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceReport {
    pub name: String,
    pub harness: String,
    pub outcome: Result<Checked, String>,
}

/// A source whose sessions were checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checked {
    /// The upstream revision the sessions were read at
    pub revision: String,
    /// The revision was already in the cache
    pub cached: bool,
    pub sessions: Vec<SessionResult>,
}

/// Whether every source was checked and every session passed
pub fn passed(reports: &[SourceReport]) -> bool {
    reports.iter().all(|report| match &report.outcome {
        Ok(checked) => checked.sessions.iter().all(|s| s.failure.is_none()),
        Err(_) => false,
    })
}

pub fn render(reports: &[SourceReport]) -> String {
    let mut out = String::new();
    for report in reports {
        render_source(&mut out, report);
        out.push('\n');
    }
    render_totals(&mut out, reports);
    out
}

fn render_source(out: &mut String, report: &SourceReport) {
    let SourceReport {
        name,
        harness,
        outcome,
    } = report;
    let checked = match outcome {
        Ok(checked) => checked,
        Err(error) => {
            writeln!(out, "{name} ({harness})\n  not checked: {error}").unwrap();
            return;
        }
    };
    let origin = if checked.cached {
        "cached"
    } else {
        "downloaded"
    };
    writeln!(out, "{name} ({harness}) at {}, {origin}", checked.revision).unwrap();
    let failures: Vec<&SessionResult> = checked
        .sessions
        .iter()
        .filter(|s| s.failure.is_some())
        .collect();
    writeln!(
        out,
        "  sessions: {}, passed: {}, failed: {}",
        checked.sessions.len(),
        checked.sessions.len() - failures.len(),
        failures.len()
    )
    .unwrap();
    render_counts(out, "content formats", checked, |s| {
        s.content_format.as_deref()
    });
    render_counts(out, "harness versions", checked, |s| {
        s.harness_version.as_deref()
    });
    for session in failures {
        if let Some(failure) = &session.failure {
            writeln!(
                out,
                "  FAILED {}: {}: {}",
                failure.check,
                session.file.display(),
                failure.message
            )
            .unwrap();
        }
    }
}

fn render_counts(
    out: &mut String,
    label: &str,
    checked: &Checked,
    value: impl Fn(&SessionResult) -> Option<&str>,
) {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for session in &checked.sessions {
        *counts.entry(value(session).unwrap_or("none")).or_default() += 1;
    }
    let counts: Vec<String> = counts
        .iter()
        .map(|(value, count)| format!("{value} ({count})"))
        .collect();
    writeln!(out, "  {label}: {}", counts.join(", ")).unwrap();
}

fn render_totals(out: &mut String, reports: &[SourceReport]) {
    let mut totals: BTreeMap<&str, (usize, usize, usize)> = BTreeMap::new();
    for report in reports {
        let total = totals.entry(&report.harness).or_default();
        match &report.outcome {
            Ok(checked) => {
                total.0 += checked.sessions.len();
                total.1 += checked
                    .sessions
                    .iter()
                    .filter(|s| s.failure.is_some())
                    .count();
            }
            Err(_) => total.2 += 1,
        }
    }
    for (harness, (sessions, failed, unchecked)) in totals {
        writeln!(
            out,
            "{harness}: {sessions} sessions, {failed} failed, {unchecked} sources not checked"
        )
        .unwrap();
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::check::{Check, Failure};

    fn session(file: &str, failure: Option<Failure>) -> SessionResult {
        SessionResult {
            file: PathBuf::from(file),
            content_format: Some("f 1".into()),
            harness_version: None,
            failure,
        }
    }

    #[test]
    fn a_report_lists_each_failure_and_totals_by_harness() {
        let reports = [
            SourceReport {
                name: "a".into(),
                harness: "claude".into(),
                outcome: Ok(Checked {
                    revision: "abc".into(),
                    cached: true,
                    sessions: vec![
                        session("x.jsonl", None),
                        session(
                            "y.jsonl",
                            Some(Failure {
                                check: Check::Codec,
                                message: "line 2 encodes to other bytes".into(),
                            }),
                        ),
                    ],
                }),
            },
            SourceReport {
                name: "b".into(),
                harness: "claude".into(),
                outcome: Err("git fetch: no route".into()),
            },
        ];
        assert!(!passed(&reports));
        assert_eq!(
            render(&reports),
            "a (claude) at abc, cached\n\
             \x20 sessions: 2, passed: 1, failed: 1\n\
             \x20 content formats: f 1 (2)\n\
             \x20 harness versions: none (2)\n\
             \x20 FAILED codec: y.jsonl: line 2 encodes to other bytes\n\
             \n\
             b (claude)\n\
             \x20 not checked: git fetch: no route\n\
             \n\
             claude: 2 sessions, 1 failed, 1 sources not checked\n"
        );
    }

    #[test]
    fn a_run_passes_when_every_session_of_every_source_passes() {
        let report = SourceReport {
            name: "a".into(),
            harness: "claude".into(),
            outcome: Ok(Checked {
                revision: "abc".into(),
                cached: false,
                sessions: vec![session("x.jsonl", None)],
            }),
        };
        assert!(passed(&[report]));
    }
}
