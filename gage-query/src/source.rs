//! A driver source as a source-scoped context reads it.
//!
//! The source is opened through the builtin driver registry. `''`
//! names the default source and `'claude:/path'` names one.

use std::path::{Path, PathBuf};

use datafusion::common::ScalarValue;
use datafusion::prelude::Expr;
use gage_core::cache::STORE_SCHEME;
use gage_registry::driver::DriverRegistry;
use gage_session::{DriverError, Source, SourceOptions, split_scheme};

/// The source a query names to read in place of the main store. A
/// named source is read only: a store is opened with
/// [`gage_store::Store::open_read_only`] and its context is built as
/// any store's, and a driver source is built with
/// [`ContextBuilder::build_source`](crate::ContextBuilder::build_source).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuerySource<'a> {
    /// `gage:<path>`: the Gage store at `<path>`
    Store(&'a Path),
    /// Any other value: a driver source, as the driver registry
    /// routes it
    Driver(&'a str),
}

impl<'a> QuerySource<'a> {
    pub fn from_spec(source: &'a str) -> Self {
        match split_scheme(source) {
            Some((STORE_SCHEME, path)) => QuerySource::Store(Path::new(path)),
            _ => QuerySource::Driver(source),
        }
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_store_scheme_names_a_store_and_any_other_value_a_driver_source() {
        assert_eq!(
            QuerySource::from_spec("gage:/srv/zuni.git"),
            QuerySource::Store(Path::new("/srv/zuni.git"))
        );
        for spec in ["", "claude:", "claude:/home/a/.claude", "/srv/zuni.git"] {
            assert_eq!(QuerySource::from_spec(spec), QuerySource::Driver(spec));
        }
    }
}
