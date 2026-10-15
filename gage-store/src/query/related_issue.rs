//! `related_issue(issue_id [, min_score])`: the issues similar enough
//! to one issue to be duplicate candidates, as `(related_id, score)`
//! rows, best first. `min_score` is the lowest score returned (default
//! `MIN_SCORE`). The retrieval stage of duplicate detection: a reader
//! judges the candidates it returns.
//!
//! The score is TF-IDF weighted cosine similarity over title and
//! description. IDF is computed over every live issue, so vocabulary
//! most issues share, such as prompt scaffolding, carries little
//! weight and the score concentrates on the terms that distinguish
//! one condition from another. An issue whose `name` differs from the
//! subject's keeps 80% of its score: two scanners can report one
//! condition, so the name is a signal and not a gate. Status is not
//! considered; join `issue` to narrow or to read titles. The subject
//! is never a row, and nor is an issue scoring under `min_score`. An
//! unknown id gives zero rows; an ambiguous prefix is an error.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use super::batch::{BatchSource, BatchTable, external, f64_literal, readable, string_literal};
use crate::{IssueFull, IssueStore, Store, StoreError};
use datafusion::arrow::array::{Float64Builder, StringBuilder};
use datafusion::arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::catalog::TableFunctionImpl;
use datafusion::datasource::TableProvider;
use datafusion::error::{DataFusionError, Result};
use datafusion::prelude::Expr;

/// The lowest score returned when the call names none. Calibrated on
/// a docket of 20 issues over two resolve sessions: every duplicate
/// scored 0.19 or higher against its survivor, and every novel issue's
/// best candidate scored below 0.16.
const MIN_SCORE: f64 = 0.15;
const OTHER_NAME_WEIGHT: f64 = 0.8;

/// The store-bound `related_issue` table function.
pub fn related_issue_fn(store: Arc<Mutex<Store>>) -> Arc<dyn TableFunctionImpl> {
    Arc::new(RelatedIssueFn { store })
}

fn schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("related_id", DataType::Utf8, false),
        Field::new("score", DataType::Float64, false),
    ]))
}

#[derive(Debug)]
struct RelatedIssueFn {
    store: Arc<Mutex<Store>>,
}

impl TableFunctionImpl for RelatedIssueFn {
    fn call(&self, args: &[Expr]) -> Result<Arc<dyn TableProvider>> {
        let (id_arg, min_score) = match args {
            [id] => (id, MIN_SCORE),
            [id, min_score] => {
                let min_score = f64_literal(min_score).ok_or_else(|| {
                    DataFusionError::Plan(
                        "related_issue: min_score must be a numeric literal".into(),
                    )
                })?;
                (id, min_score)
            }
            _ => {
                return Err(DataFusionError::Plan(
                    "related_issue(issue_id [, min_score]) takes one or two arguments".into(),
                ));
            }
        };
        let issue_id = string_literal(id_arg).ok_or_else(|| {
            DataFusionError::Plan("related_issue: issue_id must be a string literal".into())
        })?;
        Ok(Arc::new(BatchTable::new(
            Arc::clone(&self.store),
            Arc::new(RelatedIssueSource {
                issue_id,
                min_score,
            }),
        )))
    }
}

#[derive(Debug)]
struct RelatedIssueSource {
    issue_id: String,
    min_score: f64,
}

impl BatchSource for RelatedIssueSource {
    fn schema(&self) -> SchemaRef {
        schema()
    }

    fn build(&self, store: &Store) -> Result<RecordBatch> {
        let issues = IssueStore::from(store);
        let subject = match issues.get(&self.issue_id) {
            Ok(issue) => issue,
            Err(StoreError::ObjectNotFound(_)) => return Ok(RecordBatch::new_empty(schema())),
            Err(e) => return Err(external(e)),
        };
        let mut docket = Vec::new();
        for tip in issues.query().tips().map_err(external)? {
            if let Some(issue) = readable(issues.at_commit(&tip.sha))? {
                docket.push(issue);
            }
        }
        let related = related(&subject, &docket, self.min_score);
        let mut ids = StringBuilder::new();
        let mut scores = Float64Builder::new();
        for (id, score) in related {
            ids.append_value(id);
            scores.append_value(score);
        }
        Ok(RecordBatch::try_new(
            schema(),
            vec![Arc::new(ids.finish()), Arc::new(scores.finish())],
        )?)
    }
}

/// The issues of `docket` scoring at least `min_score` against
/// `subject`, best first, excluding the subject.
fn related<'a>(
    subject: &IssueFull,
    docket: &'a [IssueFull],
    min_score: f64,
) -> Vec<(&'a str, f64)> {
    let corpus = Corpus::new(docket.iter().chain(std::iter::once(subject)));
    let subject_vec = corpus.vector(&subject.id);
    let mut scored: Vec<(&str, f64)> = docket
        .iter()
        .filter(|issue| issue.id != subject.id)
        .filter_map(|issue| {
            let mut score = cosine(&subject_vec, &corpus.vector(&issue.id));
            if issue.name != subject.name {
                score *= OTHER_NAME_WEIGHT;
            }
            (score >= min_score).then_some((issue.id.as_str(), score))
        })
        .collect();
    scored.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    scored
}

/// TF-IDF term statistics over the docket: per-issue term counts and
/// docket-wide inverse document frequencies.
struct Corpus {
    tf: HashMap<String, HashMap<String, f64>>,
    idf: HashMap<String, f64>,
}

impl Corpus {
    /// Build the statistics over `issues`. An issue given twice
    /// counts once.
    fn new<'a>(issues: impl Iterator<Item = &'a IssueFull>) -> Self {
        let mut tf: HashMap<String, HashMap<String, f64>> = HashMap::new();
        let mut df: HashMap<String, usize> = HashMap::new();
        for issue in issues {
            if tf.contains_key(&issue.id) {
                continue;
            }
            let text = format!(
                "{} {}",
                issue.title,
                issue.description.as_deref().unwrap_or_default()
            );
            let mut counts: HashMap<String, f64> = HashMap::new();
            for token in tokens(&text) {
                *counts.entry(token).or_insert(0.0) += 1.0;
            }
            for term in counts.keys() {
                *df.entry(term.clone()).or_insert(0) += 1;
            }
            tf.insert(issue.id.clone(), counts);
        }
        let n = tf.len() as f64;
        let idf = df
            .into_iter()
            .map(|(term, count)| (term, (n / count as f64).ln()))
            .collect();
        Corpus { tf, idf }
    }

    /// The TF-IDF weighted term vector of an issue; empty for an
    /// issue with no text.
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

/// Lowercased alphanumeric word tokens
fn tokens(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(str::to_lowercase)
}

/// Cosine similarity of two weighted vectors; zero when either has
/// no weight
fn cosine(a: &HashMap<&str, f64>, b: &HashMap<&str, f64>) -> f64 {
    let norm = |v: &HashMap<&str, f64>| v.values().map(|w| w * w).sum::<f64>().sqrt();
    let (na, nb) = (norm(a), norm(b));
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    let dot: f64 = a
        .iter()
        .filter_map(|(term, wa)| b.get(term).map(|wb| wa * wb))
        .sum();
    dot / (na * nb)
}
