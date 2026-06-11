use std::any::Any;
use std::fmt::{self, Formatter};
use std::sync::Arc;

use async_trait::async_trait;
use datafusion::arrow::array::{Int64Builder, StringBuilder};
use datafusion::arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::catalog::Session;
use datafusion::datasource::{TableProvider, TableType};
use datafusion::error::Result;
use datafusion::execution::context::TaskContext;
use datafusion::physical_expr::EquivalenceProperties;
use datafusion::physical_plan::execution_plan::{Boundedness, EmissionType};
use datafusion::physical_plan::memory::MemoryStream;
use datafusion::physical_plan::{
    DisplayAs, DisplayFormatType, ExecutionPlan, Partitioning, PlanProperties,
    SendableRecordBatchStream,
};
use datafusion::prelude::*;
use gage_db::issue;

fn issue_evidence_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("issue_id", DataType::Utf8, false),
        Field::new("note_id", DataType::Utf8, false),
        Field::new("name", DataType::Utf8, false),
        Field::new("timestamp", DataType::Int64, false),
        Field::new("digest", DataType::Utf8, true),
    ]))
}

#[derive(Debug, Clone)]
pub struct IssueEvidenceTable {
    schema: SchemaRef,
}

impl IssueEvidenceTable {
    pub fn new() -> Self {
        Self {
            schema: issue_evidence_schema(),
        }
    }
}

impl Default for IssueEvidenceTable {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TableProvider for IssueEvidenceTable {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }

    fn table_type(&self) -> TableType {
        TableType::Base
    }

    async fn scan(
        &self,
        _state: &dyn Session,
        projection: Option<&Vec<usize>>,
        _filters: &[Expr],
        _limit: Option<usize>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        let projected_schema = match projection {
            Some(indices) => Arc::new(self.schema.project(indices)?),
            None => self.schema.clone(),
        };
        Ok(Arc::new(IssueEvidenceExec::new(
            self.schema.clone(),
            projected_schema,
            projection.cloned(),
        )))
    }
}

#[derive(Debug, Clone)]
struct IssueEvidenceExec {
    full_schema: SchemaRef,
    projected_schema: SchemaRef,
    projection: Option<Vec<usize>>,
    properties: PlanProperties,
}

impl IssueEvidenceExec {
    fn new(
        full_schema: SchemaRef,
        projected_schema: SchemaRef,
        projection: Option<Vec<usize>>,
    ) -> Self {
        let properties = PlanProperties::new(
            EquivalenceProperties::new(projected_schema.clone()),
            Partitioning::UnknownPartitioning(1),
            EmissionType::Incremental,
            Boundedness::Bounded,
        );
        Self {
            full_schema,
            projected_schema,
            projection,
            properties,
        }
    }
}

impl DisplayAs for IssueEvidenceExec {
    fn fmt_as(&self, _t: DisplayFormatType, f: &mut Formatter) -> fmt::Result {
        write!(f, "IssueEvidenceExec")
    }
}

impl ExecutionPlan for IssueEvidenceExec {
    fn name(&self) -> &'static str {
        "IssueEvidenceExec"
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn properties(&self) -> &PlanProperties {
        &self.properties
    }

    fn children(&self) -> Vec<&Arc<dyn ExecutionPlan>> {
        vec![]
    }

    fn with_new_children(
        self: Arc<Self>,
        _children: Vec<Arc<dyn ExecutionPlan>>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        Ok(self)
    }

    fn execute(
        &self,
        _partition: usize,
        _context: Arc<TaskContext>,
    ) -> Result<SendableRecordBatchStream> {
        let conn = gage_db::db::open_db().unwrap();
        let rows = issue::list_issue_evidence(&conn)
            .map_err(|e| datafusion::error::DataFusionError::External(Box::new(e)))?;

        let len = rows.len();
        let mut issue_ids = StringBuilder::with_capacity(len, len * 36);
        let mut note_ids = StringBuilder::with_capacity(len, len * 36);
        let mut names = StringBuilder::with_capacity(len, len * 16);
        let mut timestamps = Int64Builder::with_capacity(len);
        let mut digests = StringBuilder::with_capacity(len, len * 16);

        for ev in &rows {
            issue_ids.append_value(&ev.issue_id);
            note_ids.append_value(&ev.note_id);
            names.append_value(&ev.name);
            timestamps.append_value(ev.timestamp);
            digests.append_option(ev.digest.as_deref());
        }

        let batch = RecordBatch::try_new(
            self.full_schema.clone(),
            vec![
                Arc::new(issue_ids.finish()),
                Arc::new(note_ids.finish()),
                Arc::new(names.finish()),
                Arc::new(timestamps.finish()),
                Arc::new(digests.finish()),
            ],
        )?;

        Ok(Box::pin(MemoryStream::try_new(
            vec![batch],
            self.projected_schema.clone(),
            self.projection.clone(),
        )?))
    }
}
