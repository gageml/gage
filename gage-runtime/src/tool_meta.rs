//! `ToolMeta`: the MCP request's `_meta` object as the second
//! argument of a tool handler declared with `meta_arg()`.

use rune::alloc::fmt::TryWrite;
use rune::runtime::{Formatter, Value, VmError};
use rune::{Any, ContextError, Module};
use serde_json::Value as JsonValue;

use crate::value::json_to_value;

/// The second argument to a Rune tool fn: the MCP request's `_meta`
/// object, read-only, with standard object semantics (`meta[key]`,
/// `get`, `keys`, `values`, `len`, `contains_key`, iteration over
/// `(key, value)` pairs). `agent_tool_use()` derives the author of the
/// call from the wire data itself.
#[derive(Any)]
#[rune(item = ::gage)]
pub struct ToolMeta {
    #[rune(skip)]
    entries: serde_json::Map<String, JsonValue>,
    #[rune(skip)]
    scanner_name: String,
}

/// `_meta` key carrying the calling tool-use block id — the id of the
/// `tool_use` entry in the agent's session transcript that made this
/// call.
const TOOL_USE_ID_KEY: &str = "claudecode/toolUseId";

impl ToolMeta {
    /// Wrap a request's `_meta` value. Anything but a JSON object
    /// (including absent meta, passed as `Null`) wraps as empty.
    pub fn new(meta: JsonValue, scanner_name: String) -> Self {
        let entries = match meta {
            JsonValue::Object(m) => m,
            _ => serde_json::Map::new(),
        };
        ToolMeta {
            entries,
            scanner_name,
        }
    }

    /// Author of this tool call: `agent:{scanner}?call={toolUseId}`,
    /// per the convention in [`gage_core::author`]. The author is the
    /// authoring call — the tool-use id ties the written item to the
    /// exact transcript entry that wrote it. Errors when the client
    /// sent no tool-use id.
    #[rune::function(instance)]
    fn agent_tool_use(&self) -> Result<String, VmError> {
        match self.entries.get(TOOL_USE_ID_KEY).and_then(|v| v.as_str()) {
            Some(id) => Ok(gage_core::author::compose(
                "agent",
                &self.scanner_name,
                &[("call", id)],
            )),
            None => Err(VmError::panic(format!(
                "meta has no `{TOOL_USE_ID_KEY}`; cannot derive an author"
            ))),
        }
    }

    #[rune::function(instance, protocol = INDEX_GET)]
    fn index_get(&self, key: &str) -> Result<Value, VmError> {
        match self.entries.get(key) {
            Some(v) => Ok(json_to_value(v)),
            None => Err(VmError::panic(format!("missing field `{key}` in meta"))),
        }
    }

    #[rune::function(instance)]
    fn get(&self, key: &str) -> Option<Value> {
        self.entries.get(key).map(json_to_value)
    }

    #[rune::function(instance)]
    fn contains_key(&self, key: &str) -> bool {
        self.entries.contains_key(key)
    }

    #[rune::function(instance)]
    fn keys(&self) -> Vec<String> {
        self.entries.keys().cloned().collect()
    }

    #[rune::function(instance)]
    fn values(&self) -> Vec<Value> {
        self.entries.values().map(json_to_value).collect()
    }

    #[rune::function(instance)]
    fn len(&self) -> usize {
        self.entries.len()
    }

    #[rune::function(instance, protocol = LEN)]
    fn len_protocol(&self) -> usize {
        self.entries.len()
    }

    #[rune::function(instance, protocol = INTO_ITER)]
    fn into_iter(&self) -> ToolMetaIter {
        ToolMetaIter {
            items: self
                .entries
                .iter()
                .map(|(k, v)| (k.clone(), json_to_value(v)))
                .collect::<Vec<_>>()
                .into_iter(),
        }
    }

    #[rune::function(instance, protocol = DEBUG_FMT)]
    fn debug(&self, f: &mut Formatter) -> Result<(), VmError> {
        write!(f, "ToolMeta({})", JsonValue::Object(self.entries.clone()))?;
        Ok(())
    }
}

/// Iterator over a [`ToolMeta`]'s `(key, value)` pairs.
#[derive(Any)]
#[rune(item = ::gage)]
pub struct ToolMetaIter {
    #[rune(skip)]
    items: std::vec::IntoIter<(String, Value)>,
}

impl ToolMetaIter {
    #[rune::function(instance, protocol = NEXT)]
    fn next(&mut self) -> Option<(String, Value)> {
        self.items.next()
    }

    #[rune::function(instance, protocol = SIZE_HINT)]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.items.size_hint()
    }
}

pub fn register(m: &mut Module) -> Result<(), ContextError> {
    m.ty::<ToolMeta>()?;
    m.function_meta(ToolMeta::agent_tool_use)?;
    m.function_meta(ToolMeta::index_get)?;
    m.function_meta(ToolMeta::get)?;
    m.function_meta(ToolMeta::contains_key)?;
    m.function_meta(ToolMeta::keys)?;
    m.function_meta(ToolMeta::values)?;
    m.function_meta(ToolMeta::len)?;
    m.function_meta(ToolMeta::len_protocol)?;
    m.function_meta(ToolMeta::into_iter)?;
    m.function_meta(ToolMeta::debug)?;
    m.ty::<ToolMetaIter>()?;
    m.function_meta(ToolMetaIter::next)?;
    m.function_meta(ToolMetaIter::size_hint)?;
    Ok(())
}
