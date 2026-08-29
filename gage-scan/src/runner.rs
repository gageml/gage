use std::collections::HashMap;
use std::fmt;
use std::io;
use std::sync::{Arc, Mutex};

use gage_claude::home::ClaudeHome;
use gage_claude::project::{Project, project_for_session_name};
use gage_claude::session::SessionInfo;
use gage_db::rusqlite::Connection;
use gage_db::scan::{Scan, ScanTask, TaskStatus, insert_scan, insert_scan_session, insert_task};
use gage_query::ScanSessionContext;
use rune::alloc::prelude::TryToOwned;
use rune::runtime::Vm;
use rune::sync::Arc as RuneArc;
use rune::{Diagnostics, Source, Sources};
use tokio_util::sync::CancellationToken;

use crate::event::{RunSummary, ScanEvent};
use crate::scheduler;
use gage_registry::scanner::{Scanner, ScannerDef, scanners_dir};
use gage_runtime as runtime;
use gage_runtime::state::{RunContext, ScanContext, ScannerSlot};

pub enum RunError {
    Io(io::Error),
    Db(gage_db::scan::ScanError),
    Compile { name: String, diagnostics: String },
    MissingTask { scanner: String, task: String },
    Plan(String),
    Agent(String),
    Emitted,
    Canceled,
}

impl fmt::Display for RunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RunError::Io(e) => write!(f, "{e}"),
            RunError::Db(e) => write!(f, "{e}"),
            RunError::Compile { name, diagnostics } => {
                write!(f, "scanner '{name}' failed to compile\n{diagnostics}")
            }
            RunError::MissingTask { scanner, task } => write!(
                f,
                "scanner '{scanner}' declares task '{task}' but defines no matching function"
            ),
            RunError::Plan(msg) => write!(f, "plan error: {msg}"),
            RunError::Agent(msg) => write!(f, "{msg}"),
            RunError::Emitted => Ok(()),
            RunError::Canceled => write!(f, "scan canceled"),
        }
    }
}

impl From<io::Error> for RunError {
    fn from(e: io::Error) -> Self {
        RunError::Io(e)
    }
}

impl From<gage_db::scan::ScanError> for RunError {
    fn from(e: gage_db::scan::ScanError) -> Self {
        RunError::Db(e)
    }
}

