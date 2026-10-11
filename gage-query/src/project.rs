//! `project_for_path(path)` --- resolve a directory path to the driver's
//! project name, for filtering the `project` column.
//!
//! The mapping is the driver's, obtained through `Source::project_name`
//! on the default source, the same interface that provides the native
//! session table. An input that is not an existing directory passes
//! through unchanged, so a caller may pass either a directory or a
//! project name and filter with `project = project_for_path(<value>)`.

use std::any::Any;
use std::path::Path;
use std::sync::Arc;

use datafusion::arrow::array::{Array, StringArray, StringBuilder};
use datafusion::arrow::datatypes::DataType;
use datafusion::error::{DataFusionError, Result};
use datafusion::logical_expr::{
    ColumnarValue, ScalarFunctionArgs, ScalarUDF, ScalarUDFImpl, Signature, Volatility,
};
use datafusion::scalar::ScalarValue;
use gage_registry::driver::DriverRegistry;
use gage_session::Source;

pub fn project_for_path_udf() -> ScalarUDF {
    ScalarUDF::from(ProjectForPath::new())
}

#[derive(Debug, PartialEq, Eq, Hash)]
struct ProjectForPath {
    signature: Signature,
}

impl ProjectForPath {
    fn new() -> Self {
        Self {
            signature: Signature::exact(vec![DataType::Utf8], Volatility::Stable),
        }
    }
}

impl ScalarUDFImpl for ProjectForPath {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn name(&self) -> &str {
        "project_for_path"
    }

    fn signature(&self) -> &Signature {
        &self.signature
    }

    fn return_type(&self, _arg_types: &[DataType]) -> Result<DataType> {
        Ok(DataType::Utf8)
    }

    fn invoke_with_args(&self, args: ScalarFunctionArgs) -> Result<ColumnarValue> {
        let arg = args.args.into_iter().next().ok_or_else(|| {
            DataFusionError::Internal("project_for_path takes one argument".into())
        })?;
        match arg {
            ColumnarValue::Array(array) => {
                let strs = array
                    .as_any()
                    .downcast_ref::<StringArray>()
                    .ok_or_else(|| {
                        DataFusionError::Internal("project_for_path expects Utf8".into())
                    })?;
                let mut resolver = Resolver::new();
                let mut out = StringBuilder::with_capacity(strs.len(), strs.value_data().len());
                for i in 0..strs.len() {
                    if strs.is_null(i) {
                        out.append_null();
                    } else {
                        out.append_value(resolver.resolve(strs.value(i))?);
                    }
                }
                Ok(ColumnarValue::Array(Arc::new(out.finish())))
            }
            ColumnarValue::Scalar(ScalarValue::Utf8(Some(s))) => {
                let mut resolver = Resolver::new();
                Ok(ColumnarValue::Scalar(ScalarValue::Utf8(Some(
                    resolver.resolve(&s)?,
                ))))
            }
            ColumnarValue::Scalar(ScalarValue::Utf8(None)) => {
                Ok(ColumnarValue::Scalar(ScalarValue::Utf8(None)))
            }
            _ => Err(DataFusionError::Internal(
                "project_for_path expects Utf8".into(),
            )),
        }
    }
}

/// Opens the default source once, on the first directory input, and
/// reuses it. Inputs that are not directories never open a source.
struct Resolver {
    source: Option<Box<dyn Source>>,
}

impl Resolver {
    fn new() -> Self {
        Self { source: None }
    }

    fn resolve(&mut self, input: &str) -> Result<String> {
        let path = Path::new(input);
        if !path.is_dir() {
            return Ok(input.to_string());
        }
        if self.source.is_none() {
            let registry = DriverRegistry::builtin();
            self.source = Some(registry.open_source("").map_err(external)?);
        }
        let source = self.source.as_ref().expect("source opened above");
        source.project_name(path).map_err(external)
    }
}

fn external(e: gage_session::DriverError) -> DataFusionError {
    DataFusionError::External(Box::new(e))
}
