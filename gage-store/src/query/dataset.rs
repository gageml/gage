//! The `dataset` table: one row per live dataset object, from the
//! index alone. Membership is the `dataset_session_link` table.

use std::sync::{Arc, Mutex};

use datafusion::arrow::array::{StringBuilder, TimestampMillisecondBuilder};
use datafusion::arrow::datatypes::{DataType, Field, Schema, SchemaRef, TimeUnit};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::datasource::TableProvider;
use datafusion::error::Result;
use gage_core::uuid::short_uuid;

use super::batch::{BatchSource, BatchTable, external, marker, unique_prefix_lens};
use crate::{DATASET_TYPE, DatasetStore, Order, Store};

fn schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("id", DataType::Utf8, false),
        // The commit's `modified` marker
        Field::new(
            "modified",
            DataType::Timestamp(TimeUnit::Millisecond, Some("UTC".into())),
            false,
        ),
        Field::new(
            "created",
            DataType::Timestamp(TimeUnit::Millisecond, Some("UTC".into())),
            false,
        ),
        // System
        Field::new("id_display", DataType::Utf8, false),
        Field::new("id_prefix", DataType::Utf8, false),
        Field::new("locator", DataType::Utf8, false),
        // The current commit, which `dataset_session_link` keys on
        Field::new("commit", DataType::Utf8, false),
    ]))
}

#[derive(Debug)]
struct Source;

impl BatchSource for Source {
    fn schema(&self) -> SchemaRef {
        schema()
    }

    fn build(&self, store: &Store) -> Result<RecordBatch> {
        let tips = DatasetStore::from(store)
            .query()
            .order(Order::ModifiedDesc)
            .tips()
            .map_err(external)?;
        let prefix_len = unique_prefix_lens(
            store
                .short_prefix_ids(Some(DATASET_TYPE))
                .map_err(external)?,
        );
        let len = tips.len();
        let mut ids = StringBuilder::with_capacity(len, len * 26);
        let mut modifieds = TimestampMillisecondBuilder::with_capacity(len);
        let mut createds = TimestampMillisecondBuilder::with_capacity(len);
        let mut id_displays = StringBuilder::with_capacity(len, len * 8);
        let mut id_prefixes = StringBuilder::with_capacity(len, len * 4);
        let mut locators = StringBuilder::with_capacity(len, len * 44);
        let mut commits = StringBuilder::with_capacity(len, len * 40);
        for tip in &tips {
            ids.append_value(&tip.id);
            modifieds.append_value(marker(&tip.id, "modified", tip.modified_ms)?);
            createds.append_value(marker(&tip.id, "created", tip.created_ms)?);
            id_displays.append_value(short_uuid(&tip.id));
            let n = prefix_len.get(&tip.id).copied().unwrap_or(tip.id.len());
            id_prefixes.append_value(tip.id.chars().take(n).collect::<String>());
            locators.append_value(format!("git:{}", tip.sha));
            commits.append_value(&tip.sha);
        }
        Ok(RecordBatch::try_new(
            schema(),
            vec![
                Arc::new(ids.finish()),
                Arc::new(modifieds.finish().with_timezone("UTC")),
                Arc::new(createds.finish().with_timezone("UTC")),
                Arc::new(id_displays.finish()),
                Arc::new(id_prefixes.finish()),
                Arc::new(locators.finish()),
                Arc::new(commits.finish()),
            ],
        )?)
    }
}

/// The store-bound `dataset` table.
pub fn dataset_table(store: Arc<Mutex<Store>>) -> Arc<dyn TableProvider> {
    Arc::new(BatchTable::new(store, Arc::new(Source)))
}
