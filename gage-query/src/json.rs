//! `json_extract(json, path)` --- one value from a JSON string by a
//! `$`-rooted path, as text.
//!
//! The path is a string literal: `$`, then `.key` and `[index]` steps,
//! as `'$.message.content[0].name'`. The call is rewritten at plan
//! time into the `json_as_text` function of `datafusion-functions-json`
//! with the steps as its key arguments, so a malformed or non-literal
//! path fails planning with a message that names it. A string value
//! comes back unquoted; a number, boolean, object, or array comes back
//! as its JSON text; a JSON null or a missing step is SQL NULL.

use std::any::Any;
use std::sync::Arc;

use datafusion::arrow::datatypes::{DataType, Field};
use datafusion::error::{DataFusionError, Result};
use datafusion::logical_expr::expr::ScalarFunction;
use datafusion::logical_expr::simplify::{ExprSimplifyResult, SimplifyInfo};
use datafusion::logical_expr::{
    ColumnarValue, Expr, ScalarFunctionArgs, ScalarUDF, ScalarUDFImpl, Signature, Volatility,
};
use datafusion::prelude::lit;
use datafusion::scalar::ScalarValue;
use datafusion_functions_json::udfs::json_as_text_udf;

pub fn json_extract_udf() -> ScalarUDF {
    ScalarUDF::from(JsonExtract::new())
}

#[derive(Debug, PartialEq, Eq, Hash)]
struct JsonExtract {
    signature: Signature,
}

impl JsonExtract {
    fn new() -> Self {
        Self {
            signature: Signature::exact(
                vec![DataType::Utf8, DataType::Utf8],
                Volatility::Immutable,
            ),
        }
    }
}

impl ScalarUDFImpl for JsonExtract {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn name(&self) -> &str {
        "json_extract"
    }

    fn signature(&self) -> &Signature {
        &self.signature
    }

    fn return_type(&self, _arg_types: &[DataType]) -> Result<DataType> {
        Ok(DataType::Utf8)
    }

    fn simplify(&self, args: Vec<Expr>, _info: &dyn SimplifyInfo) -> Result<ExprSimplifyResult> {
        let [json, path] = <[Expr; 2]>::try_from(args).map_err(|args| {
            DataFusionError::Plan(format!(
                "json_extract takes two arguments, got {}",
                args.len()
            ))
        })?;
        let Expr::Literal(ScalarValue::Utf8(Some(path)), _) = &path else {
            return Err(DataFusionError::Plan(
                "json_extract(json, path): path must be a string literal".into(),
            ));
        };
        let mut args = vec![json];
        args.extend(parse_path(path)?.into_iter().map(Step::into_expr));
        Ok(ExprSimplifyResult::Simplified(Expr::ScalarFunction(
            ScalarFunction::new_udf(json_as_text_udf(), args),
        )))
    }

    /// Reached only when the planner skipped simplification; the same
    /// rewrite, applied to the evaluated arguments.
    fn invoke_with_args(&self, args: ScalarFunctionArgs) -> Result<ColumnarValue> {
        let json_field = args.arg_fields.first().cloned().ok_or_else(|| {
            DataFusionError::Internal("json_extract has no argument fields".into())
        })?;
        let [json, path] = <[ColumnarValue; 2]>::try_from(args.args).map_err(|args| {
            DataFusionError::Internal(format!(
                "json_extract takes two arguments, got {}",
                args.len()
            ))
        })?;
        let ColumnarValue::Scalar(ScalarValue::Utf8(Some(path))) = &path else {
            return Err(DataFusionError::Plan(
                "json_extract(json, path): path must be a string literal".into(),
            ));
        };
        let steps = parse_path(path)?;
        let mut arg_fields = vec![json_field];
        arg_fields.extend(
            steps
                .iter()
                .map(|s| Arc::new(Field::new("step", s.data_type(), false))),
        );
        let mut values = vec![json];
        values.extend(steps.into_iter().map(Step::into_value));
        json_as_text_udf().invoke_with_args(ScalarFunctionArgs {
            args: values,
            arg_fields,
            number_rows: args.number_rows,
            return_field: args.return_field,
            config_options: args.config_options,
        })
    }
}

