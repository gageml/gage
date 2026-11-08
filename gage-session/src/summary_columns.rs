//! The Arrow builders for the derived columns of a session table.
//!
//! The `session` and `native_session` tables carry the normalized
//! attributes of [`SummaryAttrs`] as typed columns and the whole
//! object as the `summary` JSON column. One builder fills all of them
//! from one value, so a column and its `summary` field always agree
//! on a row. A row with no summary is null in every one of them.

use std::sync::Arc;

use datafusion::arrow::array::{
    ArrayRef, BooleanBuilder, Float64Builder, Int64Builder, StringBuilder,
};

use crate::SummaryAttrs;

pub struct SummaryColumns {
    title: StringBuilder,
    model: StringBuilder,
    message_count: Int64Builder,
    line_count: Int64Builder,
    input_tokens: Int64Builder,
    output_tokens: Int64Builder,
    cached_tokens: Int64Builder,
    cost_usd: Float64Builder,
    summary: StringBuilder,
    is_empty: BooleanBuilder,
}

/// The finished arrays, one per derived column.
pub struct SummaryArrays {
    pub title: ArrayRef,
    pub model: ArrayRef,
    pub message_count: ArrayRef,
    pub line_count: ArrayRef,
    pub input_tokens: ArrayRef,
    pub output_tokens: ArrayRef,
    pub cached_tokens: ArrayRef,
    pub cost_usd: ArrayRef,
    pub summary: ArrayRef,
    pub is_empty: ArrayRef,
}

impl SummaryColumns {
    pub fn with_capacity(rows: usize) -> Self {
        Self {
            title: StringBuilder::new(),
            model: StringBuilder::new(),
            message_count: Int64Builder::with_capacity(rows),
            line_count: Int64Builder::with_capacity(rows),
            input_tokens: Int64Builder::with_capacity(rows),
            output_tokens: Int64Builder::with_capacity(rows),
            cached_tokens: Int64Builder::with_capacity(rows),
            cost_usd: Float64Builder::with_capacity(rows),
            summary: StringBuilder::new(),
            is_empty: BooleanBuilder::with_capacity(rows),
        }
    }

    /// Append one row. `None` appends a null to every column.
    pub fn append(&mut self, summary: Option<&SummaryAttrs>) -> Result<(), serde_json::Error> {
        self.title
            .append_option(summary.and_then(|s| s.title.as_deref()));
        self.model
            .append_option(summary.and_then(|s| s.model.as_deref()));
        self.message_count
            .append_option(summary.and_then(|s| s.message_count).map(count));
        self.line_count
            .append_option(summary.and_then(|s| s.line_count).map(count));
        self.input_tokens
            .append_option(summary.and_then(|s| s.input_tokens).map(count));
        self.output_tokens
            .append_option(summary.and_then(|s| s.output_tokens).map(count));
        self.cached_tokens
            .append_option(summary.and_then(|s| s.cached_tokens).map(count));
        self.cost_usd
            .append_option(summary.and_then(|s| s.cost_usd));
        self.is_empty
            .append_option(summary.and_then(|s| s.is_empty));
        match summary {
            Some(s) => self.summary.append_value(s.to_json()?),
            None => self.summary.append_null(),
        }
        Ok(())
    }

    pub fn finish(mut self) -> SummaryArrays {
        SummaryArrays {
            title: Arc::new(self.title.finish()),
            model: Arc::new(self.model.finish()),
            message_count: Arc::new(self.message_count.finish()),
            line_count: Arc::new(self.line_count.finish()),
            input_tokens: Arc::new(self.input_tokens.finish()),
            output_tokens: Arc::new(self.output_tokens.finish()),
            cached_tokens: Arc::new(self.cached_tokens.finish()),
            cost_usd: Arc::new(self.cost_usd.finish()),
            summary: Arc::new(self.summary.finish()),
            is_empty: Arc::new(self.is_empty.finish()),
        }
    }
}

/// A count as the `Int64` column carries it
fn count(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use datafusion::arrow::array::{Array, BooleanArray, Float64Array, Int64Array, StringArray};
    use serde_json::{Map, Value};

    /// Every typed column equals its `summary` field on the same row,
    /// and a row with no summary is null throughout.
    #[test]
    fn columns_and_summary_agree_row_by_row() {
        let mut extra = Map::new();
        extra.insert("read_error".to_string(), Value::from("bad line"));
        let full = SummaryAttrs {
            title: Some("Fix".to_string()),
            model: Some("claude-x".to_string()),
            message_count: Some(3),
            is_empty: Some(false),
            line_count: Some(7),
            input_tokens: Some(10),
            output_tokens: Some(4),
            cached_tokens: Some(6),
            cost_usd: Some(0.25),
            extra: Map::new(),
        };
        let failed = SummaryAttrs {
            extra,
            ..SummaryAttrs::default()
        };
        let mut cols = SummaryColumns::with_capacity(3);
        cols.append(Some(&full)).unwrap();
        cols.append(Some(&failed)).unwrap();
        cols.append(None).unwrap();
        let arrays = cols.finish();

        let strs = |a: &ArrayRef| a.as_any().downcast_ref::<StringArray>().unwrap().clone();
        let ints = |a: &ArrayRef| a.as_any().downcast_ref::<Int64Array>().unwrap().clone();
        assert_eq!(strs(&arrays.title).value(0), "Fix");
        assert_eq!(strs(&arrays.model).value(0), "claude-x");
        assert_eq!(ints(&arrays.message_count).value(0), 3);
        assert_eq!(ints(&arrays.line_count).value(0), 7);
        assert_eq!(ints(&arrays.input_tokens).value(0), 10);
        assert_eq!(ints(&arrays.output_tokens).value(0), 4);
        assert_eq!(ints(&arrays.cached_tokens).value(0), 6);
        let costs = arrays
            .cost_usd
            .as_any()
            .downcast_ref::<Float64Array>()
            .unwrap();
        assert_eq!(costs.value(0), 0.25);
        let empties = arrays
            .is_empty
            .as_any()
            .downcast_ref::<BooleanArray>()
            .unwrap();
        assert!(!empties.value(0));
        let json: Value = serde_json::from_str(strs(&arrays.summary).value(0)).unwrap();
        assert_eq!(json["model"], "claude-x");
        assert_eq!(json["cost_usd"], 0.25);
        assert_eq!(json["is_empty"], false);

        for a in [
            &arrays.title,
            &arrays.model,
            &arrays.message_count,
            &arrays.line_count,
            &arrays.input_tokens,
            &arrays.output_tokens,
            &arrays.cached_tokens,
            &arrays.cost_usd,
            &arrays.is_empty,
        ] {
            assert!(
                a.is_null(1),
                "a read error leaves every derived column null"
            );
            assert!(a.is_null(2), "no summary leaves every derived column null");
        }
        let json: Value = serde_json::from_str(strs(&arrays.summary).value(1)).unwrap();
        assert_eq!(json, serde_json::json!({ "read_error": "bad line" }));
        assert!(arrays.summary.is_null(2));
    }
}