/// Run scanners against the selected sessions.
///
/// `db` is the process-wide shared connection. Every ScannerSlot gets
/// a clone of this Arc; every task read and write goes through this
/// Mutex. Combined with the scheduler's DAG gating (a downstream task
/// is only enqueued after the upstream task's worker returns from
/// dispatch_task, which has already released this Mutex), this is what
/// makes a `notes.wants` pattern mean "sees every matching note written
/// by an upstream task." Callers are expected to hold the same Arc so
/// they can query the resulting notes after the run completes — do not
/// open a second connection for that, since separate WAL connections
/// take snapshot reads.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    db: Arc<Mutex<Connection>>,
    scan_id: String,
    scanners: Vec<Scanner<'_>>,
    slots: Vec<ScannerSlot>,
    selected: Arc<[SessionInfo]>,
    scan_ctx: Arc<ScanSessionContext>,
    jobs: usize,
    agent_jobs: usize,
    cancel: CancellationToken,
    on_event: impl FnMut(ScanEvent) + Send,
) -> Result<RunSummary, RunError> {
    let run_started = std::time::Instant::now();

    // Init scan + per-scanner records, recording the selected sessions
    {
        let conn = db.lock().unwrap();
        init_run(&scan_id, &selected, &conn)?;
    }

    // Resolve distinct projects from `~/.claude.json`. Sessions key
    // off the encoded directory name they were stored under; that
    // encoding is lossy, so the lookup picks the first project whose
    // path encodes to the same name. Sessions whose project isn't in
    // `.claude.json` (e.g. the user deleted the directory) silently
    // resolve to no project.
    let claude_home = ClaudeHome::from_env()
        .map_err(|e| io::Error::new(e.kind(), format!("resolving Claude home: {e}")))?;
    let mut projects: HashMap<String, Arc<Project>> = HashMap::new();
    for s in selected.iter() {
        let name = s.project_name().to_string();
        if projects.contains_key(&name) {
            continue;
        }
        let resolved = project_for_session_name(&claude_home, &name).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!(
                    "resolving project for session {} (encoded name {name}) under {}: {e}",
                    s.id,
                    claude_home.path().display(),
                ),
            )
        })?;
        if let Some(p) = resolved {
            projects.insert(name, Arc::new(p));
        }
    }

    let mcp_host = match gage_mcp::McpHost::start().await {
        Ok(h) => Some(Arc::new(h)),
        Err(e) => {
            tracing::warn!("mcp host failed to start: {e}; call_agent will be unavailable");
            None
        }
    };
    let run = Arc::new(RunContext {
        scan_id: scan_id.clone(),
        selected,
        projects,
        scan_ctx,
        mcp_host,
        dispatcher: std::sync::OnceLock::new(),
        agent_pool: Arc::new(tokio::sync::Semaphore::new(agent_jobs)),
        agent_fault: std::sync::OnceLock::new(),
    });
    // Start the Rune tool dispatcher. Held by `RunContext` so scanners
    // can reach it through `current_scan_ctx().run.dispatcher`. The
    // dispatcher itself holds only a Weak<RunContext> so this edge
    // does not form a strong cycle.
    let dispatcher = gage_runtime::dispatcher::ToolDispatcher::start(Arc::downgrade(&run));
    run.dispatcher
        .set(dispatcher)
        .ok()
        .expect("dispatcher should only be set once on a fresh RunContext");

    // Slots are compiled by the caller (see `compile_scanners`) so a
    // broken scanner is a full stop before any scan state exists.
    // Register each compiled module with the dispatcher so MCP
    // tool-call requests for a scanner can resolve to its
    // rt/unit/scanner-name without crossing rune state through the
    // channel.
    let mut scanner_tasks: Vec<HashMap<String, gage_registry::scanner::TaskDef>> = Vec::new();
    for (s, slot) in scanners.iter().zip(&slots) {
        if let Some(dispatcher) = run.dispatcher.get() {
            dispatcher.register(
                slot.name.clone(),
                slot.rt.clone(),
                slot.unit.clone(),
                slot.sources.clone(),
                slot.name.clone(),
                slot.db.clone(),
            );
        }
        // A scanner pulled in via `required_by` plans only its pulled
        // tasks; an explicitly selected scanner plans all of them.
        let tasks: HashMap<String, _> = s
            .def
            .tasks
            .iter()
            .filter(|(k, _)| s.only_tasks.as_ref().is_none_or(|only| only.contains(k)))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        scanner_tasks.push(tasks);
    }

    let scanner_names: Vec<String> = slots.iter().map(|s| s.name.clone()).collect();
    let plan = scheduler::plan(&scanner_names, &scanner_tasks)
        .map_err(|e| RunError::Plan(e.to_string()))?;

    // Every planned task gets its row up front (status `pending`), so
    // the plan is on record even if the run dies before dispatch. The
    // scheduler updates each row on dispatch and completion.
    {
        let conn = db.lock().unwrap();
        insert_plan_tasks(&conn, &scan_id, &plan, &scanners)?;
    }

    let slots = Arc::new(slots);
    let result = scheduler::run_plan(plan, slots, run, jobs, cancel, on_event).await;

    match result {
        Ok(summary) => {
            {
                let conn = db.lock().unwrap();
                persist_run_summary(&conn, &summary, run_started.elapsed())?;
            }
            if summary.canceled {
                Err(RunError::Canceled)
            } else if summary.failed > 0 {
                Err(RunError::Emitted)
            } else {
                Ok(summary)
            }
        }
        Err(scheduler::RunError::Channel) => Err(RunError::Emitted),
        Err(scheduler::RunError::Db(e)) => Err(RunError::Db(e)),
    }
}

