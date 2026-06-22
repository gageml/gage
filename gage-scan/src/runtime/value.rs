use rune::alloc;
use rune::runtime::{Object, Value, Vec as RuneVec};
use serde_json as json;

use super::json::Null;

pub(crate) fn json_to_object(val: &json::Value) -> Object {
    let mut obj = Object::new();
    if let json::Value::Object(map) = val {
        for (k, v) in map {
            let key = alloc::String::try_from(k.as_str()).unwrap();
            let val = json_to_value(v);
            obj.insert(key, val).unwrap();
        }
    }
    obj
}

pub(crate) fn json_to_value(val: &json::Value) -> Value {
    match val {
        json::Value::Null => rune::to_value(Null).unwrap(),
        json::Value::Bool(b) => rune::to_value(*b).unwrap(),
        json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                rune::to_value(i).unwrap()
            } else if let Some(f) = n.as_f64() {
                rune::to_value(f).unwrap()
            } else {
                rune::to_value(()).unwrap()
            }
        }
        json::Value::String(s) => {
            let rs = alloc::String::try_from(s.as_str()).unwrap();
            Value::new(rs).unwrap()
        }
        json::Value::Array(arr) => {
            let vec: Vec<Value> = arr.iter().map(json_to_value).collect();
            rune::to_value(vec).unwrap()
        }
        json::Value::Object(_) => rune::to_value(json_to_object(val)).unwrap(),
    }
}

/// Convert a Rune `Value` to a `serde_json::Value`, recognising `json::Null`
/// as JSON null. Rune's built-in `Serialize` for `Value` matches external
/// types by type-hash and rejects any it doesn't know with "cannot serialize
/// external references", so we have to intercept `Null` before falling back
/// to it. Object and Vec values are walked recursively for the same reason.
pub(crate) fn value_to_json(v: &Value) -> Result<json::Value, String> {
    if v.borrow_ref::<Null>().is_ok() {
        return Ok(json::Value::Null);
    }
    if let Ok(obj) = v.borrow_ref::<Object>() {
        let mut map = json::Map::with_capacity(obj.len());
        for (k, val) in obj.iter() {
            map.insert(k.as_str().to_owned(), value_to_json(val)?);
        }
        return Ok(json::Value::Object(map));
    }
    if let Ok(vec) = v.borrow_ref::<RuneVec>() {
        let mut out = Vec::with_capacity(vec.len());
        for val in vec.iter() {
            out.push(value_to_json(val)?);
        }
        return Ok(json::Value::Array(out));
    }
    json::to_value(v).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    use rune::TypeHash;
    use serde_json::json;

    #[test]
    fn null_maps_to_null_type() {
        assert_eq!(json_to_value(&json!(null)).type_hash(), Null::HASH);
    }

    #[test]
    fn non_null_is_unaffected() {
        assert_eq!(
            rune::from_value::<i64>(json_to_value(&json!(7))).unwrap(),
            7
        );
    }
}
