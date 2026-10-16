//! Gage's own Jinja filters, registered on every template's
//! environment beside minijinja's built-ins.

use minijinja::value::{Value, ValueKind};
use minijinja::{Error, ErrorKind};

/// `sql_in_list`: a sequence of scalars as the body of a SQL `IN`
/// list. A string is single-quoted with an embedded quote doubled; a
/// number is bare. An empty sequence renders `NULL`, which keeps the
/// SQL valid and matches no row. Any other value is a render error.
pub fn sql_in_list(value: Value) -> Result<String, Error> {
    let items = value
        .try_iter()?
        .map(sql_literal)
        .collect::<Result<Vec<_>, _>>()?;
    if items.is_empty() {
        return Ok("NULL".to_string());
    }
    Ok(items.join(", "))
}

fn sql_literal(value: Value) -> Result<String, Error> {
    match value.kind() {
        ValueKind::String => {
            let s = value.as_str().expect("a string-kind value has str");
            Ok(format!("'{}'", s.replace('\'', "''")))
        }
        ValueKind::Number => Ok(value.to_string()),
        kind => Err(Error::new(
            ErrorKind::InvalidOperation,
            format!("sql_in_list: {kind} value {value} is not a SQL literal"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strings_are_quoted_and_numbers_are_bare() {
        let out = sql_in_list(Value::from(vec![
            Value::from("a1"),
            Value::from("it's"),
            Value::from(3),
            Value::from(2.5),
        ]))
        .unwrap();
        assert_eq!(out, "'a1', 'it''s', 3, 2.5");
    }

    #[test]
    fn an_empty_sequence_renders_null() {
        assert_eq!(
            sql_in_list(Value::from(Vec::<Value>::new())).unwrap(),
            "NULL"
        );
    }

    #[test]
    fn a_non_scalar_item_and_a_non_sequence_are_errors() {
        let err = sql_in_list(Value::from(vec![Value::from(true)])).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidOperation);
        assert!(err.to_string().contains("sql_in_list"), "{err}");
        assert!(sql_in_list(Value::from(7)).is_err());
    }
}
