//! Rune values as serde data for template contexts, and the
//! conversion of a minijinja error into `gage::Error::Template`.

use rune::runtime::{Object, Value};
use serde::ser::{Error as _, Serialize, SerializeMap, SerializeSeq, Serializer};

use crate::error::Error;
use crate::row::{Entry, Message};

impl From<minijinja::Error> for Error {
    fn from(e: minijinja::Error) -> Self {
        Error::Template(e.to_string())
    }
}

/// A Rune object as a serde map, for template contexts.
pub struct SerObject<'a>(pub &'a Object);

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
        if self.0.into_unit().is_ok() {
            return ser.serialize_none();
        }
        if let Ok(s) = self.0.borrow_string_ref() {
            return ser.serialize_str(&s);
        }
        if let Ok(i) = self.0.as_integer::<i64>() {
            return ser.serialize_i64(i);
        }
        if let Ok(f) = self.0.as_float() {
            return ser.serialize_f64(f);
        }
        if let Ok(b) = self.0.as_bool() {
            return ser.serialize_bool(b);
        }
        if let Ok(msg) = self.0.borrow_ref::<Message>() {
            return SerObject(&msg.inner).serialize(ser);
        }
        if let Ok(entry) = self.0.borrow_ref::<Entry>() {
            return SerObject(&entry.inner).serialize(ser);
        }
        if let Ok(vec) = self.0.borrow_ref::<rune::runtime::Vec>() {
            let mut seq = ser.serialize_seq(Some(vec.len()))?;
            for item in vec.iter() {
                seq.serialize_element(&SerValue(item))?;
            }
            return seq.end();
        }
        if let Ok(obj) = self.0.borrow_ref::<Object>() {
            return SerObject(&obj).serialize(ser);
        }
        if let Ok(Some(inner)) = self.0.borrow_ref::<Option<Value>>().as_deref() {
            return SerValue(inner).serialize(ser);
        }
        Err(S::Error::custom(format!(
            "a {} value cannot be rendered in a template context",
            self.0.type_info()
        )))
    }
}
