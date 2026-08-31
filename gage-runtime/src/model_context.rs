//! `model_context(model)` — Rune builder that, on await, probes the
//! context window of a `claude` invocation via `claude -p /context`
//! and returns the parsed numbers as a [`ModelContext`].
//!
//! The probe runs through the same sandboxed spawn path agents use
//! ([`gage_agent::Agent::run_print`]) with the same invocation shape
//! (model resolution and system prompt handling, shared via
//! [`crate::agent::apply_call_shape`]), so the numbers reflect the
//! exact configuration a subsequent `call_agent` runs with. Results
//! are cached on the `RunContext` for the life of the scan; they are
//! meaningless outside the scan process.

use gage_agent::{AgentBuilder as GageAgentBuilder, SystemPrompt};
use rune::Any;
use rune::alloc::fmt::TryWrite;
use rune::runtime::{Formatter, Protocol, Ref, VmError};
use rune::{ContextError, Module};

use crate::agent::{apply_call_shape, with_fault_barrier};
use crate::error::Error;
use crate::state::current_scan_ctx;

pub(crate) fn register(m: &mut Module) -> Result<(), ContextError> {
    m.ty::<ModelContext>()?;
    m.function_meta(ModelContext::debug)?;
    m.ty::<ModelContextCall>()?;
    m.function_meta(ModelContextCall::system_prompt)?;
    m.function_meta(ModelContextCall::default_system_prompt)?;
    m.function_meta(ModelContextCall::default_system_prompt_append)?;
    m.function_meta(model_context)?;
    m.associated_function(&Protocol::INTO_FUTURE, |c: ModelContextCall| async move {
        with_fault_barrier(do_model_context(c)).await
    })?;
    Ok(())
}

#[rune::function]
fn model_context(model: Ref<str>) -> ModelContextCall {
    ModelContextCall {
        model: model.to_owned(),
        system_prompt: SystemPrompt::Empty,
        system_prompt_append: None,
    }
}

/// Builder produced by `model_context(model)`. Accumulates the system
/// prompt configuration; `await` runs the probe (or returns the run's
/// cached result) and yields a [`ModelContext`].
#[derive(Any)]
#[rune(item = ::gage)]
pub(crate) struct ModelContextCall {
    #[rune(skip)]
    model: String,
    #[rune(skip)]
    system_prompt: SystemPrompt,
    #[rune(skip)]
    system_prompt_append: Option<String>,
}

impl ModelContextCall {
    #[rune::function(instance)]
    fn system_prompt(mut self, s: Ref<str>) -> Self {
        self.system_prompt = SystemPrompt::Custom(s.to_owned());
        self
    }

    /// Use Claude Code's default system prompt instead of the empty
    /// default.
    #[rune::function(instance)]
    fn default_system_prompt(mut self) -> Self {
        self.system_prompt = SystemPrompt::ClaudeDefault;
        self
    }

    /// Append to Claude Code's default system prompt (implies it).
    #[rune::function(instance)]
    fn default_system_prompt_append(mut self, s: Ref<str>) -> Self {
        self.system_prompt = SystemPrompt::ClaudeDefault;
        self.system_prompt_append = Some(s.to_owned());
        self
    }
}

async fn do_model_context(c: ModelContextCall) -> crate::Result<ModelContext> {
    // Refuse when the run-wide agent fault is set — the probe spawns a
    // claude child like any agent call.
    let ctx = current_scan_ctx();
    if let Some(msg) = ctx.run.agent_fault.get() {
        tracing::debug!("model_context refused: run-wide agent fault is set");
        return Err(Error::Agent(msg.clone()));
    }

    let resolved_model = ctx.run.model_map.resolve(Some(&c.model));
    let key = cache_key(&resolved_model, &c.system_prompt, &c.system_prompt_append);
    if let Some(cached) = ctx.run.model_contexts.lock().unwrap().get(&key) {
        return Ok(cached.clone());
    }

    // The probe holds an agent-pool permit while the claude child
    // runs. No pool events are sent: those feed per-task agent
    // occupancy in the UI and the probe is not an agent call.
    let _permit = ctx
        .run
        .agent_pool
        .clone()
        .acquire_owned()
        .await
        .expect("agent_pool is closed only at process shutdown");

    let builder = apply_call_shape(
        GageAgentBuilder::new().name("model-context"),
        resolved_model.clone(),
        &c.system_prompt,
        &c.system_prompt_append,
    );
    let output = tokio::task::spawn_blocking(move || builder.build().run_print("/context"))
        .await
        .unwrap()
        .map_err(|e| Error::Agent(format!("model_context: claude spawn failed: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(Error::Agent(format!(
            "model_context: claude exited with {}: {}",
            output.status,
            stderr.trim()
        )));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let Some(parsed) = parse_context_output(&stdout) else {
        // Unparseable output invalidates every context-sized agent
        // call for the rest of the run, so it disables the agent
        // facility like an auth failure does. The barrier upgrades
        // the returned error to an uncatchable VM abort.
        let msg = format!("model_context: unparseable /context output for model {resolved_model}");
        if ctx.run.agent_fault.set(msg.clone()).is_ok() {
            tracing::error!("disabling agent calls for this scan: {msg}");
        }
        tracing::debug!(output = %stdout, "unparseable /context output");
        return Err(Error::Agent(msg));
    };

    ctx.run
        .model_contexts
        .lock()
        .unwrap()
        .insert(key, parsed.clone());
    Ok(parsed)
}

