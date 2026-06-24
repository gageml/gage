//! `agent(prompt)` — builder that spawns an isolated judge `claude`
//! process from a scanner and returns an `AgentSession` handle on await.

use std::time::Duration;

use std::collections::HashSet;

use gage_agent::{
    AgentBuilder, AgentOutput as Output, AgentSession as Session, SandboxSpec, ToolPolicy,
};
use rune::alloc::fmt::TryWrite;
use rune::alloc::prelude::TryClone;
use rune::runtime::{Formatter, Mut, Protocol, VmError};
use rune::{Any, ContextError, Module};

use crate::runtime::error::Error;
use crate::runtime::llm::anthropic;
use crate::runtime::state::current_scan_ctx;

const DEFAULT_NAME: &str = "judge";

#[derive(Any)]
#[rune(item = ::gage)]
pub(crate) struct AgentSession {
    inner: Session,
}

#[derive(Any)]
#[rune(item = ::gage)]
pub(crate) struct AgentOutput {
    #[rune(get, copy)]
    status: AgentStatus,
    #[rune(get)]
    stdout: String,
    #[rune(get)]
    stderr: String,
}

#[derive(Clone, Copy, Any)]
#[rune(item = ::gage)]
pub(crate) struct AgentStatus {
    #[rune(get, copy)]
    code: Option<i64>,
    #[rune(get, copy)]
    success: bool,
}

impl TryClone for AgentStatus {
    fn try_clone(&self) -> Result<Self, rune::alloc::Error> {
        Ok(*self)
    }
}

impl AgentStatus {
    #[rune::function(instance)]
    fn success(&self) -> bool {
        self.success
    }

    #[rune::function(instance)]
    fn code(&self) -> Option<i64> {
        self.code
    }

    #[rune::function(protocol = DEBUG_FMT)]
    fn debug(&self, f: &mut Formatter) -> Result<(), VmError> {
        write!(
            f,
            "AgentStatus {{ success: {}, code: {:?} }}",
            self.success, self.code
        )?;
        Ok(())
    }
}

impl AgentOutput {
    #[rune::function(protocol = DEBUG_FMT)]
    fn debug(&self, f: &mut Formatter) -> Result<(), VmError> {
        write!(
            f,
            "AgentOutput {{ status: AgentStatus {{ success: {}, code: {:?} }}, \
             stdout: {:?}, stderr: {:?} }}",
            self.status.success, self.status.code, self.stdout, self.stderr
        )?;
        Ok(())
    }
}

impl From<Output> for AgentOutput {
    fn from(o: Output) -> Self {
        AgentOutput {
            status: AgentStatus {
                code: o.status.code().map(i64::from),
                success: o.status.success(),
            },
            stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
        }
    }
}

#[rune::function(instance)]
async fn wait(mut this: Mut<AgentSession>) -> super::Result<AgentOutput> {
    let out = this
        .inner
        .wait()
        .await
        .map_err(|e| Error::Agent(e.to_string()))?;
    Ok(AgentOutput::from(out))
}

#[rune::function(instance)]
async fn kill(mut this: Mut<AgentSession>, grace_secs: i64) -> super::Result<()> {
    this.inner
        .kill(secs(grace_secs))
        .await
        .map_err(|e| Error::Agent(e.to_string()))
}

#[rune::function(instance)]
fn id(this: &AgentSession) -> Option<i64> {
    this.inner.id().map(i64::from)
}

#[rune::function(instance)]
fn output(this: &AgentSession) -> Option<AgentOutput> {
    this.inner.output().map(AgentOutput::from)
}

fn secs(value: i64) -> Duration {
    Duration::from_secs(value.max(0) as u64)
}

#[derive(Any)]
#[rune(item = ::gage)]
pub(crate) struct CallAgent {
    #[rune(skip)]
    prompt: String,
    #[rune(skip)]
    name: Option<String>,
    #[rune(skip)]
    model: Option<String>,
    #[rune(skip)]
    max_turns: Option<u32>,
    #[rune(skip)]
    timeout: Option<usize>,
}

