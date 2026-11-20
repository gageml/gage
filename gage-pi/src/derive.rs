//! Derivation: a session file to derived rows and session attributes.
//!
//! Every line of the file is a row, in file order. A file of format
//! version 2 or later holds a tree, in which an entry names its
//! parent; the rows are every entry of every branch, as the lines
//! stand. The entry's type is the line's `type`. Its subtype is what
//! names the entry further: the role of a `message`, the `customType`
//! of a `custom` or `custom_message` entry, and the `kind` of a
//! `usage` entry.
//!
//! The `message` rows:
//!
//! | Entry | Message |
//! |---|---|
//! | `message`, role `user` | user, text |
//! | `message`, role `assistant` with a tool call | assistant, tool_use |
//! | `message`, role `assistant` with thinking and no tool call | assistant, thinking |
//! | `message`, role `assistant` otherwise | assistant, text |
//! | `message`, role `toolResult` | user, tool_result |
//! | `message`, role `bashExecution` | user, meta |
//! | `message`, role `custom` or `hookMessage` | user, meta |
//! | `message`, role `system` | system, meta |
//! | `custom_message` | user, meta |
//! | `branch_summary` | user, meta |
//! | `compaction` | user, compact_summary |
//!
//! Pi writes one assistant response as one entry, so the text of an
//! assistant message is every block of the response in order. Every
//! other entry is an entry and no message.

use std::io::Read;
use std::path::Path;

use chrono::DateTime;
use gage_corpus::{
    CorpusSessionAttrs, DerivedRows, DerivedSession, Fingerprint, ReadError, Result, SessionReader,
};
use gage_session::{DriverError, Entry, INVALID_ENTRY_TYPE, Message};
use serde_json::{Map, Value};

use crate::codec::codec;
use crate::session_file::{HEADER_TYPE, Header, MAX_FORMAT_VERSION};
use crate::tool_use::ToolUseCollector;

/// The generation of the attributes [`derive_session`] produces.
/// Raised on any change to what they hold or mean.
pub(crate) const SUMMARY_VERSION: u32 = 1;

/// Maximum number of characters of a title taken from the first
/// prompt
const MAX_TITLE_CHARS: usize = 60;

/// The role Pi gave an extension's message before format version 3
/// named it `custom`
const LEGACY_CUSTOM_ROLE: &str = "hookMessage";

/// Parse the session file at `path` and derive its rows and
/// attributes. `project` is the session's project name, which the
/// listing reads from the file's header.
pub(crate) fn derive_session(id: &str, path: &Path, project: String) -> Result<DerivedSession> {
    let fingerprint = Fingerprint::stat(path)?;
    derive_session_from(
        id,
        SessionReader::open(path, codec())?,
        fingerprint,
        project,
    )
}

