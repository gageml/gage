//! Run one agent def (`gage agent <scanner>::<fn>`): compile the
//! scanner, stand up a minimal run context around a registered scan,
//! evaluate the def to its `CallAgent` builder, and either drive it
//! headless or hand back an interactive launch spec.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use gage_claude::session::SessionInfo;
use gage_db::rusqlite::Connection;
use gage_db::scan::{
    AgentRunSummary, Scan, ScanTask, TaskFinish, TaskStatus, finish_task, insert_scan,
    insert_scan_session, insert_task, set_agent_summary,
};
use gage_query::ScanSessionContext;
use gage_registry::scanner::Scanner;
use gage_runtime as runtime;
use gage_runtime::agent::{AgentResult, InteractiveSpec};
use gage_runtime::state::{RunContext, SCAN_CTX, ScanContext, ScannerSlot};

use crate::runner::{RunError, compile_scanner};

pub enum AgentDefOutcome {
    Headless(AgentResult),
    Interactive(InteractiveRun),
}

/// An interactive launch spec plus the run state that must stay alive
/// for the session's lifetime: the MCP service registration, the run
/// context (dispatcher, host), and the compiled scanner backing any
/// custom-tool callbacks.
pub struct InteractiveRun {
    pub spec: InteractiveSpec,
    db: Arc<Mutex<Connection>>,
    scan_id: String,
    agent: String,
    scanner_name: String,
    task_name: String,
    started: std::time::Instant,
    _run: Arc<RunContext>,
    _slot: Arc<ScannerSlot>,
}

impl InteractiveRun {
    /// Record the run's summary on its proxy scan once the session
    /// ends. Consumes the run, releasing the state held for the
    /// session's lifetime.
    pub fn finish(self, is_error: bool) -> Result<(), gage_db::scan::ScanError> {
        let summary = AgentRunSummary {
            agent: self.agent.clone(),
            elapsed_ms: self.started.elapsed().as_millis() as u64,
            is_error,
        };
        let conn = self.db.lock().unwrap();
        finish_task(
            &conn,
            &self.scan_id,
            &self.scanner_name,
            &self.task_name,
            TaskFinish {
                status: if is_error {
                    TaskStatus::Failed
                } else {
                    TaskStatus::Completed
                },
                stopped_ms: Some(gage_core::datetime::now_ms()),
                error: None,
                // Agent runs have no pool accounting; readers fall
                // back to the row's wall-clock timestamps
                metadata: None,
            },
        )?;
        set_agent_summary(&conn, &self.scan_id, &summary)
    }
}

