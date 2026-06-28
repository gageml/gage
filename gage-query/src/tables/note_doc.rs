//! `note_doc` table — one row per `notes.writes` declaration across all
//! registered scanners. Surfaces scanner-declared note documentation
//! through SQL.

use std::sync::Arc;

use datafusion::arrow::array::StringBuilder;
use datafusion::arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::datasource::MemTable;
use datafusion::error::Result;
use gage_registry::scanner::ScannerRegistry;

fn schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("note_name", DataType::Utf8, false),
        Field::new("doc", DataType::Utf8, false),
        Field::new("written_by", DataType::Utf8, false),
    ]))
}

pub fn note_doc_table() -> Result<Arc<MemTable>> {
    let registry = ScannerRegistry::load();
    let schema = schema();
    let batch = build_batch(&schema, &registry)?;
    Ok(Arc::new(MemTable::try_new(schema, vec![vec![batch]])?))
}

fn build_batch(schema: &SchemaRef, registry: &ScannerRegistry) -> Result<RecordBatch> {
    let mut note_names = StringBuilder::new();
    let mut docs = StringBuilder::new();
    let mut written_by = StringBuilder::new();

    for def in registry.list() {
        for (task_name, task) in &def.tasks {
            for (note_name, doc) in &task.notes.writes {
                note_names.append_value(note_name);
                docs.append_value(doc);
                written_by.append_value(format!("{}::{}", def.name, task_name));
            }
        }
    }

    Ok(RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(note_names.finish()),
            Arc::new(docs.finish()),
            Arc::new(written_by.finish()),
        ],
    )?)
}