/// Derive the rows and attributes of the session `reader` yields.
/// `fingerprint` and `project` are the facts the caller holds about
/// the content.
///
/// The first line that parses has to be a header, of a format version
/// this build reads. Past the header the derivation goes by the shape
/// of each entry and not by the header's version: Pi can write a
/// version 3 header over entries of an earlier shape.
pub(crate) fn derive_session_from<R: Read>(
    id: &str,
    reader: SessionReader<R>,
    fingerprint: Fingerprint,
    project: String,
) -> Result<DerivedSession> {
    let mut rows = DerivedRows::new();
    let mut tool_uses = ToolUseCollector::new();
    let mut attrs = CorpusSessionAttrs {
        native_mtime_ms: fingerprint.mtime_ms,
        native_size: fingerprint.size,
        project,
        is_empty: true,
        ..Default::default()
    };
    let mut has_header = false;
    let mut prompt_title: Option<String> = None;
    // The name the session was last given; `Some(None)` is a name
    // cleared after it was set.
    let mut name: Option<Option<String>> = None;
    let mut selected_model: Option<String> = None;

    for result in reader {
        // A line that does not read or decode fails the session. The
        // codec stands in for the corruption it recognizes with an
        // invalid entry, which derives a row like any other line.
        let (line_num, raw, decoded) = result?;
        let line = Value::from(decoded);
        let entry_type = line_type(&line);

        // Pi looks for the header past the lines it cannot parse.
        if !has_header && entry_type != Some(INVALID_ENTRY_TYPE) {
            let header = Header::from_line(&line).ok_or_else(|| {
                refused(
                    line_num,
                    "not a Pi session: the first entry is not a session header",
                )
            })?;
            if header.version > MAX_FORMAT_VERSION {
                return Err(refused(
                    line_num,
                    &format!(
                        "Pi session format version {} is newer than this build reads, \
                         which is up to version {MAX_FORMAT_VERSION} (upgrade gage)",
                        header.version
                    ),
                ));
            }
            attrs.format_version = Some(header.version);
            has_header = true;
        }

        attrs.line_count = u64::from(line_num);
        if let Some(ms) = timestamp_ms(&line) {
            attrs.first_timestamp_ms = Some(attrs.first_timestamp_ms.map_or(ms, |f| f.min(ms)));
            attrs.last_timestamp_ms = Some(attrs.last_timestamp_ms.map_or(ms, |l| l.max(ms)));
        }
        if let Some(usage) = usage_of(&line) {
            add_usage(&mut attrs, usage);
        }
        match entry_type {
            Some("session_info") => {
                let given = line.get("name").and_then(Value::as_str).map(str::trim);
                name = Some(given.filter(|n| !n.is_empty()).map(String::from));
            }
            Some("model_change") if selected_model.is_none() => {
                selected_model = str_at(&line, "modelId").map(String::from);
            }
            Some("message") if attrs.model.is_none() && role(&line) == Some("assistant") => {
                attrs.model = line
                    .pointer("/message/model")
                    .and_then(Value::as_str)
                    .map(String::from);
            }
            _ => {}
        }

        if let Some(entry) = derive_entry(id, line_num, &line, raw) {
            if let Some(message) = &entry.message {
                let kind = (message.message_type.as_str(), message.subtype.as_deref());
                if matches!(kind.0, "user" | "assistant") {
                    attrs.message_count += 1;
                }
                if matches!(kind, ("user", Some("text")) | ("assistant", _)) {
                    attrs.is_empty = false;
                }
                if prompt_title.is_none() && kind == ("user", Some("text")) {
                    prompt_title = title_of(&message.text);
                }
            }
            rows.push(id, &entry);
        }
        tool_uses.push(line_num, &line);
    }

    if !has_header {
        return Err(refused(
            1,
            "not a Pi session: the file has no session header",
        ));
    }
    // The model that answered first, else the one first selected in a
    // session no model answered
    if attrs.model.is_none() {
        attrs.model = selected_model;
    }
    // Pi shows a session's name in place of its first prompt.
    attrs.title = name.flatten().or(prompt_title);
    DerivedSession::new(id, rows, tool_uses.finish(), attrs, fingerprint)
}

/// The error of a file this build does not read as a Pi session,
/// reported against `line`.
fn refused(line: u32, message: &str) -> gage_corpus::IndexError {
    ReadError::Decode {
        line,
        error: DriverError::Other(message.to_string()),
    }
    .into()
}

/// A title from a prompt: its first non-empty line, cut to
/// [`MAX_TITLE_CHARS`] characters.
fn title_of(prompt: &str) -> Option<String> {
    let line = prompt.lines().map(str::trim).find(|l| !l.is_empty())?;
    if line.chars().count() <= MAX_TITLE_CHARS {
        return Some(line.to_string());
    }
    let cut: String = line.chars().take(MAX_TITLE_CHARS).collect();
    Some(format!("{}...", cut.trim_end()))
}

/// The usage a line records for one model request, when it records
/// one. These are the entries Pi's own session statistics total: an
/// assistant message, a tool result whose tool ran a model, a `usage`
/// entry, and the summary request behind a `compaction` or a
/// `branch_summary`. The totals run over every branch of the session,
/// as Pi's do.
fn usage_of(line: &Value) -> Option<&Value> {
    match line_type(line)? {
        "usage" | "compaction" | "branch_summary" => line.get("usage"),
        "message" if matches!(role(line), Some("assistant" | "toolResult")) => {
            line.pointer("/message/usage")
        }
        _ => None,
    }
    .filter(|usage| usage.is_object())
}

