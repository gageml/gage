use std::sync::Arc;

use datafusion::arrow::array::{Array, BooleanArray, StringArray};
use datafusion::arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::common::DFSchema;
use datafusion::error::Result;
use datafusion::execution::context::ExecutionProps;
use datafusion::logical_expr::TableProviderFilterPushDown;
use datafusion::physical_expr::{PhysicalExpr, create_physical_expr};
use datafusion::prelude::Expr;

/// A predicate on a session-id column, evaluated against the cheap
/// directory listing so a scan parses only the matching session files.
///
/// A `message`/`entry`/`session` scan can satisfy any filter that
/// references *only* the id column (`session_id`, or `id` on the
/// `session` table), because that value comes from the listing and
/// needs no file parse. We compile such filters with DataFusion's own
/// expression planner and evaluate them here — so `=`, `IN`, `LIKE`,
/// `ILIKE`, `<>`, … all match exactly as DataFusion would, with no
/// hand-rolled pattern semantics. This is why `pushdown` returns
/// `Exact`: the same engine evaluates the predicate either way.
#[derive(Debug, Clone)]
pub(crate) struct IdFilter {
    predicate: Arc<dyn PhysicalExpr>,
    schema: SchemaRef,
}

impl IdFilter {
    /// Compile the subset of `filters` that reference only `col_name`
    /// into one predicate over a single `[col_name: Utf8]` column.
    /// `None` when no filter prunes this column.
    pub(crate) fn new(filters: &[Expr], col_name: &str) -> Result<Option<Self>> {
        let combined = filters
            .iter()
            .filter(|expr| references_only(expr, col_name))
            .cloned()
            .reduce(Expr::and);
        let Some(combined) = combined else {
            return Ok(None);
        };
        let schema = id_schema(col_name);
        let df_schema = DFSchema::try_from(schema.clone())?;
        let predicate = create_physical_expr(&combined, &df_schema, &ExecutionProps::new())?;
        Ok(Some(Self { predicate, schema }))
    }

    /// Retain only the items whose id satisfies the predicate.
    pub(crate) fn retain<T>(
        &self,
        items: impl IntoIterator<Item = T>,
        id_of: impl for<'a> Fn(&'a T) -> &'a str,
    ) -> Result<Vec<T>> {
        let items: Vec<T> = items.into_iter().collect();
        let mask = {
            let ids: Vec<&str> = items.iter().map(&id_of).collect();
            self.mask(&ids)?
        };
        Ok(items
            .into_iter()
            .zip(mask)
            .filter_map(|(item, keep)| keep.then_some(item))
            .collect())
    }

    /// A keep-mask aligned with `ids`: `true` where the predicate holds.
    /// A null result (the predicate evaluated to NULL) counts as no
    /// match, matching SQL `WHERE` semantics.
    fn mask(&self, ids: &[&str]) -> Result<Vec<bool>> {
        let column = Arc::new(StringArray::from(ids.to_vec()));
        let batch = RecordBatch::try_new(self.schema.clone(), vec![column])?;
        let evaluated = self.predicate.evaluate(&batch)?.into_array(ids.len())?;
        let bools = evaluated
            .as_any()
            .downcast_ref::<BooleanArray>()
            .expect("a WHERE predicate evaluates to Boolean");
        Ok((0..bools.len())
            .map(|i| bools.is_valid(i) && bools.value(i))
            .collect())
    }
}

/// Whether the scan can satisfy `expr` itself for the id column
/// `col_name`. Any filter referencing only that column is `Exact` —
/// [`IdFilter`] evaluates it with DataFusion's engine, so the post-scan
/// `FilterExec` is redundant and DataFusion drops it. Everything else
/// (filters touching parsed columns like `text`/`raw`) is `Inexact`.
pub(crate) fn pushdown(expr: &Expr, col_name: &str) -> TableProviderFilterPushDown {
    if references_only(expr, col_name) {
        TableProviderFilterPushDown::Exact
    } else {
        TableProviderFilterPushDown::Inexact
    }
}