/// Insert one `scan_task` row per planned task, status `pending`.
fn insert_plan_tasks(
    conn: &Connection,
    scan_id: &str,
    plan: &scheduler::Plan,
    scanners: &[Scanner<'_>],
) -> Result<(), RunError> {
    for task in &plan.tasks {
        let def = scanners
            .get(task.scanner_idx)
            .map(|s| s.def)
            .expect("plan task indexes the scanner list it was built from");
        insert_task(
            conn,
            &ScanTask {
                scan_id: scan_id.to_string(),
                scanner_name: def.name.clone(),
                scanner_version: def.version.clone(),
                task_name: task.task_name.clone(),
                status: TaskStatus::Pending,
                started: None,
                stopped: None,
                error: None,
                metadata: None,
            },
        )?;
    }
    Ok(())
}

/// Persist the run summary to `scan.metadata`. Written on completion
/// and cancellation alike — only a run that died leaves the column
/// NULL, which marks the scan as incomplete.
fn persist_run_summary(
    conn: &Connection,
    summary: &RunSummary,
    elapsed: std::time::Duration,
) -> Result<(), RunError> {
    gage_db::scan::set_scan_summary(
        conn,
        &summary.scan_id,
        &gage_db::scan::ScanSummary {
            total: summary.total,
            completed: summary.completed,
            failed: summary.failed,
            skipped: summary.skipped,
            canceled: summary.canceled,
            elapsed_ms: elapsed.as_millis() as u64,
        },
    )?;
    Ok(())
}

fn init_run(scan_id: &str, selected: &[SessionInfo], db: &Connection) -> Result<(), RunError> {
    insert_scan(
        db,
        &Scan {
            id: scan_id.to_string(),
            created: gage_core::datetime::now_ms(),
            // Replaced by the end-of-run summary; a dead pid under this
            // payload marks a run that died
            metadata: Some(gage_db::scan::running_metadata(std::process::id())),
        },
    )?;

    for s in selected {
        insert_scan_session(db, scan_id, &s.id, is_agent_session(s))?;
    }

    Ok(())
}

/// A session living in the agent corpus, recorded on its `scan_session`
/// row so readers outside the scan's `-A` mode can find it.
pub(crate) fn is_agent_session(s: &SessionInfo) -> bool {
    s.src.starts_with(gage_core::config::agent_sessions_dir())
}

/// Compile every scanner and verify its declared tasks, in the order
/// given. Called before `run` so a scanner that fails to compile is a
/// full stop: no scan record is created and no UI is started. The
/// returned slots pair positionally with the input scanners.
pub fn compile_scanners(
    scanners: &[Scanner<'_>],
    db: Arc<Mutex<Connection>>,
) -> Result<Vec<ScannerSlot>, RunError> {
    let mut slots = Vec::with_capacity(scanners.len());
    for s in scanners {
        let slot = compile_scanner(s, db.clone())?;
        verify_tasks(&slot, s.def)?;
        slots.push(slot);
    }
    Ok(slots)
}

pub(crate) fn compile_scanner(
    scanner: &Scanner<'_>,
    db: Arc<Mutex<Connection>>,
) -> Result<ScannerSlot, RunError> {
    let dir = scanners_dir();
    let scanner_path = dir.join(&scanner.def.embed_key);

    // Build context without rune's default stdio (print/println) so we
    // can install our own that routes through SCAN_CTX.
    let mut context = rune_modules::with_config(false).unwrap();
    context.install(runtime::io_module().unwrap()).unwrap();
    context.install(runtime::types_module().unwrap()).unwrap();
    context.install(runtime::macros_module().unwrap()).unwrap();
    context.install(runtime::gage_module().unwrap()).unwrap();
    context.install(runtime::tools_module().unwrap()).unwrap();
    context.install(runtime::log_module().unwrap()).unwrap();
    context.install(runtime::stats_module().unwrap()).unwrap();
    context.install(runtime::json_module().unwrap()).unwrap();
    let rt = RuneArc::try_new(context.runtime().unwrap()).unwrap();

    let mut sources = Sources::new();
    sources
        .insert(Source::with_path(&scanner.def.name, scanner.def.source(), &scanner_path).unwrap())
        .unwrap();

    let mut diagnostics = Diagnostics::new();
    let result = rune::prepare(&mut sources)
        .with_context(&context)
        .with_diagnostics(&mut diagnostics)
        .build();

    // Render diagnostics to a plain-text buffer rather than stderr:
    // on failure they travel in the error so the caller controls
    // presentation (the CLI dialog owns the terminal at that point).
    let rendered = if diagnostics.is_empty() {
        String::new()
    } else {
        let mut buf = rune::termcolor::Buffer::no_color();
        diagnostics.emit(&mut buf, &sources).unwrap();
        String::from_utf8(buf.into_inner()).unwrap()
    };

    let unit = match result {
        Ok(unit) => {
            if !rendered.is_empty() {
                eprint!("{rendered}");
            }
            RuneArc::try_new(unit).unwrap()
        }
        Err(_) => {
            return Err(RunError::Compile {
                name: scanner.def.name.clone(),
                diagnostics: rendered,
            });
        }
    };

    Ok(ScannerSlot {
        name: scanner.def.name.clone(),
        embed_key: scanner.def.embed_key.clone(),
        source_path: scanner_path,
        source: scanner.def.source().to_string(),
        params: scanner.params.clone(),
        rt,
        unit,
        sources: Arc::new(sources),
        db,
        fault: Mutex::new(None),
    })
}

// Every declared task must map to a function of the same name in the
// compiled unit. A missing function is a broken scanner, not a runtime
// hiccup, so we surface it here before any session is touched rather
// than letting the scheduler hit "missing entry" mid-scan.
fn verify_tasks(slot: &ScannerSlot, def: &ScannerDef) -> Result<(), RunError> {
    let vm = rune::Vm::new(slot.rt.clone(), slot.unit.clone());
    for task in def.tasks.keys() {
        match vm.lookup_function([task.as_str()]) {
            Ok(_) => {}
            Err(_) => {
                return Err(RunError::MissingTask {
                    scanner: slot.name.clone(),
                    task: task.clone(),
                });
            }
        }
    }
    Ok(())
}

pub async fn test_scanners(scanners: Vec<Scanner<'_>>) -> Result<(), RunError> {
    let mut failed = false;

    for scanner in scanners {
        let name = scanner.def.name.clone();

        let dir = scanners_dir();
        let scanner_path = dir.join(&scanner.def.embed_key);

        let mut context = rune_modules::with_config(false).unwrap();
        context.install(runtime::io_module().unwrap()).unwrap();
        context.install(runtime::types_module().unwrap()).unwrap();
        context.install(runtime::macros_module().unwrap()).unwrap();
        context.install(runtime::gage_module().unwrap()).unwrap();
        context.install(runtime::tools_module().unwrap()).unwrap();
        context.install(runtime::log_module().unwrap()).unwrap();
        context.install(runtime::stats_module().unwrap()).unwrap();
        context.install(runtime::json_module().unwrap()).unwrap();
        let rt = RuneArc::try_new(context.runtime().unwrap()).unwrap();

        let mut sources = Sources::new();
        sources
            .insert(
                Source::with_path(&scanner.def.name, scanner.def.source(), &scanner_path).unwrap(),
            )
            .unwrap();

        let mut test_visitor = TestVisitor::default();
        let mut diagnostics = Diagnostics::new();
        let result = rune::prepare(&mut sources)
            .with_context(&context)
            .with_diagnostics(&mut diagnostics)
            .with_visitor(&mut test_visitor)
            .unwrap()
            .build();

        if !diagnostics.is_empty() {
            let mut writer =
                rune::termcolor::StandardStream::stderr(rune::termcolor::ColorChoice::Auto);
            diagnostics.emit(&mut writer, &sources).unwrap();
        }

        let unit = match result {
            Ok(unit) => RuneArc::try_new(unit).unwrap(),
            Err(_) => {
                failed = true;
                continue;
            }
        };

        let tests = test_visitor.into_functions();
        if tests.is_empty() {
            tracing::warn!("{name}: no #[test] functions found");
            continue;
        }

        let stub_selected: Arc<[SessionInfo]> =
            Arc::from(Vec::<SessionInfo>::new().into_boxed_slice());
        let stub_run = Arc::new(RunContext {
            scan_id: "test".to_string(),
            selected: stub_selected.clone(),
            projects: HashMap::new(),
            scan_ctx: Arc::new(ScanSessionContext::new(&stub_selected)),
            mcp_host: None,
            dispatcher: std::sync::OnceLock::new(),
            agent_pool: Arc::new(tokio::sync::Semaphore::new(1)),
            agent_fault: std::sync::OnceLock::new(),
        });
        let stub_db = Arc::new(Mutex::new(gage_db::db::open_db_in_memory().unwrap()));
        let (stub_tx, _stub_rx) = tokio::sync::mpsc::unbounded_channel();

        // Test loop does not exercise call_agent custom-tool dispatch,
        // so a stub empty Sources is fine. (rune::Sources is not Clone,
        // and Arc'ing the existing instance would funnel every test
        // iteration through one heap allocation for no benefit.)
        let stub_sources = Arc::new(rune::Sources::new());
        for (hash, item) in &tests {
            let mut vm = Vm::new(rt.clone(), unit.clone());
            let ctx = Arc::new(ScanContext {
                scanner_name: name.clone(),
                task_name: item.to_string(),
                params: scanner.params.clone(),
                run: stub_run.clone(),
                db: stub_db.clone(),
                runtime_tx: stub_tx.clone(),
                rt: rt.clone(),
                unit: unit.clone(),
                sources: stub_sources.clone(),
                task_fault: Mutex::new(None),
            });
            let result = runtime::state::SCAN_CTX
                .scope(ctx, async move {
                    vm.execute(*hash, ()).unwrap().async_complete().await
                })
                .await;
            match result {
                Err(e) => {
                    let raw = e.to_string();
                    let detail = raw.strip_prefix("Panicked: ").unwrap_or(&raw);
                    let msg = format!("{name}::{item} failed: {detail}");
                    emit_vm_error_with_message(&e, &sources, &msg);
                    failed = true;
                }
                Ok(value) => {
                    #[expect(
                        clippy::disallowed_methods,
                        reason = "takes the VM execution's return value; the runner \
                                  holds the only live handle"
                    )]
                    match rune::from_value::<Result<rune::runtime::Value, rune::runtime::Value>>(
                        value,
                    ) {
                        Ok(Err(err)) if !runtime::ignore::is_ignore(&err) => {
                            emit_scanner_err(&name, &item.to_string(), "returned Err(...)");
                            failed = true;
                        }
                        _ => emit_test_pass(&format!("{name}::{item}")),
                    }
                }
            }
        }
    }

    if failed {
        Err(RunError::Emitted)
    } else {
        Ok(())
    }
}

