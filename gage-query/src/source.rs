//! A driver source as a source-scoped context reads it.
//!
//! The source is opened through the builtin driver registry. `''`
//! names the default source and `'claude:/path'` names one.

use std::path::PathBuf;

use datafusion::common::ScalarValue;
use datafusion::prelude::Expr;
use gage_registry::driver::DriverRegistry;
use gage_session::{DriverError, Source, SourceOptions};

/// A driver source and how to open it
#[derive(Debug, Clone)]
pub(crate) struct SourceSpec {
    pub source: String,
    /// Read the runtime's own in-flight agent sessions too
    pub show_agents: bool,
    /// Where the opened source keeps its derived state; `None` is the
    /// driver's default root
    pub cache_root: Option<PathBuf>,
}

impl SourceSpec {
    pub fn open(&self) -> Result<Box<dyn Source>, DriverError> {
        let mut options = SourceOptions::new(&self.source);
        options.show_agents(self.show_agents);
        if let Some(root) = &self.cache_root {
            options.cache_root(root);
        }
        DriverRegistry::builtin().open_source(&options)
    }
}

pub(crate) fn string_literal(e: &Expr) -> Option<String> {
    match e {
        Expr::Literal(ScalarValue::Utf8(Some(s)), _)
        | Expr::Literal(ScalarValue::LargeUtf8(Some(s)), _)
        | Expr::Literal(ScalarValue::Utf8View(Some(s)), _) => Some(s.clone()),
        _ => None,
    }
}
