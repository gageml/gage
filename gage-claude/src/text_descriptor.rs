//! The Claude driver's text descriptor: which fields of a transcript
//! line are identifiers, timestamps, digests, signatures, or
//! enumerations rather than text, which fields a named filter treats
//! as sensitive by type, and which entries the harness wrote for its
//! own bookkeeping. The publish redaction engine reads it.
//! `schema/claude-jsonl-1/text_descriptor.json` holds the pattern
//! lists beside the content schemas of the same format.

use std::collections::BTreeMap;

use gage_session::TextDescriptor;
use serde::Deserialize;

const DESCRIPTOR: &str = include_str!("../schema/claude-jsonl-1/text_descriptor.json");

#[derive(Deserialize)]
struct Lists {
    safe: Vec<String>,
    #[serde(default)]
    filters: BTreeMap<String, FilterLists>,
}

#[derive(Deserialize)]
struct FilterLists {
    #[serde(default)]
    literal: Vec<String>,
    #[serde(default)]
    value: Vec<String>,
    #[serde(default)]
    entries: Vec<String>,
}

pub(crate) fn text_descriptor() -> TextDescriptor {
    let lists: Lists =
        serde_json::from_str(DESCRIPTOR).expect("descriptor file should hold pattern lists");
    fn strs(v: &[String]) -> Vec<&str> {
        v.iter().map(String::as_str).collect()
    }
    let mut descriptor =
        TextDescriptor::new(&strs(&lists.safe)).expect("descriptor patterns should be JSONPath");
    for (name, paths) in &lists.filters {
        descriptor = descriptor
            .with_filter(
                name,
                &strs(&paths.literal),
                &strs(&paths.value),
                &strs(&paths.entries),
            )
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

    use super::*;
    use crate::entry::{SYSTEM_MESSAGE_SUBTYPES, message_subtype};
    use gage_session::{FilterValues, TextValue};

    fn pointers(found: &FilterValues) -> Vec<String> {
        found.values.iter().map(|h| h.pointer.clone()).collect()
    }

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

    /// A prompt, a thinking block, the working directory, a tool
    /// call's input, a tool result's content, and the harness's
    /// `toolUseResult` copy are all text values.
    #[test]
    fn sample_values_are_text() {
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
        for (line, pointer) in [
            (1, "/cwd"),
            (2, "/message/content/0/thinking"),
            (3, "/message/content/0/input/command"),
            (4, "/message/content/0/content"),
            (4, "/toolUseResult/stdout"),
        ] {
            assert!(!find(line, pointer).text.is_empty());
        }
    }

    /// A value the descriptor excludes as safe is an opaque token: no
    /// whitespace, and only the characters of an identifier, a
    /// timestamp, a digest, a signature, or an enumeration. The
    /// excluded strings are found by subtraction: every string of the
    /// line, through a descriptor with no safe list, minus the text
    /// values the real descriptor keeps. A safe pattern that covers
    /// free text fails here.
    fn check_safe_shapes(text: &str) -> Vec<String> {
        let all = TextDescriptor::new(&[]).unwrap();
        let real = text_descriptor();
        let opaque = |s: &str| {
            s.chars()
                .all(|c| c.is_ascii_alphanumeric() || "_:+/=.-".contains(c))
        };
        let mut failures = Vec::new();
        for (i, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let Ok(every) = all.text_values(line) else {
                continue;
            };
            let kept: HashSet<String> = real
                .text_values(line)
                .unwrap()
                .into_iter()
                .map(|v| v.pointer)
                .collect();
            for v in every {
                if !kept.contains(&v.pointer) && !opaque(&v.text) {
                    failures.push(format!(
                        "line {}: safe path {} holds text {:?}",
                        i + 1,
                        v.pointer,
                        v.text
                    ));
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

    /// The `harness` filter leaves out every entry that is not a user
    /// prompt, an assistant message, or a system entry the message
    /// table promotes, and leaves out a user entry the harness injected
    /// as meta. The selectors agree with the entry classifier on the
    /// sample transcript and on one entry of each shape.
    #[test]
    fn harness_filter_leaves_out_every_entry_that_is_not_conversation() {
        let descriptor = text_descriptor();
        let dropped = |line: &str| descriptor.filter_values("harness", line).unwrap().dropped;
        let conversation = |line: &str| {
            let entry: serde_json::Value = serde_json::from_str(line).unwrap();
            let ty = entry["type"].as_str().unwrap_or_default();
            let meta = entry["isMeta"].as_bool() == Some(true);
            match ty {
                "user" => !meta,
                "assistant" => true,
                "system" => message_subtype(&entry).is_some(),
                _ => false,
            }
        };
        for line in SAMPLE.lines().filter(|l| !l.trim().is_empty()) {
            assert_eq!(dropped(line), !conversation(line), "{line}");
        }
        let shapes = [
            r#"{"type":"user","message":{"role":"user","content":"hi"}}"#,
            r#"{"type":"user","isMeta":true,"message":{"role":"user","content":"<meta>"}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hi"}]}}"#,
            r#"{"type":"system","subtype":"turn_duration","durationMs":5}"#,
            r#"{"type":"system","subtype":"informational","content":"note"}"#,
            r#"{"type":"system","content":"no subtype"}"#,
            r#"{"type":"attachment","attachment":{"type":"skill_listing"}}"#,
            r#"{"type":"progress","data":{}}"#,
            r#"{"type":"file-history-snapshot","snapshot":{}}"#,
            r#"{"type":"api-request","params":{}}"#,
            r#"{"type":"mode","mode":"normal"}"#,
        ];
        for line in shapes {
            assert_eq!(dropped(line), !conversation(line), "{line}");
        }
        for subtype in SYSTEM_MESSAGE_SUBTYPES {
            let line = format!(r#"{{"type":"system","subtype":"{subtype}","content":"x"}}"#);
            assert!(!dropped(&line), "{line}");
        }
    }

    #[test]
    fn pii_filter_paths_select_the_username_and_the_email_field() {
        let line = r#"{"type":"api-request","params":{"safeguards":[{"classifier_context":{"user_identity":"garrett","home_dir":"/home/garrett"}}]}}"#;
        let found = text_descriptor().filter_values("pii", line).unwrap();
        assert_eq!(found.literals, ["garrett", "/home/garrett"]);
        let attachment = r#"{"type":"attachment","attachment":{"context":{"userEmail":"The user's email address is g@x.y"}}}"#;
        let found = text_descriptor().filter_values("pii", attachment).unwrap();
        assert_eq!(pointers(&found), ["/attachment/context/userEmail"]);
        assert_eq!(found.values[0].path, "$.attachment.context.userEmail");
    }

    /// The `system` filter names every field that holds a path, a
    /// host, or an OS version, wherever the classifier context nests it.
    #[test]
    fn system_filter_paths_select_paths_hosts_and_the_os_version() {
        let descriptor = text_descriptor();
        let user = r#"{"type":"user","cwd":"/home/garrett/proj","gitBranch":"main","message":{"role":"user","content":"hi"}}"#;
        let found = descriptor.filter_values("system", user).unwrap();
        assert_eq!(pointers(&found), ["/cwd"]);
        assert_eq!(found.values[0].path, "$.cwd");
        let request = r#"{"type":"api-request","params":{"safeguards":[{"classifier_context":{"user_identity":"garrett","home_dir":"/home/garrett","live_cwd":"/home/garrett/proj","rule_roots":{"userSettings":"/home/garrett/.claude"},"trusted_directories":{"primary":{"path":"/home/garrett/proj","resolved":["/home/garrett/proj"]}},"git_state":{"cwd":"/home/garrett/proj","root":"/home/garrett/proj","branch":"main","visibility":{"push_remote":"origin","origin":{"remote":"git@git.example:a/b.git","host":"git.example","visibility":"private"},"remotes":[{"name":"origin","remote":"git@git.example:a/b.git","host":"git.example","visibility":"private"}]}},"prior_turn_context":[{"context":{"live_cwd":"/home/garrett/proj"}}]}}]}}"#;
        let found = descriptor.filter_values("system", request).unwrap();
        let context = "/params/safeguards/0/classifier_context";
        let expected: Vec<String> = [
            "/live_cwd",
            "/prior_turn_context/0/context/live_cwd",
            "/home_dir",
            "/git_state/cwd",
            "/git_state/root",
            "/trusted_directories/primary/path",
            "/trusted_directories/primary/resolved/0",
            "/rule_roots/userSettings",
            "/git_state/visibility/origin/host",
            "/git_state/visibility/remotes/0/host",
            "/git_state/visibility/origin/remote",
            "/git_state/visibility/remotes/0/remote",
            "/git_state/visibility/push_remote",
        ]
        .iter()
        .map(|p| format!("{context}{p}"))
        .collect();
        let mut found_sorted = pointers(&found);
        found_sorted.sort();
        let mut expected_sorted = expected.clone();
        expected_sorted.sort();
        assert_eq!(found_sorted, expected_sorted, "{found:#?}");
        assert!(!found.values.iter().any(|h| h.pointer.ends_with("/branch")));
        assert!(
            !found
                .values
                .iter()
                .any(|h| h.pointer.ends_with("/user_identity"))
        );

        let snapshot = r#"{"type":"attachment","attachment":{"type":"environment_snapshot","snapshot":{"workingDirectory":"/home/garrett/proj","scratchpadDirectory":"/tmp/s","additionalWorkingDirectories":["/home/garrett/other"],"platform":"linux","osVersion":"Linux 6.17.0-1032-oem","shell":"bash"}}}"#;
        let found = descriptor.filter_values("system", snapshot).unwrap();
        assert_eq!(
            pointers(&found),
            [
                "/attachment/snapshot/workingDirectory",
                "/attachment/snapshot/scratchpadDirectory",
                "/attachment/snapshot/additionalWorkingDirectories/0",
                "/attachment/snapshot/osVersion",
            ]
        );
        let backups = r#"{"type":"file-history-snapshot","snapshot":{"trackedFileBackups":{"src/a.rs":{"realParentDir":"/home/garrett/proj/src","backupFileName":"abc@v1"}}}}"#;
        let found = descriptor.filter_values("system", backups).unwrap();
        assert_eq!(
            pointers(&found),
            ["/snapshot/trackedFileBackups/src~1a.rs/realParentDir"]
        );
    }
}
