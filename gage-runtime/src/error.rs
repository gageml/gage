//! Typed error values returned from runtime functions to Rune scripts.
//!
//! Variants are kept tight: a new kind lands only when an existing
//! call site needs to surface it. Construction from Rune is via the
//! per-variant `#[rune(constructor)]`; pattern matching uses
//! `::gage::Error::<Variant>`.

use rune::alloc::fmt::TryWrite;
use rune::runtime::{Formatter, VmError};
use rune::{Any, ContextError, Module};

#[derive(Any, Clone)]
#[rune(item = ::gage)]
pub enum Error {
    /// Bad argument from script: missing field, wrong type, out of range.
    #[rune(constructor)]
    Args(#[rune(get)] String),

    /// Database operation failed (insert, query, ...).
    #[rune(constructor)]
    Db(#[rune(get)] String),

    /// Required configuration missing or invalid.
    #[rune(constructor)]
    Config(#[rune(get)] String),

    /// Network failure before/around the HTTP exchange (DNS, TCP, TLS, timeout).
    #[rune(constructor)]
    Network(#[rune(get)] String),

    /// HTTP error from an upstream call.
    #[rune(constructor)]
    Http {
        #[rune(get)]
        status: i64,
        #[rune(get)]
        body: String,
    },

    /// Response decoding failed (JSON parse, schema mismatch).
    #[rune(constructor)]
    Decode(#[rune(get)] String),

    /// Template rendering failed.
    #[rune(constructor)]
    Template(#[rune(get)] String),

    /// Agent subprocess failure (spawn, wait timeout, sandbox merge).
    #[rune(constructor)]
    Agent(#[rune(get)] String),
}

impl Error {
    #[rune::function(protocol = DEBUG_FMT)]
    fn debug(&self, f: &mut Formatter) -> Result<(), VmError> {
        write!(f, "{self:?}")?;
        Ok(())
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Args(m) => write!(f, "args: {m}"),
            Error::Db(m) => write!(f, "db: {m}"),
            Error::Config(m) => write!(f, "config: {m}"),
            Error::Network(m) => write!(f, "network: {m}"),
            Error::Http { status, body } => write!(f, "http {status}: {body}"),
            Error::Decode(m) => write!(f, "decode: {m}"),
            Error::Template(m) => write!(f, "template: {m}"),
            Error::Agent(m) => write!(f, "agent: {m}"),
        }
    }
}

// Debug carries the detail a programmer needs when a task returns an
// unexpected `Err`: `render_task_error` renders failed tasks with `{:?}`.
impl std::fmt::Debug for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Args(m) => write!(f, "Args({m:?})"),
            Error::Db(m) => write!(f, "Db({m:?})"),
            Error::Config(m) => write!(f, "Config({m:?})"),
            Error::Network(m) => write!(f, "Network({m:?})"),
            Error::Http { status, body } => f
                .debug_struct("Http")
                .field("status", status)
                .field("body", body)
                .finish(),
            Error::Decode(m) => write!(f, "Decode({m:?})"),
            Error::Template(m) => write!(f, "Template({m:?})"),
            Error::Agent(m) => write!(f, "Agent({m:?})"),
        }
    }
}

impl std::error::Error for Error {}

pub(crate) fn register_types(m: &mut Module) -> Result<(), ContextError> {
    m.ty::<Error>()?;
    m.function_meta(Error::debug)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_args() {
        assert_eq!(Error::Args("foo".into()).to_string(), "args: foo");
    }

    #[test]
    fn display_db() {
        assert_eq!(Error::Db("locked".into()).to_string(), "db: locked");
    }

    #[test]
    fn display_config() {
        assert_eq!(
            Error::Config("missing key".into()).to_string(),
            "config: missing key"
        );
    }

    #[test]
    fn display_network() {
        assert_eq!(
            Error::Network("timeout".into()).to_string(),
            "network: timeout"
        );
    }

    #[test]
    fn display_http() {
        let e = Error::Http {
            status: 404,
            body: "not found".into(),
        };
        assert_eq!(e.to_string(), "http 404: not found");
    }

    #[test]
    fn display_decode() {
        assert_eq!(
            Error::Decode("bad json".into()).to_string(),
            "decode: bad json"
        );
    }

    #[test]
    fn display_template() {
        assert_eq!(
            Error::Template("bad syntax".into()).to_string(),
            "template: bad syntax"
        );
    }
}
