//! `Template`: a Jinja template compiled once and rendered from Rune
//! objects.
//!
//! `Template::new(source)` compiles the source and reports a syntax
//! error as `gage::Error::Template`. `render(ctx)` renders the
//! compiled template with a Rune object as its variables; a render
//! error (an unknown filter or function, an operation on incompatible
//! values) is the same error type. Undefined variables render empty
//! and test false. Neither function performs I/O, so neither is
//! async. The environment is empty: no built-in filters, tests, or
//! globals. Context serialization is the first generation's.

use gage_runtime::error::Error;
use gage_runtime::template::SerObject;
use minijinja::Environment;
use rune::runtime::{Object, Ref};
use rune::{Any, ContextError, Module};

pub(crate) fn module() -> Result<Module, ContextError> {
    let mut m = Module::with_crate("gage")?;
    m.ty::<Template>()?;
    m.function_meta(Template::new)?;
    m.function_meta(render)?;
    Ok(m)
}

const NAME: &str = "template";

#[derive(Any)]
#[rune(item = ::gage)]
pub struct Template {
    /// Holds the one compiled template under `NAME`
    #[rune(skip)]
    env: Environment<'static>,
}

impl Template {
    #[rune::function(path = Self::new)]
    fn new(source: String) -> Result<Self, Error> {
        Self::compile(source)
    }

    fn compile(source: String) -> Result<Self, Error> {
        let mut env = Environment::empty();
        env.add_template_owned(NAME, source)?;
        Ok(Template { env })
    }

    fn render(&self, context: &Object) -> Result<String, Error> {
        let template = self
            .env
            .get_template(NAME)
            .expect("the template is added at construction");
        Ok(template.render(SerObject(context))?)
    }
}

#[rune::function(instance)]
fn render(this: Ref<Template>, context: Object) -> Result<String, Error> {
    this.render(&context)
}

#[cfg(test)]
mod tests {
    use rune::runtime::Value;

    use super::*;

    fn object(pairs: &[(&str, Value)]) -> Object {
        let mut obj = Object::new();
        for (k, v) in pairs {
            obj.insert(rune::alloc::String::try_from(*k).unwrap(), v.clone())
                .unwrap();
        }
        obj
    }

    #[test]
    fn compile_rejects_a_syntax_error() {
        let err = Template::compile("{% if".to_string()).err().unwrap();
        assert!(matches!(err, Error::Template(_)), "{err:?}");
    }

    #[test]
    fn renders_the_compiled_template_repeatedly() {
        let t =
            Template::compile("Hi {{ name }}{% if extra %} ({{ extra }}){% endif %}".to_string())
                .unwrap();
        let name = rune::to_value("Ann").unwrap();
        let extra = rune::to_value("more").unwrap();
        let first = t.render(&object(&[("name", name.clone())])).unwrap();
        let second = t
            .render(&object(&[("name", name), ("extra", extra)]))
            .unwrap();
        assert_eq!(first, "Hi Ann");
        assert_eq!(second, "Hi Ann (more)");
    }

    #[test]
    fn render_reports_an_unknown_filter() {
        let t = Template::compile("{{ name | upper }}".to_string()).unwrap();
        let err = t.render(&object(&[])).err().unwrap();
        assert!(matches!(err, Error::Template(_)), "{err:?}");
    }
}
