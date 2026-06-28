//! The `Ignore` sentinel. A task returns `Err(Ignore)` to exit early
//! without being counted as a failure — "nothing to do here", not an
//! error. The scheduler recognizes it by type hash and treats it as a
//! successful run.

use rune::runtime::Value;
use rune::{Any, ContextError, Module, TypeHash};

#[derive(Any, Clone, Debug)]
#[rune(item = ::gage, constructor)]
pub struct Ignore;

pub(crate) fn register_types(m: &mut Module) -> Result<(), ContextError> {
    m.ty::<Ignore>()?;
    Ok(())
}

/// True when a task's `Err` value is the `Ignore` sentinel, an early
/// exit that should not count as a failure.
pub fn is_ignore(err: &Value) -> bool {
    err.type_hash() == Ignore::HASH
}
