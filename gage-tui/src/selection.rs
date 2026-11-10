//! Declarative entry bodies. An entry that no custom renderer covers
//! is shown through a rule, found by the entry's type and subtype,
//! that names the values to show as JSON pointers into the entry.

use std::collections::HashMap;
use std::sync::LazyLock;

use ratatui::text::Line;
use serde_json::Value;

use crate::attrs::attr_lines;

/// The values a rule selected from an entry.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Selection {
    /// Caption and value rows; a caption is the last segment of the
    /// pointer that selected the value
    pub attrs: Vec<(&'static str, String)>,
    /// Text items in rule order
    pub text: Vec<String>,
}

impl Selection {
    fn is_empty(&self) -> bool {
        self.attrs.is_empty() && self.text.is_empty()
    }
}

/// The attribute rows, then each text item after a blank line.
pub fn body(selection: &Selection) -> Vec<Line<'static>> {
    let attrs: Vec<(&'static str, &str)> = selection
        .attrs
        .iter()
        .map(|(caption, value)| (*caption, value.as_str()))
        .collect();
    let mut out = attr_lines(&attrs);
    for text in &selection.text {
        if !out.is_empty() {
            out.push(Line::raw(""));
        }
        out.extend(text.split('\n').map(|line| Line::raw(line.to_string())));
    }
    out
}

/// What the rules select from `entry`: the subtype's rule when it
/// selects anything, else the type's default rule. `None` when no rule
/// matches or no pointer selects a value.
pub fn select(entry: &Value) -> Option<Selection> {
    let entry_type = entry.get("type")?.as_str()?;
    let rules = RULES.get(entry_type)?;
    let subtype_rule = subtype(entry_type, entry).and_then(|s| rules.subtypes.get(s));
    subtype_rule
        .into_iter()
        .chain(&rules.default)
        .map(|rule| rule.apply(entry))
        .find(|selection| !selection.is_empty())
}

fn subtype<'a>(entry_type: &str, entry: &'a Value) -> Option<&'a str> {
    let pointer = match entry_type {
        "attachment" => "/attachment/type",
        _ => "/subtype",
    };
    entry.pointer(pointer)?.as_str()
}

struct Rule {
    /// Pointers to scalars shown as caption and value rows
    attrs: &'static [&'static str],
    /// Pointers to strings and string arrays shown as text
    text: &'static [&'static str],
}

impl Rule {
    fn apply(&self, entry: &Value) -> Selection {
        Selection {
            attrs: self
                .attrs
                .iter()
                .filter_map(|pointer| Some((caption(pointer), scalar(entry.pointer(pointer)?)?)))
                .collect(),
            text: self
                .text
                .iter()
                .filter_map(|pointer| entry.pointer(pointer))
                .flat_map(strings)
                .collect(),
        }
    }
}

fn caption(pointer: &'static str) -> &'static str {
    pointer.rsplit_once('/').map_or(pointer, |(_, last)| last)
}

/// A non-empty string, a number, or a boolean as text; nothing for any
/// other value.
fn scalar(value: &Value) -> Option<String> {
    match value {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// The trimmed, non-empty strings of a string or of an array whose
/// elements are all strings; nothing for any other value.
fn strings(value: &Value) -> Vec<String> {
    let items: Vec<&str> = match value {
        Value::String(s) => vec![s],
        Value::Array(items) => match items.iter().map(Value::as_str).collect() {
            Some(strings) => strings,
            None => return Vec::new(),
        },
        _ => return Vec::new(),
    };
    items
        .into_iter()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

#[derive(Default)]
struct TypeRules {
    default: Option<&'static Rule>,
    subtypes: HashMap<&'static str, &'static Rule>,
}

static RULES: LazyLock<HashMap<&'static str, TypeRules>> = LazyLock::new(|| {
    let mut rules: HashMap<&'static str, TypeRules> = HashMap::new();
    for (entry_type, subtype, rule) in TABLE {
        let type_rules = rules.entry(entry_type).or_default();
        match subtype {
            Some(subtype) => {
                type_rules.subtypes.insert(subtype, rule);
            }
            None => type_rules.default = Some(rule),
        }
    }
    rules
});

const fn rule(attrs: &'static [&'static str], text: &'static [&'static str]) -> Rule {
    Rule { attrs, text }
}