/// Evaluate and run `fn_name` from `scanner`.
///
/// Registers a proxy scan over `selected` (scan row + `scan_session`
/// edges) and executes the def inside a scan
/// context scoped to it — `scan().id` resolves, and the def opts tools
/// into the scan explicitly via `.scan(..)`. When the run completes an
/// [`AgentRunSummary`] is written to the proxy scan's metadata.
pub async fn run_agent_def(
    db: Arc<Mutex<Connection>>,
    scanner: Scanner<'_>,
    fn_name: &str,
    selected: Arc<[SessionInfo]>,
    interactive: bool,
) -> Result<AgentDefOutcome, RunError> {
    let scan_id = register_scan(&db, &selected)?;

    let mcp_host = match gage_mcp::McpHost::start().await {
        Ok(h) => Some(Arc::new(h)),
        Err(e) => {
            tracing::warn!("mcp host failed to start: {e}; agent tools will be unavailable");
            None
        }
    };
    let run = Arc::new(RunContext {
        scan_id,
        selected: selected.clone(),
        projects: HashMap::new(),
        scan_ctx: Arc::new(ScanSessionContext::new(&selected)),
        mcp_host,
        dispatcher: std::sync::OnceLock::new(),
        agent_pool: Arc::new(tokio::sync::Semaphore::new(1)),
        model_map: Default::default(),
        invalidate: false,
        agent_fault: std::sync::OnceLock::new(),
    });
    let dispatcher = runtime::dispatcher::ToolDispatcher::start(Arc::downgrade(&run));
    run.dispatcher
        .set(dispatcher)
        .ok()
        .expect("dispatcher should only be set once on a fresh RunContext");

    let slot = Arc::new(compile_scanner(&scanner, db.clone())?);
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

    {
        let vm = rune::Vm::new(slot.rt.clone(), slot.unit.clone());
        if vm.lookup_function([fn_name]).is_err() {
            return Err(RunError::MissingTask {
                scanner: slot.name.clone(),
                task: fn_name.to_string(),
            });
        }
    }

    // Route the def's print/println to this process's stdout.
    let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel();
    let printer = tokio::spawn(async move {
        while let Some(out) = out_rx.recv().await {
            match out {
                runtime::RuntimeOutput::Print(s) => print!("{s}"),
                runtime::RuntimeOutput::Println(s) => println!("{s}"),
                // Agent defs run without a progress or pool consumer
                runtime::RuntimeOutput::Progress { .. } => {}
                runtime::RuntimeOutput::AgentPool { .. } => {}
            }
        }
    });

    // The proxy scan's single task, named `agent:<fn>` to mark it as
    // an agent run rather than a scanner task. `ScanContext.task_name`
    // uses the same string so the `task_agent` rows written by
    // `call_agent` reference this row.
    let task_name = format!("agent:{fn_name}");
    {
        let conn = db.lock().unwrap();
        insert_task(
            &conn,
            &ScanTask {
                scan_id: run.scan_id.clone(),
                scanner_name: slot.name.clone(),
                scanner_version: scanner.def.version.clone(),
                task_name: task_name.clone(),
                status: TaskStatus::Started,
                started: Some(gage_core::datetime::now_ms()),
                stopped: None,
                error: None,
                metadata: None,
            },
        )?;
    }

    let ctx = Arc::new(ScanContext {
        scanner_name: slot.name.clone(),
        task_name: task_name.clone(),
        params: slot.params.clone(),
        run: run.clone(),
        db: slot.db.clone(),
        runtime_tx: out_tx,
        rt: slot.rt.clone(),
        unit: slot.unit.clone(),
        sources: slot.sources.clone(),
        task_fault: Mutex::new(None),
    });

    let agent = format!("{}::{fn_name}", slot.name);
    let started = std::time::Instant::now();
    let fn_name = fn_name.to_string();
    let task_name_scope = task_name.clone();
    let slot_scope = slot.clone();
    let run_scope = run.clone();
    let agent_scope = agent.clone();
    let db_scope = db.clone();
    let outcome = SCAN_CTX
        .scope(ctx, async move {
            let vm = rune::Vm::new(slot_scope.rt.clone(), slot_scope.unit.clone());
            let execution = vm
                .send_execute([fn_name.as_str()], ())
                .map_err(|e| RunError::Agent(e.to_string()))?;
            let value = execution
                .complete()
                .await
                .map_err(|e| RunError::Agent(e.to_string()))?;
            if interactive {
                let spec = runtime::agent::interactive_spec(value)
                    .map_err(|e| RunError::Agent(e.to_string()))?;
                Ok(AgentDefOutcome::Interactive(InteractiveRun {
                    spec,
                    db: db_scope,
                    scan_id: run_scope.scan_id.clone(),
                    agent: agent_scope,
                    scanner_name: slot_scope.name.clone(),
                    task_name: task_name_scope,
                    started,
                    _run: run_scope,
                    _slot: slot_scope,
                }))
            } else {
                let result = runtime::agent::run_def_headless(value)
                    .await
                    .map_err(|e| RunError::Agent(e.to_string()))?;
                Ok(AgentDefOutcome::Headless(result))
            }
        })
        .await;

    printer.abort();

    // Headless runs are over here, so the summary is written now.
    // Interactive runs outlive this call; the summary is written by
    // `InteractiveRun::finish` when the session ends.
    if let Ok(AgentDefOutcome::Headless(result)) = &outcome {
        let is_error = result.is_error || result.exit_code != 0;
        let summary = AgentRunSummary {
            agent,
            elapsed_ms: started.elapsed().as_millis() as u64,
            is_error,
        };
        let conn = db.lock().unwrap();
        finish_task(
            &conn,
            &run.scan_id,
            &slot.name,
            &task_name,
            TaskFinish {
                status: if is_error {
                    TaskStatus::Failed
                } else {
                    TaskStatus::Completed
                },
                stopped_ms: Some(gage_core::datetime::now_ms()),
                error: None,
                // Agent runs have no pool accounting; readers fall
                // back to the row's wall-clock timestamps
                metadata: None,
            },
        )?;
        set_agent_summary(&conn, &run.scan_id, &summary)?;
    }
    outcome
}

/// Insert a fresh `scan` row and populate `scan_session` with the ids
/// the agent can read.
fn register_scan(
    db: &Arc<Mutex<Connection>>,
    selected: &Arc<[SessionInfo]>,
) -> Result<String, RunError> {
    let conn = db.lock().unwrap();
    let scan_id = gage_core::uuid::new_uuid();
    insert_scan(
        &conn,
        &Scan {
            id: scan_id.clone(),
            created: gage_core::datetime::now_ms(),
            // Replaced by the agent-run summary; a dead pid under this
            // payload marks a run that died
            metadata: Some(gage_db::scan::running_metadata(std::process::id())),
        },
    )?;
    for s in selected.iter() {
        insert_scan_session(&conn, &scan_id, &s.id, crate::runner::is_agent_session(s))?;
    }
    Ok(scan_id)
}