/// One step of a parsed path
#[derive(Debug, PartialEq, Eq)]
enum Step {
    Key(String),
    Index(i64),
}

impl Step {
    fn into_expr(self) -> Expr {
        match self {
            Step::Key(k) => lit(k),
            Step::Index(i) => lit(i),
        }
    }

    fn into_value(self) -> ColumnarValue {
        match self {
            Step::Key(k) => ColumnarValue::Scalar(ScalarValue::Utf8(Some(k))),
            Step::Index(i) => ColumnarValue::Scalar(ScalarValue::Int64(Some(i))),
        }
    }

    fn data_type(&self) -> DataType {
        match self {
            Step::Key(_) => DataType::Utf8,
            Step::Index(_) => DataType::Int64,
        }
    }
}

/// `$` followed by `.key` and `[index]` steps. A key runs to the next
/// `.` or `[`; an index is a run of digits.
fn parse_path(path: &str) -> Result<Vec<Step>> {
    let bad = |what: &str| {
        DataFusionError::Plan(format!(
            "json_extract path {path:?}: {what}; expected '$' followed by .key and [index] steps"
        ))
    };
    let mut rest = path
        .strip_prefix('$')
        .ok_or_else(|| bad("must start with '$'"))?;
    let mut steps = Vec::new();
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix('.') {
            let end = after.find(['.', '[']).unwrap_or(after.len());
            let key = &after[..end];
            if key.is_empty() {
                return Err(bad("empty key"));
            }
            steps.push(Step::Key(key.to_string()));
            rest = &after[end..];
        } else if let Some(after) = rest.strip_prefix('[') {
            let (digits, after) = after.split_once(']').ok_or_else(|| bad("unclosed '['"))?;
            if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                return Err(bad("index must be a non-negative integer"));
            }
            let index = digits
                .parse::<i64>()
                .map_err(|e| bad(&format!("index {digits}: {e}")))?;
            steps.push(Step::Index(index));
            rest = after;
        } else {
            return Err(bad("unexpected character"));
        }
    }
    Ok(steps)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use datafusion::arrow::array::{Array, StringArray};
    use datafusion::arrow::datatypes::Schema;
    use datafusion::arrow::record_batch::RecordBatch;
    use datafusion::datasource::MemTable;
    use datafusion::prelude::SessionContext;

    use super::*;

    fn context() -> SessionContext {
        let schema = Arc::new(Schema::new(vec![Field::new("raw", DataType::Utf8, true)]));
        let batch = RecordBatch::try_new(
            Arc::clone(&schema),
            vec![Arc::new(StringArray::from(vec![
                Some(r#"{"message":{"usage":{"output_tokens":42},"content":[{"type":"text","text":"hi"},{"type":"tool_use","name":"Bash"}]},"flag":true,"nothing":null}"#),
                None,
            ]))],
        )
        .unwrap();
        let mut ctx = SessionContext::new();
        datafusion_functions_json::register_all(&mut ctx).unwrap();
        ctx.register_udf(json_extract_udf());
        ctx.register_table(
            "entry",
            Arc::new(MemTable::try_new(schema, vec![vec![batch]]).unwrap()),
        )
        .unwrap();
        ctx
    }

    async fn texts(ctx: &SessionContext, sql: &str) -> Vec<Option<String>> {
        let batches = ctx.sql(sql).await.unwrap().collect().await.unwrap();
        batches
            .iter()
            .flat_map(|b| {
                let col = b.column(0).as_any().downcast_ref::<StringArray>().unwrap();
                (0..col.len())
                    .map(|i| col.is_valid(i).then(|| col.value(i).to_string()))
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    #[test]
    fn parses_keys_and_indexes() {
        assert_eq!(parse_path("$").unwrap(), []);
        assert_eq!(
            parse_path("$.message.content[0].name").unwrap(),
            [
                Step::Key("message".into()),
                Step::Key("content".into()),
                Step::Index(0),
                Step::Key("name".into()),
            ]
        );
        assert_eq!(parse_path("$[2]").unwrap(), [Step::Index(2)]);
    }

    #[test]
    fn rejects_malformed_paths() {
        for bad in [
            "message.model",
            "$.",
            "$..a",
            "$[",
            "$[-1]",
            "$[x]",
            "$a",
            "$.a b[0",
        ] {
            let err = parse_path(bad).unwrap_err().to_string();
            assert!(err.contains(&format!("{bad:?}")), "{bad}: {err}");
        }
    }

    #[tokio::test]
    async fn extracts_scalars_arrays_and_nulls_as_text() {
        let ctx = context();
        assert_eq!(
            texts(
                &ctx,
                "SELECT json_extract(raw, '$.message.usage.output_tokens') FROM entry"
            )
            .await,
            [Some("42".into()), None]
        );
        assert_eq!(
            texts(
                &ctx,
                "SELECT json_extract(raw, '$.message.content[1].name') FROM entry"
            )
            .await,
            [Some("Bash".into()), None]
        );
        assert_eq!(
            texts(
                &ctx,
                "SELECT json_extract(raw, '$.message.usage') FROM entry"
            )
            .await,
            [Some(r#"{"output_tokens":42}"#.into()), None]
        );
        assert_eq!(
            texts(&ctx, "SELECT json_extract(raw, '$.flag') FROM entry").await,
            [Some("true".into()), None]
        );
        assert_eq!(
            texts(&ctx, "SELECT json_extract(raw, '$.nothing') FROM entry").await,
            [None, None]
        );
        assert_eq!(
            texts(&ctx, "SELECT json_extract(raw, '$.absent.key') FROM entry").await,
            [None, None]
        );
    }

    #[tokio::test]
    async fn casts_and_filters_on_extracted_values() {
        let ctx = context();
        let batches = ctx
            .sql("SELECT sum(CAST(json_extract(raw, '$.message.usage.output_tokens') AS BIGINT)) FROM entry")
            .await
            .unwrap()
            .collect()
            .await
            .unwrap();
        let sum = batches[0]
            .column(0)
            .as_any()
            .downcast_ref::<datafusion::arrow::array::Int64Array>()
            .unwrap()
            .value(0);
        assert_eq!(sum, 42);
        assert_eq!(
            texts(&ctx, "SELECT json_extract(raw, '$.flag') FROM entry WHERE json_extract(raw, '$.message.content[0].type') = 'text'").await,
            [Some("true".into())]
        );
    }

    #[tokio::test]
    async fn path_errors_name_the_path_at_plan_time() {
        let ctx = context();
        let err = ctx
            .sql("SELECT json_extract(raw, 'message.model') FROM entry")
            .await
            .unwrap()
            .collect()
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("\"message.model\""), "{err}");
        let err = ctx
            .sql("SELECT json_extract(raw, raw) FROM entry")
            .await
            .unwrap()
            .collect()
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("string literal"), "{err}");
    }

    #[tokio::test]
    async fn crate_operators_remain_available() {
        let ctx = context();
        assert_eq!(
            texts(
                &ctx,
                "SELECT raw->'message'->'usage'->>'output_tokens' FROM entry"
            )
            .await,
            [Some("42".into()), None]
        );
        assert_eq!(
            texts(
                &ctx,
                "SELECT raw->>'flag' FROM entry WHERE (raw->>'flag') = 'true'"
            )
            .await,
            [Some("true".into())]
        );
    }

    /// `->>` binds looser than `=`, so an unparenthesized comparison
    /// hands the comparison to the operator as its key
    #[tokio::test]
    async fn operator_comparison_without_parentheses_fails() {
        let ctx = context();
        let err = ctx
            .sql("SELECT raw->>'flag' FROM entry WHERE raw->>'flag' = 'true'")
            .await
            .unwrap()
            .collect()
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("got Boolean"), "{err}");
    }
}
