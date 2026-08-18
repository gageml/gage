//! Pending issue resolution: validate a whole disposition plan, then
//! apply it in one transaction. Backs the `IssuePendingResolve` MCP
//! tool. Every resolution disposes one pending issue — promoting it,
//! or closing it as a duplicate of another issue. Validation runs in
//! full before any write, so an invalid plan changes nothing.

use rusqlite::Connection;

use crate::issue::{
    self, Issue, IssueError, IssueStatus, StatusReason, comment_in_tx, set_status_in_tx,
};

/// One disposition in a resolution plan.
#[derive(Debug, Clone)]
pub enum Resolution {
    /// Promote the pending issue to open.
    Open { issue: String },
    /// Close the pending issue as a duplicate of `of`. `comment`
    /// carries material from the closed issue onto the surviving one.
    /// `reopen` returns a closed `of` to the docket before the close
    /// is applied against it.
    Duplicate {
        issue: String,
        of: String,
        comment: Option<String>,
        reopen: bool,
    },
}

/// Counts from an applied plan.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Applied {
    pub promoted: usize,
    pub closed: usize,
    pub reopened: usize,
    pub comments: usize,
    /// Pending issues remaining after the plan (a plan need not
    /// dispose the whole pending set).
    pub pending_remaining: usize,
}

