//! The two task-exit sentinels. A task that returns `Err(Fail(msg))`
//! fails and the scan reports `msg` alone; any other `Err` is a
//! scanner defect and is reported with a source location. A task that
//! returns `Err(Ignore)` has nothing to do and succeeds.

use gage_runtime::ignore::Ignore;
use rune::{Any, ContextError, Module};

#[derive(Any, Clone, Debug)]
#[rune(item = ::gage, constructor)]
pub struct Fail(#[rune(get)] String);

impl Fail {
    pub fn message(&self) -> &str {
        &self.0
    }

    #[rune::function(instance, path = Self::message)]
    fn rune_message(&self) -> String {
        self.0.clone()
    }
}

pub(crate) fn types_module() -> Result<Module, ContextError> {
    let mut m = Module::new();
    m.ty::<Fail>()?;
    m.function_meta(Fail::rune_message)?;
    m.ty::<Ignore>()?;
    Ok(m)
}
