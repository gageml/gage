//! `native_session(source)`, `native_message(source)`, and
//! `native_entry(source)` --- table functions over a driver source.
//!
//! Each returns one of the driver's native tables for a source, so a
//! user can see what a source holds and choose which sessions to add
//! to the store. The source is opened through the builtin driver
//! registry and the provider taken from what it returns. An empty or
//! omitted argument names the default source; `'claude:/path'` names
//! one.

use std::sync::Arc;

use datafusion::catalog::TableFunctionImpl;
use datafusion::common::ScalarValue;
use datafusion::datasource::TableProvider;
use datafusion::error::{DataFusionError, Result};
use datafusion::prelude::Expr;
use gage_registry::driver::DriverRegistry;
use gage_session::DriverTables;

use crate::system_cols::SkipSystemCols;

/// Which of a driver's native tables a function returns
#[derive(Debug, Clone, Copy)]
pub enum NativeTable {
    Session,
    Message,
    Entry,
}

impl NativeTable {
    pub fn function_name(self) -> &'static str {
        match self {
            NativeTable::Session => "native_session",
            NativeTable::Message => "native_message",
            NativeTable::Entry => "native_entry",
        }
    }

    fn select(self, tables: DriverTables) -> Arc<dyn TableProvider> {
        match self {
            NativeTable::Session => tables.session,
            NativeTable::Message => tables.message,
            NativeTable::Entry => tables.entry,
        }
    }
}

#[derive(Debug)]
pub struct NativeTableFn {
    pub table: NativeTable,
    /// Hide the system columns of the returned table
    pub skip_system_cols: bool,
}

impl TableFunctionImpl for NativeTableFn {
    fn call(&self, args: &[Expr]) -> Result<Arc<dyn TableProvider>> {
        let name = self.table.function_name();
        let source = match args {
            [] => String::new(),
            [arg] => string_literal(arg).ok_or_else(|| {
                DataFusionError::Plan(format!("{name}(source): source must be a string literal"))
            })?,
            _ => {
                return Err(DataFusionError::Plan(format!(
                    "{name} takes at most one argument"
                )));
            }
        };
        let registry = DriverRegistry::builtin();
        let opened = registry.open_source(&source).map_err(external)?;
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
