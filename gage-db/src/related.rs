//! Related-issue retrieval. Two issues are related when they share a
//! name, share at least one session, and their text similarity meets a
//! threshold. This is the single implementation behind the
//! `related_issues(issue_id)` table valued function, the `related`
//! attribute of `gage issue show`, and pending issue resolution.
//!
//! Similarity is TF-IDF weighted cosine over title and description.
//! IDF is computed over the whole issue docket so vocabulary shared by
//! most issues (prompt scaffolding) carries little weight and scores
//! concentrate on the terms that distinguish one condition from
//! another. Status is never considered; callers narrow by status as
//! needed.

use std::collections::HashMap;

use rusqlite::{Connection, params};
use tracing::{Level, debug, enabled};

use crate::issue::{self, IssueError};

/// A related issue and its similarity score.
#[derive(Debug, Clone)]
pub struct RelatedIssue {
    pub id: String,
    pub score: f64,
}

/// Issues related to `issue_id` (or prefix), ordered by descending
/// score. Candidates share the subject's `name` and at least one
/// session per `session_issue`; a candidate is related when its TF-IDF
/// cosine score is at or above `threshold`.
pub fn related_issues(
    conn: &Connection,
    issue_id: &str,
    threshold: f64,
) -> Result<Vec<RelatedIssue>, IssueError> {
    let subject = issue::get(conn, issue_id)?;

    let mut stmt = conn.prepare(
        "SELECT DISTINCT i.id FROM issue i
         JOIN session_issue si ON si.issue_id = i.id
         WHERE i.name = ?1 AND i.id <> ?2
           AND si.session_id IN
               (SELECT session_id FROM session_issue WHERE issue_id = ?2)
         ORDER BY i.id",
    )?;
    let candidates: Vec<String> = stmt
        .query_map(params![subject.name, subject.id], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    if enabled!(Level::DEBUG) {
        let name_gate: i64 = conn.query_row(
            "SELECT count(*) FROM issue WHERE name = ?1 AND id <> ?2",
            params![subject.name, subject.id],
            |row| row.get(0),
        )?;
        debug!(
            issue = %subject.id,
            name_gate,
            session_gate = candidates.len(),
            threshold,
            "related_issues candidates"
        );
    }
    if candidates.is_empty() {
        return Ok(Vec::new());
    }

    let corpus = Corpus::load(conn)?;
    let subject_vec = corpus.vector(&subject.id);

    let mut related = Vec::new();
    for id in candidates {
        let (score, top_terms) = cosine(&subject_vec, &corpus.vector(&id));
        debug!(
            issue = %subject.id,
            related = %id,
            score,
            terms = ?top_terms,
            "related_issues pair"
        );
        if score >= threshold {
            related.push(RelatedIssue { id, score });
        }
    }
    related.sort_by(|a, b| b.score.total_cmp(&a.score));
    Ok(related)
}

/// TF-IDF term statistics over the issue docket: per-issue term
/// frequencies and docket-wide inverse document frequencies.
struct Corpus {
    /// issue id -> term -> term count
    tf: HashMap<String, HashMap<String, f64>>,
    /// term -> ln(docket size / documents containing term)
    idf: HashMap<String, f64>,
}

impl Corpus {
    fn load(conn: &Connection) -> Result<Self, IssueError> {
        let mut stmt = conn.prepare("SELECT id, title, description FROM issue")?;
        let docs: Vec<(String, String, Option<String>)> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect::<Result<_, _>>()?;

        let mut tf: HashMap<String, HashMap<String, f64>> = HashMap::new();
        let mut df: HashMap<String, usize> = HashMap::new();
        let n = docs.len() as f64;
        for (id, title, description) in docs {
            let text = format!("{title} {}", description.unwrap_or_default());
            let mut counts: HashMap<String, f64> = HashMap::new();
            for token in tokens(&text) {
                *counts.entry(token).or_insert(0.0) += 1.0;
            }
            for term in counts.keys() {
                *df.entry(term.clone()).or_insert(0) += 1;
            }
            tf.insert(id, counts);
        }
        let idf = df
            .into_iter()
            .map(|(term, count)| (term, (n / count as f64).ln()))
            .collect();
        Ok(Self { tf, idf })
    }

    /// TF-IDF weighted term vector for an issue. Empty when the issue
    /// has no text (or is unknown, which callers preclude).
    fn vector(&self, id: &str) -> HashMap<&str, f64> {
        let Some(counts) = self.tf.get(id) else {
            return HashMap::new();
        };
        counts
            .iter()
            .map(|(term, count)| {
                let idf = self.idf.get(term).copied().unwrap_or(0.0);
                (term.as_str(), count * idf)
            })
            .collect()
    }
}

/// Lowercased alphanumeric word tokens.
fn tokens(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(str::to_lowercase)
}

/// Cosine similarity between two weighted vectors, with the top terms
/// contributing to the score (for debug tracing). Zero when either
/// vector has no weight.
fn cosine(a: &HashMap<&str, f64>, b: &HashMap<&str, f64>) -> (f64, Vec<(String, f64)>) {
    let norm = |v: &HashMap<&str, f64>| v.values().map(|w| w * w).sum::<f64>().sqrt();
    let (na, nb) = (norm(a), norm(b));
    if na == 0.0 || nb == 0.0 {
        return (0.0, Vec::new());
    }
    let mut contributions: Vec<(String, f64)> = a
        .iter()
        .filter_map(|(term, wa)| b.get(term).map(|wb| (term.to_string(), wa * wb)))
        .collect();
    let dot: f64 = contributions.iter().map(|(_, c)| c).sum();
    contributions.sort_by(|x, y| y.1.total_cmp(&x.1));
    contributions.truncate(5);
    (dot / (na * nb), contributions)
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::db::open_db_in_memory;
    use crate::issue::{Issue, IssueStatus, insert, insert_session_issue};

    fn add_issue(conn: &Connection, id: &str, title: &str, sessions: &[&str]) {
        add_named_issue(conn, id, "general", title, sessions);
    }

    fn add_named_issue(conn: &Connection, id: &str, name: &str, title: &str, sessions: &[&str]) {
        let issue = Issue {
            id: id.to_string(),
            name: name.to_string(),
            title: title.to_string(),
            description: None,
            status: IssueStatus::Pending,
            status_reason: None,
            created: 1_000,
            modified: None,
            // Unique author per issue so the (name, author) dup key
            // never fires across test fixtures
            author: format!("agent:test?call={id}"),
        };
        insert(conn, &issue).unwrap();
        for s in sessions {
            insert_session_issue(conn, s, id).unwrap();
        }
    }

    #[test]
    fn duplicate_pair_scores_above_unrelated() {
        let conn = open_db_in_memory().unwrap();
        add_issue(
            &conn,
            "i-a",
            "Retry loop never terminates in fetch",
            &["s1"],
        );
        add_issue(&conn, "i-b", "Fetch retry loop runs forever", &["s1"]);
        add_issue(&conn, "i-c", "Missing test coverage for parser", &["s1"]);

        let related = related_issues(&conn, "i-a", 0.0).unwrap();
        assert_eq!(related.len(), 2);
        assert_eq!(related[0].id, "i-b", "duplicate should rank first");
        assert!(related[0].score > related[1].score);
    }

    #[test]
    fn threshold_excludes_low_scores() {
        let conn = open_db_in_memory().unwrap();
        add_issue(
            &conn,
            "i-a",
            "Retry loop never terminates in fetch",
            &["s1"],
        );
        add_issue(&conn, "i-b", "Fetch retry loop runs forever", &["s1"]);
        add_issue(&conn, "i-c", "Missing test coverage for parser", &["s1"]);

        let all = related_issues(&conn, "i-a", 0.0).unwrap();
        let unrelated_score = all.iter().find(|r| r.id == "i-c").unwrap().score;
        let related = related_issues(&conn, "i-a", unrelated_score + 0.01).unwrap();
        assert!(related.iter().all(|r| r.id != "i-c"));
        assert!(related.iter().any(|r| r.id == "i-b"));
    }

    #[test]
    fn name_and_session_gates_exclude() {
        let conn = open_db_in_memory().unwrap();
        add_issue(
            &conn,
            "i-a",
            "Retry loop never terminates in fetch",
            &["s1"],
        );
        // Same text, different name
        add_named_issue(
            &conn,
            "i-b",
            "code-review",
            "Retry loop never terminates in fetch",
            &["s1"],
        );
        // Same text, no shared session
        add_issue(
            &conn,
            "i-c",
            "Retry loop never terminates in fetch",
            &["s2"],
        );
        // Same text, no session at all
        add_issue(&conn, "i-d", "Retry loop never terminates in fetch", &[]);

        let related = related_issues(&conn, "i-a", 0.0).unwrap();
        assert!(related.is_empty(), "got {related:?}");
    }

    #[test]
    fn status_is_ignored() {
        let conn = open_db_in_memory().unwrap();
        add_issue(
            &conn,
            "i-a",
            "Retry loop never terminates in fetch",
            &["s1"],
        );
        add_issue(&conn, "i-b", "Fetch retry loop runs forever", &["s1"]);
        crate::issue::close(
            &conn,
            "i-b",
            crate::issue::StatusReason::Completed,
            "user:test",
            None,
            2_000,
        )
        .unwrap();

        let related = related_issues(&conn, "i-a", 0.0).unwrap();
        assert_eq!(related.len(), 1);
        assert_eq!(related[0].id, "i-b");
    }

    #[test]
    fn unknown_issue_errors() {
        let conn = open_db_in_memory().unwrap();
        assert!(matches!(
            related_issues(&conn, "nope", 0.0),
            Err(IssueError::NotFound(_))
        ));
    }
}