/// Add one request's usage to the session's totals. Pi counts prompt
/// tokens read from and written to a cache apart from `input`, as the
/// attributes do. The cost is the request's `cost.total`, Pi's own
/// estimate from the model's listed prices; a session none of whose
/// entries carries a cost has none.
fn add_usage(attrs: &mut CorpusSessionAttrs, usage: &Value) {
    let count = |key| usage.get(key).and_then(Value::as_i64).unwrap_or(0);
    attrs.input_tokens += count("input");
    attrs.output_tokens += count("output");
    attrs.cache_read_input_tokens += count("cacheRead");
    attrs.cache_creation_input_tokens += count("cacheWrite");
    if let Some(cost) = usage.pointer("/cost/total").and_then(Value::as_f64) {
        attrs.cost_usd = Some(attrs.cost_usd.unwrap_or(0.0) + cost);
    }
}

pub(crate) fn line_type(line: &Value) -> Option<&str> {
    str_at(line, "type")
}

/// The role of a `message` entry's message
pub(crate) fn role(line: &Value) -> Option<&str> {
    line.pointer("/message/role").and_then(Value::as_str)
}

pub(crate) fn str_at<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

/// The line's `timestamp` as epoch milliseconds; `None` when absent
/// or not RFC 3339.
pub(crate) fn timestamp_ms(line: &Value) -> Option<i64> {
    let s = str_at(line, "timestamp")?;
    Some(DateTime::parse_from_rfc3339(s).ok()?.timestamp_millis())
}

/// Normalize one parsed session line into `entry` table shape, with
/// its message when the line is one the `message` table holds.
/// `session_id` labels diagnostics only. A line without a `type` is
/// not an entry and yields `None`. `raw` is the line's text as the
/// codec renders it, the `raw` column.
///
/// The header's `id` names the session and not an entry, so the
/// header has no `uuid`. An entry of format version 1 has no id.
pub(crate) fn derive_entry(
    session_id: &str,
    line_num: u32,
    line: &Value,
    raw: String,
) -> Option<Entry> {
    let Some(entry_type) = line_type(line) else {
        tracing::warn!(session_id, line = line_num, "entry without a type");
        return None;
    };
    Some(Entry {
        line: line_num,
        uuid: (entry_type != HEADER_TYPE)
            .then(|| str_at(line, "id"))
            .flatten()
            .map(String::from),
        entry_type: entry_type.to_string(),
        subtype: subtype_of(entry_type, line).map(String::from),
        timestamp_ms: timestamp_ms(line),
        raw: Some(raw),
        message: message_of(line_num, entry_type, line),
    })
}

fn subtype_of<'a>(entry_type: &str, line: &'a Value) -> Option<&'a str> {
    match entry_type {
        "message" => role(line).map(current_role),
        "custom" | "custom_message" => str_at(line, "customType"),
        "usage" => str_at(line, "kind"),
        _ => None,
    }
}

/// A role as format version 3 names it.
fn current_role(role: &str) -> &str {
    match role {
        LEGACY_CUSTOM_ROLE => "custom",
        role => role,
    }
}

/// The message a session line is, when it is one.
fn message_of(line_num: u32, entry_type: &str, line: &Value) -> Option<Message> {
    let message = |message_type: &str, subtype: &str, text: String| Message {
        message_type: message_type.to_string(),
        subtype: Some(subtype.to_string()),
        text,
        attachments: None,
        ide_tags: None,
    };
    match entry_type {
        "message" => {
            let body = line.get("message")?;
            let content = body.get("content");
            let (message_type, subtype, text) = match current_role(str_at(body, "role")?) {
                "user" => ("user", "text", content_text(content)),
                "assistant" => (
                    "assistant",
                    assistant_subtype(content),
                    assistant_text(content),
                ),
                "toolResult" => ("user", "tool_result", content_text(content)),
                "bashExecution" => ("user", "meta", bash_execution_text(body)),
                "custom" => ("user", "meta", content_text(content)),
                "system" => ("system", "meta", system_text(body)),
                _ => return None,
            };
            Some(Message {
                attachments: attachments(line_num, content),
                ..message(message_type, subtype, text)
            })
        }
        "custom_message" => {
            let content = line.get("content");
            Some(Message {
                attachments: attachments(line_num, content),
                ..message("user", "meta", content_text(content))
            })
        }
        "branch_summary" => Some(message(
            "user",
            "meta",
            str_at(line, "summary").unwrap_or_default().to_string(),
        )),
        "compaction" => Some(message(
            "user",
            "compact_summary",
            str_at(line, "summary").unwrap_or_default().to_string(),
        )),
        _ => None,
    }
}

