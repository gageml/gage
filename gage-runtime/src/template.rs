//! `Template`: a Jinja template compiled once and rendered from Rune
//! objects.
//!
//! `Template::new(source)` compiles the source and reports a syntax
//! error as `gage::Error::Template`. `render(ctx)` renders the
//! compiled template with a Rune object as its variables; a render
//! error (an unknown filter or function, an operation on incompatible
//! values, a context value of a type the serializer does not render,
//! such as an iterator) is the same error type. Undefined variables
//! render empty and test false. Neither function performs I/O, so neither is
//! async. The environment carries minijinja's built-in filters,
//! tests, and globals, the filters in [`crate::filters`], and never
//! escapes output. Context serialization is [`crate::ser`]'s.

use crate::error::Error;
use crate::filters;
use crate::ser::SerObject;
use minijinja::{AutoEscape, Environment};
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
        let mut env = Environment::new();
        env.set_auto_escape_callback(|_| AutoEscape::None);
        env.add_filter("sql_in_list", filters::sql_in_list);
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
        let t = Template::compile("{{ name | nosuchfilter }}".to_string()).unwrap();
        let err = t.render(&object(&[])).err().unwrap();
        assert!(matches!(err, Error::Template(_)), "{err:?}");
    }

    #[test]
    fn built_in_filters_read_the_serialized_context() {
        let t = Template::compile(
            "{{ findings | map(attribute='id') | join(', ') }} ({{ findings | length }})"
                .to_string(),
        )
        .unwrap();
        let findings: Vec<Object> = ["a1", "b2", "c3"]
            .iter()
            .map(|id| object(&[("id", rune::to_value(*id).unwrap())]))
            .collect();
        let findings = rune::to_value(findings).unwrap();
        let out = t.render(&object(&[("findings", findings)])).unwrap();
        assert_eq!(out, "a1, b2, c3 (3)");
    }

    #[test]
    fn gage_filters_render_through_a_template() {
        let t = Template::compile(
            "WHERE id IN ({{ findings | map(attribute='id') | sql_in_list }})".to_string(),
        )
        .unwrap();
        let findings: Vec<Object> = ["a1", "b2"]
            .iter()
            .map(|id| object(&[("id", rune::to_value(*id).unwrap())]))
            .collect();
        let findings = rune::to_value(findings).unwrap();
        let out = t.render(&object(&[("findings", findings)])).unwrap();
        assert_eq!(out, "WHERE id IN ('a1', 'b2')");
    }

    #[test]
    fn render_rejects_a_context_value_it_cannot_serialize() {
        let t = Template::compile("{% for x in pair %}{{ x }}{% endfor %}".to_string()).unwrap();
        let pair = rune::to_value((1i64, 2i64)).unwrap();
        let err = t.render(&object(&[("pair", pair)])).err().unwrap();
        assert!(matches!(err, Error::Template(_)), "{err:?}");
        assert!(err.to_string().contains("cannot be rendered"), "{err}");
    }

    #[test]
    fn render_never_escapes_output() {
        let t = Template::compile("{{ text }}".to_string()).unwrap();
        let text = rune::to_value("<session_id> & \"quoted\"").unwrap();
        let out = t.render(&object(&[("text", text)])).unwrap();
        assert_eq!(out, "<session_id> & \"quoted\"");
    }
}