/// Entry type, subtype, and rule. A row without a subtype is the
/// type's default.
const TABLE: &[(&str, Option<&str>, Rule)] = &[
    ("ai-title", None, rule(&[], &["/aiTitle"])),
    ("custom-title", None, rule(&[], &["/customTitle"])),
    ("agent-name", None, rule(&[], &["/agentName"])),
    ("last-prompt", None, rule(&["/leafUuid"], &["/lastPrompt"])),
    ("mode", None, rule(&["/mode"], &[])),
    ("permission-mode", None, rule(&["/permissionMode"], &[])),
    ("tools-used", None, rule(&[], &["/tools"])),
    (
        "queue-operation",
        None,
        rule(&["/operation", "/reason"], &["/content"]),
    ),
    ("atis-latch", None, rule(&["/atis"], &[])),
    (
        "session-owner",
        None,
        rule(&["/accountUuid", "/organizationUuid"], &[]),
    ),
    (
        "bridge-session",
        None,
        rule(
            &[
                "/bridgeSessionId",
                "/lastSequenceNum",
                "/ownerAccountUuid",
                "/ownerOrganizationUuid",
            ],
            &[],
        ),
    ),
    (
        "file-history-snapshot",
        None,
        rule(
            &["/messageId", "/isSnapshotUpdate", "/snapshot/timestamp"],
            &[],
        ),
    ),
    (
        "file-history-delta",
        None,
        rule(
            &[
                "/trackingPath",
                "/backup/backupFileName",
                "/backup/version",
                "/backup/backupTime",
            ],
            &[],
        ),
    ),
    (
        "api-request",
        None,
        rule(
            &[
                "/id",
                "/version",
                "/querySource",
                "/params/model",
                "/params/max_tokens",
                "/shapeHash",
            ],
            &[],
        ),
    ),
    (
        "api-request-blob",
        None,
        rule(&["/hash", "/message/role"], &["/message/content"]),
    ),
    (
        "api-request-shape",
        None,
        rule(&["/shapeHash", "/version"], &[]),
    ),
    (
        "cost-state",
        None,
        rule(
            &[
                "/totalCostUSD",
                "/totalDuration",
                "/totalAPIDuration",
                "/totalToolDuration",
                "/totalLinesAdded",
                "/totalLinesRemoved",
            ],
            &[],
        ),
    ),
    (
        "frame-link",
        None,
        rule(&["/title", "/frameUrl", "/path", "/artifactCount"], &[]),
    ),
    ("system", None, rule(&[], &["/content"])),
    (
        "system",
        Some("turn_duration"),
        rule(&["/durationMs", "/messageCount"], &[]),
    ),
    (
        "system",
        Some("stop_hook_summary"),
        rule(
            &[
                "/hookCount",
                "/preventedContinuation",
                "/stopReason",
                "/hasOutput",
            ],
            &["/hookErrors"],
        ),
    ),
    (
        "system",
        Some("bridge_status"),
        rule(&["/url"], &["/content"]),
    ),
    (
        "attachment",
        None,
        rule(&[], &["/attachment/text", "/attachment/content"]),
    ),
    (
        "attachment",
        Some("prompt_snapshot"),
        rule(
            &[
                "/attachment/contextRendering",
                "/attachment/echoWireToolInputs",
                "/attachment/reminderFold",
            ],
            &["/attachment/systemPrompt"],
        ),
    ),
    (
        "attachment",
        Some("edited_text_file"),
        rule(&["/attachment/filename"], &["/attachment/snippet"]),
    ),
    (
        "attachment",
        Some("nested_memory"),
        rule(&["/attachment/path"], &["/attachment/content/content"]),
    ),
    (
        "attachment",
        Some("directory"),
        rule(&["/attachment/path"], &["/attachment/content"]),
    ),
    (
        "attachment",
        Some("agent_listing_delta"),
        rule(&[], &["/attachment/addedLines"]),
    ),
    (
        "attachment",
        Some("deferred_tools_delta"),
        rule(&[], &["/attachment/addedLines"]),
    ),
    (
        "attachment",
        Some("mcp_instructions_delta"),
        rule(&[], &["/attachment/addedBlocks"]),
    ),
    (
        "attachment",
        Some("credential_org"),
        rule(&["/attachment/organizationUuid"], &[]),
    ),
    (
        "attachment",
        Some("command_permissions"),
        rule(&[], &["/attachment/allowedTools"]),
    ),
    (
        "attachment",
        Some("date_change"),
        rule(&["/attachment/newDate"], &[]),
    ),
    (
        "attachment",
        Some("read_truncation_notice"),
        rule(&["/attachment/toolUseID"], &["/attachment/banner"]),
    ),
    (
        "attachment",
        Some("hook_success"),
        rule(
            &["/attachment/command", "/attachment/exitCode"],
            &["/attachment/content"],
        ),
    ),
    (
        "attachment",
        Some("hook_non_blocking_error"),
        rule(
            &["/attachment/command", "/attachment/exitCode"],
            &["/attachment/stderr", "/attachment/stdout"],
        ),
    ),
    (
        "attachment",
        Some("hook_cancelled"),
        rule(
            &[
                "/attachment/command",
                "/attachment/durationMs",
                "/attachment/timedOut",
            ],
            &[],
        ),
    ),
    (
        "attachment",
        Some("plan_mode"),
        rule(&["/attachment/planFilePath", "/attachment/planExists"], &[]),
    ),
    (
        "attachment",
        Some("plan_mode_exit"),
        rule(&["/attachment/planFilePath", "/attachment/planExists"], &[]),
    ),
    (
        "attachment",
        Some("compact_file_reference"),
        rule(&["/attachment/filename"], &[]),
    ),
    (
        "attachment",
        Some("bash_output_audience_note"),
        rule(&["/attachment/toolUseID"], &[]),
    ),
    (
        "attachment",
        Some("dynamic_skill"),
        rule(&["/attachment/skillDir"], &["/attachment/skillNames"]),
    ),
    (
        "attachment",
        Some("queued_command"),
        rule(
            &["/attachment/commandMode"],
            &["/attachment/prompt", "/attachment/prompt/0/text"],
        ),
    ),
    (
        "attachment",
        Some("task_reminder"),
        rule(&["/attachment/itemCount"], &[]),
    ),
    (
        "attachment",
        Some("thinking_drop"),
        rule(
            &[
                "/attachment/requestId",
                "/attachment/model",
                "/attachment/querySource",
                "/attachment/newlyDropped/reason",
                "/attachment/newlyDropped/blockCount",
            ],
            &[],
        ),
    ),
];

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    /// A subtype's rule is used over the type's default rule.
    #[test]
    fn a_subtype_rule_is_used_before_the_default() {
        let entry = json!({
            "type": "system",
            "subtype": "turn_duration",
            "content": "ignored",
            "durationMs": 5352,
            "messageCount": 19,
        });
        assert_eq!(
            select(&entry),
            Some(Selection {
                attrs: vec![
                    ("durationMs", "5352".to_string()),
                    ("messageCount", "19".to_string()),
                ],
                text: vec![],
            })
        );
    }

    /// A subtype without a rule, and a subtype whose rule selects
    /// nothing, fall back to the type's default rule.
    #[test]
    fn the_default_rule_covers_what_a_subtype_rule_does_not() {
        let no_rule = json!({
            "type": "attachment",
            "attachment": {"type": "batching_reminder_sent", "text": "List first."},
        });
        assert_eq!(select(&no_rule).unwrap().text, ["List first."]);

        let selects_nothing = json!({
            "type": "system",
            "subtype": "turn_duration",
            "content": "Turn ended",
        });
        assert_eq!(select(&selects_nothing).unwrap().text, ["Turn ended"]);
    }

    /// An array of strings is one text item per element, trimmed, with
    /// empty elements left out; scalars of each kind become rows.
    #[test]
    fn string_arrays_and_scalars_are_selected() {
        let entry = json!({
            "type": "attachment",
            "attachment": {
                "type": "prompt_snapshot",
                "systemPrompt": ["\nYou are an agent.\n", "", "# Harness"],
                "contextRendering": "announced",
                "echoWireToolInputs": true,
                "reminderFold": false,
            },
        });
        assert_eq!(
            select(&entry),
            Some(Selection {
                attrs: vec![
                    ("contextRendering", "announced".to_string()),
                    ("echoWireToolInputs", "true".to_string()),
                    ("reminderFold", "false".to_string()),
                ],
                text: vec!["You are an agent.".to_string(), "# Harness".to_string()],
            })
        );
    }

    /// A pointer that selects an object, an array with a non-string
    /// element, null, or an empty string selects nothing.
    #[test]
    fn values_of_another_shape_select_nothing() {
        for content in [
            json!({"nested": "x"}),
            json!(["a", 1]),
            json!(null),
            json!(""),
        ] {
            let entry = json!({"type": "system", "content": content});
            assert_eq!(select(&entry), None, "{entry}");
        }
        let entry = json!({"type": "mode", "mode": {"nested": "x"}});
        assert_eq!(select(&entry), None);
    }

    /// A type without a rule, and an entry without a type, select
    /// nothing.
    #[test]
    fn an_unknown_type_selects_nothing() {
        assert_eq!(select(&json!({"type": "not-a-type", "content": "x"})), None);
        assert_eq!(select(&json!({"content": "x"})), None);
    }

    /// The table names each type and subtype pair once, so no row
    /// replaces another when the map is built.
    #[test]
    fn the_table_has_no_duplicate_keys() {
        let mut keys: Vec<(&str, Option<&str>)> = TABLE.iter().map(|(t, s, _)| (*t, *s)).collect();
        let count = keys.len();
        keys.sort();
        keys.dedup();
        assert_eq!(keys.len(), count);
    }

    /// The body is the attribute rows, then each text item after a
    /// blank line; text alone opens without a blank line.
    #[test]
    fn the_body_is_rows_then_text() {
        let selection = Selection {
            attrs: vec![("operation", "remove".to_string())],
            text: vec!["first\nsecond".to_string(), "third".to_string()],
        };
        let lines: Vec<String> = body(&selection).iter().map(text).collect();
        assert_eq!(
            lines,
            ["operation  remove", "", "first", "second", "", "third"]
        );

        let text_only = Selection {
            attrs: vec![],
            text: vec!["Review documentation".to_string()],
        };
        let lines: Vec<String> = body(&text_only).iter().map(text).collect();
        assert_eq!(lines, ["Review documentation"]);
    }
}
