use std::sync::Arc;

use datafusion::arrow::array::{Array, BooleanArray, StringArray};
use datafusion::arrow::compute::filter_record_batch;
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

/// [`pushdown`] extended with the line dimension: a filter over only
/// `{col_name, line}` is `Exact` because the scan applies it — id-only
/// filters through [`IdFilter`] path pruning, line-involving filters
/// through [`RowFilter`] row masking.
pub(crate) fn pushdown_lines(expr: &Expr, col_name: &str) -> TableProviderFilterPushDown {
    if references_only_set(expr, &[col_name, LINE_COL]) {
        TableProviderFilterPushDown::Exact
    } else {
        TableProviderFilterPushDown::Inexact
    }
}

fn references_only(expr: &Expr, col_name: &str) -> bool {
    let refs = expr.column_refs();
    !refs.is_empty() && refs.iter().all(|c| c.name == col_name)
}

fn references_only_set(expr: &Expr, col_names: &[&str]) -> bool {
    let refs = expr.column_refs();
    !refs.is_empty() && refs.iter().all(|c| col_names.contains(&c.name.as_str()))
}

const LINE_COL: &str = "line";

/// A predicate over the id and `line` columns, evaluated against a
/// scanned batch to mask out rows outside a scope's line ranges.
///
/// Compiles the subset of pushed-down filters that reference `line`
/// (alone or with the id column) into one DataFusion physical
/// expression over a `[<id_col>: Utf8, line: Int64]` schema. Filters
/// referencing only the id column are excluded — [`IdFilter`] already
/// enforces those by path pruning.
#[derive(Debug, Clone)]
pub(crate) struct RowFilter {
    predicate: Arc<dyn PhysicalExpr>,
    schema: SchemaRef,
}

impl RowFilter {
    /// Compile the line-involving subset of `filters`. `None` when no
    /// filter constrains `line`.
    pub(crate) fn new(filters: &[Expr], id_col: &str) -> Result<Option<Self>> {
        let combined = filters
            .iter()
            .filter(|expr| {
                references_only_set(expr, &[id_col, LINE_COL])
                    && expr.column_refs().iter().any(|c| c.name == LINE_COL)
            })
            .cloned()
            .reduce(Expr::and);
        let Some(combined) = combined else {
            return Ok(None);
        };
        let schema: SchemaRef = Arc::new(Schema::new(vec![
            Field::new(id_col, DataType::Utf8, false),
            Field::new(LINE_COL, DataType::Int64, false),
        ]));
        let df_schema = DFSchema::try_from(schema.clone())?;
        let predicate = create_physical_expr(&combined, &df_schema, &ExecutionProps::new())?;
        Ok(Some(Self { predicate, schema }))
    }

    /// Retain the rows of `batch` whose `(id, line)` satisfy the
    /// predicate. `id_idx`/`line_idx` locate the two columns in
    /// `batch`. A null predicate result drops the row, matching SQL
    /// `WHERE` semantics.
    pub(crate) fn filter_batch(
        &self,
        batch: &RecordBatch,
        id_idx: usize,
        line_idx: usize,
    ) -> Result<RecordBatch> {
        let inputs = RecordBatch::try_new(
            self.schema.clone(),
            vec![
                Arc::clone(batch.column(id_idx)),
                Arc::clone(batch.column(line_idx)),
            ],
        )?;
        let evaluated = self
            .predicate
            .evaluate(&inputs)?
            .into_array(batch.num_rows())?;
        let bools = evaluated
            .as_any()
            .downcast_ref::<BooleanArray>()
            .expect("a WHERE predicate evaluates to Boolean");
        let mask = BooleanArray::from_iter(
            (0..bools.len()).map(|i| Some(bools.is_valid(i) && bools.value(i))),
        );
        Ok(filter_record_batch(batch, &mask)?)
    }
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

    fn lit_i64(n: i64) -> Expr {
        Expr::Literal(ScalarValue::Int64(Some(n)), None)
    }

    /// The scope shape: `(session_id = 'b' AND line >= 3 AND line <= 5)
    /// OR session_id IN ('a')`.
    fn range_expr() -> Expr {
        let ranged = col("session_id")
            .eq(lit_str("b"))
            .and(col("line").gt_eq(lit_i64(3)))
            .and(col("line").lt_eq(lit_i64(5)));
        let unranged = Expr::InList(InList::new(
            Box::new(col("session_id")),
            vec![lit_str("a")],
            false,
        ));
        ranged.or(unranged)
    }

    fn two_col_batch(rows: &[(&str, i64)]) -> RecordBatch {
        use datafusion::arrow::array::Int64Array;
        let schema: SchemaRef = Arc::new(Schema::new(vec![
            Field::new("session_id", DataType::Utf8, false),
            Field::new("line", DataType::Int64, false),
        ]));
        let ids: Vec<&str> = rows.iter().map(|(id, _)| *id).collect();
        let lines: Vec<i64> = rows.iter().map(|(_, l)| *l).collect();
        RecordBatch::try_new(
            schema,
            vec![
                Arc::new(StringArray::from(ids)),
                Arc::new(Int64Array::from(lines)),
            ],
        )
        .unwrap()
    }

    #[test]
    fn range_expr_is_exact_for_lines() {
        let e = range_expr();
        assert!(matches!(
            pushdown_lines(&e, "session_id"),
            TableProviderFilterPushDown::Exact
        ));
        // The id-only pushdown stays Inexact for the same expression
        assert!(matches!(
            pushdown(&e, "session_id"),
            TableProviderFilterPushDown::Inexact
        ));
    }

    #[test]
    fn row_filter_masks_out_of_range_rows() {
        let f = RowFilter::new(&[range_expr()], "session_id")
            .unwrap()
            .unwrap();
        let batch = two_col_batch(&[("a", 1), ("a", 9), ("b", 2), ("b", 3), ("b", 5), ("b", 6)]);
        let filtered = f.filter_batch(&batch, 0, 1).unwrap();
        let ids = filtered
            .column(0)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        let lines = filtered
            .column(1)
            .as_any()
            .downcast_ref::<datafusion::arrow::array::Int64Array>()
            .unwrap();
        let rows: Vec<(String, i64)> = (0..filtered.num_rows())
            .map(|i| (ids.value(i).to_string(), lines.value(i)))
            .collect();
        // 'a' is unconstrained; 'b' keeps only lines 3..=5
        assert_eq!(
            rows,
            [
                ("a".to_string(), 1),
                ("a".to_string(), 9),
                ("b".to_string(), 3),
                ("b".to_string(), 5),
            ]
        );
    }

    #[test]
    fn id_only_filters_do_not_build_a_row_filter() {
        let f = vec![Expr::InList(InList::new(
            Box::new(col("session_id")),
            vec![lit_str("a")],
            false,
        ))];
        assert!(RowFilter::new(&f, "session_id").unwrap().is_none());
    }
}