fn references_only(expr: &Expr, col_name: &str) -> bool {
    let refs = expr.column_refs();
    !refs.is_empty() && refs.iter().all(|c| c.name == col_name)
}

fn id_schema(col_name: &str) -> SchemaRef {
    Arc::new(Schema::new(vec![Field::new(
        col_name,
        DataType::Utf8,
        false,
    )]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use datafusion::common::ScalarValue;
    use datafusion::logical_expr::{BinaryExpr, Like, Operator, expr::InList};

    fn col(name: &str) -> Expr {
        Expr::Column(name.into())
    }

    fn lit_str(s: &str) -> Expr {
        Expr::Literal(ScalarValue::Utf8(Some(s.to_string())), None)
    }

    /// Filter a fixed id set through the compiled predicate.
    fn surviving(filters: &[Expr], col_name: &str, ids: &[&str]) -> Vec<String> {
        let filter = IdFilter::new(filters, col_name).unwrap().unwrap();
        let mask = filter.mask(ids).unwrap();
        ids.iter()
            .zip(mask)
            .filter(|(_, keep)| *keep)
            .map(|(id, _)| id.to_string())
            .collect()
    }

    const IDS: &[&str] = &["4045c48b-aaaa", "4045c48b-bbbb", "9000ffff-cccc"];

    #[test]
    fn eq_matches_one() {
        let f = vec![Expr::BinaryExpr(BinaryExpr::new(
            Box::new(col("session_id")),
            Operator::Eq,
            Box::new(lit_str("4045c48b-aaaa")),
        ))];
        assert_eq!(surviving(&f, "session_id", IDS), ["4045c48b-aaaa"]);
        assert!(matches!(
            pushdown(f.first().unwrap(), "session_id"),
            TableProviderFilterPushDown::Exact
        ));
    }

    #[test]
    fn in_list_matches_set() {
        let f = vec![Expr::InList(InList::new(
            Box::new(col("id")),
            vec![lit_str("4045c48b-bbbb"), lit_str("9000ffff-cccc")],
            false,
        ))];
        assert_eq!(surviving(&f, "id", IDS), ["4045c48b-bbbb", "9000ffff-cccc"]);
    }

    #[test]
    fn like_prefix_matches() {
        let f = vec![like("session_id", "4045c48b%", false, false)];
        assert_eq!(
            surviving(&f, "session_id", IDS),
            ["4045c48b-aaaa", "4045c48b-bbbb"]
        );
        assert!(matches!(
            pushdown(f.first().unwrap(), "session_id"),
            TableProviderFilterPushDown::Exact
        ));
    }

    #[test]
    fn like_with_interior_wildcards_matches() {
        // a pattern the old prefix-only path could not handle
        let f = vec![like("session_id", "%c48b-bbbb", false, false)];
        assert_eq!(surviving(&f, "session_id", IDS), ["4045c48b-bbbb"]);
    }

    #[test]
    fn underscore_matches_single_char() {
        let f = vec![like("session_id", "4045c48b-aaa_", false, false)];
        assert_eq!(surviving(&f, "session_id", IDS), ["4045c48b-aaaa"]);
    }

    #[test]
    fn not_like_negates() {
        let f = vec![like("session_id", "4045c48b%", true, false)];
        assert_eq!(surviving(&f, "session_id", IDS), ["9000ffff-cccc"]);
    }

    #[test]
    fn filter_on_parsed_column_is_inexact_and_not_compiled() {
        let f = vec![Expr::BinaryExpr(BinaryExpr::new(
            Box::new(col("text")),
            Operator::Eq,
            Box::new(lit_str("hi")),
        ))];
        assert!(IdFilter::new(&f, "session_id").unwrap().is_none());
        assert!(matches!(
            pushdown(f.first().unwrap(), "session_id"),
            TableProviderFilterPushDown::Inexact
        ));
    }

    fn like(col_name: &str, pattern: &str, negated: bool, case_insensitive: bool) -> Expr {
        Expr::Like(Like::new(
            negated,
            Box::new(col(col_name)),
            Box::new(lit_str(pattern)),
            None,
            case_insensitive,
        ))
    }
}