/// The blocks of a content array
pub(crate) fn blocks(content: Option<&Value>) -> &[Value] {
    content
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
}

fn block_type(block: &Value) -> Option<&str> {
    str_at(block, "type")
}

/// What an assistant response is, by the most telling block it holds:
/// a tool call, else thinking, else text.
fn assistant_subtype(content: Option<&Value>) -> &'static str {
    let holds = |kind| blocks(content).iter().any(|b| block_type(b) == Some(kind));
    match (holds("toolCall"), holds("thinking")) {
        (true, _) => "tool_use",
        (false, true) => "thinking",
        (false, false) => "text",
    }
}

/// The text of an assistant response: each block in order, a tool
/// call as its name and arguments, joined by blank lines.
fn assistant_text(content: Option<&Value>) -> String {
    let texts: Vec<String> = blocks(content)
        .iter()
        .filter_map(|block| match block_type(block)? {
            "text" => str_at(block, "text").map(String::from),
            "thinking" => str_at(block, "thinking").map(String::from),
            "toolCall" => Some(tool_call_text(
                str_at(block, "name").unwrap_or_default(),
                block.get("arguments").unwrap_or(&Value::Null),
            )),
            _ => None,
        })
        .filter(|text| !text.is_empty())
        .collect();
    texts.join("\n\n")
}

/// The text of a message's content: a string as it is, or the text
/// blocks of an array, joined by blank lines.
pub(crate) fn content_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .filter(|item| block_type(item) == Some("text"))
            .filter_map(|item| str_at(item, "text"))
            .collect::<Vec<_>>()
            .join("\n\n"),
        _ => String::new(),
    }
}

