//! `related_issues(issue_id)` — issues related to one issue, as
//! `(related_id, score)` rows ordered by descending score.
//!
//! Candidates share the subject's name and at least one session; the
//! score is TF-IDF cosine over title and description
//! ([`gage_db::related`]). The reporting threshold comes from the
//! `issues.related_threshold` config key. Status is not considered —
//! join `issue` to narrow or to fetch titles. `issue_id` is matched as
//! a prefix; an ambiguous prefix is an error and an unknown id returns
//! zero rows.

use std::any::Any;
use std::sync::{Arc, LazyLock};

use async_trait::async_trait;
use datafusion::arrow::array::{Float64Builder, StringBuilder};
use datafusion::arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::catalog::{Session, TableFunctionImpl};
use datafusion::common::ScalarValue;
use datafusion::datasource::{MemTable, TableProvider, TableType};
use datafusion::error::{DataFusionError, Result};
use datafusion::physical_plan::ExecutionPlan;
use datafusion::prelude::Expr;
use gage_core::config::Config;
use gage_db::db::open_db;
use gage_db::issue::IssueError;
use gage_db::related::related_issues;

pub const RELATED_ISSUES_ARGS: &str = "issue_id text";

pub fn related_issues_schema() -> SchemaRef {
    SCHEMA.clone()
}

static SCHEMA: LazyLock<SchemaRef> = LazyLock::new(|| {
    Arc::new(Schema::new(vec![
        Field::new("related_id", DataType::Utf8, false),
        Field::new("score", DataType::Float64, false),
    ]))
});

#[derive(Debug, Default)]
pub struct RelatedIssuesFn;

impl RelatedIssuesFn {
    pub fn new() -> Self {
        Self
    }
}

impl TableFunctionImpl for RelatedIssuesFn {
    fn call(&self, args: &[Expr]) -> Result<Arc<dyn TableProvider>> {
        let [arg] = args else {
            return Err(DataFusionError::Plan(
                "related_issues(issue_id) takes exactly one argument".into(),
            ));
        };
        let issue_id = string_literal(arg).ok_or_else(|| {
            DataFusionError::Plan("related_issues issue_id must be a string literal".into())
        })?;
        Ok(Arc::new(RelatedIssuesTable { issue_id }))
    }
}

fn string_literal(e: &Expr) -> Option<String> {
    match e {
        Expr::Literal(ScalarValue::Utf8(Some(s)), _)
        | Expr::Literal(ScalarValue::LargeUtf8(Some(s)), _)
        | Expr::Literal(ScalarValue::Utf8View(Some(s)), _) => Some(s.clone()),
        _ => None,
    }
}

#[derive(Debug)]
struct RelatedIssuesTable {
    issue_id: String,
}

#[async_trait]
impl TableProvider for RelatedIssuesTable {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn schema(&self) -> SchemaRef {
        SCHEMA.clone()
    }

    fn table_type(&self) -> TableType {
        TableType::Base
    }

    async fn scan(
        &self,
        state: &dyn Session,
        projection: Option<&Vec<usize>>,
        _filters: &[Expr],
        _limit: Option<usize>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        let batch = build_rows(&self.issue_id)?;
        let mem = MemTable::try_new(SCHEMA.clone(), vec![vec![batch]])?;
        mem.scan(state, projection, &[], None).await
    }
}

fn build_rows(issue_id: &str) -> Result<RecordBatch> {
    let conn = open_db().map_err(|e| DataFusionError::Execution(format!("open gage db: {e}")))?;
    let config = Config::load_user()
        .map_err(|e| DataFusionError::Execution(format!("load gage config: {e}")))?;
    let related = match related_issues(&conn, issue_id, config.issues.related_threshold) {
        Ok(r) => r,
        Err(IssueError::NotFound(_)) => return Ok(RecordBatch::new_empty(SCHEMA.clone())),
        Err(e @ IssueError::Ambiguous(_, _)) => {
            return Err(DataFusionError::Plan(e.to_string()));
        }
        Err(e) => return Err(DataFusionError::Execution(e.to_string())),
    };

    let mut ids = StringBuilder::new();
    let mut scores = Float64Builder::new();
    for r in &related {
        ids.append_value(&r.id);
        scores.append_value(r.score);
    }
    Ok(RecordBatch::try_new(
        SCHEMA.clone(),
        vec![Arc::new(ids.finish()), Arc::new(scores.finish())],
    )?)
}
