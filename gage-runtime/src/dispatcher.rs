//! Tool dispatch server — runs Rune-backed MCP tool calls.
//!
//! Peer to [`gage_mcp::McpHost`]. One server per scan/agent process,
//! running on a dedicated thread with its own current-thread tokio
//! runtime and [`tokio::task::LocalSet`]. The thread runs every Rune
//! Vm and every gage-runtime async fn invoked by those Vms — no
//! cross-runtime polling, no Send constraints inside dispatched
//! tasks.
//!
//! Payload across the channel is data-only: a `module_id` to look up
//! a registered scanner, a `fn_name`, JSON args, and a oneshot reply
//! channel. The dispatcher carries no per-call state across the
//! boundary.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};
use std::thread;

use serde_json::Value as JsonValue;
use tokio::sync::{mpsc, oneshot};

use crate::RuntimeOutput;
use crate::state::{RunContext, SCAN_CTX, ScanContext};

/// Identifier for a registered scanner module in the dispatcher. The
/// scanner name is used today; UUIDs or path-derived ids would slot
/// in equivalently.
pub type ModuleId = String;

/// One request the dispatcher accepts off the channel. All fields are
/// data-only (`Send`); no Rune state crosses the channel — the
/// dispatcher resolves `module_id` against its registry to obtain
/// rt/unit/scanner-name.
pub struct DispatchRequest {
    pub module_id: ModuleId,
    pub fn_name: String,
    pub args: JsonValue,
    pub reply: oneshot::Sender<Result<JsonValue, String>>,
}

#[derive(Clone)]
struct ModuleHandle {
    rt: rune::sync::Arc<rune::runtime::RuntimeContext>,
    unit: rune::sync::Arc<rune::runtime::Unit>,
    sources: Arc<rune::Sources>,
    scanner_name: String,
    db: Arc<Mutex<gage_db::rusqlite::Connection>>,
}

type Registry = Arc<Mutex<HashMap<ModuleId, ModuleHandle>>>;

/// Public handle to the dispatcher. Stored in [`RunContext`]; cloned
/// into [`call_agent`] custom-tool callbacks (as a sender) and into
/// scheduler-side registration calls.
pub struct ToolDispatcher {
    registry: Registry,
    sender: mpsc::UnboundedSender<DispatchRequest>,
    shutdown: Mutex<Option<oneshot::Sender<()>>>,
    join: Mutex<Option<thread::JoinHandle<()>>>,
}

impl ToolDispatcher {
    /// Spawn the dispatcher thread. The `run` argument is held as a
    /// `Weak` so the dispatcher → RunContext edge does not form a
    /// strong cycle through `RunContext.dispatcher`.
    pub fn start(run: Weak<RunContext>) -> Arc<Self> {
        let registry: Registry = Arc::new(Mutex::new(HashMap::new()));
        let (sender, rx) = mpsc::unbounded_channel();
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let registry_for_thread = Arc::clone(&registry);
        let join = thread::Builder::new()
            .name("gage-tool-dispatcher".into())
            .spawn(move || {
                let tokio_rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("build dispatcher tokio runtime");
                let local = tokio::task::LocalSet::new();
                tokio_rt.block_on(local.run_until(server_loop(
                    rx,
                    shutdown_rx,
                    registry_for_thread,
                    run,
                )));
            })
            .expect("spawn dispatcher thread");
        Arc::new(Self {
            registry,
            sender,
            shutdown: Mutex::new(Some(shutdown_tx)),
            join: Mutex::new(Some(join)),
        })
    }

    /// Register a compiled scanner module. Idempotent — repeated
    /// registrations overwrite (no harm because the inputs are
    /// derived from the same scanner source).
    pub fn register(
        &self,
        module_id: ModuleId,
        rt: rune::sync::Arc<rune::runtime::RuntimeContext>,
        unit: rune::sync::Arc<rune::runtime::Unit>,
        sources: Arc<rune::Sources>,
        scanner_name: String,
        db: Arc<Mutex<gage_db::rusqlite::Connection>>,
    ) {
        self.registry.lock().unwrap().insert(
            module_id,
            ModuleHandle {
                rt,
                unit,
                sources,
                scanner_name,
                db,
            },
        );
    }

    /// Sender end of the dispatch channel — cloned into MCP-side
    /// callbacks.
    pub fn sender(&self) -> mpsc::UnboundedSender<DispatchRequest> {
        self.sender.clone()
    }
}

