//! `context()`: the scan configuration a scanner's `init` function
//! fills in.
//!
//! A scanner's `init` runs once, before any task of the scan, under
//! an [`InitContext`] scoped through [`INIT_CTX`]; `context()` outside
//! one is a VM error. `context()` returns a [`ScannerContext`], a
//! handle on what the scanner has configured: every call in one
//! `init` writes the same configuration, and the scan takes it when
//! `init` returns. The configuration is declarative. It names what the
//! scan sets up, and the scan sets it up; `init` performs no
//! operation itself.
//!
//! Today the configuration is the scanner's query views. A query view
//! is a named `SELECT` over the scan's tables that the query tool
//! serves beside them, to every agent of the scan. [`create_views`]
//! creates the views of a scan on a query context.

use std::sync::{Arc, Mutex};

use datafusion::datasource::ViewTable;
use datafusion::execution::context::SQLOptions;
use datafusion::logical_expr::LogicalPlan;
use datafusion::prelude::SessionContext;
use rune::runtime::{Ref, VmError};
use rune::{Any, ContextError, Module};
use serde_json as json;

tokio::task_local! {
    /// The context of the running `init`, read by [`context`] and
    /// `params()`
    pub static INIT_CTX: InitContext;
}

/// The `init` a `context()` call runs under: the scanner's name, its
/// resolved params, and the views it defines.
#[derive(Debug, Clone)]
pub struct InitContext {
    pub scanner: String,
    pub params: Option<json::Value>,
    views: Arc<Mutex<Vec<QueryViewDef>>>,
}