impl CallAgent {
    #[rune::function(instance)]
    fn name(mut self, name: String) -> Self {
        self.name = Some(name);
        self
    }

    #[rune::function(instance)]
    fn model(mut self, model: String) -> Self {
        self.model = Some(model);
        self
    }

    #[rune::function(instance)]
    fn max_turns(mut self, max_turns: i64) -> Self {
        self.max_turns = Some(max_turns.max(0) as u32);
        self
    }

    #[rune::function(instance)]
    fn timeout(mut self, timeout: i64) -> Self {
        self.timeout = Some(timeout.max(0) as usize);
        self
    }
}

#[rune::function]
fn agent(prompt: String) -> CallAgent {
    CallAgent {
        prompt,
        name: None,
        model: None,
        max_turns: None,
        timeout: None,
    }
}

pub(crate) fn register(m: &mut Module) -> Result<(), ContextError> {
    m.ty::<AgentSession>()?;
    m.ty::<AgentOutput>()?;
    m.ty::<AgentStatus>()?;
    m.function_meta(AgentStatus::success)?;
    m.function_meta(AgentStatus::code)?;
    m.function_meta(AgentStatus::debug)?;
    m.function_meta(AgentOutput::debug)?;
    m.function_meta(wait)?;
    m.function_meta(kill)?;
    m.function_meta(id)?;
    m.function_meta(output)?;

    m.ty::<CallAgent>()?;
    m.function_meta(CallAgent::name)?;
    m.function_meta(CallAgent::model)?;
    m.function_meta(CallAgent::max_turns)?;
    m.function_meta(CallAgent::timeout)?;
    m.function_meta(agent)?;
    m.associated_function(&Protocol::INTO_FUTURE, |c: CallAgent| async move {
        do_call_agent(c).await
    })?;

    Ok(())
}

async fn do_call_agent(c: CallAgent) -> super::Result<AgentSession> {
    let name = c.name.unwrap_or_else(|| DEFAULT_NAME.to_string());
    let scan_id = current_scan_ctx().run.scan_id.clone();
    let sandbox = scan_sandbox_spec(&scan_id).map_err(|e| Error::Agent(e.to_string()))?;

    let mut builder = AgentBuilder::new()
        .name(name)
        .sandbox(sandbox)
        .tools(ToolPolicy::default_interactive());
    if let Some(m) = c.model {
        builder = builder.model(anthropic::resolve_model(&m).to_string());
    }
    if let Some(n) = c.max_turns {
        builder = builder.max_turns(n);
    }
    if let Some(t) = c.timeout {
        builder = builder.timeout(t);
    }
    let inner = builder
        .build()
        .start_session(&c.prompt)
        .await
        .map_err(|e| Error::Agent(e.to_string()))?;
    Ok(AgentSession { inner })
}

/// Build a [`SandboxSpec`] restricting the sandbox to the rows recorded
/// by the running scan: its `scan_session`, `scan_note`, and
/// `scan_issue` sets. Reads the canonical gage db once.
fn scan_sandbox_spec(scan_id: &str) -> Result<SandboxSpec, String> {
    let conn = gage_db::db::open_db().map_err(|e| e.to_string())?;
    let sessions: HashSet<String> = gage_db::scan::session_ids_for_scan(&conn, scan_id)
        .map_err(|e| e.to_string())?
        .into_iter()
        .collect();
    let notes: HashSet<String> = gage_db::scan::note_ids_for_scan(&conn, scan_id)
        .map_err(|e| e.to_string())?
        .into_iter()
        .collect();
    let issues: HashSet<String> = gage_db::scan::issue_ids_for_scan(&conn, scan_id)
        .map_err(|e| e.to_string())?
        .into_iter()
        .collect();
    Ok(SandboxSpec {
        sessions: Some(sessions),
        notes: Some(notes),
        issues: Some(issues),
    })
}

#[cfg(test)]
mod tests {
    use rune::Module;

    #[test]
    fn registers_into_module() {
        // Building the full gage module exercises register(): a duplicate
        // name or malformed function meta would surface here.
        let mut m = Module::with_crate("gage").unwrap();
        super::register(&mut m).unwrap();
    }
}
