//! What a harness tells a process it hosts about the session the
//! process runs in.
//!
//! A harness names the current session in its own way: in the
//! environment of the process it starts, or in the `_meta` of each
//! MCP tool call it makes. A [`HostContext`] carries both so that
//! [`crate::Driver::current_session`] can read whichever its harness
//! uses.

use std::env;

use serde_json::{Map, Value};

/// The environment a process was started with, and the `_meta` of the
/// MCP tool call being served when there is one.
#[derive(Debug, Clone, Copy)]
pub struct HostContext<'a> {
    env: Env<'a>,
    call_meta: Option<&'a Map<String, Value>>,
}

#[derive(Debug, Clone, Copy)]
enum Env<'a> {
    /// The environment of this process
    Process,
    /// The given variables and no others
    Vars(&'a [(&'a str, &'a str)]),
}

impl<'a> HostContext<'a> {
    /// The context of this process: its own environment, and no tool
    /// call.
    pub fn process() -> Self {
        Self {
            env: Env::Process,
            call_meta: None,
        }
    }

    /// A context whose environment is `vars` alone, and no tool call.
    pub fn with_vars(vars: &'a [(&'a str, &'a str)]) -> Self {
        Self {
            env: Env::Vars(vars),
            call_meta: None,
        }
    }

    /// The context while serving the MCP tool call whose request
    /// `_meta` is `meta`.
    pub fn serving_call(mut self, meta: &'a Map<String, Value>) -> Self {
        self.call_meta = Some(meta);
        self
    }

    /// The value of the environment variable `name`. A variable that
    /// is unset, empty, or not Unicode is `None`.
    pub fn var(&self, name: &str) -> Option<String> {
        let value = match self.env {
            Env::Process => env::var(name).ok()?,
            Env::Vars(vars) => vars.iter().find(|(n, _)| *n == name)?.1.to_string(),
        };
        (!value.is_empty()).then_some(value)
    }

    /// The `_meta` object of the MCP tool call being served. `None`
    /// outside a tool call.
    pub fn call_meta(&self) -> Option<&'a Map<String, Value>> {
        self.call_meta
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn var_reads_the_given_variables_and_treats_empty_as_unset() {
        let vars = [("A", "one"), ("B", "")];
        let host = HostContext::with_vars(&vars);
        assert_eq!(host.var("A").as_deref(), Some("one"));
        assert_eq!(host.var("B"), None);
        assert_eq!(host.var("C"), None);
    }

    #[test]
    fn call_meta_is_present_only_while_serving_a_call() {
        let host = HostContext::with_vars(&[]);
        assert!(host.call_meta().is_none());
        let meta = json!({"k": "v"});
        let host = host.serving_call(meta.as_object().unwrap());
        assert_eq!(host.call_meta().unwrap()["k"], "v");
    }
}
