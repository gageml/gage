//! `age(timestamp)` --- the time since a timestamp as the CLI shows it:
//! `12s ago`, `5m ago`, `3h ago`, `2d ago`.

use std::any::Any;
use std::sync::Arc;

use datafusion::arrow::array::{Array, StringBuilder, TimestampMillisecondArray};
use datafusion::arrow::datatypes::{DataType, TimeUnit};
use datafusion::error::{DataFusionError, Result};
use datafusion::logical_expr::{
    ColumnarValue, ScalarFunctionArgs, ScalarUDF, ScalarUDFImpl, Signature, TypeSignature,
    Volatility,
};
use datafusion::scalar::ScalarValue;
use gage_core::datetime::format_elapsed_ms;

pub fn age_udf() -> ScalarUDF {
    ScalarUDF::from(Age::new())
}

#[derive(Debug, PartialEq, Eq, Hash)]
struct Age {
    signature: Signature,
}

impl Age {
    fn new() -> Self {
        let millis = |tz: Option<&str>| {
            TypeSignature::Exact(vec![DataType::Timestamp(
                TimeUnit::Millisecond,
                tz.map(Into::into),
            )])
        };
        Self {
            signature: Signature::one_of(
                vec![millis(Some("UTC")), millis(None)],
                Volatility::Volatile,
            ),
        }
    }
}

impl ScalarUDFImpl for Age {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn name(&self) -> &str {
        "age"
    }

    fn signature(&self) -> &Signature {
        &self.signature
    }

    fn return_type(&self, _arg_types: &[DataType]) -> Result<DataType> {
        Ok(DataType::Utf8)
    }

    fn invoke_with_args(&self, args: ScalarFunctionArgs) -> Result<ColumnarValue> {
        let arg = args
            .args
            .into_iter()
            .next()
            .ok_or_else(|| DataFusionError::Internal("age takes one argument".into()))?;
        match arg {
            ColumnarValue::Array(array) => {
                let times = array
                    .as_any()
                    .downcast_ref::<TimestampMillisecondArray>()
                    .ok_or_else(|| {
                        DataFusionError::Internal("age expects a millisecond timestamp".into())
                    })?;
                let mut out = StringBuilder::with_capacity(times.len(), times.len() * 8);
                for i in 0..times.len() {
                    if times.is_null(i) {
                        out.append_null();
                    } else {
                        out.append_value(format_elapsed_ms(times.value(i)));
                    }
                }
                Ok(ColumnarValue::Array(Arc::new(out.finish())))
            }
            ColumnarValue::Scalar(ScalarValue::TimestampMillisecond(ms, _)) => Ok(
                ColumnarValue::Scalar(ScalarValue::Utf8(ms.map(format_elapsed_ms))),
            ),
            _ => Err(DataFusionError::Internal(
                "age expects a millisecond timestamp".into(),
            )),
        }
    }
}
