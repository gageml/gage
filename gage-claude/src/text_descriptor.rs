//! The Claude driver's text descriptor: which fields of a transcript
//! line are identifiers, timestamps, digests, signatures, or
//! enumerations rather than text, which blocks are atomic, and which
//! fields a named filter treats as sensitive by type. The `text_block` table and the publish
//! redaction engine read it.
//! `schema/claude-jsonl-1/text_descriptor.json` holds the pattern
//! lists beside the content schemas of the same format.

use std::collections::BTreeMap;

use gage_session::TextDescriptor;
use serde::Deserialize;

const DESCRIPTOR: &str = include_str!("../schema/claude-jsonl-1/text_descriptor.json");

#[derive(Deserialize)]
struct Lists {
    safe: Vec<String>,
    atomic: Vec<String>,
    #[serde(default)]
    filters: BTreeMap<String, FilterLists>,
}

#[derive(Deserialize)]
struct FilterLists {
    #[serde(default)]
    literal: Vec<String>,
    #[serde(default)]
    value: Vec<String>,
}

pub(crate) fn text_descriptor() -> TextDescriptor {
    let lists: Lists =
        serde_json::from_str(DESCRIPTOR).expect("descriptor file should hold pattern lists");
    fn strs(v: &[String]) -> Vec<&str> {
        v.iter().map(String::as_str).collect()
    }
    let mut descriptor = TextDescriptor::new(&strs(&lists.safe), &strs(&lists.atomic))
        .expect("descriptor patterns should be JSONPath");
    for (name, paths) in &lists.filters {
        descriptor = descriptor
            .with_filter(name, &strs(&paths.literal), &strs(&paths.value))
            .expect("descriptor filter patterns should be JSONPath");
    }
    descriptor
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::env;
    use std::fs;
    use std::path::Path;

    use gage_session::TextValue;
    use serde_json::Value;

    use super::*;

    const SAMPLE: &str = include_str!("../schema/claude-jsonl-1/sample.jsonl");

    fn values_of(text: &str) -> Vec<(usize, Vec<TextValue>)> {
        let descriptor = text_descriptor();
        text.lines()
            .enumerate()
            .filter(|(_, l)| !l.trim().is_empty())
            .map(|(i, l)| (i + 1, descriptor.text_values(l).unwrap()))
            .collect()
    }

    /// No identifier, timestamp, signature, or enumeration of the sample
    /// transcript is a text value.
    #[test]
    fn identifiers_timestamps_and_enumerations_are_not_text() {
        let structural: HashSet<&str> = [
            "/uuid",
            "/parentUuid",
            "/sessionId",
            "/timestamp",
            "/requestId",
            "/promptId",
            "/sourceToolAssistantUUID",
            "/type",
            "/version",
            "/entrypoint",
            "/userType",
            "/permissionMode",
            "/effort",
            "/requestedModel",
            "/message/id",
            "/message/role",
            "/message/type",
            "/message/model",
            "/message/stop_reason",
            "/message/usage/service_tier",
            "/message/content/0/type",
            "/message/content/0/id",
            "/message/content/0/name",
            "/message/content/0/tool_use_id",
            "/message/content/0/signature",
            "/message/content/0/caller/type",
        ]
        .into_iter()
        .collect();
        for (line, values) in values_of(SAMPLE) {
            for v in values {
                assert!(
                    !structural.contains(v.pointer.as_str()),
                    "line {line}: {} is structural",
                    v.pointer
                );
            }
        }
    }

    /// A prompt, a thinking block, and the working directory are text
    /// and not atomic; a tool call's input, a tool result's content,
    /// and the harness's `toolUseResult` copy are atomic.
    #[test]
    fn sample_values_are_classified() {
        let values = values_of(SAMPLE);
        let find = |line: usize, pointer: &str| -> TextValue {
            values
                .iter()
                .find(|(l, _)| *l == line)
                .and_then(|(_, vs)| vs.iter().find(|v| v.pointer == pointer))
                .cloned()
                .unwrap_or_else(|| panic!("line {line} has no text value at {pointer}"))
        };
        let prompt = find(1, "/message/content");
        assert_eq!(prompt.text, "Count the files");
        assert!(!prompt.atomic);
        assert!(!find(1, "/cwd").atomic);
        assert!(!find(2, "/message/content/0/thinking").atomic);
        assert!(find(3, "/message/content/0/input/command").atomic);
        assert!(find(4, "/message/content/0/content").atomic);
        assert!(find(4, "/toolUseResult/stdout").atomic);
    }

    /// A value at a safe path is an opaque token: no whitespace, and
    /// only the characters of an identifier, a timestamp, a digest, or
    /// a signature. A safe pattern that selects free text fails here.
    fn check_safe_shapes(text: &str) -> Vec<String> {
        let lists: Lists = serde_json::from_str(DESCRIPTOR).unwrap();
        let paths: Vec<serde_json_path::JsonPath> = lists
            .safe
            .iter()
            .map(|p| serde_json_path::JsonPath::parse(p).unwrap())
            .collect();
        let opaque = |s: &str| {
            s.chars()
                .all(|c| c.is_ascii_alphanumeric() || "_:+/=.-".contains(c))
        };
        let mut failures = Vec::new();
        for (i, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let Ok(entry) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            for (path, compiled) in lists.safe.iter().zip(&paths) {
                for node in compiled.query_located(&entry).iter() {
                    if let Some(s) = node.node().as_str()
                        && !opaque(s)
                    {
                        failures.push(format!(
                            "line {}: {path} selects text at {}: {s:?}",
                            i + 1,
                            node.location().to_json_pointer()
                        ));
                    }
                }
            }
        }
        failures
    }

    #[test]
    fn safe_paths_select_only_opaque_tokens_in_the_sample() {
        let failures = check_safe_shapes(SAMPLE);
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    /// The same check over a real transcript named by
    /// `GAGE_CLAUDE_SCHEMA_TRANSCRIPT`. Passes without one.
    #[test]
    fn safe_paths_select_only_opaque_tokens_in_a_real_transcript_when_named() {
        let Some(path) = env::var_os("GAGE_CLAUDE_SCHEMA_TRANSCRIPT") else {
            return;
        };
        let text = fs::read_to_string(Path::new(&path)).unwrap();
        let failures = check_safe_shapes(&text);
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    #[test]
    fn pii_filter_paths_select_the_username_and_the_email_field() {
        let line = r#"{"type":"api-request","params":{"safeguards":[{"classifier_context":{"user_identity":"garrett","home_dir":"/home/garrett"}}]}}"#;
        let found = text_descriptor().filter_values("pii", line).unwrap();
        assert_eq!(found.literals, ["garrett", "/home/garrett"]);
        let attachment = r#"{"type":"attachment","attachment":{"context":{"userEmail":"The user's email address is g@x.y"}}}"#;
        let found = text_descriptor().filter_values("pii", attachment).unwrap();
        assert_eq!(found.values, ["/attachment/context/userEmail"]);
    }
}
