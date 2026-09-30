//! `Progress`: task progress reporting for scanners.
//!
//! Each call sends an absolute `(pos, total)` snapshot through the
//! task's output channel as [`Output::Progress`]. The consumer folds
//! the latest snapshot into its view of the task; a task that never
//! reports is indeterminate. Two forms:
//!
//! ```rune
//! // Counter form
//! let p = Progress::new(sessions.len());
//! for s in sessions {
//!     handle(s).await?;
//!     p.tick();
//! }
//!
//! // Iterator form, over any exact-size iterable
//! for s in Progress::iter(sessions.iter()) {
//!     handle(s).await?;
//! }
//! ```

use rune::runtime::{Iterator as RuneIterator, Value, VmError};
use rune::{Any, ContextError, Module};

use crate::{Output, send};

pub(crate) fn types_module() -> Result<Module, ContextError> {
    let mut m = Module::new();
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
    Ok(m)
}

#[derive(Any)]
#[rune(item = ::gage)]
pub struct Progress {
    #[rune(skip)]
    pos: u64,
    #[rune(skip)]
    total: u64,
    /// The wrapped iterable of the `iter` form
    #[rune(skip)]
    inner: Option<RuneIterator>,
}

impl Progress {
    /// Counter form: announce a total, then advance with `tick`,
    /// `inc`, or `set`.
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
    /// An iterable without an exact size is an error, since there is
    /// no total to report.
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

    #[rune::function(keep, instance)]
    fn tick(&mut self) {
        self.inc(1);
    }

    #[rune::function(keep, instance)]
    fn inc(&mut self, n: u64) {
        self.pos = self.pos.saturating_add(n);
        self.report();
    }

    #[rune::function(keep, instance)]
    fn set(&mut self, pos: u64) {
        self.pos = pos;
        self.report();
    }

    /// Restart at zero with a new total
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

    fn report(&self) {
        send(Output::Progress {
            pos: self.pos,
            total: self.total,
        });
    }
}