impl Drop for ToolDispatcher {
    fn drop(&mut self) {
        // Signal the server loop to exit. Cannot rely on sender drop
        // alone — rmcp's session manager retains per-call services
        // (which capture sender clones) for some time after a
        // ServiceHandle drops, so the receiver-end of the mpsc
        // wouldn't close until those sessions expire.
        if let Ok(mut guard) = self.shutdown.lock()
            && let Some(tx) = guard.take()
        {
            #[allow(clippy::unused_result_ok)]
            tx.send(()).ok();
        }
        if let Ok(mut guard) = self.join.lock()
            && let Some(handle) = guard.take()
        {
            #[allow(clippy::let_underscore_must_use)]
            let _ = handle.join();
        }
    }
}

async fn server_loop(
    mut rx: mpsc::UnboundedReceiver<DispatchRequest>,
    mut shutdown: oneshot::Receiver<()>,
    registry: Registry,
    run: Weak<RunContext>,
) {
    loop {
        tokio::select! {
            biased;
            _ = &mut shutdown => break,
            req = rx.recv() => {
                let Some(req) = req else { break };
                let registry = Arc::clone(&registry);
                let run = run.clone();
                tokio::task::spawn_local(async move {
                    handle_one(req, registry, run).await;
                });
            }
        }
    }
}

async fn handle_one(req: DispatchRequest, registry: Registry, run: Weak<RunContext>) {
    let DispatchRequest {
        module_id,
        fn_name,
        args,
        reply,
    } = req;

    let module = registry.lock().unwrap().get(&module_id).cloned();
    let Some(module) = module else {
        let _ = reply.send(Err(format!("dispatcher: unknown module '{module_id}'")));
        return;
    };

    let Some(run) = run.upgrade() else {
        let _ = reply.send(Err("dispatcher: scan run already dropped".into()));
        return;
    };

    // Per-call ScanContext. params is None — tool fns shouldn't
    // depend on the calling task's per-task params. The runtime
    // output channel is local to the dispatcher; we discard the
    // receiver since print/println from a tool body has no obvious
    // consumer in this path.
    let (out_tx, _out_rx) = mpsc::unbounded_channel::<RuntimeOutput>();
    let ctx = Arc::new(ScanContext {
        scanner_name: module.scanner_name.clone(),
        params: None,
        run,
        db: module.db.clone(),
        runtime_tx: out_tx,
        rt: module.rt.clone(),
        unit: module.unit.clone(),
        sources: module.sources.clone(),
    });

    let result = SCAN_CTX.scope(ctx, run_one(module, fn_name, args)).await;
    let _ = reply.send(result);
}

async fn run_one(
    module: ModuleHandle,
    fn_name: String,
    args: JsonValue,
) -> Result<JsonValue, String> {
    let args_obj = match &args {
        JsonValue::Object(_) => crate::value::json_to_object(&args),
        _ => rune::runtime::Object::new(),
    };
    let mut vm = rune::Vm::new(module.rt, module.unit);
    let mut execution = match vm.execute([fn_name.as_str()], (args_obj,)) {
        Ok(e) => e,
        Err(e) => return Err(format!("vm execute '{fn_name}': {e}")),
    };
    let val = match execution.async_complete().await {
        Ok(v) => v,
        Err(e) => return Err(format!("vm complete '{fn_name}': {e}")),
    };
    interpret_return(&val)
}

/// A scanner tool function returns either a bare value or a Rune
/// `Result`. Treat `Ok(v)` and bare values as success, `Err(v)` as
/// failure. Where the value is JSON-serializable, return it as JSON;
/// where it isn't (Rune externals like `Note`), fall back to the
/// type name as a string so the model still gets a non-empty
/// success result.
fn interpret_return(val: &rune::runtime::Value) -> Result<JsonValue, String> {
    if let Ok(result) =
        rune::from_value::<Result<rune::runtime::Value, rune::runtime::Value>>(val.clone())
    {
        match result {
            Ok(inner) => Ok(value_to_json_lossy(&inner)),
            Err(inner) => {
                let msg = value_to_json_lossy(&inner);
                Err(match msg {
                    JsonValue::String(s) => s,
                    other => other.to_string(),
                })
            }
        }
    } else {
        Ok(value_to_json_lossy(val))
    }
}

/// JSON-encode the value when possible; otherwise return `"ok"`.
/// Many scanner tools return Rune external values (e.g. a `Note`
/// from `write_note`) that don't serialize through `serde_json`.
/// The model rarely needs the structured detail — a non-empty
/// success result is enough.
fn value_to_json_lossy(v: &rune::runtime::Value) -> JsonValue {
    crate::value::value_to_json(v).unwrap_or_else(|_| JsonValue::String("ok".into()))
}