#[derive(Default)]
struct TestVisitor {
    functions: Vec<(rune::Hash, rune::ItemBuf)>,
}

impl TestVisitor {
    fn into_functions(self) -> Vec<(rune::Hash, rune::ItemBuf)> {
        self.functions
    }
}

impl rune::compile::CompileVisitor for TestVisitor {
    fn register_meta(
        &mut self,
        meta: rune::compile::MetaRef<'_>,
    ) -> Result<(), rune::compile::MetaError> {
        if let rune::compile::meta::Kind::Function { is_test: true, .. } = meta.kind {
            self.functions
                .push((meta.hash, meta.item.try_to_owned().unwrap()));
        }
        Ok(())
    }
}

fn emit_test_pass(scanner_name: &str) {
    use rune::termcolor::{Color, ColorChoice, ColorSpec, StandardStream, WriteColor};
    use std::io::Write;

    let mut stderr = StandardStream::stderr(ColorChoice::Auto);
    #[allow(clippy::let_underscore_must_use)]
    let _ = (|| -> std::io::Result<()> {
        stderr.set_color(
            ColorSpec::new()
                .set_fg(Some(Color::Green))
                .set_intense(true),
        )?;
        write!(stderr, "pass")?;
        stderr.reset()?;
        writeln!(stderr, ": {scanner_name}")
    })();
}

