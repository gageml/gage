//! The `json` module: `json::parse(text)` reads a JSON text into a
//! Rune value and `json::to_string(value)` and
//! `json::to_string_pretty(value)` write one back. JSON `null` is
//! surfaced to scanners as a distinct `json::Null` value rather than
//! unit, so it reads clearly in errors and debug output and can be
//! tested with `value is json::Null`.

use rune::alloc::fmt::TryWrite;
use rune::runtime::{Formatter, Value, VmError};
use rune::{Any, ContextError, Module};

use crate::error::Error;
use crate::value::{json_to_value, value_to_json};

pub(crate) fn module() -> Result<Module, ContextError> {
    let mut m = Module::with_crate("json")?;
    m.ty::<Null>()?;
    m.function_meta(Null::display)?;
    m.function_meta(Null::debug)?;
    m.function_meta(Null::partial_eq)?;
    m.function_meta(Null::eq)?;
    m.function("parse", parse).build()?;
    m.function("to_string", to_string).build()?;
    m.function("to_string_pretty", to_string_pretty).build()?;
    Ok(m)
}

/// The value of a JSON text: an object, a vector, a string, a number,
/// a bool, or `json::Null`. A text that is not JSON is `Error::Decode`.
fn parse(text: &str) -> Result<Value, Error> {
    let json: serde_json::Value =
        serde_json::from_str(text).map_err(|e| Error::Decode(format!("json::parse: {e}")))?;
    Ok(json_to_value(&json))
}

/// The JSON text of a value on one line. A value with no JSON form,
/// such as a native object, is `Error::Args`.
fn to_string(value: Value) -> Result<String, Error> {
    let json = to_json(&value, "json::to_string")?;
    serde_json::to_string(&json).map_err(|e| Error::Args(format!("json::to_string: {e}")))
}

/// The JSON text of a value, indented.
fn to_string_pretty(value: Value) -> Result<String, Error> {
    let json = to_json(&value, "json::to_string_pretty")?;
    serde_json::to_string_pretty(&json)
        .map_err(|e| Error::Args(format!("json::to_string_pretty: {e}")))
}

fn to_json(value: &Value, what: &str) -> Result<serde_json::Value, Error> {
    value_to_json(value).map_err(|e| Error::Args(format!("{what}: {e}")))
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
    use rune::{Context, Module, Value, Vm};

    fn eval(expr: &str) -> Value {
        let mut context = Context::with_default_modules().unwrap();
        context.install(module().unwrap()).unwrap();
        let mut errors = Module::with_crate("gage").unwrap();
        crate::error::register_types(&mut errors).unwrap();
        context.install(errors).unwrap();
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

    #[expect(
        clippy::disallowed_methods,
        reason = "takes the VM execution's return value; the test holds the only live handle"
    )]
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
    #[expect(
        clippy::disallowed_methods,
        reason = "takes the VM execution's return value; the test holds the only live handle"
    )]
    fn null_renders_as_null() {
        assert_eq!(
            rune::from_value::<String>(eval("format!(\"{}\", json::Null)")).unwrap(),
            "null"
        );
    }

    #[expect(
        clippy::disallowed_methods,
        reason = "takes the VM execution's return value; the test holds the only live handle"
    )]
    fn eval_string(expr: &str) -> String {
        rune::from_value::<String>(eval(expr)).unwrap()
    }

    #[test]
    fn parse_reads_objects_arrays_and_null() {
        assert!(eval_bool(
            "let v = json::parse(\"{\\\"a\\\": [1, null], \\\"b\\\": true}\").unwrap(); \
             v.a[0] == 1 && v.a[1] is json::Null && v.b"
        ));
    }

    #[test]
    fn parse_rejects_a_text_that_is_not_json() {
        assert!(eval_bool("json::parse(\"{\").is_err()"));
    }

    #[test]
    fn to_string_writes_null_and_nested_values() {
        assert_eq!(
            eval_string("json::to_string(#{ a: [1, json::Null], b: \"x\" }).unwrap()"),
            "{\"a\":[1,null],\"b\":\"x\"}"
        );
    }

    #[test]
    fn to_string_pretty_indents() {
        assert_eq!(
            eval_string("json::to_string_pretty(#{ a: 1 }).unwrap()"),
            "{\n  \"a\": 1\n}"
        );
    }

    #[test]
    fn parse_and_to_string_round_trip() {
        assert_eq!(
            eval_string(
                "json::to_string(json::parse(\"[1,2.5,\\\"s\\\",null]\").unwrap()).unwrap()"
            ),
            "[1,2.5,\"s\",null]"
        );
    }
}
