//! The `text_search(text, query)` scalar UDF.
//!
//! A genuine predicate with row-wise semantics: each batch is
//! evaluated by building a transient RAM Tantivy index over the
//! batch's string values and running the query (the Lucene
//! MemoryIndex pattern). Because the implementation is real, there
//! are no usage restrictions — `OR`, `NOT`, select-list use, and any
//! string column all work, degrading to scan speed when no index
//! applies. The persistent index in `gage-index` is purely an
//! accelerator; both paths are the same engine over the same
//! tokenizer chain.

use std::any::Any;
use std::sync::Arc;

use datafusion::arrow::array::{Array, ArrayRef, BooleanArray};
use datafusion::arrow::datatypes::DataType;
use datafusion::common::ScalarValue;
use datafusion::error::{DataFusionError, Result};
use datafusion::logical_expr::{
    ColumnarValue, ScalarFunctionArgs, ScalarUDF, ScalarUDFImpl, Signature, Volatility,
};

pub(crate) const TEXT_SEARCH_NAME: &str = "text_search";

pub fn text_search_udf() -> ScalarUDF {
    ScalarUDF::from(TextSearch::new())
}

#[derive(Debug, PartialEq, Eq, Hash)]
struct TextSearch {
    signature: Signature,
}

impl TextSearch {
    fn new() -> Self {
        Self {
            signature: Signature::string(2, Volatility::Immutable),
        }
    }
}

impl ScalarUDFImpl for TextSearch {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn name(&self) -> &str {
        TEXT_SEARCH_NAME
    }

    fn signature(&self) -> &Signature {
        &self.signature
    }

    fn return_type(&self, _arg_types: &[DataType]) -> Result<DataType> {
        Ok(DataType::Boolean)
    }

    fn invoke_with_args(&self, args: ScalarFunctionArgs) -> Result<ColumnarValue> {
        let query = match args.args.get(1) {
            Some(ColumnarValue::Scalar(s)) => scalar_str(s)?.ok_or_else(|| {
                DataFusionError::Execution("text_search query must not be NULL".into())
            })?,
            _ => {
                return Err(DataFusionError::Execution(
                    "text_search query must be a literal string".into(),
                ));
            }
        };
        match args.args.first() {
            Some(ColumnarValue::Array(array)) => {
                let mask = eval_mask(array, &query)?;
                Ok(ColumnarValue::Array(Arc::new(mask)))
            }
            Some(ColumnarValue::Scalar(s)) => {
                let text = scalar_str(s)?;
                let mask = gage_index::text_search_mask([text.as_deref()], &query)
                    .map_err(index_err)?;
                Ok(ColumnarValue::Scalar(ScalarValue::Boolean(
                    mask.into_iter().next().flatten(),
                )))
            }
            None => Err(DataFusionError::Execution(
                "text_search takes two arguments".into(),
            )),
        }
    }
}

fn scalar_str(s: &ScalarValue) -> Result<Option<String>> {
    match s {
        ScalarValue::Utf8(v) | ScalarValue::LargeUtf8(v) | ScalarValue::Utf8View(v) => {
            Ok(v.clone())
        }
        other => Err(DataFusionError::Execution(format!(
            "text_search expects string arguments, got {}",
            other.data_type()
        ))),
    }
}

fn eval_mask(array: &ArrayRef, query: &str) -> Result<BooleanArray> {
    use datafusion::arrow::array::{LargeStringArray, StringArray, StringViewArray};

    let values: Vec<Option<&str>> = if let Some(a) = array.as_any().downcast_ref::<StringArray>() {
        a.iter().collect()
    } else if let Some(a) = array.as_any().downcast_ref::<LargeStringArray>() {
        a.iter().collect()
    } else if let Some(a) = array.as_any().downcast_ref::<StringViewArray>() {
        a.iter().collect()
    } else {
        return Err(DataFusionError::Execution(format!(
            "text_search expects a string column, got {}",
            array.data_type()
        )));
    };
    let mask = gage_index::text_search_mask(values, query).map_err(index_err)?;
    Ok(BooleanArray::from(mask))
}

fn index_err(e: gage_index::IndexError) -> DataFusionError {
    DataFusionError::Execution(e.to_string())
}