fn emit_scanner_err(scanner_name: &str, func: &str, detail: &str) {
    use rune::termcolor::{Color, ColorChoice, ColorSpec, StandardStream, WriteColor};
    use std::io::Write;

    let mut stderr = StandardStream::stderr(ColorChoice::Auto);
    #[allow(clippy::let_underscore_must_use)]
    let _ = (|| -> std::io::Result<()> {
        stderr.set_color(ColorSpec::new().set_fg(Some(Color::Red)).set_bold(true))?;
        write!(stderr, "error")?;
        stderr.set_color(ColorSpec::new().set_bold(true))?;
        write!(stderr, ": {scanner_name} {func}() failed: {detail}")?;
        stderr.reset()?;
        writeln!(stderr)
    })();
}

fn emit_vm_error_with_message(e: &rune::runtime::VmError, sources: &Sources, msg: &str) {
    use rune::termcolor::{ColorChoice, StandardStream};

    let mut writer = StandardStream::stderr(ColorChoice::Auto);
    write_vm_error(&mut writer, e, sources, msg);
}

/// Render a VM error as plain text. No ANSI: the string lands in the
/// `.err` capture file, the persisted task outcome, and the TUI — all
/// of which need it formatting-free.
pub(crate) fn render_vm_error(e: &rune::runtime::VmError, sources: &Sources, msg: &str) -> String {
    let mut buf = rune::termcolor::Buffer::no_color();
    write_vm_error(&mut buf, e, sources, msg);
    String::from_utf8(buf.into_inner()).unwrap()
}

