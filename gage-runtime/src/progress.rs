//! Task progress reporting for Rune scanners.
//!
//! `Progress` is opt-in, fire-and-forget reporting: each call sends an
//! absolute `(pos, total)` snapshot through the task's `runtime_tx`.
//! The runner folds the latest snapshot into its run status; a task
//! that never reports renders as indeterminate. There is no guarantee
//! a consumer is listening — in particular, calls made inside a
//! scanner tool callback (dispatched during `call_agent`) run under a
//! per-call context whose output channel has no receiver, and are
//! effectively no-ops.
//!
//! Two forms:
//!
//! ```rune
//! // Counter form
//! let p = Progress::new(sessions.len());
//! for s in sessions {
//!     handle(s).await?;
//!     p.tick();
//! }
//!
//! // Iterator form — wraps any exact-size iterable
//! for s in Progress::iter(scan().sessions()) {
//!     handle(s).await?;
//! }
//! ```

use rune::runtime::{Iterator as RuneIterator, Value, VmError};
use rune::{Any, ContextError, Module};

use crate::RuntimeOutput;
use crate::state::current_scan_ctx;

pub(crate) fn register_types(m: &mut Module) -> Result<(), ContextError> {
    m.ty::<Progress>()?;
    m.function_meta(Progress::new__meta)?;
    m.function_meta(Progress::iter__meta)?;
    m.function_meta(Progress::tick__meta)?;
    m.function_meta(Progress::inc__meta)?;
    m.function_meta(Progress::set__meta)?;
    m.function_meta(Progress::reset__meta)?;
    m.function_meta(Progress::next__meta)?;
    m.function_meta(Progress::size_hint__meta)?;
    m.implement_trait::<Progress>(rune::item!(::std::iter::Iterator))?;
    Ok(())
}

#[derive(Any)]
#[rune(item = ::gage)]
pub struct Progress {
    #[rune(skip)]
    pos: u64,
    #[rune(skip)]
    total: u64,
    /// Wrapped iterable for the `iter` form; None for the counter form.
    #[rune(skip)]
    inner: Option<RuneIterator>,
}

impl Progress {
    /// Counter form: announce a total, advance with `tick`/`inc`/`set`.
    #[rune::function(keep, path = Self::new)]
    fn new(total: u64) -> Progress {
        let p = Progress {
            pos: 0,
            total,
            inner: None,
        };
        p.report();
        p
    }

    /// Iterator form: wrap an exact-size iterable and tick per item.
    /// Errors when the iterable's size hint is not exact — without a
    /// total there is nothing to report (absent progress already
    /// renders as indeterminate).
    #[rune::function(keep, path = Self::iter)]
    fn iter(inner: RuneIterator) -> Result<Progress, VmError> {
        let total = match inner.size_hint()? {
            (lo, Some(hi)) if lo == hi => lo as u64,
            _ => {
                return Err(VmError::panic(
                    "Progress::iter requires an exact-size iterable",
                ));
            }
        };
        let p = Progress {
            pos: 0,
            total,
            inner: Some(inner),
        };
        p.report();
        Ok(p)
    }

    /// Advance position by one and report.
    #[rune::function(keep, instance)]
    fn tick(&mut self) {
        self.inc(1);
    }

    /// Advance position by `n` and report.
    #[rune::function(keep, instance)]
    fn inc(&mut self, n: u64) {
        self.pos = self.pos.saturating_add(n);
        self.report();
    }

    /// Set the absolute position and report.
    #[rune::function(keep, instance)]
    fn set(&mut self, pos: u64) {
        self.pos = pos;
        self.report();
    }

    /// Restart at zero with a new total and report.
    #[rune::function(keep, instance)]
    fn reset(&mut self, total: u64) {
        self.pos = 0;
        self.total = total;
        self.report();
    }

    #[rune::function(keep, instance, protocol = NEXT)]
    fn next(&mut self) -> Result<Option<Value>, VmError> {
        let Some(inner) = &mut self.inner else {
            return Err(VmError::panic(
                "Progress built with new() is not an iterator; use Progress::iter",
            ));
        };
        let item = inner.next()?;
        if item.is_some() {
            self.pos = self.pos.saturating_add(1);
            self.report();
        }
        Ok(item)
    }

    #[rune::function(keep, instance, protocol = SIZE_HINT)]
    fn size_hint(&self) -> (usize, Option<usize>) {
        let len = self.total.saturating_sub(self.pos) as usize;
        (len, Some(len))
    }

    /// Send the current snapshot. Fire-and-forget: a closed channel
    /// means no consumer, which is the documented contract.
    fn report(&self) {
        let ctx = current_scan_ctx();
        #[allow(clippy::let_underscore_must_use)]
        let _ = ctx.runtime_tx.send(RuntimeOutput::Progress {
            scanner: ctx.scanner_name.clone(),
            task: ctx.task_name.clone(),
            pos: self.pos,
            total: self.total,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use rune::sync::Arc;
    use rune::{Context, Vm};

    // The happy paths (counter and iterator forms) need a scan context
    // and live in scanners/tests.rn. The inexact-size runtime error
    // raises before any report, so it is testable without one — and a
    // runtime error can't be asserted from Rune anyway.
    fn call(expr: &str) -> Result<Value, VmError> {
        let mut context = Context::with_default_modules().unwrap();
        context.install(crate::types_module().unwrap()).unwrap();
        context.install(crate::gage_module().unwrap()).unwrap();
        let runtime = Arc::try_new(context.runtime().unwrap()).unwrap();

        let mut sources = rune::Sources::new();
        sources
            .insert(rune::Source::memory(format!("pub fn main() {{ {expr} }}")).unwrap())
            .unwrap();

        let unit = rune::prepare(&mut sources)
            .with_context(&context)
            .build()
            .unwrap();
        let mut vm = Vm::new(runtime, Arc::try_new(unit).unwrap());

        vm.call(["main"], ())
    }

    #[test]
    fn iter_of_inexact_size_is_runtime_error() {
        let err = call("gage::Progress::iter([1, 2, 3].iter().filter(|n| n > 1))").unwrap_err();
        assert!(err.to_string().contains("exact-size iterable"), "{err}");
    }
}
