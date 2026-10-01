//! Tool inventory entry and `.md`-to-rmcp parsing.
//!
//! Each module under `tools/` declares `pub const TOOL: Tool = route;`,
//! where `route` returns a fully-built [`ToolRoute`] sourced from the
//! companion `<name>.md` file. The `.md` is the single source of truth
//! for a tool's name, parameter schema, annotations, and description.

use std::collections::BTreeMap;
use std::sync::Arc;

use rmcp::handler::server::router::tool::ToolRoute;
use rmcp::model::{JsonObject, Tool as ToolMeta, ToolAnnotations};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::server::GageServer;

/// A `tools/<name>.rs` module exports `pub const TOOL: ToolDef = route;`,
/// and `server.rs` collects them into the router. Distinct from
/// [`rmcp::model::Tool`], which is the wire-format metadata built from
/// the companion `.md` by [`build_tool_meta`].
pub type ToolDef = fn() -> ToolRoute<GageServer>;

/// Claude Code truncates MCP tool descriptions at 2048 bytes; anything
/// past this is silently invisible to the model. Each tool module
/// enforces this at compile time via [`description_byte_len`].
pub const MAX_DESCRIPTION_BYTES: usize = 2048;

/// Compile-time length of the description body in `md` (the bytes after
/// the `+++` frontmatter, with leading/trailing ASCII whitespace
/// trimmed). Mirrors what [`parse_md`] does at runtime so a `const _`
/// assertion can verify the body fits Claude Code's cap before the
/// crate even links.
#[allow(clippy::indexing_slicing)]
pub const fn description_byte_len(md: &str) -> usize {
    let bytes = md.as_bytes();
    assert!(
        bytes.len() >= 4
            && bytes[0] == b'+'
            && bytes[1] == b'+'
            && bytes[2] == b'+'
            && bytes[3] == b'\n',
        "tool .md must start with `+++` line",
    );
    let mut i = 4;
    let body_start = loop {
        assert!(
            i + 5 <= bytes.len(),
            "tool .md must close frontmatter with `+++` line",
        );
        if bytes[i] == b'\n'
            && bytes[i + 1] == b'+'
            && bytes[i + 2] == b'+'
            && bytes[i + 3] == b'+'
            && bytes[i + 4] == b'\n'
        {
            break i + 5;
        }
        i += 1;
    };
    let mut start = body_start;
    while start < bytes.len() && is_ascii_ws(bytes[start]) {
        start += 1;
    }
    let mut end = bytes.len();
    while end > start && is_ascii_ws(bytes[end - 1]) {
        end -= 1;
    }
    end - start
}

const fn is_ascii_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r')
}

/// Build the rmcp `Tool` metadata for an entire `.md` source.
pub fn build_tool_meta(md: &str) -> ToolMeta {
    let (fm, body) = parse_md(md);
    ToolMeta {
        name: fm.name.into(),
        title: None,
        description: Some(body.to_string().into()),
        input_schema: Arc::new(build_input_schema(&fm.parameters)),
        output_schema: None,
        annotations: fm.annotations.as_ref().map(annotations_to_rmcp),
        execution: None,
        icons: None,
        meta: None,
    }
}

#[derive(Deserialize)]
struct Frontmatter {
    name: String,
    #[serde(default)]
    parameters: BTreeMap<String, Parameter>,
    #[serde(default)]
    annotations: Option<Annotations>,
}

#[derive(Deserialize)]
struct Parameter {
    #[serde(rename = "type")]
    ty: String,
    #[serde(default)]
    required: bool,
    description: String,
    /// For `type = "array"`: declares the element type.
    #[serde(default)]
    items: Option<Items>,
}

#[derive(Deserialize)]
struct Items {
    #[serde(rename = "type")]
    ty: String,
}

#[derive(Deserialize)]
struct Annotations {
    #[serde(default)]
    read_only_hint: Option<bool>,
    #[serde(default)]
    destructive_hint: Option<bool>,
    #[serde(default)]
    idempotent_hint: Option<bool>,
    #[serde(default)]
    open_world_hint: Option<bool>,
}

fn parse_md(md: &str) -> (Frontmatter, &str) {
    let after_open = md
        .strip_prefix("+++\n")
        .expect("tool .md must start with `+++` line");
    let (fm_str, body) = after_open
        .split_once("\n+++\n")
        .expect("tool .md must close frontmatter with `+++` line");
    let fm: Frontmatter = toml::from_str(fm_str).expect("invalid TOML frontmatter");
    (fm, body.trim())
}

fn build_input_schema(parameters: &BTreeMap<String, Parameter>) -> JsonObject {
    let mut properties = JsonObject::new();
    let mut required: Vec<Value> = Vec::new();
    for (name, p) in parameters {
        let mut prop = JsonObject::new();
        prop.insert("type".into(), Value::String(p.ty.clone()));
        prop.insert("description".into(), Value::String(p.description.clone()));
        if let Some(items) = &p.items {
            prop.insert("items".into(), json!({ "type": items.ty }));
        }
        properties.insert(name.clone(), Value::Object(prop));
        if p.required {
            required.push(Value::String(name.clone()));
        }
    }
    let mut obj = JsonObject::new();
    obj.insert("type".into(), Value::String("object".into()));
    obj.insert("properties".into(), Value::Object(properties));
    obj.insert("required".into(), Value::Array(required));
    obj
}

fn annotations_to_rmcp(a: &Annotations) -> ToolAnnotations {
    ToolAnnotations {
        title: None,
        read_only_hint: a.read_only_hint,
        destructive_hint: a.destructive_hint,
        idempotent_hint: a.idempotent_hint,
        open_world_hint: a.open_world_hint,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "+++\n\
        name = \"sample\"\n\
        \n\
        [parameters.foo]\n\
        type = \"string\"\n\
        required = true\n\
        description = \"the foo\"\n\
        \n\
        [parameters.tags]\n\
        type = \"array\"\n\
        description = \"tag list\"\n\
        items = { type = \"string\" }\n\
        \n\
        [annotations]\n\
        read_only_hint = true\n\
        +++\n\
        \n\
        Body text here.\n";

    #[test]
    fn parses_name_body_params_annotations() {
        let (fm, body) = parse_md(SAMPLE);
        assert_eq!(fm.name, "sample");
        assert_eq!(body, "Body text here.");
        let foo = fm.parameters.get("foo").expect("foo declared");
        assert_eq!(foo.ty, "string");
        assert!(foo.required);
        assert_eq!(foo.description, "the foo");
        assert_eq!(fm.annotations.and_then(|a| a.read_only_hint), Some(true));
    }

    #[test]
    fn meta_has_object_schema_with_required_and_array_items() {
        let meta = build_tool_meta(SAMPLE);
        assert_eq!(meta.name, "sample");
        assert_eq!(meta.description.as_deref(), Some("Body text here."));
        let schema = &*meta.input_schema;
        assert_eq!(schema.get("type"), Some(&Value::String("object".into())));
        let required = schema
            .get("required")
            .and_then(|v| v.as_array())
            .expect("required array");
        assert_eq!(required, &vec![Value::String("foo".into())]);
        let tags = schema
            .get("properties")
            .and_then(|v| v.get("tags"))
            .expect("tags property");
        assert_eq!(
            tags.get("items").and_then(|v| v.get("type")),
            Some(&Value::String("string".into()))
        );
    }
}
