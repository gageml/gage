use std::sync::Arc;

use datafusion::arrow::array::{Array, BooleanArray, StringArray};
use datafusion::arrow::compute::filter_record_batch;
use datafusion::arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::common::DFSchema;
use datafusion::common::ScalarValue;
use datafusion::error::Result;
use datafusion::execution::context::ExecutionProps;
use datafusion::logical_expr::{Operator, TableProviderFilterPushDown};
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
pub struct IdFilter {
    predicate: Arc<dyn PhysicalExpr>,
    schema: SchemaRef,
}

impl IdFilter {
    /// Compile the subset of `filters` that reference only `col_name`
    /// into one predicate over a single `[col_name: Utf8]` column.
    /// `None` when no filter prunes this column.
    pub fn new(filters: &[Expr], col_name: &str) -> Result<Option<Self>> {
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
    pub fn retain<T>(
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
pub fn pushdown(expr: &Expr, col_name: &str) -> TableProviderFilterPushDown {
    if references_only(expr, col_name) {
        TableProviderFilterPushDown::Exact
    } else {
        TableProviderFilterPushDown::Inexact
    }
}

/// Whether the scan can satisfy `expr` itself when it produces `cols`
/// without parsing anything: a filter referencing only those columns
/// is `Exact`, because [`ColumnFilter`] evaluates it with DataFusion's
/// engine before the scan parses a row.
pub fn pushdown_cols(expr: &Expr, cols: &[&str]) -> TableProviderFilterPushDown {
    if references_only_set(expr, cols) {
        TableProviderFilterPushDown::Exact
    } else {
        TableProviderFilterPushDown::Inexact
    }
}

/// Whether every column `expr` references is one of `cols`.
pub fn covered_by(expr: &Expr, cols: &[&str]) -> bool {
    references_only_set(expr, cols)
}

/// [`pushdown`] extended with the line dimension: a filter over only
/// `{col_name, line}` is `Exact` because the scan applies it — id-only
/// filters through [`IdFilter`] path pruning, line-involving filters
/// through [`RowFilter`] row masking.
pub fn pushdown_lines(expr: &Expr, col_name: &str) -> TableProviderFilterPushDown {
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
pub struct RowFilter {
    predicate: Arc<dyn PhysicalExpr>,
    schema: SchemaRef,
}

impl RowFilter {
    /// Compile the line-involving subset of `filters`. `None` when no
    /// filter constrains `line`.
    pub fn new(filters: &[Expr], id_col: &str) -> Result<Option<Self>> {
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
    pub fn filter_batch(
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

/// A predicate over the columns a scan produces without parsing,
/// compiled with DataFusion's planner and evaluated against a batch
/// of those columns, so the scan parses only the rows that survive.
/// The generalization of [`IdFilter`] to any cheap column set.
#[derive(Debug, Clone)]
pub struct ColumnFilter {
    predicate: Arc<dyn PhysicalExpr>,
    schema: SchemaRef,
}

impl ColumnFilter {
    /// Compile the subset of `filters` whose columns all belong to
    /// `schema`. `None` when no filter does.
    pub fn new(filters: &[Expr], schema: SchemaRef) -> Result<Option<Self>> {
        let cols: Vec<&str> = schema.fields().iter().map(|f| f.name().as_str()).collect();
        let combined = filters
            .iter()
            .filter(|expr| references_only_set(expr, &cols))
            .cloned()
            .reduce(Expr::and);
        let Some(combined) = combined else {
            return Ok(None);
        };
        let df_schema = DFSchema::try_from(schema.clone())?;
        let predicate = create_physical_expr(&combined, &df_schema, &ExecutionProps::new())?;
        Ok(Some(Self { predicate, schema }))
    }

    /// The schema a batch handed to [`mask`](Self::mask) must have.
    pub fn schema(&self) -> &SchemaRef {
        &self.schema
    }

    /// A keep-mask over the rows of `batch`: `true` where the
    /// predicate holds. A null result counts as no match, matching
    /// SQL `WHERE` semantics.
    pub fn mask(&self, batch: &RecordBatch) -> Result<Vec<bool>> {
        let evaluated = self
            .predicate
            .evaluate(batch)?
            .into_array(batch.num_rows())?;
        let bools = evaluated
            .as_any()
            .downcast_ref::<BooleanArray>()
            .expect("a WHERE predicate evaluates to Boolean");
        Ok((0..bools.len())
            .map(|i| bools.is_valid(i) && bools.value(i))
            .collect())
    }
}

fn id_schema(col_name: &str) -> SchemaRef {
    Arc::new(Schema::new(vec![Field::new(
        col_name,
        DataType::Utf8,
        false,
    )]))
}

/// The bounds a set of pushed filters puts on a timestamp column, as
/// a walk reads them from each session's first and last entry time
/// to skip sessions whose span cannot hold a matching row. Only
/// comparisons of the column against a timestamp literal, and
/// `BETWEEN`, bound it; any other shape leaves the bound open. The
/// bounds are inclusive on both sides, which admits a session on the
/// boundary of a strict comparison and never drops one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TimeBounds {
    /// Epoch milliseconds a matching row's time is at or after
    pub after: Option<i64>,
    /// Epoch milliseconds a matching row's time is at or before
    pub before: Option<i64>,
}

impl TimeBounds {
    pub fn is_open(&self) -> bool {
        self.after.is_none() && self.before.is_none()
    }

    /// Whether a span of `first..=last` can hold a row in the bounds.
    pub fn admits(&self, first: i64, last: i64) -> bool {
        self.after.is_none_or(|a| last >= a) && self.before.is_none_or(|b| first <= b)
    }

    fn narrow_after(&mut self, ms: i64) {
        self.after = Some(self.after.map_or(ms, |a| a.max(ms)));
    }

    fn narrow_before(&mut self, ms: i64) {
        self.before = Some(self.before.map_or(ms, |b| b.min(ms)));
    }
}

/// The [`TimeBounds`] the conjunction of `filters` puts on `col`.
pub fn time_bounds(filters: &[Expr], col: &str) -> TimeBounds {
    let mut bounds = TimeBounds::default();
    for f in filters {
        narrow_bounds(f, col, &mut bounds);
    }
    bounds
}

fn narrow_bounds(expr: &Expr, col: &str, bounds: &mut TimeBounds) {
    match expr {
        Expr::BinaryExpr(b) if b.op == Operator::And => {
            narrow_bounds(&b.left, col, bounds);
            narrow_bounds(&b.right, col, bounds);
        }
        Expr::BinaryExpr(b) => {
            let (op, ms) = match (is_column(&b.left, col), is_column(&b.right, col)) {
                (true, false) => match timestamp_ms(&b.right) {
                    Some(ms) => (b.op, ms),
                    None => return,
                },
                (false, true) => match (timestamp_ms(&b.left), b.op.swap()) {
                    (Some(ms), Some(op)) => (op, ms),
                    _ => return,
                },
                _ => return,
            };
            match op {
                Operator::Gt | Operator::GtEq => bounds.narrow_after(ms),
                Operator::Lt | Operator::LtEq => bounds.narrow_before(ms),
                Operator::Eq => {
                    bounds.narrow_after(ms);
                    bounds.narrow_before(ms);
                }
                _ => {}
            }
        }
        Expr::Between(b) if !b.negated && is_column(&b.expr, col) => {
            if let Some(ms) = timestamp_ms(&b.low) {
                bounds.narrow_after(ms);
            }
            if let Some(ms) = timestamp_ms(&b.high) {
                bounds.narrow_before(ms);
            }
        }
        _ => {}
    }
}

/// The values `filters` require `col`, a list column, to hold through
/// `array_has(col, '<value>')`, from the conjunction's top level. A
/// walk skips a session whose recorded members lack any of them.
pub fn required_members(filters: &[Expr], col: &str) -> Vec<String> {
    let mut out = Vec::new();
    for f in filters {
        collect_members(f, col, &mut out);
    }
    out
}

fn collect_members(expr: &Expr, col: &str, out: &mut Vec<String>) {
    match expr {
        Expr::BinaryExpr(b) if b.op == Operator::And => {
            collect_members(&b.left, col, out);
            collect_members(&b.right, col, out);
        }
        Expr::ScalarFunction(f) if f.func.name() == "array_has" => {
            if let [haystack, Expr::Literal(ScalarValue::Utf8(Some(needle)), _)] = f.args.as_slice()
                && is_column(haystack, col)
            {
                out.push(needle.clone());
            }
        }
        _ => {}
    }
}

/// Whether `expr` is the column `col`, through any cast type coercion
/// wrapped it in.
fn is_column(expr: &Expr, col: &str) -> bool {
    match expr {
        Expr::Column(c) => c.name == col,
        Expr::Cast(c) => is_column(&c.expr, col),
        _ => false,
    }
}

/// A timestamp literal as epoch milliseconds, through any cast.
fn timestamp_ms(expr: &Expr) -> Option<i64> {
    match expr {
        Expr::Literal(v, _) => match v {
            ScalarValue::TimestampSecond(Some(s), _) => Some(s.checked_mul(1_000)?),
            ScalarValue::TimestampMillisecond(Some(ms), _) => Some(*ms),
            ScalarValue::TimestampMicrosecond(Some(us), _) => Some(us.div_euclid(1_000)),
            ScalarValue::TimestampNanosecond(Some(ns), _) => Some(ns.div_euclid(1_000_000)),
            _ => None,
        },
        Expr::Cast(c) => timestamp_ms(&c.expr),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use datafusion::functions_nested::expr_fn::array_has;
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
    fn column_filter_keeps_only_filters_over_its_columns() {
        use datafusion::arrow::array::Int64Array;

        let schema: SchemaRef = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Utf8, false),
            Field::new("size", DataType::Int64, false),
        ]));
        let filters = vec![
            Expr::BinaryExpr(BinaryExpr::new(
                Box::new(col("size")),
                Operator::Gt,
                Box::new(Expr::Literal(ScalarValue::Int64(Some(5)), None)),
            )),
            Expr::Like(Like::new(
                false,
                Box::new(col("id")),
                Box::new(lit_str("a%")),
                None,
                false,
            )),
            // References a parsed column; the scan cannot apply it
            Expr::IsNotNull(Box::new(col("title"))),
        ];
        let filter = ColumnFilter::new(&filters, schema.clone())
            .unwrap()
            .unwrap();
        let batch = RecordBatch::try_new(
            schema,
            vec![
                Arc::new(StringArray::from(vec!["a1", "a2", "b1"])),
                Arc::new(Int64Array::from(vec![9, 1, 9])),
            ],
        )
        .unwrap();
        assert_eq!(filter.mask(&batch).unwrap(), [true, false, false]);
        assert!(
            ColumnFilter::new(&filters[2..], filter.schema().clone())
                .unwrap()
                .is_none()
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

    fn ts(ms: i64) -> Expr {
        Expr::Literal(
            ScalarValue::TimestampMillisecond(Some(ms), Some("UTC".into())),
            None,
        )
    }

    fn cmp(left: Expr, op: Operator, right: Expr) -> Expr {
        Expr::BinaryExpr(BinaryExpr::new(Box::new(left), op, Box::new(right)))
    }

    #[test]
    fn time_bounds_narrow_from_comparisons_either_way_round() {
        let filters = [
            cmp(col("timestamp"), Operator::Gt, ts(100)),
            cmp(ts(900), Operator::GtEq, col("timestamp")),
            cmp(col("timestamp"), Operator::GtEq, ts(50)),
            cmp(col("other"), Operator::Lt, ts(10)),
        ];
        let bounds = time_bounds(&filters, "timestamp");
        assert_eq!(
            bounds,
            TimeBounds {
                after: Some(100),
                before: Some(900)
            }
        );
        assert!(bounds.admits(0, 100) && bounds.admits(900, 1000));
        assert!(!bounds.admits(0, 99) && !bounds.admits(901, 1000));
    }

    #[test]
    fn time_bounds_read_between_and_conjunctions_but_not_disjunctions() {
        let between = Expr::Between(datafusion::logical_expr::Between::new(
            Box::new(col("timestamp")),
            false,
            Box::new(ts(10)),
            Box::new(ts(20)),
        ));
        assert_eq!(
            time_bounds(std::slice::from_ref(&between), "timestamp"),
            TimeBounds {
                after: Some(10),
                before: Some(20)
            }
        );
        let conj = cmp(
            cmp(col("timestamp"), Operator::Gt, ts(5)),
            Operator::And,
            cmp(col("timestamp"), Operator::Lt, ts(7)),
        );
        assert_eq!(
            time_bounds(std::slice::from_ref(&conj), "timestamp"),
            TimeBounds {
                after: Some(5),
                before: Some(7)
            }
        );
        let disj = cmp(
            cmp(col("timestamp"), Operator::Gt, ts(5)),
            Operator::Or,
            cmp(col("timestamp"), Operator::Lt, ts(7)),
        );
        assert!(time_bounds(std::slice::from_ref(&disj), "timestamp").is_open());
    }

    #[test]
    fn required_members_come_from_top_level_array_has() {
        let filters = [
            array_has(col("files_changed"), lit_str("/w/a.rs")),
            cmp(
                array_has(col("files_changed"), lit_str("/w/b.rs")),
                Operator::And,
                cmp(col("line"), Operator::Gt, ts(1)),
            ),
            cmp(
                array_has(col("files_changed"), lit_str("/w/c.rs")),
                Operator::Or,
                col("files_changed").is_null(),
            ),
            array_has(col("other"), lit_str("/w/d.rs")),
        ];
        assert_eq!(
            required_members(&filters, "files_changed"),
            ["/w/a.rs", "/w/b.rs"]
        );
    }
}
