//! `resolve_ref(text)` scalar UDF — expand `scanner:/abs/path` URIs to
//! the contents of the referenced file. Inputs without a `scanner:`
//! prefix pass through unchanged. Lookup failures produce an inline
//! `(unresolved {input}: {error})` marker rather than failing the query.

use std::any::Any;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use datafusion::arrow::array::{Array, StringArray, StringBuilder};
use datafusion::arrow::datatypes::DataType;
use datafusion::error::{DataFusionError, Result};
use datafusion::logical_expr::{
    ColumnarValue, ScalarFunctionArgs, ScalarUDF, ScalarUDFImpl, Signature, Volatility,
};
use datafusion::scalar::ScalarValue;
use gage_registry::scanner::scanner_home_paths;

pub fn resolve_ref_udf() -> ScalarUDF {
    ScalarUDF::from(ResolveRef::new())
}

#[derive(Debug, PartialEq, Eq, Hash)]
struct ResolveRef {
    signature: Signature,
}

impl ResolveRef {
    fn new() -> Self {
        Self {
            signature: Signature::exact(vec![DataType::Utf8], Volatility::Stable),
        }
    }
}

impl ScalarUDFImpl for ResolveRef {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn name(&self) -> &str {
        "resolve_ref"
    }

    fn signature(&self) -> &Signature {
        &self.signature
    }

    fn return_type(&self, _arg_types: &[DataType]) -> Result<DataType> {
        Ok(DataType::Utf8)
    }

    fn invoke_with_args(&self, args: ScalarFunctionArgs) -> Result<ColumnarValue> {
        let arg =
            args.args.into_iter().next().ok_or_else(|| {
                DataFusionError::Internal("resolve_ref takes one argument".into())
            })?;
        match arg {
            ColumnarValue::Array(array) => {
                let strs = array
                    .as_any()
                    .downcast_ref::<StringArray>()
                    .ok_or_else(|| DataFusionError::Internal("resolve_ref expects Utf8".into()))?;
                let homes = scanner_home_paths();
                let mut out = StringBuilder::with_capacity(strs.len(), strs.value_data().len());
                for i in 0..strs.len() {
                    if strs.is_null(i) {
                        out.append_null();
                    } else {
                        out.append_value(resolve_one(strs.value(i), &homes));
                    }
                }
                Ok(ColumnarValue::Array(Arc::new(out.finish())))
            }
            ColumnarValue::Scalar(ScalarValue::Utf8(Some(s))) => {
                let homes = scanner_home_paths();
                Ok(ColumnarValue::Scalar(ScalarValue::Utf8(Some(resolve_one(
                    &s, &homes,
                )))))
            }
            ColumnarValue::Scalar(ScalarValue::Utf8(None)) => {
                Ok(ColumnarValue::Scalar(ScalarValue::Utf8(None)))
            }
            _ => Err(DataFusionError::Internal("resolve_ref expects Utf8".into())),
        }
    }
}

/// Resolve a single value. Non-`scanner:` inputs return verbatim.
/// Absolute `scanner:/...` reads the first matching file under any
/// scanner home path; lookup failures (no home matched, read error,
/// missing leading `/`) return an inline `(unresolved ...)` marker.
pub fn resolve_one(input: &str, homes: &[PathBuf]) -> String {
    let Some(rest) = input.strip_prefix("scanner:") else {
        return input.to_string();
    };
    let Some(abs) = rest.strip_prefix('/') else {
        return format!("(unresolved {input}: relative scanner refs are not supported)");
    };
    let abs_path = Path::new(abs);
    let mut searched = Vec::with_capacity(homes.len());
    for home in homes {
        let candidate = home.join(abs_path);
        match std::fs::read_to_string(&candidate) {
            Ok(text) => return text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                searched.push(candidate.display().to_string());
            }
            Err(e) => {
                return format!("(unresolved {input}: read {}: {e})", candidate.display());
            }
        }
    }
    format!("(unresolved {input}: not found in {})", searched.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn seed(dir: &Path, name: &str, contents: &str) {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn passthrough_non_scanner_input() {
        let out = resolve_one("plain text", &[]);
        assert_eq!(out, "plain text");
    }

    #[test]
    fn relative_ref_is_unresolved() {
        let out = resolve_one("scanner:rel/path.md", &[]);
        assert!(out.contains("(unresolved scanner:rel/path.md"));
        assert!(out.contains("relative"));
    }

    #[test]
    fn absolute_ref_reads_first_home_match() {
        let home_a = TempDir::new().unwrap();
        let home_b = TempDir::new().unwrap();
        seed(home_b.path(), "scanner-a/fix.md", "b-version");

        let out = resolve_one(
            "scanner:/scanner-a/fix.md",
            &[home_a.path().to_path_buf(), home_b.path().to_path_buf()],
        );
        assert_eq!(out, "b-version");
    }

    #[test]
    fn not_found_lists_searched_paths() {
        let home = TempDir::new().unwrap();
        let out = resolve_one("scanner:/nope/missing.md", &[home.path().to_path_buf()]);
        assert!(out.contains("(unresolved scanner:/nope/missing.md"));
        assert!(out.contains("not found in"));
    }
}
