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
use gage_db::note::{self, NoteFilters};

fn note_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("id", DataType::Utf8, false),
        Field::new("author", DataType::Utf8, false),
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
        Field::new("target", DataType::Utf8, false),
        Field::new("name", DataType::Utf8, false),
        Field::new("value", DataType::Utf8, false),
        Field::new("metadata", DataType::Utf8, true),
        Field::new("explanation", DataType::Utf8, true),
    ]))
}

#[derive(Debug, Clone)]
pub struct NoteTable {
    schema: SchemaRef,
}

impl NoteTable {
    pub fn new() -> Self {
        Self {
            schema: note_schema(),
        }
    }
}

impl Default for NoteTable {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TableProvider for NoteTable {
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
        Ok(Arc::new(NoteExec::new(
            self.schema.clone(),
            projected_schema,
            projection.cloned(),
        )))
    }
}

#[derive(Debug, Clone)]
struct NoteExec {
    full_schema: SchemaRef,
    projected_schema: SchemaRef,
    projection: Option<Vec<usize>>,
    properties: PlanProperties,
}

impl NoteExec {
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

impl DisplayAs for NoteExec {
    fn fmt_as(&self, _t: DisplayFormatType, f: &mut Formatter) -> fmt::Result {
        write!(f, "NoteExec")
    }
}

fn append_opt_str(builder: &mut StringBuilder, value: &Option<String>) {
    match value {
        Some(v) => builder.append_value(v),
        None => builder.append_null(),
    }
}

impl ExecutionPlan for NoteExec {
    fn name(&self) -> &'static str {
        "NoteExec"
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
        let notes = note::find_raw(&conn, &NoteFilters::default())
            .map_err(|e| datafusion::error::DataFusionError::External(Box::new(e)))?;

        let len = notes.len();
        let mut ids = StringBuilder::with_capacity(len, len * 36);
        let mut authors = StringBuilder::with_capacity(len, len * 16);
        let mut createds = TimestampMillisecondBuilder::with_capacity(len);
        let mut modifieds = TimestampMillisecondBuilder::with_capacity(len);
        let mut targets = StringBuilder::with_capacity(len, len * 64);
        let mut names = StringBuilder::with_capacity(len, len * 16);
        let mut values = StringBuilder::with_capacity(len, len * 64);
        let mut explanations = StringBuilder::with_capacity(len, len * 64);
        let mut metadatas = StringBuilder::with_capacity(len, len * 32);

        for n in &notes {
            ids.append_value(&n.id);
            authors.append_value(&n.author);
            createds.append_value(n.created);
            modifieds.append_option(n.modified);
            targets.append_value(n.target.to_uri());
            names.append_value(&n.name);
            values.append_value(&n.value);
            append_opt_str(&mut explanations, &n.explanation);
            append_opt_str(&mut metadatas, &n.metadata);
        }

        let batch = RecordBatch::try_new(
            self.full_schema.clone(),
            vec![
                Arc::new(ids.finish()),
                Arc::new(authors.finish()),
                Arc::new(createds.finish().with_timezone("UTC")),
                Arc::new(modifieds.finish().with_timezone("UTC")),
                Arc::new(targets.finish()),
                Arc::new(names.finish()),
                Arc::new(values.finish()),
                Arc::new(metadatas.finish()),
                Arc::new(explanations.finish()),
            ],
        )?;

        Ok(Box::pin(MemoryStream::try_new(
            vec![batch],
            self.projected_schema.clone(),
            self.projection.clone(),
        )?))
    }
}
