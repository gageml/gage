//! Reconcile-then-delegate scan machinery shared by the session-file
//! table providers.
//!
//! Every scan of `session`, `entry`, or `message` first reconciles
//! the derived artifacts, then scans the columnar store through
//! DataFusion's Parquet machinery (`ListingTable`), inheriting
//! projection pushdown, row-group pruning, parallel decode, and late
//! materialization. Per-session files give file-level pruning for
//! `session_id` filters.

use std::path::PathBuf;
use std::sync::Arc;

use datafusion::catalog::Session;
use datafusion::common::tree_node::{Transformed, TreeNode};
use datafusion::common::{Column, ScalarValue};
use datafusion::datasource::TableProvider;
use datafusion::datasource::file_format::parquet::ParquetFormat;
use datafusion::datasource::listing::{
    ListingOptions, ListingTable, ListingTableConfig, ListingTableUrl,
};
use datafusion::error::{DataFusionError, Result};
use datafusion::physical_plan::ExecutionPlan;
use datafusion::physical_plan::empty::EmptyExec;
use datafusion::prelude::Expr;
use gage_index::{IndexStore, LockMode};

use crate::filter::IdFilter;
use crate::udf::TEXT_SEARCH_NAME;

/// Refresh the derived artifacts for the store's corpus. Queries
/// try-lock: on contention the contending process is reconciling the
/// same corpus, so we skip and scan the current committed snapshot —
/// staleness is bounded at one pass and one-directional (missed
/// recent rows, never wrong rows).
pub(crate) async fn reconcile_for_query(store: &Arc<IndexStore>) -> Result<()> {
    let store = Arc::clone(store);
    let outcome = tokio::task::spawn_blocking(move || store.reconcile(LockMode::Try))
        .await
        .map_err(|e| DataFusionError::Execution(format!("reconcile task failed: {e}")))?
        .map_err(|e| DataFusionError::Execution(format!("reconcile failed: {e}")))?;
    if outcome.skipped {
        tracing::debug!("reconcile lock contended; scanning current committed snapshot");
    }
    Ok(())
}

/// The store files surviving any `session_id` filters. The id comes
/// from the file name, so this prunes without opening files, and the
/// filter is applied exactly (each file holds only that session's
/// rows).
pub(crate) fn store_files(
    store: &IndexStore,
    filters: &[Expr],
    id_col: &str,
) -> Result<Vec<(String, PathBuf)>> {
    let files = store.session_files();
    match IdFilter::new(filters, id_col)? {
        Some(f) => f.retain(files, |(id, _)| id.as_str()),
        None => Ok(files),
    }
}

/// Strip table qualifiers from column refs so exprs resolve against
/// the unqualified store schema.
pub(crate) fn unqualify(expr: &Expr) -> Expr {
    expr.clone()
        .transform(|e| match e {
            Expr::Column(c) if c.relation.is_some() => Ok(Transformed::yes(Expr::Column(
                Column::new_unqualified(c.name),
            ))),
            other => Ok(Transformed::no(other)),
        })
        .map(|t| t.data)
        .unwrap_or_else(|_| expr.clone())
}

/// Whether an expression references the `text_search` UDF anywhere.
pub(crate) fn references_text_search(expr: &Expr) -> bool {
    expr.exists(|e| {
        Ok(matches!(e, Expr::ScalarFunction(sf) if sf.func.name() == TEXT_SEARCH_NAME))
    })
    .unwrap_or(true)
}

/// Extract the queries of `text_search(text, '<literal>')` conjuncts —
/// the only shape the index accelerates. Anything else (other
/// columns, `OR`/`NOT` composition, non-literal queries) evaluates
/// through the row-wise path alone.
pub(crate) fn text_search_queries(filters: &[Expr]) -> Vec<String> {
    filters
        .iter()
        .filter_map(|f| {
            let Expr::ScalarFunction(sf) = f else {
                return None;
            };
            if sf.func.name() != TEXT_SEARCH_NAME {
                return None;
            }
            match sf.args.first() {
                Some(Expr::Column(c)) if c.name == "text" => {}
                _ => return None,
            }
            match sf.args.get(1) {
                Some(Expr::Literal(ScalarValue::Utf8(Some(q)), _)) => Some(q.clone()),
                Some(Expr::Literal(ScalarValue::LargeUtf8(Some(q)), _)) => Some(q.clone()),
                Some(Expr::Literal(ScalarValue::Utf8View(Some(q)), _)) => Some(q.clone()),
                _ => None,
            }
        })
        .collect()
}

/// Scan the given store files with DataFusion's Parquet machinery.
/// `projection` indexes the store schema; `filters` must be
/// unqualified and free of `text_search` calls (the listing scan uses
/// them for pruning only).
pub(crate) async fn scan_store_files(
    state: &dyn Session,
    files: &[PathBuf],
    projection: Option<Vec<usize>>,
    filters: &[Expr],
    limit: Option<usize>,
) -> Result<Arc<dyn ExecutionPlan>> {
    let schema = gage_index::store_schema();
    if files.is_empty() {
        let projected = match &projection {
            Some(indices) => Arc::new(schema.project(indices)?),
            None => schema,
        };
        return Ok(Arc::new(EmptyExec::new(projected)));
    }
    let urls = files
        .iter()
        .map(|p| ListingTableUrl::parse(p.to_string_lossy().as_ref()))
        .collect::<Result<Vec<_>>>()?;
    let options =
        ListingOptions::new(Arc::new(ParquetFormat::default())).with_file_extension(".parquet");
    let config = ListingTableConfig::new_with_multi_paths(urls)
        .with_listing_options(options)
        .with_schema(schema);
    let table = ListingTable::try_new(config)?;
    table.scan(state, projection.as_ref(), filters, limit).await
}
