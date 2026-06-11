use std::any::Any;
use std::fmt::{self, Formatter};
use std::sync::Arc;

use async_trait::async_trait;
use datafusion::arrow::array::{StringBuilder, TimestampMillisecondBuilder};
use datafusion::arrow::datatypes::{DataType, Field, Schema, SchemaRef, TimeUnit};
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
use gage_db::issue::{self, IssueFilters, IssueStatusFilter};

fn issue_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("id", DataType::Utf8, false),
        Field::new("name", DataType::Utf8, false),
        Field::new("title", DataType::Utf8, false),
        Field::new("description", DataType::Utf8, true),
        Field::new("status", DataType::Utf8, false),
        Field::new("closed_reason", DataType::Utf8, true),
        Field::new(
            "created",
            DataType::Timestamp(TimeUnit::Millisecond, Some("UTC".into())),
            false,
        ),
        Field::new(
            "modified",
            DataType::Timestamp(TimeUnit::Millisecond, Some("UTC".into())),
            true,
        ),
        Field::new("author", DataType::Utf8, false),
    ]))
}

#[derive(Debug, Clone)]
pub struct IssueTable {
    schema: SchemaRef,
}

impl IssueTable {
    pub fn new() -> Self {
        Self {
            schema: issue_schema(),
        }
    }
}

impl Default for IssueTable {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TableProvider for IssueTable {
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
        Ok(Arc::new(IssueExec::new(
            self.schema.clone(),
            projected_schema,
            projection.cloned(),
        )))
    }
}

#[derive(Debug, Clone)]
struct IssueExec {
    full_schema: SchemaRef,
    projected_schema: SchemaRef,
    projection: Option<Vec<usize>>,
    properties: PlanProperties,
}

impl IssueExec {
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

impl DisplayAs for IssueExec {
    fn fmt_as(&self, _t: DisplayFormatType, f: &mut Formatter) -> fmt::Result {
        write!(f, "IssueExec")
    }
}

fn append_opt_str(builder: &mut StringBuilder, value: &Option<String>) {
    match value {
        Some(v) => builder.append_value(v),
        None => builder.append_null(),
    }
}

impl ExecutionPlan for IssueExec {
    fn name(&self) -> &'static str {
        "IssueExec"
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
        let issues = issue::find(
            &conn,
            &IssueFilters {
                status: IssueStatusFilter::Any,
                ..Default::default()
            },
        )
        .map_err(|e| datafusion::error::DataFusionError::External(Box::new(e)))?;

        let len = issues.len();
        let mut ids = StringBuilder::with_capacity(len, len * 36);
        let mut names = StringBuilder::with_capacity(len, len * 16);
        let mut titles = StringBuilder::with_capacity(len, len * 64);
        let mut descriptions = StringBuilder::with_capacity(len, len * 64);
        let mut statuses = StringBuilder::with_capacity(len, len * 8);
        let mut closed_reasons = StringBuilder::with_capacity(len, len * 10);
        let mut createds = TimestampMillisecondBuilder::with_capacity(len);
        let mut modifieds = TimestampMillisecondBuilder::with_capacity(len);
        let mut authors = StringBuilder::with_capacity(len, len * 16);

        for i in &issues {
            ids.append_value(&i.id);
            names.append_value(&i.name);
            titles.append_value(&i.title);
            append_opt_str(&mut descriptions, &i.description);
            statuses.append_value(i.status.as_str());
            match i.closed_reason {
                Some(r) => closed_reasons.append_value(r.as_str()),
                None => closed_reasons.append_null(),
            }
            createds.append_value(i.created);
            modifieds.append_option(i.modified);
            authors.append_value(&i.author);
        }

        let batch = RecordBatch::try_new(
            self.full_schema.clone(),
            vec![
                Arc::new(ids.finish()),
                Arc::new(names.finish()),
                Arc::new(titles.finish()),
                Arc::new(descriptions.finish()),
                Arc::new(statuses.finish()),
                Arc::new(closed_reasons.finish()),
                Arc::new(createds.finish().with_timezone("UTC")),
                Arc::new(modifieds.finish().with_timezone("UTC")),
                Arc::new(authors.finish()),
            ],
        )?;

        Ok(Box::pin(MemoryStream::try_new(
            vec![batch],
            self.projected_schema.clone(),
            self.projection.clone(),
        )?))
    }
}