/// The text of a system message: its content, then each prompt
/// section it sets, in the order it names them. A section it removes,
/// and the tools it adds and removes, are not text.
pub(crate) fn system_text(message: &Value) -> String {
    let content = content_text(message.get("content"));
    let sections = message.get("sections").and_then(Value::as_object);
    std::iter::once(content.as_str())
        .chain(
            sections
                .into_iter()
                .flatten()
                .filter_map(|(_, s)| s.as_str()),
        )
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// The text of a shell command the user ran from Pi: the command,
/// then what it printed.
pub(crate) fn bash_execution_text(message: &Value) -> String {
    let command = str_at(message, "command").unwrap_or_default();
    match str_at(message, "output").filter(|output| !output.is_empty()) {
        Some(output) => format!("{command}\n\n{output}"),
        None => command.to_string(),
    }
}

/// The content blocks of a message that are not text, each with a
/// `ref` of `[line, content_index]`, as a JSON array; `None` when
/// every block is text or the content is a string.
fn attachments(line_num: u32, content: Option<&Value>) -> Option<String> {
    let attached: Vec<Value> = blocks(content)
        .iter()
        .enumerate()
        .filter(|(_, block)| block_type(block).is_some_and(|kind| kind != "text"))
        .map(|(index, block)| {
            let mut block = block.clone();
            if let Some(object) = block.as_object_mut() {
                object.insert("ref".to_string(), serde_json::json!([line_num, index]));
            }
            block
        })
        .collect();
    (!attached.is_empty()).then(|| Value::Array(attached).to_string())
}

/// The text of a tool call: a YAML mapping of the tool's name to its
/// arguments.
pub(crate) fn tool_call_text(name: &str, arguments: &Value) -> String {
    let mut call = Map::new();
    call.insert(name.to_string(), arguments.clone());
    match serde_yaml::to_string(&Value::Object(call)) {
        Ok(yaml) => yaml.trim_end().to_string(),
        // A JSON value always has a YAML form; the call's JSON stands
        // in for one that somehow does not.
        Err(_) => format!("{name}: {arguments}"),
    }
}

#[cfg(test)]
mod tests {
    use arrow::array::{Array, Int64Array, StringArray};
    use gage_corpus::{
        COL_LINE, COL_MESSAGE_SUBTYPE, COL_MESSAGE_TYPE, COL_SUBTYPE, COL_TEXT, COL_TYPE, COL_UUID,
    };
    use gage_session::SessionAttrs;
    use serde_json::json;

    use super::*;
    use crate::testing::{
        GENERATED, V1_EXTENDED, V1_EXTENDED_MIGRATED, V1_PLAIN, V3_BRANCH, V3_HEADER_V1_ENTRIES,
        V3_TORN, derive, generated, pi_stats, try_derive,
    };

    fn strings(derived: &DerivedSession, column: usize) -> Vec<Option<String>> {
        let array = derived.batch.column(column);
        let array = array.as_any().downcast_ref::<StringArray>().unwrap();
        array.iter().map(|v| v.map(String::from)).collect()
    }

    fn line_numbers(derived: &DerivedSession) -> Vec<i64> {
        let array = derived.batch.column(COL_LINE);
        let array = array.as_any().downcast_ref::<Int64Array>().unwrap();
        array.values().to_vec()
    }

    /// The message of each row that is one, as `(line, type, subtype)`
    fn messages(derived: &DerivedSession) -> Vec<(i64, String, String)> {
        let types = strings(derived, COL_MESSAGE_TYPE);
        let subtypes = strings(derived, COL_MESSAGE_SUBTYPE);
        line_numbers(derived)
            .into_iter()
            .zip(types.into_iter().zip(subtypes))
            .filter_map(|(line, (type_, subtype))| Some((line, type_?, subtype?)))
            .collect()
    }

    fn expected(messages: &[(i64, &str, &str)]) -> Vec<(i64, String, String)> {
        messages
            .iter()
            .map(|(line, type_, subtype)| (*line, type_.to_string(), subtype.to_string()))
            .collect()
    }

    fn read_error(text: &str) -> String {
        match try_derive("s", text) {
            Ok(_) => panic!("the session derived"),
            Err(e) => e.to_string(),
        }
    }

    #[test]
    fn a_version_1_file_derives_by_the_shape_of_its_entries() {
        let derived = derive(V1_EXTENDED);
        assert_eq!(derived.attrs.format_version, Some(1));
        assert_eq!(derived.attrs.line_count(), Some(10));
        assert_eq!(
            line_numbers(&derived),
            [1, 2, 3, 4, 6, 7, 8, 9, 10],
            "the blank line 5 is no row; the torn line 9 is one"
        );
        assert!(
            strings(&derived, COL_UUID).iter().all(Option::is_none),
            "version 1 entries have no ids, and the header's id is the session's"
        );
        assert_eq!(
            messages(&derived),
            expected(&[
                (2, "user", "text"),
                (3, "assistant", "tool_use"),
                (4, "user", "tool_result"),
                (6, "user", "compact_summary"),
                (7, "user", "meta"),
                (8, "assistant", "text"),
                (10, "user", "text"),
            ])
        );
        let subtypes = strings(&derived, COL_SUBTYPE);
        assert_eq!(
            subtypes[5].as_deref(),
            Some("custom"),
            "the hookMessage role reads as the role version 3 names it"
        );
        assert_eq!(
            strings(&derived, COL_TYPE)[7].as_deref(),
            Some(INVALID_ENTRY_TYPE)
        );

        let attrs = &derived.attrs;
        assert_eq!(attrs.title(), Some("Rename the helper"));
        assert_eq!(attrs.model(), Some("claude-old"));
        assert_eq!(attrs.project_name(), Some("-w-proj"));
        assert_eq!(attrs.message_count(), Some(7));
        assert_eq!(attrs.is_empty(), Some(false));
        assert_eq!(
            attrs.input_tokens(),
            Some(300),
            "250 uncached and 50 read from the cache"
        );
        assert_eq!(attrs.cached_tokens(), Some(50));
        assert_eq!(attrs.output_tokens(), Some(30));
        assert_eq!(attrs.cost_usd(), Some(0.0015 + 0.000005));
        assert_eq!(attrs.first_timestamp_ms, Some(1_748_772_000_000));
        assert_eq!(attrs.last_timestamp_ms, Some(1_748_772_007_000));
    }

    #[test]
    fn a_plain_version_1_file_derives() {
        let derived = derive(V1_PLAIN);
        assert_eq!(derived.attrs.format_version, Some(1));
        assert_eq!(derived.batch.num_rows(), 4);
        assert_eq!(
            messages(&derived),
            expected(&[(2, "user", "text"), (4, "assistant", "thinking")])
        );
        assert_eq!(
            strings(&derived, COL_TEXT)[3].as_deref(),
            Some("A greeting.\n\nHi."),
            "one response is one entry, so its text is every block"
        );
        assert_eq!(derived.attrs.title(), Some("Hello"));
        assert_eq!(derived.attrs.model(), Some("gpt-old"));
        assert_eq!(derived.attrs.cost_usd(), Some(0.0));
    }

    #[test]
    fn every_entry_of_every_branch_is_a_row_in_file_order() {
        let derived = derive(V3_BRANCH);
        assert_eq!(derived.attrs.format_version, Some(3));
        assert_eq!(line_numbers(&derived), [1, 2, 3, 4, 5, 6, 7, 8]);
        let uuids = strings(&derived, COL_UUID);
        assert_eq!(uuids[0], None, "the header");
        assert_eq!(uuids[4].as_deref(), Some("a0000004"));
        assert_eq!(
            messages(&derived),
            expected(&[
                (2, "user", "text"),
                (3, "assistant", "text"),
                (4, "user", "text"),
                (5, "assistant", "text"),
                (6, "user", "meta"),
                (7, "user", "text"),
                (8, "assistant", "text"),
            ]),
            "lines 4 and 5 are the branch that was left, and line 6 its summary"
        );
        assert_eq!(derived.attrs.message_count(), Some(7));
        assert_eq!(
            derived.attrs.cost_usd(),
            Some(0.001 + 0.002 + 0.004),
            "the cost of both branches, as Pi totals a session"
        );
        assert_eq!(derived.attrs.input_tokens(), Some(60));
    }

    #[test]
    fn a_version_3_header_over_entries_with_no_ids_derives() {
        let derived = derive(V3_HEADER_V1_ENTRIES);
        assert_eq!(derived.attrs.format_version, Some(3));
        assert_eq!(derived.batch.num_rows(), 5);
        assert_eq!(
            strings(&derived, COL_UUID),
            [None, None, None, None, Some("b0000001".to_string())]
        );
        assert_eq!(
            messages(&derived),
            expected(&[
                (2, "user", "text"),
                (4, "assistant", "thinking"),
                (5, "user", "text"),
            ])
        );
    }

    #[test]
    fn a_torn_line_is_a_row_and_a_blank_line_is_none() {
        let derived = derive(V3_TORN);
        assert_eq!(line_numbers(&derived), [1, 2, 3, 5]);
        assert_eq!(derived.attrs.line_count(), Some(5));
        let types = strings(&derived, COL_TYPE);
        assert_eq!(types[2].as_deref(), Some(INVALID_ENTRY_TYPE));
        assert_eq!(
            messages(&derived),
            expected(&[(2, "user", "text"), (5, "assistant", "text")]),
            "the entry the torn line ran into is lost with it"
        );
    }

    #[test]
    fn a_line_that_is_not_an_object_fails_the_session() {
        let text = V1_PLAIN.replacen('\n', "\nnot json at all\n", 1);
        let error = read_error(&text);
        assert!(error.starts_with("line 2:"), "{error}");
    }

    #[test]
    fn a_migrated_file_derives_the_messages_of_the_file_it_replaced() {
        let before = derive(V1_EXTENDED);
        let after = derive(V1_EXTENDED_MIGRATED);
        assert_eq!(after.attrs.format_version, Some(3));
        assert_eq!(after.attrs.line_count(), Some(8), "two lines shorter");
        let kinds = |derived: &DerivedSession| -> Vec<(String, String)> {
            let all = messages(derived).into_iter();
            all.map(|(_, type_, subtype)| (type_, subtype)).collect()
        };
        assert_eq!(kinds(&after), kinds(&before));
        assert_eq!(after.attrs.cost_usd(), before.attrs.cost_usd());
        assert!(strings(&after, COL_UUID)[1..].iter().all(Option::is_some));
    }

    #[test]
    fn a_header_of_a_later_format_version_is_refused_by_version() {
        let text = V3_BRANCH.replacen(r#""version":3"#, r#""version":4"#, 1);
        let error = read_error(&text);
        assert!(error.contains("format version 4"), "{error}");
        assert!(error.contains("upgrade gage"), "{error}");
    }

    #[test]
    fn a_file_whose_first_entry_is_not_a_header_is_not_a_pi_session() {
        let late: String = V3_BRANCH
            .lines()
            .skip(1)
            .map(|l| format!("{l}\n"))
            .collect();
        let error = read_error(&late);
        assert!(error.contains("not a Pi session"), "{error}");
        assert!(read_error("").contains("no session header"));

        // Pi looks for the header past a line it cannot parse.
        let led = format!("{{\"type\":\"sess\n{V3_BRANCH}");
        assert_eq!(derive(&led).attrs.format_version, Some(3));
    }

    #[test]
    fn an_unknown_entry_type_is_an_entry_and_no_message() {
        let header = V3_BRANCH.lines().next().unwrap();
        let entry = json!({"type": "future_entry", "id": "f0000001", "parentId": null,
            "timestamp": "2026-01-02T03:04:05.000Z", "note": "x"});
        let derived = derive(&format!("{header}\n{entry}\n"));
        assert_eq!(
            strings(&derived, COL_TYPE)[1].as_deref(),
            Some("future_entry")
        );
        assert_eq!(messages(&derived), []);
        assert_eq!(derived.attrs.is_empty(), Some(true));
    }

    #[test]
    fn the_totals_are_the_ones_pi_reports() {
        let stats = pi_stats();
        for name in GENERATED {
            let (_, text) = generated(name);
            let derived = derive(&text);
            let attrs = &derived.attrs;
            let pi = &stats[name];
            let tokens = |key| pi["tokens"][key].as_i64().unwrap();
            assert_eq!(attrs.input_tokens, tokens("input"), "{name}");
            assert_eq!(attrs.output_tokens, tokens("output"), "{name}");
            assert_eq!(attrs.cache_read_input_tokens, tokens("cacheRead"), "{name}");
            assert_eq!(
                attrs.cache_creation_input_tokens,
                tokens("cacheWrite"),
                "{name}"
            );
            assert_eq!(attrs.cost_usd, pi["cost"].as_f64(), "{name}");
            assert_eq!(
                attrs.line_count,
                pi["entries"].as_u64().unwrap() + 1,
                "{name}"
            );

            let count = |type_: &str, subtype: &str| {
                let all = messages(&derived).into_iter();
                all.filter(|(_, t, s)| t == type_ && s == subtype).count() as u64
            };
            assert_eq!(count("user", "text"), pi["userMessages"], "{name}");
            assert_eq!(count("user", "tool_result"), pi["toolResults"], "{name}");
            let assistant: u64 = ["text", "thinking", "tool_use"]
                .iter()
                .map(|subtype| count("assistant", subtype))
                .sum();
            assert_eq!(assistant, pi["assistantMessages"], "{name}");
            assert_eq!(
                derived.tool_uses.num_rows() as u64,
                pi["toolCalls"],
                "{name}"
            );
        }
    }

    #[test]
    fn a_session_pi_wrote_derives_its_entries() {
        let (_, text) = generated("run");
        let derived = derive(&text);
        let attrs = &derived.attrs;
        assert_eq!(attrs.format_version, Some(3));
        assert_eq!(
            attrs.title(),
            Some("Reading the notes"),
            "the name it was given"
        );
        assert_eq!(attrs.model(), Some("faux-1"));

        let types = strings(&derived, COL_TYPE);
        let subtypes = strings(&derived, COL_SUBTYPE);
        let texts = strings(&derived, COL_TEXT);
        let row = |type_: &str, subtype: Option<&str>| {
            let found = (0..types.len())
                .find(|&i| types[i].as_deref() == Some(type_) && subtypes[i].as_deref() == subtype);
            found.unwrap_or_else(|| panic!("no {type_} {subtype:?} row"))
        };
        let system = texts[row("message", Some("system"))].clone().unwrap();
        assert!(system.contains("You are a coding assistant"), "{system}");
        assert!(system.contains("<cwd>"), "every section it sets: {system}");
        assert_eq!(
            texts[row("message", Some("bashExecution"))].as_deref(),
            Some("echo delta >> notes.txt")
        );
        assert_eq!(
            texts[row("custom_message", Some("gage-fixture"))].as_deref(),
            Some("The user prefers short answers.")
        );
        let summary = texts[row("compaction", None)].clone().unwrap();
        assert!(summary.starts_with("## Goal"), "{summary}");
        assert_eq!(texts[row("usage", Some("cache_warm"))], None);
        assert_eq!(texts[row("usage", Some("gage_fixture"))], None);
        assert_eq!(texts[row("session_info", None)], None);

        let (_, text) = generated("branches");
        let branches = derive(&text);
        assert_eq!(
            branches.attrs.title(),
            Some("How should the notes be ordered?"),
            "the first prompt of a session with no name"
        );
    }
}