fn cache_key(
    resolved_model: &str,
    system_prompt: &SystemPrompt,
    system_prompt_append: &Option<String>,
) -> String {
    let sp = match system_prompt {
        SystemPrompt::Empty => "empty".to_string(),
        SystemPrompt::ClaudeDefault => "default".to_string(),
        SystemPrompt::Custom(s) => format!("custom:{s}"),
    };
    let append = system_prompt_append.as_deref().unwrap_or("");
    format!("{resolved_model}\u{0}{sp}\u{0}{append}")
}

/// Parse claude's `/context` report. `model` comes from the
/// `**Model:**` line, `limit_tokens` from the total on the
/// `**Tokens:** <used> / <limit>` line, and `free_tokens` from the
/// `Free space` table row — the actionable budget, which already
/// excludes the autocompact buffer.
fn parse_context_output(text: &str) -> Option<ModelContext> {
    let mut model = None;
    let mut limit_tokens = None;
    let mut free_tokens = None;
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("**Model:**") {
            model = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("**Tokens:**") {
            let (_used, rest) = rest.split_once('/')?;
            let limit = rest.split('(').next()?;
            limit_tokens = Some(parse_tokens(limit)?);
        } else if line.starts_with('|') {
            let mut cells = line.split('|').map(str::trim);
            cells.next()?; // leading empty cell
            if cells.next() == Some("Free space") {
                free_tokens = Some(parse_tokens(cells.next()?)?);
            }
        }
    }
    Some(ModelContext {
        model: model?,
        free_tokens: free_tokens?,
        limit_tokens: limit_tokens?,
    })
}

/// One token count from the report: a plain number or a decimal with
/// a `k`/`m` suffix (`188`, `30.2k`, `1m`).
fn parse_tokens(s: &str) -> Option<i64> {
    let s = s.trim();
    let (num, mult) = match s.strip_suffix(['k', 'K']) {
        Some(n) => (n, 1_000.0),
        None => match s.strip_suffix(['m', 'M']) {
            Some(n) => (n, 1_000_000.0),
            None => (s, 1.0),
        },
    };
    let v = num.trim().parse::<f64>().ok()?;
    if !v.is_finite() || v < 0.0 {
        return None;
    }
    Some((v * mult).round() as i64)
}

/// Context probe result. Cached per scan on the `RunContext`.
#[derive(Any, Clone)]
#[rune(item = ::gage)]
pub struct ModelContext {
    /// Model name reported by claude (e.g. `claude-sonnet-5`).
    #[rune(get)]
    pub model: String,
    /// Tokens free for prompt content: the report's `Free space` row,
    /// which excludes claude's autocompact buffer.
    #[rune(get, copy)]
    pub free_tokens: i64,
    /// Total context window for the model.
    #[rune(get, copy)]
    pub limit_tokens: i64,
}

impl ModelContext {
    #[rune::function(protocol = DEBUG_FMT)]
    fn debug(&self, f: &mut Formatter) -> Result<(), VmError> {
        write!(
            f,
            "ModelContext {{ model: {:?}, free_tokens: {}, limit_tokens: {} }}",
            self.model, self.free_tokens, self.limit_tokens
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_context_output, parse_tokens};

    // Captured from `claude -p /context --model sonnet` (2026-08-30),
    // trimmed to the parsed shape.
    const SAMPLE: &str = "\
## Context Usage

**Model:** claude-sonnet-5
**Tokens:** 30.2k / 1m (3%)

### Estimated usage by category

| Category | Tokens | Percentage |
|----------|--------|------------|
| System prompt | 188 | 0.0% |
| Messages | 8 | 0.0% |
| Free space | 936.8k | 93.7% |
| Autocompact buffer | 33k | 3.3% |
";

    #[test]
    fn parses_sample_report() {
        let ctx = parse_context_output(SAMPLE).unwrap();
        assert_eq!(ctx.model, "claude-sonnet-5");
        assert_eq!(ctx.limit_tokens, 1_000_000);
        assert_eq!(ctx.free_tokens, 936_800);
    }

    #[test]
    fn missing_free_space_row_is_unparseable() {
        let text = "**Model:** m\n**Tokens:** 1k / 2k (50%)\n";
        assert!(parse_context_output(text).is_none());
    }

    #[test]
    fn token_values() {
        assert_eq!(parse_tokens("188"), Some(188));
        assert_eq!(parse_tokens("30.2k"), Some(30_200));
        assert_eq!(parse_tokens(" 1m "), Some(1_000_000));
        assert_eq!(parse_tokens("936.8k"), Some(936_800));
        assert_eq!(parse_tokens(""), None);
        assert_eq!(parse_tokens("abc"), None);
        assert_eq!(parse_tokens("-1k"), None);
    }
}
