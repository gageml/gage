//! `native_session(source)` --- a table function over a driver source.
//!
//! Enumerates the native sessions a source exposes, so a user can see
//! what a source holds and choose which to add to the store. The rows
//! are the driver's native `session` table, obtained by opening the
//! source through the builtin driver registry and taking the provider
//! it returns. `native_session()` with no argument uses the default
//! source; `native_session('claude:/path')` names one.

use std::sync::Arc;

use datafusion::catalog::TableFunctionImpl;
use datafusion::common::ScalarValue;
use datafusion::datasource::TableProvider;
use datafusion::error::{DataFusionError, Result};
use datafusion::prelude::Expr;
use gage_registry::driver::DriverRegistry;

use crate::system_cols::SkipSystemCols;

#[derive(Debug)]
pub struct NativeSessionFn {
    /// Hide the system columns of the returned table
    pub skip_system_cols: bool,
}

impl TableFunctionImpl for NativeSessionFn {
    fn call(&self, args: &[Expr]) -> Result<Arc<dyn TableProvider>> {
        let source = match args {
            [] => String::new(),
            [arg] => string_literal(arg).ok_or_else(|| {
                DataFusionError::Plan(
                    "native_session(source): source must be a string literal".into(),
                )
            })?,
            _ => {
                return Err(DataFusionError::Plan(
                    "native_session takes at most one argument".into(),
                ));
            }
        };
        let registry = DriverRegistry::builtin();
        let opened = registry.open_source(&source).map_err(external)?;
        // The provider holds its own handle to the driver's index; the
        // opened source may drop once the tables are taken.
        let tables = opened.tables().map_err(external)?;
        if self.skip_system_cols {
            Ok(Arc::new(SkipSystemCols::new(tables.session)))
        } else {
            Ok(tables.session)
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