impl InitContext {
    pub fn new(scanner: &str, params: Option<json::Value>) -> Self {
        Self {
            scanner: scanner.to_string(),
            params,
            views: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// The views `init` defined, in call order. Called once `init`
    /// has returned; a handle `init` left behind writes views nothing
    /// reads.
    pub fn take_views(&self) -> Vec<QueryViewDef> {
        std::mem::take(&mut *self.views.lock().unwrap())
    }
}

/// One query view a scanner defines: a named `SELECT` over the scan's
/// tables.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryViewDef {
    /// The scanner whose `init` defined the view
    pub scanner: String,
    pub name: String,
    pub sql: String,
    /// What the query tool's description says the view holds
    pub description: Option<String>,
}

pub(crate) fn module() -> Result<Module, ContextError> {
    let mut m = Module::with_crate("gage")?;
    m.function("context", context).build()?;
    Ok(m)
}

pub(crate) fn types_module() -> Result<Module, ContextError> {
    let mut m = Module::new();
    m.ty::<ScannerContext>()?;
    m.function_meta(ScannerContext::query_view)?;
    m.ty::<QueryView>()?;
    m.function_meta(QueryView::description)?;
    Ok(m)
}

/// The running `init`'s scan configuration.
fn context() -> Result<ScannerContext, VmError> {
    INIT_CTX
        .try_with(|ctx| ScannerContext {
            scanner: ctx.scanner.clone(),
            views: Arc::clone(&ctx.views),
        })
        .map_err(|_outside_scope| {
            VmError::panic("context() is available only inside a scanner's init function")
        })
}

/// The running `init`'s params, when the caller runs under an init
/// context.
pub(crate) fn current_params() -> Option<Option<json::Value>> {
    INIT_CTX.try_with(|ctx| ctx.params.clone()).ok()
}

/// The handle `context()` returns: the scanner's scan configuration,
/// written by each call.
#[derive(Any, Clone)]
#[rune(item = ::gage)]
pub struct ScannerContext {
    #[rune(skip)]
    scanner: String,
    #[rune(skip)]
    views: Arc<Mutex<Vec<QueryViewDef>>>,
}

impl ScannerContext {
    /// Define the query view `name` as `sql`, one `SELECT` over the
    /// scan's tables and the views defined before it. The name is
    /// lowercase letters, digits, and underscores, and starts with a
    /// letter or an underscore. The SQL is planned when the scan
    /// creates the view, after `init` returns.
    #[rune::function(instance)]
    fn query_view(&self, name: Ref<str>, sql: Ref<str>) -> Result<QueryView, VmError> {
        let name = name.as_ref();
        if !valid_view_name(name) {
            return Err(VmError::panic(format!(
                "query_view: `{name}` is not a view name; use lowercase letters, digits, \
                 and underscores, starting with a letter or an underscore"
            )));
        }
        if sql.trim().is_empty() {
            return Err(VmError::panic(format!(
                "query_view: view `{name}` has no SQL"
            )));
        }
        let mut views = self.views.lock().unwrap();
        if views.iter().any(|v| v.name == name) {
            return Err(VmError::panic(format!(
                "query_view: view `{name}` is already defined"
            )));
        }
        views.push(QueryViewDef {
            scanner: self.scanner.clone(),
            name: name.to_string(),
            sql: sql.to_string(),
            description: None,
        });
        Ok(QueryView {
            name: name.to_string(),
            views: Arc::clone(&self.views),
        })
    }
}

fn valid_view_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// The handle `query_view` returns: one view of the scanner's scan
/// configuration.
#[derive(Any, Clone)]
#[rune(item = ::gage)]
pub struct QueryView {
    #[rune(skip)]
    name: String,
    #[rune(skip)]
    views: Arc<Mutex<Vec<QueryViewDef>>>,
}

impl QueryView {
    /// What the view holds, as the query tool's description states it
    /// to an agent.
    #[rune::function(instance)]
    fn description(&self, text: Ref<str>) -> Self {
        let mut views = self.views.lock().unwrap();
        if let Some(view) = views.iter_mut().find(|v| v.name == self.name) {
            view.description = Some(text.to_string());
        }
        drop(views);
        self.clone()
    }
}

/// Create `views` on `ctx`, in order, each planned over the tables and
/// views `ctx` holds when it is reached. A view of a name `ctx`
/// already holds, a statement that is not a query, and SQL that does
/// not plan are each an error naming the scanner and the view.
pub(crate) async fn create_views(
    ctx: &SessionContext,
    views: &[QueryViewDef],
) -> Result<(), String> {
    for view in views {
        let at = format!("scanner {}: view `{}`", view.scanner, view.name);
        let taken = ctx
            .table_exist(view.name.as_str())
            .map_err(|e| format!("{at}: {e}"))?;
        if taken {
            return Err(format!("{at}: a table or view already has that name"));
        }
        let plan = ctx
            .state()
            .create_logical_plan(&view.sql)
            .await
            .map_err(|e| format!("{at}: {e}"))?;
        let query_only = SQLOptions::new()
            .with_allow_ddl(false)
            .with_allow_dml(false)
            .with_allow_statements(false)
            .verify_plan(&plan)
            .is_ok()
            && !matches!(
                plan,
                LogicalPlan::Explain(_) | LogicalPlan::Analyze(_) | LogicalPlan::DescribeTable(_)
            );
        if !query_only {
            return Err(format!("{at}: the SQL is not a SELECT"));
        }
        ctx.register_table(
            view.name.as_str(),
            Arc::new(ViewTable::new(plan, Some(view.sql.clone()))),
        )
        .map_err(|e| format!("{at}: {e}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use datafusion::arrow::array::{Int64Array, StringArray};
    use datafusion::arrow::datatypes::{DataType, Field, Schema};
    use datafusion::arrow::record_batch::RecordBatch;
    use datafusion::datasource::MemTable;
    use rune::sync::Arc as RuneArc;
    use rune::{Diagnostics, Source, Sources, Vm};

    use super::*;
    use crate::scan::render_vm_error;

    fn vm(script: &str) -> Vm {
        let context = crate::context().unwrap();
        let rt = RuneArc::try_new(context.runtime().unwrap()).unwrap();
        let mut sources = Sources::new();
        sources.insert(Source::memory(script).unwrap()).unwrap();
        let mut diagnostics = Diagnostics::new();
        let unit = rune::prepare(&mut sources)
            .with_context(&context)
            .with_diagnostics(&mut diagnostics)
            .build()
            .unwrap();
        Vm::new(rt, RuneArc::try_new(unit).unwrap())
    }

    fn init(script: &str, params: Option<json::Value>) -> Result<Vec<QueryViewDef>, String> {
        let ctx = InitContext::new("demo", params);
        let mut vm = vm(script);
        INIT_CTX
            .sync_scope(ctx.clone(), || vm.call(["init"], ()))
            .map_err(|e| render_vm_error(&e, None))?;
        Ok(ctx.take_views())
    }

    fn view(name: &str, sql: &str) -> QueryViewDef {
        QueryViewDef {
            scanner: "demo".into(),
            name: name.into(),
            sql: sql.into(),
            description: None,
        }
    }

    fn tool_use_ctx() -> SessionContext {
        let ctx = SessionContext::new();
        let schema = Arc::new(Schema::new(vec![
            Field::new("line", DataType::Int64, false),
            Field::new("name", DataType::Utf8, false),
        ]));
        let batch = RecordBatch::try_new(
            Arc::clone(&schema),
            vec![
                Arc::new(Int64Array::from(vec![1, 2, 3, 4])),
                Arc::new(StringArray::from(vec!["Bash", "Read", "Bash", "Edit"])),
            ],
        )
        .unwrap();
        ctx.register_table(
            "tool_use",
            Arc::new(MemTable::try_new(schema, vec![vec![batch]]).unwrap()),
        )
        .unwrap();
        ctx
    }

    async fn lines(ctx: &SessionContext, sql: &str) -> Vec<i64> {
        let batches = ctx.sql(sql).await.unwrap().collect().await.unwrap();
        batches
            .iter()
            .flat_map(|b| {
                b.column(0)
                    .as_any()
                    .downcast_ref::<Int64Array>()
                    .unwrap()
                    .values()
                    .to_vec()
            })
            .collect()
    }

    /// `init` records each view in call order, with the description
    /// its handle was given, and builds the SQL from `params()`.
    #[test]
    fn init_records_the_views_it_defines() {
        let views = init(
            r#"
            use gage::{context, params};
            pub fn init() {
                let tool = params().tool;
                context()
                    .query_view("bash_call", `SELECT line FROM tool_use WHERE name = '${tool}'`)
                    .description("Calls to the shell tool");
                context().query_view("late_call", "SELECT line FROM bash_call WHERE line > 2");
            }
            "#,
            Some(json::json!({ "tool": "Bash" })),
        )
        .unwrap();
        assert_eq!(
            views,
            vec![
                QueryViewDef {
                    scanner: "demo".into(),
                    name: "bash_call".into(),
                    sql: "SELECT line FROM tool_use WHERE name = 'Bash'".into(),
                    description: Some("Calls to the shell tool".into()),
                },
                view("late_call", "SELECT line FROM bash_call WHERE line > 2"),
            ]
        );
    }

    #[test]
    fn query_view_refuses_a_bad_name_empty_sql_and_a_repeat() {
        for (call, message) in [
            (
                r#"context().query_view("Bash-Call", "SELECT 1")"#,
                "`Bash-Call` is not a view name",
            ),
            (
                r#"context().query_view("1st", "SELECT 1")"#,
                "`1st` is not a view name",
            ),
            (r#"context().query_view("v", "  ")"#, "view `v` has no SQL"),
            (
                r#"context().query_view("v", "SELECT 1"); context().query_view("v", "SELECT 2")"#,
                "view `v` is already defined",
            ),
        ] {
            let script = format!("use gage::context; pub fn init() {{ {call}; }}");
            let err = init(&script, None).unwrap_err();
            assert!(err.contains(message), "{call}: {err}");
        }
    }

    #[test]
    fn context_is_refused_outside_init() {
        let mut vm = vm("use gage::context; pub fn init() { context(); }");
        let err = vm.call(["init"], ()).unwrap_err();
        let rendered = render_vm_error(&err, None);
        assert!(
            rendered.contains("context() is available only inside a scanner's init function"),
            "{rendered}"
        );
    }

    /// A created view is read by name beside the table it selects
    /// from, a later view reads an earlier one, and the table keeps
    /// its rows.
    #[tokio::test]
    async fn create_views_serves_each_view_beside_the_tables() {
        let ctx = tool_use_ctx();
        create_views(
            &ctx,
            &[
                view("bash_call", "SELECT line FROM tool_use WHERE name = 'Bash'"),
                view("late_call", "SELECT line FROM bash_call WHERE line > 2"),
            ],
        )
        .await
        .unwrap();
        assert_eq!(
            lines(&ctx, "SELECT line FROM bash_call ORDER BY line").await,
            [1, 3]
        );
        assert_eq!(lines(&ctx, "SELECT line FROM late_call").await, [3]);
        assert_eq!(
            lines(&ctx, "SELECT line FROM tool_use ORDER BY line").await,
            [1, 2, 3, 4]
        );
    }

    /// A view created after the tables are narrowed to a line range
    /// reads the narrowed rows.
    #[tokio::test]
    async fn a_view_reads_the_narrowed_table() {
        let ctx = tool_use_ctx();
        let narrowed = ctx
            .sql("SELECT * FROM tool_use WHERE line >= 2 AND line <= 4")
            .await
            .unwrap()
            .into_view();
        ctx.deregister_table("tool_use").unwrap();
        ctx.register_table("tool_use", narrowed).unwrap();
        create_views(
            &ctx,
            &[view(
                "bash_call",
                "SELECT line FROM tool_use WHERE name = 'Bash'",
            )],
        )
        .await
        .unwrap();
        assert_eq!(lines(&ctx, "SELECT line FROM bash_call").await, [3]);
    }

    #[tokio::test]
    async fn create_views_refuses_a_taken_name_a_non_select_and_bad_sql() {
        for (def, message) in [
            (
                view("tool_use", "SELECT 1"),
                "scanner demo: view `tool_use`: a table or view already has that name",
            ),
            (
                view("v", "CREATE TABLE t AS SELECT 1"),
                "scanner demo: view `v`: the SQL is not a SELECT",
            ),
            (
                view("v", "DROP TABLE tool_use"),
                "scanner demo: view `v`: the SQL is not a SELECT",
            ),
            (
                view("v", "SELECT nope FROM tool_use"),
                "scanner demo: view `v`:",
            ),
        ] {
            let ctx = tool_use_ctx();
            let err = create_views(&ctx, &[def]).await.unwrap_err();
            assert!(err.contains(message), "{err}");
            assert_eq!(
                lines(&ctx, "SELECT line FROM tool_use ORDER BY line").await,
                [1, 2, 3, 4],
                "the table is unchanged"
            );
        }
    }
}
