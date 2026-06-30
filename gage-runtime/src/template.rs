use minijinja::Environment;
use rune::runtime::{Object, Ref, Value};
use rune::{Any, ContextError, Module};
use serde::ser::{Serialize, SerializeMap, SerializeSeq, Serializer};

use super::error::Error;
use super::query::{Entry, Message};

pub(crate) fn register(m: &mut Module) -> Result<(), ContextError> {
    m.function("render_template", |template: String, context: Object| {
        do_render(&template, context)
    })
    .build()?;

    m.ty::<Template>()?;
    m.function_meta(Template::new)?;
    m.function_meta(render)?;
    Ok(())
}

fn do_render(template: &str, context: Object) -> super::Result<String> {
    let env = Environment::empty();
    let rendered = env.render_str(template, SerObject(&context))?;
    Ok(rendered)
}

/// Pre-validated Jinja-style template source. `new` parses the source
/// to surface syntax errors early; `render` substitutes a Rune object
/// context using minijinja.
#[derive(Any)]
#[rune(item = ::gage)]
pub struct Template {
    #[rune(skip)]
    source: String,
}

impl Template {
    #[rune::function(path = Self::new)]
    async fn new(source: String) -> super::Result<Self> {
        let env = Environment::empty();
        env.template_from_str(&source)?;
        Ok(Template { source })
    }
}

#[rune::function(instance)]
async fn render(this: Ref<Template>, context: Object) -> super::Result<String> {
    do_render(&this.source, context)
}

impl From<minijinja::Error> for Error {
    fn from(e: minijinja::Error) -> Self {
        Error::Template(e.to_string())
    }
}

struct SerObject<'a>(&'a Object);

impl Serialize for SerObject<'_> {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        let mut map = ser.serialize_map(Some(self.0.len()))?;
        for (k, v) in self.0.iter() {
            map.serialize_entry(k.as_str(), &SerValue(v))?;
        }
        map.end()
    }
}

struct SerValue<'a>(&'a Value);

impl Serialize for SerValue<'_> {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        if rune::from_value::<()>(self.0.clone()).is_ok() {
            return ser.serialize_none();
        }
        if let Ok(s) = self.0.borrow_string_ref() {
            return ser.serialize_str(&s);
        }
        if let Ok(i) = rune::from_value::<i64>(self.0.clone()) {
            return ser.serialize_i64(i);
        }
        if let Ok(f) = rune::from_value::<f64>(self.0.clone()) {
            return ser.serialize_f64(f);
        }
        if let Ok(b) = rune::from_value::<bool>(self.0.clone()) {
            return ser.serialize_bool(b);
        }
        if let Ok(msg) = self.0.borrow_ref::<Message>() {
            return SerObject(&msg.inner).serialize(ser);
        }
        if let Ok(entry) = self.0.borrow_ref::<Entry>() {
            return SerObject(&entry.inner).serialize(ser);
        }
        if let Ok(vec) = rune::from_value::<Vec<Value>>(self.0.clone()) {
            let mut seq = ser.serialize_seq(Some(vec.len()))?;
            for item in &vec {
                seq.serialize_element(&SerValue(item))?;
            }
            return seq.end();
        }
        if let Ok(obj) = rune::from_value::<Object>(self.0.clone()) {
            return SerObject(&obj).serialize(ser);
        }
        if let Ok(Some(inner)) = rune::from_value::<Option<Value>>(self.0.clone()) {
            return SerValue(&inner).serialize(ser);
        }
        ser.serialize_none()
    }
}
