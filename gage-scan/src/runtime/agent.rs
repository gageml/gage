//! `call_agent_judge` — spawn an isolated judge `claude` process from a
//! scanner and drive it through the `AgentSession` handle.

use std::time::Duration;

use gage_agent::{AgentOutput as Output, AgentSession as Session, JudgeOpts};
use rune::alloc::fmt::TryWrite;
use rune::alloc::prelude::TryClone;
use rune::runtime::{Formatter, Mut, Object, VmError};
use rune::{Any, ContextError, Module};

use crate::runtime::error::Error;

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
async fn wait(mut this: Mut<AgentSession>, timeout_secs: i64) -> super::Result<AgentOutput> {
    let out = this
        .inner
        .wait(secs(timeout_secs))
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

    m.function(
        "call_agent_judge",
        |prompt: String, opts: Object| async move { do_call_agent_judge(prompt, opts).await },
    )
    .build()?;

    Ok(())
}

async fn do_call_agent_judge(prompt: String, opts: Object) -> super::Result<AgentSession> {
    let name = opt_string(&opts, "name")?.unwrap_or_else(|| DEFAULT_NAME.to_string());
    let model = opt_string(&opts, "model")?;
    let max_turns = opt_i64(&opts, "max_turns")?.map(|v| v as u32);

    let inner = gage_agent::spawn_judge(
        &prompt,
        &JudgeOpts {
            name,
            model,
            max_turns,
        },
    )
    .await
    .map_err(|e| Error::Agent(e.to_string()))?;
    Ok(AgentSession { inner })
}

fn opt_string(obj: &Object, field: &str) -> super::Result<Option<String>> {
    match obj.get(field) {
        Some(v) => v
            .borrow_string_ref()
            .map(|s| Some(s.to_string()))
            .map_err(|_e| Error::Args(format!("call_agent_judge: '{field}' must be a string"))),
        None => Ok(None),
    }
}

fn opt_i64(obj: &Object, field: &str) -> super::Result<Option<i64>> {
    use rune::runtime::FromValue;
    match obj.get(field) {
        Some(v) => i64::from_value(v.clone())
            .map(Some)
            .map_err(|_e| Error::Args(format!("call_agent_judge: '{field}' must be an integer"))),
        None => Ok(None),
    }
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
