//! `native_session(source)`, `native_message(source)`, and
//! `native_entry(source)`, and `native_tool_use(source)` --- table
//! functions over a driver source.
//!
//! Each returns one of the driver's native tables for a source, so a
//! user can see what a source holds and choose which sessions to add
//! to the store. The source is opened through the builtin driver
//! registry and the provider taken from what it returns. The argument
//! is required: `''` names the default source and `'claude:/path'`
//! names one.

use std::sync::Arc;

use datafusion::catalog::TableFunctionImpl;
use datafusion::common::ScalarValue;
use datafusion::datasource::TableProvider;
use datafusion::error::{DataFusionError, Result};
use datafusion::prelude::Expr;
use gage_registry::driver::DriverRegistry;
use gage_session::{DriverTables, SourceOptions};

use crate::system_cols::SkipSystemCols;

/// Which of a driver's native tables a function returns
#[derive(Debug, Clone, Copy)]
pub enum NativeTable {
    Session,
    Message,
    Entry,
    ToolUse,
}

impl NativeTable {
    pub fn function_name(self) -> &'static str {
        match self {
            NativeTable::Session => "native_session",
            NativeTable::Message => "native_message",
            NativeTable::Entry => "native_entry",
            NativeTable::ToolUse => "native_tool_use",
        }
    }

    fn select(self, tables: DriverTables) -> Arc<dyn TableProvider> {
        match self {
            NativeTable::Session => tables.session,
            NativeTable::Message => tables.message,
            NativeTable::Entry => tables.entry,
            NativeTable::ToolUse => tables.tool_use,
        }
    }
}

#[derive(Debug)]
pub struct NativeTableFn {
    pub table: NativeTable,
    /// Hide the system columns of the returned table
    pub skip_system_cols: bool,
    /// Read the runtime's own in-flight agent sessions too
    pub show_agents: bool,
}

impl TableFunctionImpl for NativeTableFn {
    fn call(&self, args: &[Expr]) -> Result<Arc<dyn TableProvider>> {
        let name = self.table.function_name();
        let [arg] = args else {
            return Err(DataFusionError::Plan(format!(
                "{name}(source) takes one argument; '' names the default source"
            )));
        };
        let source = string_literal(arg).ok_or_else(|| {
            DataFusionError::Plan(format!("{name}(source): source must be a string literal"))
        })?;
        let registry = DriverRegistry::builtin();
        let mut options = SourceOptions::new(&source);
        options.show_agents(self.show_agents);
        let opened = registry.open_source(&options).map_err(external)?;
        // The provider holds its own handle to the driver's index; the
        // opened source may drop once the tables are taken.
        let table = self.table.select(opened.tables().map_err(external)?);
        if self.skip_system_cols {
            Ok(Arc::new(SkipSystemCols::new(table)))
        } else {
            Ok(table)
        }
    }
}

fn string_literal(e: &Expr) -> Option<String> {
    match e {
        Expr::Literal(ScalarValue::Utf8(Some(s)), _)
        | Expr::Literal(ScalarValue::LargeUtf8(Some(s)), _)
        | Expr::Literal(ScalarValue::Utf8View(Some(s)), _) => Some(s.clone()),
        _ => None,
    }
}

fn external(e: gage_session::DriverError) -> DataFusionError {
    DataFusionError::External(Box::new(e))
}

#[cfg(test)]
mod tests {
    use datafusion::prelude::lit;

    use super::*;

    fn session_fn() -> NativeTableFn {
        NativeTableFn {
            table: NativeTable::Session,
            skip_system_cols: false,
            show_agents: false,
        }
    }

    #[test]
    fn source_argument_is_required() {
        let err = session_fn().call(&[]).err().unwrap();
        assert!(
            matches!(err, DataFusionError::Plan(ref m) if m.contains("'' names the default source")),
            "{err}"
        );
    }

    #[test]
    fn a_second_argument_is_rejected() {
        let err = session_fn().call(&[lit(""), lit("")]).err().unwrap();
        assert!(matches!(err, DataFusionError::Plan(_)), "{err}");
    }

    #[test]
    fn source_must_be_a_string_literal() {
        let err = session_fn().call(&[lit(1)]).err().unwrap();
        assert!(
            matches!(err, DataFusionError::Plan(ref m) if m.contains("string literal")),
            "{err}"
        );
    }
}
