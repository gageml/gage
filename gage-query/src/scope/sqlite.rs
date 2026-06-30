//! Scope injection for sqlite-backed providers via the federation
//! rewrite layer. [`ScopedSqliteSource`] is a [`SQLTable`] impl that
//! plugs into [`SQLTableSource::new_with_table`]; its
//! [`logical_optimizer`] hook walks the federated sub-plan, finds
//! every `TableScan` of the scoped table, and wraps each one in a
//! `Filter(InList(id_col, …))`. The federation rule then unparses the
//! filter into the single SQL query it ships to sqlite.
//!
//! [`SQLTable`]: datafusion_federation::sql::SQLTable
//! [`SQLTableSource::new_with_table`]: datafusion_federation::sql::SQLTableSource::new_with_table
//! [`logical_optimizer`]: datafusion_federation::sql::SQLTable::logical_optimizer

use std::any::Any;
use std::fmt;

use datafusion::arrow::datatypes::SchemaRef;
use datafusion::common::Column;
use datafusion::common::tree_node::{Transformed, TreeNode};
use datafusion::error::Result as DfResult;
use datafusion::logical_expr::{Expr, LogicalPlan, LogicalPlanBuilder};
use datafusion::sql::TableReference;
use datafusion_federation::sql::{LogicalOptimizer, SQLTable};

use super::{Scope, in_list_expr};

/// `SQLTable` impl that the federation layer drives. Holds the table
/// reference (used to identify matching `TableScan` nodes in the
/// plan), the id column to filter on, and the [`Scope`] whose
/// `load_ids` resolves the in-scope id set per query.
pub struct ScopedSqliteSource {
    table_ref: TableReference,
    schema: SchemaRef,
    id_col: &'static str,
    scope: Scope,
}

impl ScopedSqliteSource {
    pub fn new(
        table_ref: TableReference,
        schema: SchemaRef,
        id_col: &'static str,
        scope: Scope,
    ) -> Self {
        Self {
            table_ref,
            schema,
            id_col,
            scope,
        }
    }
}

impl fmt::Debug for ScopedSqliteSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ScopedSqliteSource")
            .field("table_ref", &self.table_ref)
            .field("id_col", &self.id_col)
            .field("scope", &self.scope)
            .finish()
    }
}

impl SQLTable for ScopedSqliteSource {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn table_reference(&self) -> TableReference {
        self.table_ref.clone()
    }

    fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }

    fn logical_optimizer(&self) -> Option<LogicalOptimizer> {
        let scope = self.scope.clone();
        let id_col = self.id_col;
        let target = self.table_ref.clone();
        Some(Box::new(move |plan: LogicalPlan| -> DfResult<LogicalPlan> {
            let ids = scope.load_ids()?;
            // Walk leaves-first so the wrapped TableScan is not
            // re-visited by the same pass (the wrapper is a Filter,
            // which the next visit upward — at the original parent —
            // does not match).
            plan.transform_up(|node| match node {
                LogicalPlan::TableScan(ref scan) if scan.table_name == target => {
                    let col = Expr::Column(Column::new(Some(target.clone()), id_col));
                    let filter = in_list_expr(col, &ids);
                    let wrapped = LogicalPlanBuilder::from(node.clone())
                        .filter(filter)?
                        .build()?;
                    Ok(Transformed::yes(wrapped))
                }
                _ => Ok(Transformed::no(node)),
            })
            .map(|t| t.data)
        }))
    }
}
