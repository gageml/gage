//! System-column tagging for query-table schemas.
//!
//! A table schema's user-facing columns describe the object for a
//! person or model writing queries. System columns exist for the
//! program rendering or addressing a row and for joins between
//! versions. The two tiers coexist in one schema; this module tags a
//! field as system and tests for the tag.
//!
//! The tag lives in the field's metadata under [`SYSTEM_META_KEY`] with
//! value `"true"`. [`system`] sets it; [`is_system`] reads it.

use datafusion::arrow::datatypes::Field;

/// Metadata key marking a field as a system-tier column.
pub const SYSTEM_META_KEY: &str = "gage.system";

/// Return `field` tagged as a system column.
pub fn system(field: Field) -> Field {
    let mut md = field.metadata().clone();
    md.insert(SYSTEM_META_KEY.to_string(), "true".to_string());
    field.with_metadata(md)
}

/// True when `field` carries the system-column tag.
pub fn is_system(field: &Field) -> bool {
    field.metadata().get(SYSTEM_META_KEY).map(String::as_str) == Some("true")
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use datafusion::arrow::datatypes::DataType;

    use super::*;

    #[test]
    fn plain_field_is_not_system() {
        let f = Field::new("id", DataType::Utf8, false);
        assert!(!is_system(&f));
    }

    #[test]
    fn system_tags_and_preserves_other_metadata() {
        let base = Field::new("id_prefix", DataType::Utf8, false).with_metadata(
            [("other".to_string(), "x".to_string())]
                .into_iter()
                .collect::<HashMap<_, _>>(),
        );
        let tagged = system(base);
        assert!(is_system(&tagged));
        assert_eq!(
            tagged.metadata().get("other").map(String::as_str),
            Some("x")
        );
    }
}
