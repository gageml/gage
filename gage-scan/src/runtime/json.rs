//! The `json` module. JSON `null` is surfaced to scanners as a distinct
//! `json::Null` value rather than unit, so it reads clearly in errors and
//! debug output and can be tested with `value is json::Null`.

use rune::alloc::fmt::TryWrite;
use rune::runtime::{Formatter, VmError};
use rune::{Any, ContextError, Module};

pub(crate) fn module() -> Result<Module, ContextError> {
    let mut m = Module::with_crate("json")?;
    m.ty::<Null>()?;
    m.function_meta(Null::display)?;
    m.function_meta(Null::debug)?;
    m.function_meta(Null::partial_eq)?;
    m.function_meta(Null::eq)?;
    Ok(m)
}

/// JSON `null`.
#[derive(Any, Debug, Clone, Copy)]
#[rune(item = ::json, constructor)]
pub(crate) struct Null;

impl Null {
    #[rune::function(protocol = DISPLAY_FMT)]
    fn display(&self, f: &mut Formatter) -> Result<(), VmError> {
        write!(f, "null")?;
        Ok(())
    }

    #[rune::function(protocol = DEBUG_FMT)]
    fn debug(&self, f: &mut Formatter) -> Result<(), VmError> {
        write!(f, "null")?;
        Ok(())
    }

    #[rune::function(protocol = PARTIAL_EQ)]
    fn partial_eq(&self, _other: &Null) -> bool {
        true
    }

    #[rune::function(protocol = EQ)]
    fn eq(&self, _other: &Null) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use rune::sync::Arc;
    use rune::{Context, Value, Vm};

    fn eval(expr: &str) -> Value {
        let mut context = Context::with_default_modules().unwrap();
        context.install(module().unwrap()).unwrap();
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

        vm.call(["main"], ()).unwrap()
    }

    fn eval_bool(expr: &str) -> bool {
        rune::from_value::<bool>(eval(expr)).unwrap()
    }

    #[test]
    fn null_is_null() {
        assert!(eval_bool("json::Null is json::Null"));
    }

    #[test]
    fn other_values_are_not_null() {
        assert!(eval_bool("1 is not json::Null"));
        assert!(eval_bool("\"x\" is not json::Null"));
    }

    #[test]
    fn null_equals_null() {
        assert!(eval_bool("json::Null == json::Null"));
    }

    #[test]
    fn null_renders_as_null() {
        assert_eq!(
            rune::from_value::<String>(eval("format!(\"{}\", json::Null)")).unwrap(),
            "null"
        );
    }
}
