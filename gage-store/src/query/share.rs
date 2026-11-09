//! The `share` table: one row per live share object. The tips come
//! from the index; `description` and `filters` are read from each
//! tip's `attrs.json`. Membership is the `share_session_link` table.

use std::sync::{Arc, Mutex};

use datafusion::arrow::array::{StringBuilder, TimestampMillisecondBuilder};
use datafusion::arrow::datatypes::{DataType, Field, Schema, SchemaRef, TimeUnit};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::datasource::TableProvider;
use datafusion::error::Result;
use gage_core::uuid::short_uuid;
use gage_session::system;

use super::batch::{BatchSource, BatchTable, external, marker, readable, unique_prefix_lens};
use crate::{Order, SHARE_TYPE, ShareStore, Store};

pub(crate) fn share_schema() -> SchemaRef {
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
        // What the share is for; null when none was given
        Field::new("description", DataType::Utf8, true),
        // The filter spec every member was published with, as the
        // comma-separated entry list the CLI takes; null when the
        // share requires none
        Field::new("filters", DataType::Utf8, true),
        // System
        system(Field::new("id_display", DataType::Utf8, false)),
        system(Field::new("id_prefix", DataType::Utf8, false)),
        system(Field::new("locator", DataType::Utf8, false)),
        // The current commit, which `share_session_link` keys on
        system(Field::new("commit", DataType::Utf8, false)),
    ]))
}

#[derive(Debug)]
struct Source;

impl BatchSource for Source {
    fn schema(&self) -> SchemaRef {
        share_schema()
    }

    fn build(&self, store: &Store) -> Result<RecordBatch> {
        let tips = ShareStore::from(store)
            .query()
            .order(Order::ModifiedDesc)
            .tips()
            .map_err(external)?;
        let prefix_len =
            unique_prefix_lens(store.short_prefix_ids(Some(SHARE_TYPE)).map_err(external)?);
        let shares = ShareStore::from(store);
        let len = tips.len();
        let mut ids = StringBuilder::with_capacity(len, len * 26);
        let mut modifieds = TimestampMillisecondBuilder::with_capacity(len);
        let mut createds = TimestampMillisecondBuilder::with_capacity(len);
        let mut descriptions = StringBuilder::new();
        let mut filters = StringBuilder::new();
        let mut id_displays = StringBuilder::with_capacity(len, len * 8);
        let mut id_prefixes = StringBuilder::with_capacity(len, len * 4);
        let mut locators = StringBuilder::with_capacity(len, len * 44);
        let mut commits = StringBuilder::with_capacity(len, len * 40);
        for tip in &tips {
            let Some(record) = readable(shares.at_commit(&tip.sha))? else {
                continue;
            };
            let n = prefix_len.get(&tip.id).copied().unwrap_or(tip.id.len());
            ids.append_value(&tip.id);
            modifieds.append_value(marker(&tip.id, "modified", tip.modified_ms)?);
            createds.append_value(marker(&tip.id, "created", tip.created_ms)?);
            descriptions.append_option(record.attrs.description.as_deref());
            if record.attrs.filters.is_empty() {
                filters.append_null();
            } else {
                filters.append_value(record.attrs.filters.join(", "));
            }
            id_displays.append_value(short_uuid(&tip.id));
            id_prefixes.append_value(tip.id.chars().take(n).collect::<String>());
            locators.append_value(format!("git:{}", tip.sha));
            commits.append_value(&tip.sha);
        }
        Ok(RecordBatch::try_new(
            share_schema(),
            vec![
                Arc::new(ids.finish()),
                Arc::new(modifieds.finish().with_timezone("UTC")),
                Arc::new(createds.finish().with_timezone("UTC")),
                Arc::new(descriptions.finish()),
                Arc::new(filters.finish()),
                Arc::new(id_displays.finish()),
                Arc::new(id_prefixes.finish()),
                Arc::new(locators.finish()),
                Arc::new(commits.finish()),
            ],
        )?)
    }
}

/// The store-bound `share` table.
pub fn share_table(store: Arc<Mutex<Store>>) -> Arc<dyn TableProvider> {
    Arc::new(BatchTable::new(store, Arc::new(Source)))
}