fn write_vm_error<W: rune::termcolor::WriteColor>(
    w: &mut W,
    e: &rune::runtime::VmError,
    sources: &Sources,
    msg: &str,
) {
    use codespan_reporting::diagnostic::{Diagnostic, Label};
    use codespan_reporting::term;

    let config = term::Config::default();

    if let Some(loc) = e.first_location()
        && let Some(debug_info) = loc.unit.debug_info()
        && let Some(inst) = debug_info.instruction_at(loc.ip)
    {
        let diagnostic = Diagnostic::error().with_message(msg).with_labels(vec![
            Label::primary(inst.source_id, inst.span.range()).with_message(msg),
        ]);

        term::emit_to_write_style(w, &config, sources, &diagnostic).unwrap();
    } else {
        writeln!(w, "error: {msg}").unwrap();
    }
}

// Public TestRuntime preserved for external tests (gage-scan/tests/*).
#[derive(Default)]
pub struct TestRuntime;

impl TestRuntime {
    pub fn new() -> Self {
        Self
    }

    pub fn macros_module(&self) -> Result<rune::Module, rune::ContextError> {
        runtime::macros_module()
    }

    pub fn gage_module(&self) -> Result<rune::Module, rune::ContextError> {
        runtime::gage_module()
    }

    pub fn types_module(&self) -> Result<rune::Module, rune::ContextError> {
        runtime::types_module()
    }

    pub fn test_helpers_module(&self) -> Result<rune::Module, rune::ContextError> {
        use rune::runtime::{Object, Value};

        let mut m = rune::Module::with_crate("test")?;
        m.function("make_message", |obj: Object| -> Value {
            rune::to_value(runtime::query::Message {
                inner: obj,
                object: std::sync::OnceLock::new(),
            })
            .unwrap()
        })
        .build()?;
        m.function("make_entry", |obj: Object| -> Value {
            rune::to_value(runtime::query::Entry {
                inner: obj,
                object: std::sync::OnceLock::new(),
            })
            .unwrap()
        })
        .build()?;
        Ok(m)
    }

    /// Enter a scan-context scope for tests that exercise runtime APIs
    /// requiring `current_scan_ctx()`. The provided closure runs with
    /// the SCAN_CTX task-local installed.
    pub async fn with_scope<F, Fut, T>(&self, f: F) -> T
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = T>,
    {
        let selected: Arc<[SessionInfo]> = Arc::from(Vec::<SessionInfo>::new().into_boxed_slice());
        let run = Arc::new(RunContext {
            scan_id: "test".to_string(),
            selected: selected.clone(),
            projects: HashMap::new(),
            scan_ctx: Arc::new(ScanSessionContext::new(&selected)),
            mcp_host: None,
            dispatcher: std::sync::OnceLock::new(),
            agent_pool: Arc::new(tokio::sync::Semaphore::new(1)),
            agent_fault: std::sync::OnceLock::new(),
        });
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        // Stub rt/unit/sources. Tests built through `with_scope` don't
        // exercise the call_agent custom-tool dispatch path, so empty
        // rune values are fine here.
        let stub_rt = rune::sync::Arc::try_new(rune::runtime::RuntimeContext::default()).unwrap();
        let stub_unit = rune::sync::Arc::try_new(rune::runtime::Unit::default()).unwrap();
        let stub_sources = Arc::new(rune::Sources::new());
        let ctx = Arc::new(ScanContext {
            scanner_name: "test".to_string(),
            task_name: "test".to_string(),
            params: None,
            run,
            db: Arc::new(Mutex::new(gage_db::db::open_db_in_memory().unwrap())),
            runtime_tx: tx,
            rt: stub_rt,
            unit: stub_unit,
            sources: stub_sources,
            task_fault: Mutex::new(None),
        });
        runtime::state::SCAN_CTX.scope(ctx, f()).await
    }
}
