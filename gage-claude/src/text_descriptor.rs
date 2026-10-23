//! The Claude driver's text descriptor: which fields of a transcript
//! line are not text, and which blocks are atomic. The `text_block`
//! table and the publish redaction engine read it.
//! `schema/claude-jsonl-1/text_descriptor.json` holds the pattern
//! lists beside the content schemas of the same format.

use gage_session::TextDescriptor;
use serde::Deserialize;

const DESCRIPTOR: &str = include_str!("../schema/claude-jsonl-1/text_descriptor.json");

#[derive(Deserialize)]
struct Lists {
    safe: Vec<String>,
    atomic: Vec<String>,
}

pub(crate) fn text_descriptor() -> TextDescriptor {
    let lists: Lists =
        serde_json::from_str(DESCRIPTOR).expect("descriptor file should hold two string lists");
    let safe: Vec<&str> = lists.safe.iter().map(String::as_str).collect();
    let atomic: Vec<&str> = lists.atomic.iter().map(String::as_str).collect();
    TextDescriptor::new(&safe, &atomic).expect("descriptor patterns should be JSONPath")
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use gage_session::TextValue;

    use super::*;

    const SAMPLE: &str = include_str!("../schema/claude-jsonl-1/sample.jsonl");

    fn sample_values() -> Vec<(usize, Vec<TextValue>)> {
        let descriptor = text_descriptor();
        SAMPLE
            .lines()
            .enumerate()
            .filter(|(_, l)| !l.trim().is_empty())
            .map(|(i, l)| (i + 1, descriptor.text_values(l).unwrap()))
            .collect()
    }

    /// No structural field of the sample transcript is a text value.
    #[test]
    fn structural_fields_are_not_text() {
        let structural: HashSet<&str> = [
            "/uuid",
            "/parentUuid",
            "/sessionId",
            "/timestamp",
            "/type",
            "/version",
            "/entrypoint",
            "/userType",
            "/promptId",
            "/permissionMode",
            "/requestId",
            "/effort",
            "/requestedModel",
            "/sourceToolAssistantUUID",
            "/permissionDecision/decision",
            "/permissionDecision/source",
            "/permissionDecision/reasonType",
            "/message/role",
            "/message/id",
            "/message/type",
            "/message/model",
            "/message/stop_reason",
            "/message/usage/service_tier",
            "/message/content/0/type",
            "/message/content/0/id",
            "/message/content/0/tool_use_id",
            "/message/content/0/signature",
            "/message/content/0/caller/type",
        ]
        .into_iter()
        .collect();
        for (line, values) in sample_values() {
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
        let values = sample_values();
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
}