#[derive(Debug)]
pub enum ResolveError {
    /// The plan failed validation; nothing was written. The string
    /// names the offending resolution.
    Invalid(String),
    Issue(IssueError),
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResolveError::Invalid(msg) => write!(f, "invalid plan: {msg}"),
            ResolveError::Issue(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for ResolveError {}

impl From<IssueError> for ResolveError {
    fn from(e: IssueError) -> Self {
        ResolveError::Issue(e)
    }
}

impl From<rusqlite::Error> for ResolveError {
    fn from(e: rusqlite::Error) -> Self {
        ResolveError::Issue(IssueError::from(e))
    }
}

/// Validate `resolutions` as a whole, then apply them in one
/// transaction: promotions first, then reopens, closes, and comments.
/// Issue ids are matched as prefixes, as elsewhere.
pub fn apply(
    conn: &Connection,
    resolutions: &[Resolution],
    author: &str,
    timestamp: i64,
) -> Result<Applied, ResolveError> {
    let plan = validate(conn, resolutions)?;

    let tx = conn.unchecked_transaction().map_err(IssueError::from)?;
    let mut applied = Applied::default();
    for p in &plan {
        if let Planned::Open { issue } = p {
            set_status_in_tx(&tx, issue, IssueStatus::Open, None, author, None, timestamp)?;
            applied.promoted += 1;
        }
    }
    // Deduped: two closes against one closed survivor reopen it once
    let mut reopened: Vec<&str> = Vec::new();
    for p in &plan {
        if let Planned::Duplicate {
            of, reopen: true, ..
        } = p
            && !reopened.contains(&of.as_str())
        {
            set_status_in_tx(&tx, of, IssueStatus::Open, None, author, None, timestamp)?;
            reopened.push(of);
        }
    }
    applied.reopened = reopened.len();
    for p in &plan {
        if let Planned::Duplicate { issue, of, .. } = p {
            let message = format!("duplicate of {of}");
            set_status_in_tx(
                &tx,
                issue,
                IssueStatus::Closed,
                Some(StatusReason::Duplicate),
                author,
                Some(&message),
                timestamp,
            )?;
            applied.closed += 1;
        }
    }
    for p in &plan {
        if let Planned::Duplicate {
            of,
            comment: Some(comment),
            ..
        } = p
        {
            comment_in_tx(&tx, of, author, comment, timestamp)?;
            applied.comments += 1;
        }
    }
    tx.commit().map_err(IssueError::from)?;

    applied.pending_remaining = conn.query_row(
        "SELECT count(*) FROM issue WHERE status = 'pending'",
        [],
        |row| row.get(0),
    )?;
    Ok(applied)
}

/// A resolution with its ids resolved to full issue ids.
enum Planned {
    Open {
        issue: String,
    },
    Duplicate {
        issue: String,
        of: String,
        comment: Option<String>,
        reopen: bool,
    },
}

impl Planned {
    fn issue(&self) -> &str {
        match self {
            Planned::Open { issue } | Planned::Duplicate { issue, .. } => issue,
        }
    }

    fn is_open(&self) -> bool {
        matches!(self, Planned::Open { .. })
    }
}

fn validate(conn: &Connection, resolutions: &[Resolution]) -> Result<Vec<Planned>, ResolveError> {
    let mut plan: Vec<Planned> = Vec::new();
    for r in resolutions {
        let subject = subject_issue(conn, r)?;
        if plan.iter().any(|p| p.issue() == subject.id) {
            return Err(ResolveError::Invalid(format!(
                "issue {} appears more than once",
                subject.id
            )));
        }
        plan.push(match r {
            Resolution::Open { .. } => Planned::Open { issue: subject.id },
            Resolution::Duplicate {
                of,
                comment,
                reopen,
                ..
            } => Planned::Duplicate {
                issue: subject.id,
                of: issue::get(conn, of)?.id,
                comment: comment.clone(),
                reopen: *reopen,
            },
        });
    }

    for p in &plan {
        let Planned::Duplicate {
            issue, of, reopen, ..
        } = p
        else {
            continue;
        };
        if of == issue {
            return Err(ResolveError::Invalid(format!(
                "issue {issue} closed as a duplicate of itself"
            )));
        }
        if let Some(target) = plan.iter().find(|t| t.issue() == *of) {
            // The target is itself disposed in this plan: valid only
            // as a promoted survivor. A target closed as a duplicate
            // is a chain (or cycle) the caller must flatten.
            if !target.is_open() {
                return Err(ResolveError::Invalid(format!(
                    "issue {issue} closes against {of}, which itself closes as a \
                     duplicate; flatten the chain to the final target"
                )));
            }
            if *reopen {
                return Err(ResolveError::Invalid(format!(
                    "issue {issue} sets reopen for {of}, which is not closed"
                )));
            }
            continue;
        }
        let target = issue::get(conn, of)?;
        match target.status {
            IssueStatus::Open => {
                if *reopen {
                    return Err(ResolveError::Invalid(format!(
                        "issue {issue} sets reopen for {of}, which is not closed"
                    )));
                }
            }
            IssueStatus::Pending => {
                return Err(ResolveError::Invalid(format!(
                    "issue {issue} closes against pending issue {of}, which this \
                     plan does not promote"
                )));
            }
            IssueStatus::Closed => {}
        }
    }
    Ok(plan)
}

fn subject_issue(conn: &Connection, r: &Resolution) -> Result<Issue, ResolveError> {
    let id = match r {
        Resolution::Open { issue } | Resolution::Duplicate { issue, .. } => issue,
    };
    let subject = issue::get(conn, id)?;
    if subject.status != IssueStatus::Pending {
        return Err(ResolveError::Invalid(format!(
            "issue {} is not pending (status {})",
            subject.id,
            subject.status.as_str()
        )));
    }
    Ok(subject)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_db_in_memory;
    use crate::issue::{IssueStatusFilter, insert};

    fn add(conn: &Connection, id: &str, status: IssueStatus) {
        let issue = Issue {
            id: id.to_string(),
            name: "general".to_string(),
            title: format!("Issue {id}"),
            description: None,
            target: None,
            metadata: None,
            status,
            status_reason: None,
            created: 1_000,
            modified: None,
            author: format!("agent:test?call={id}"),
            scan: None,
        };
        insert(conn, &issue).unwrap();
    }

    fn status_of(conn: &Connection, id: &str) -> (IssueStatus, Option<StatusReason>) {
        let issue = issue::get(conn, id).unwrap();
        (issue.status, issue.status_reason)
    }

    #[test]
    fn plan_applies_promotions_closes_reopens_and_comments() {
        let conn = open_db_in_memory().unwrap();
        add(&conn, "p-novel", IssueStatus::Pending);
        add(&conn, "p-dup", IssueStatus::Pending);
        add(&conn, "p-recur", IssueStatus::Pending);
        add(&conn, "p-left", IssueStatus::Pending);
        add(&conn, "o-live", IssueStatus::Open);
        add(&conn, "c-done", IssueStatus::Closed);

        let applied = apply(
            &conn,
            &[
                Resolution::Open {
                    issue: "p-novel".into(),
                },
                Resolution::Duplicate {
                    issue: "p-dup".into(),
                    of: "o-live".into(),
                    comment: Some("insight from p-dup".into()),
                    reopen: false,
                },
                Resolution::Duplicate {
                    issue: "p-recur".into(),
                    of: "c-done".into(),
                    comment: None,
                    reopen: true,
                },
            ],
            "user:test",
            2_000,
        )
        .unwrap();

        assert_eq!(
            applied,
            Applied {
                promoted: 1,
                closed: 2,
                reopened: 1,
                comments: 1,
                pending_remaining: 1,
            }
        );
        assert_eq!(status_of(&conn, "p-novel"), (IssueStatus::Open, None));
        assert_eq!(
            status_of(&conn, "p-dup"),
            (IssueStatus::Closed, Some(StatusReason::Duplicate))
        );
        assert_eq!(
            status_of(&conn, "p-recur"),
            (IssueStatus::Closed, Some(StatusReason::Duplicate))
        );
        assert_eq!(status_of(&conn, "c-done"), (IssueStatus::Open, None));
        assert_eq!(status_of(&conn, "p-left"), (IssueStatus::Pending, None));
    }

    #[test]
    fn survivor_promoted_in_same_plan_is_a_valid_target() {
        let conn = open_db_in_memory().unwrap();
        add(&conn, "p-a", IssueStatus::Pending);
        add(&conn, "p-b", IssueStatus::Pending);

        let applied = apply(
            &conn,
            &[
                Resolution::Open {
                    issue: "p-a".into(),
                },
                Resolution::Duplicate {
                    issue: "p-b".into(),
                    of: "p-a".into(),
                    comment: None,
                    reopen: false,
                },
            ],
            "user:test",
            2_000,
        )
        .unwrap();
        assert_eq!(applied.promoted, 1);
        assert_eq!(applied.closed, 1);
        assert_eq!(status_of(&conn, "p-a"), (IssueStatus::Open, None));
        assert_eq!(
            status_of(&conn, "p-b"),
            (IssueStatus::Closed, Some(StatusReason::Duplicate))
        );
    }

    #[test]
    fn invalid_plans_write_nothing() {
        let conn = open_db_in_memory().unwrap();
        add(&conn, "p-a", IssueStatus::Pending);
        add(&conn, "p-b", IssueStatus::Pending);
        add(&conn, "p-c", IssueStatus::Pending);
        add(&conn, "o-live", IssueStatus::Open);
        add(&conn, "c-done", IssueStatus::Closed);

        let cases: Vec<(&str, Vec<Resolution>)> = vec![
            (
                "non-pending subject",
                vec![Resolution::Open {
                    issue: "o-live".into(),
                }],
            ),
            (
                "subject twice",
                vec![
                    Resolution::Open {
                        issue: "p-a".into(),
                    },
                    Resolution::Open {
                        issue: "p-a".into(),
                    },
                ],
            ),
            (
                "self duplicate",
                vec![Resolution::Duplicate {
                    issue: "p-a".into(),
                    of: "p-a".into(),
                    comment: None,
                    reopen: false,
                }],
            ),
            (
                "mutual duplicates",
                vec![
                    Resolution::Duplicate {
                        issue: "p-a".into(),
                        of: "p-b".into(),
                        comment: None,
                        reopen: false,
                    },
                    Resolution::Duplicate {
                        issue: "p-b".into(),
                        of: "p-a".into(),
                        comment: None,
                        reopen: false,
                    },
                ],
            ),
            (
                "chain",
                vec![
                    Resolution::Duplicate {
                        issue: "p-a".into(),
                        of: "p-b".into(),
                        comment: None,
                        reopen: false,
                    },
                    Resolution::Duplicate {
                        issue: "p-b".into(),
                        of: "o-live".into(),
                        comment: None,
                        reopen: false,
                    },
                ],
            ),
            (
                "target pending, not promoted",
                vec![Resolution::Duplicate {
                    issue: "p-a".into(),
                    of: "p-c".into(),
                    comment: None,
                    reopen: false,
                }],
            ),
            (
                "reopen on open target",
                vec![Resolution::Duplicate {
                    issue: "p-a".into(),
                    of: "o-live".into(),
                    comment: None,
                    reopen: true,
                }],
            ),
        ];

        for (label, resolutions) in cases {
            // Mix in a valid promotion to prove the whole plan is
            // rejected, not just the offending entry
            let mut plan = vec![Resolution::Open {
                issue: "p-c".into(),
            }];
            plan.extend(resolutions);
            // "target pending, not promoted" uses p-c as its target,
            // so the promotion mix-in would legitimize it
            let plan = if label == "target pending, not promoted" {
                plan.split_off(1)
            } else {
                plan
            };
            let result = apply(&conn, &plan, "user:test", 2_000);
            assert!(
                matches!(result, Err(ResolveError::Invalid(_))),
                "{label}: expected Invalid, got {result:?}"
            );
            let pending = issue::find(
                &conn,
                &crate::issue::IssueFilters {
                    status: IssueStatusFilter::Pending,
                    ..Default::default()
                },
            )
            .unwrap();
            assert_eq!(pending.len(), 3, "{label}: pending set changed");
        }
    }

    #[test]
    fn two_recurrences_reopen_survivor_once() {
        let conn = open_db_in_memory().unwrap();
        add(&conn, "p-a", IssueStatus::Pending);
        add(&conn, "p-b", IssueStatus::Pending);
        add(&conn, "c-done", IssueStatus::Closed);

        let applied = apply(
            &conn,
            &[
                Resolution::Duplicate {
                    issue: "p-a".into(),
                    of: "c-done".into(),
                    comment: None,
                    reopen: true,
                },
                Resolution::Duplicate {
                    issue: "p-b".into(),
                    of: "c-done".into(),
                    comment: None,
                    reopen: true,
                },
            ],
            "user:test",
            2_000,
        )
        .unwrap();
        assert_eq!(applied.reopened, 1);
        let events = issue::issue_events_for(&conn, "c-done").unwrap();
        let reopens = events
            .iter()
            .filter(|e| {
                matches!(
                    e.event,
                    issue::IssueEvent::Status {
                        status: IssueStatus::Open,
                        ..
                    }
                )
            })
            .count();
        assert_eq!(reopens, 1);
    }
}
