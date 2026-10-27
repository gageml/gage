//! The publish definition, `publish/<id>.publish.json`: what the
//! staged session is derived from. The filters with their prompts,
//! the segments, every text redaction with its author, the literals
//! a filter may not record again, and the counters ids come from. It
//! may hold the redacted literals, because it is instructions and is
//! never committed. The original the staged session derives from is
//! named by the staged `attrs.json`, not here.

use std::collections::HashSet;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::PublishError;

/// The filters with a built-in definition, in listing order.
pub const NAMED_FILTERS: &[&str] = &[
    "pii",
    "language",
    "secret",
    "system",
    "non-narrative",
    "custom",
];

/// Whether the filter `name` has no instructions of its own and takes
/// its prompt as the whole instruction.
pub fn requires_prompt(name: &str) -> bool {
    name == "custom"
}

/// A reason code a redaction may carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reason {
    pub code: &'static str,
    /// One line on what the code covers, for a listing
    pub description: &'static str,
}

impl Reason {
    /// The reason with `code`, when there is one.
    pub fn find(code: &str) -> Option<&'static Reason> {
        REASONS.iter().find(|r| r.code == code)
    }
}

/// The reason codes, in listing order.
pub const REASONS: &[Reason] = &[
    Reason {
        code: "privacy",
        description: "Private conversational content (candid tone directed at an agent)",
    },
    Reason {
        code: "language",
        description: "Profanity and other language unfit for a public record",
    },
    Reason {
        code: "pii",
        description: "Content identifying a person (name, email address, phone number)",
    },
    Reason {
        code: "secret",
        description: "A credential or key (password, API key, token)",
    },
    Reason {
        code: "system",
        description: "Information about the machine or network the session ran on (path, host, OS version)",
    },
    Reason {
        code: "confidential",
        description: "Trade secrets and commercial, financial, or privileged content",
    },
    Reason {
        code: "opaque",
        description: "Content that cannot be evaluated",
    },
    Reason {
        code: "policy",
        description: "Internal personnel rules and practices of an organization",
    },
    Reason {
        code: "safety",
        description: "Content whose disclosure could endanger a person's life or safety",
    },
    Reason {
        code: "scope",
        description: "Content not applicable to the publish scope",
    },
    Reason {
        code: "statute",
        description: "Content a statute exempts from disclosure",
    },
    Reason {
        code: "non-narrative",
        description: "Harness bookkeeping that is no part of the conversation narrative",
    },
    Reason {
        code: "custom",
        description: "Content the publish's own instructions remove",
    },
];

/// One filter of a publish: a named filter, with the prompt that adds
/// to its instructions or, for a filter that requires one, is its
/// instructions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilterSpec {
    pub name: String,
    pub prompt: Option<String>,
}

impl FilterSpec {
    /// The filter `name` under `prompt`. A name outside
    /// [`NAMED_FILTERS`] is an error, and so is a filter that requires
    /// a prompt without one.
    pub fn new(name: &str, prompt: Option<String>) -> Result<Self, PublishError> {
        if !NAMED_FILTERS.contains(&name) {
            return Err(PublishError::Input(format!(
                "unknown filter {name:?}; filters are {}",
                NAMED_FILTERS.join(", ")
            )));
        }
        if requires_prompt(name) && prompt.is_none() {
            return Err(PublishError::Input(format!(
                "filter {name} requires a prompt"
            )));
        }
        Ok(Self {
            name: name.to_string(),
            prompt,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

impl FromStr for FilterSpec {
    type Err = PublishError;

    /// A bare filter name, with no prompt.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::new(text, None)
    }
}

impl fmt::Display for FilterSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.name)
    }
}

/// A filter as the definition records it: a name, with the prompt
/// that adds to its instructions or is its instructions. A filter
/// without a prompt is written as a bare string.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "FilterRepr", into = "FilterRepr")]
pub struct FilterEntry {
    pub name: String,
    pub prompt: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum FilterRepr {
    Name(String),
    Full {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prompt: Option<String>,
    },
}

impl From<FilterRepr> for FilterEntry {
    fn from(repr: FilterRepr) -> Self {
        match repr {
            FilterRepr::Name(name) => FilterEntry { name, prompt: None },
            FilterRepr::Full { name, prompt } => FilterEntry { name, prompt },
        }
    }
}

impl From<FilterEntry> for FilterRepr {
    fn from(entry: FilterEntry) -> Self {
        match entry.prompt {
            None => FilterRepr::Name(entry.name),
            prompt => FilterRepr::Full {
                name: entry.name,
                prompt,
            },
        }
    }
}

impl FilterEntry {
    pub fn new(spec: &FilterSpec) -> Self {
        FilterEntry {
            name: spec.name.clone(),
            prompt: spec.prompt.clone(),
        }
    }

    pub fn spec(&self) -> FilterSpec {
        FilterSpec {
            name: self.name.clone(),
            prompt: self.prompt.clone(),
        }
    }

    /// The author the filter's redactions carry, `filter:<name>`.
    pub fn author(&self) -> String {
        format!("filter:{}", self.name)
    }
}

/// A segment: lines of the original the publication keeps, under the
/// description its author gave. When any segment has lines, every
/// line outside every segment is dropped. A segment without lines is
/// a description awaiting its cut and drops nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "SegmentRepr", into = "SegmentRepr")]
pub struct Segment {
    pub id: u64,
    pub description: String,
    /// Inclusive 1-based line ranges
    pub lines: Vec<(u32, u32)>,
    pub author: String,
}

type SegmentRepr = (u64, String, Vec<(u32, u32)>, String);

impl From<SegmentRepr> for Segment {
    fn from((id, description, lines, author): SegmentRepr) -> Self {
        Segment {
            id,
            description,
            lines,
            author,
        }
    }
}

impl From<Segment> for SegmentRepr {
    fn from(s: Segment) -> Self {
        (s.id, s.description, s.lines, s.author)
    }
}

/// One text redaction: `literal` is redacted wherever it appears in a
/// text value, replaced by the marker when `mark` is set and removed
/// otherwise.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "TextRepr", into = "TextRepr")]
pub struct TextEntry {
    pub id: u64,
    pub literal: String,
    pub reason: String,
    /// `user:<name>`, `session:<id>`, or `filter:<name>`
    pub author: String,
    pub mark: bool,
}

type TextRepr = (u64, String, String, String, bool);

impl From<TextRepr> for TextEntry {
    fn from((id, literal, reason, author, mark): TextRepr) -> Self {
        TextEntry {
            id,
            literal,
            reason,
            author,
            mark,
        }
    }
}

impl From<TextEntry> for TextRepr {
    fn from(t: TextEntry) -> Self {
        (t.id, t.literal, t.reason, t.author, t.mark)
    }
}

/// A literal a filter found that `author` removed. A filter run does
/// not record it again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "AllowRepr", into = "AllowRepr")]
pub struct Allow {
    pub literal: String,
    pub author: String,
}

type AllowRepr = (String, String);

impl From<AllowRepr> for Allow {
    fn from((literal, author): AllowRepr) -> Self {
        Allow { literal, author }
    }
}

impl From<Allow> for AllowRepr {
    fn from(a: Allow) -> Self {
        (a.literal, a.author)
    }
}

/// The definition of one pending publish.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishDefinition {
    pub filters: Vec<FilterEntry>,
    #[serde(default)]
    pub segments: Vec<Segment>,
    #[serde(default)]
    pub text: Vec<TextEntry>,
    #[serde(default)]
    pub allow: Vec<Allow>,
    /// The id the next segment takes
    #[serde(default = "first_id")]
    pub next_segment_id: u64,
    /// The id the next text redaction takes
    #[serde(default = "first_id")]
    pub next_text_id: u64,
}

fn first_id() -> u64 {
    1
}

impl PublishDefinition {
    pub fn new(filters: &[FilterSpec]) -> Self {
        Self {
            filters: filters.iter().map(FilterEntry::new).collect(),
            segments: Vec::new(),
            text: Vec::new(),
            allow: Vec::new(),
            next_segment_id: 1,
            next_text_id: 1,
        }
    }

    /// The names of the filters on the list, in list order.
    pub fn named_filters(&self) -> Vec<String> {
        self.filters.iter().map(|f| f.name.clone()).collect()
    }

    /// The filter named `name`, with its index.
    pub fn find_filter(&self, name: &str) -> Result<(usize, &FilterEntry), PublishError> {
        self.filters
            .iter()
            .enumerate()
            .find(|(_, f)| f.name == name)
            .ok_or_else(|| PublishError::Input(format!("no filter named {name:?} is on the list")))
    }

    /// Append a text redaction under the next id and return its id.
    pub fn push_text(&mut self, literal: &str, reason: &str, author: &str, mark: bool) -> u64 {
        let id = self.next_text_id;
        self.next_text_id += 1;
        self.text.push(TextEntry {
            id,
            literal: literal.to_string(),
            reason: reason.to_string(),
            author: author.to_string(),
            mark,
        });
        id
    }

    /// Append a segment under the next id and return its id.
    pub fn push_segment(&mut self, description: &str, lines: &[(u32, u32)], author: &str) -> u64 {
        let id = self.next_segment_id;
        self.next_segment_id += 1;
        self.segments.push(Segment {
            id,
            description: description.to_string(),
            lines: lines.to_vec(),
            author: author.to_string(),
        });
        id
    }

    /// The text redaction whose literal is `literal`, compared the way
    /// redaction matches, without case.
    pub fn find_text(&self, literal: &str) -> Option<&TextEntry> {
        self.text
            .iter()
            .find(|t| t.literal.eq_ignore_ascii_case(literal))
    }

    /// Whether a filter may not record `literal`: it was removed
    /// before. Literals match the way redaction does, without case.
    pub fn is_allowed(&self, literal: &str) -> bool {
        self.allow
            .iter()
            .any(|a| a.literal.eq_ignore_ascii_case(literal))
    }

    /// Check a definition as read from disk, which may have been
    /// edited by hand: every filter is known and on the list once, a
    /// filter that requires a prompt has one, every reason is a known
    /// code, every literal is non-empty, every segment has ordered
    /// 1-based ranges, and ids are unique and below their counters.
    pub fn validate(&self) -> Result<(), PublishError> {
        let mut names = HashSet::new();
        for f in &self.filters {
            FilterSpec::new(&f.name, f.prompt.clone())?;
            if !names.insert(f.name.as_str()) {
                return Err(PublishError::Input(format!(
                    "filter {} is on the list more than once",
                    f.name
                )));
            }
        }
        let mut seen = HashSet::new();
        for s in &self.segments {
            let at = |why: String| PublishError::Input(format!("segment {}: {why}", s.id));
            if !seen.insert(s.id) {
                return Err(at("id is not unique".into()));
            }
            if s.id >= self.next_segment_id {
                return Err(at(format!(
                    "id is not below next_segment_id {}",
                    self.next_segment_id
                )));
            }
            if s.lines
                .iter()
                .any(|(start, end)| *start == 0 || end < start)
            {
                return Err(at(
                    "lines are 1-based and a range's end is not before its start".into(),
                ));
            }
        }
        let mut seen = HashSet::new();
        for t in &self.text {
            let at = |why: String| PublishError::Input(format!("redaction {}: {why}", t.id));
            if !seen.insert(t.id) {
                return Err(at("id is not unique".into()));
            }
            if t.id >= self.next_text_id {
                return Err(at(format!(
                    "id is not below next_text_id {}",
                    self.next_text_id
                )));
            }
            if t.literal.is_empty() {
                return Err(at("literal is empty".into()));
            }
            check_reason(&t.reason).map_err(|e| at(e.to_string()))?;
        }
        Ok(())
    }

    /// The definition as the sidecar holds it: one filter, segment,
    /// redaction, or allowed literal per line, so the file reads and
    /// edits by hand.
    pub fn to_json_text(&self) -> String {
        fn list<T: Serialize>(out: &mut String, key: &str, items: &[T]) {
            out.push_str(&format!("  \"{key}\": ["));
            for (i, item) in items.iter().enumerate() {
                out.push_str("\n    ");
                out.push_str(&json!(item).to_string());
                if i + 1 < items.len() {
                    out.push(',');
                } else {
                    out.push_str("\n  ");
                }
            }
            out.push_str("],\n");
        }
        let mut out = String::from("{\n");
        list(&mut out, "filters", &self.filters);
        list(&mut out, "segments", &self.segments);
        list(&mut out, "text", &self.text);
        list(&mut out, "allow", &self.allow);
        out.push_str(&format!(
            "  \"next_segment_id\": {},\n  \"next_text_id\": {}\n}}\n",
            self.next_segment_id, self.next_text_id
        ));
        out
    }
}

/// A redaction as the user or agent specifies it: a literal redacted
/// everywhere under `reason`, marked unless `mark` is false.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedactionInput {
    pub reason: String,
    pub literal: String,
    pub mark: bool,
}

impl RedactionInput {
    /// A literal redacted everywhere under `reason`, marked unless
    /// `mark` is false. An empty literal and an unknown reason are
    /// errors.
    pub fn parse_text(literal: &str, reason: &str, mark: bool) -> Result<Self, PublishError> {
        if literal.is_empty() {
            return Err(PublishError::Input("text: literal is empty".to_string()));
        }
        check_reason(reason)?;
        Ok(RedactionInput {
            reason: reason.to_string(),
            literal: literal.to_string(),
            mark,
        })
    }
}

/// `reason` is a known code.
fn check_reason(reason: &str) -> Result<(), PublishError> {
    if Reason::find(reason).is_some() {
        return Ok(());
    }
    Err(PublishError::Input(format!(
        "unknown reason {reason:?}; reasons are {}",
        REASONS
            .iter()
            .map(|r| r.code)
            .collect::<Vec<_>>()
            .join(", ")
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_takes_any_reason_and_a_non_empty_literal() {
        assert_eq!(
            RedactionInput::parse_text("sk-ant api key", "secret", true).unwrap(),
            RedactionInput {
                reason: "secret".into(),
                literal: "sk-ant api key".into(),
                mark: true,
            }
        );
        assert!(RedactionInput::parse_text("fucking", "language", false).is_ok());
        assert!(RedactionInput::parse_text("", "secret", true).is_err());
        assert!(RedactionInput::parse_text("x", "nope", true).is_err());
    }

    /// A bare name parses, a name outside the list and a custom filter
    /// without a prompt are errors, and any filter takes a prompt.
    #[test]
    fn filter_spec_takes_a_known_name_and_an_optional_prompt() {
        assert_eq!(
            "pii".parse::<FilterSpec>().unwrap(),
            FilterSpec {
                name: "pii".into(),
                prompt: None
            }
        );
        let bad = "piii".parse::<FilterSpec>().unwrap_err();
        assert!(bad.to_string().contains("piii"), "{bad}");
        let custom = "custom".parse::<FilterSpec>().unwrap_err();
        assert!(custom.to_string().contains("requires a prompt"), "{custom}");
        let with = FilterSpec::new("system", Some("Watch for hosts.".into())).unwrap();
        assert_eq!(with.to_string(), "system");
        assert!(requires_prompt("custom") && !requires_prompt("system"));
    }

    #[test]
    fn every_reason_has_a_description() {
        for r in REASONS {
            assert!(!r.description.is_empty(), "{}", r.code);
        }
        assert!(Reason::find("scope").is_some());
        assert!(Reason::find("nope").is_none());
    }

    /// The sidecar holds one filter, segment, redaction, or allowed
    /// literal per line, a bare filter name as a string, and reads
    /// back equal.
    #[test]
    fn definition_writes_one_entry_per_line_and_round_trips() {
        let mut d = PublishDefinition::new(&[
            "pii".parse().unwrap(),
            FilterSpec::new("custom", Some("Ducks only".into())).unwrap(),
        ]);
        d.filters.push(FilterEntry {
            name: "system".into(),
            prompt: Some("Watch for hosts.".into()),
        });
        let first = d.push_text("abc", "secret", "user:g", true);
        let second = d.push_text("Sam", "pii", "filter:pii", false);
        let segment = d.push_segment("The reply", &[(100, 125), (130, 130)], "session:s");
        d.allow.push(Allow {
            literal: "hunter2".into(),
            author: "user:g".into(),
        });
        assert_eq!((first, second, segment), (1, 2, 1));
        let text = d.to_json_text();
        assert_eq!(
            text,
            concat!(
                "{\n",
                "  \"filters\": [\n",
                "    \"pii\",\n",
                "    {\"name\":\"custom\",\"prompt\":\"Ducks only\"},\n",
                "    {\"name\":\"system\",\"prompt\":\"Watch for hosts.\"}\n",
                "  ],\n",
                "  \"segments\": [\n",
                "    [1,\"The reply\",[[100,125],[130,130]],\"session:s\"]\n",
                "  ],\n",
                "  \"text\": [\n",
                "    [1,\"abc\",\"secret\",\"user:g\",true],\n",
                "    [2,\"Sam\",\"pii\",\"filter:pii\",false]\n",
                "  ],\n",
                "  \"allow\": [\n",
                "    [\"hunter2\",\"user:g\"]\n",
                "  ],\n",
                "  \"next_segment_id\": 2,\n",
                "  \"next_text_id\": 3\n",
                "}\n",
            )
        );
        let back: PublishDefinition = serde_json::from_str(&text).unwrap();
        assert_eq!(back, d);
        assert_eq!(
            back.filters[1].spec(),
            FilterSpec::new("custom", Some("Ducks only".into())).unwrap()
        );
        assert_eq!(back.named_filters(), ["pii", "custom", "system"]);
        assert_eq!(back.find_filter("custom").unwrap().0, 1);
        assert!(back.find_filter("secret").is_err());
        assert!(back.is_allowed("HUNTER2"));
        assert!(!back.is_allowed("hunter"));
        assert_eq!(
            PublishDefinition::new(&[]).to_json_text(),
            "{\n  \"filters\": [],\n  \"segments\": [],\n  \"text\": [],\n  \"allow\": [],\n  \"next_segment_id\": 1,\n  \"next_text_id\": 1\n}\n"
        );
    }

    /// A missing list or counter reads as empty or 1, and the checks
    /// name the entry that is wrong.
    #[test]
    fn validate_names_the_bad_entry() {
        let d: PublishDefinition = serde_json::from_str(r#"{"filters": ["pii"]}"#).unwrap();
        assert_eq!(d, PublishDefinition::new(&["pii".parse().unwrap()]));
        d.validate().unwrap();
        let check = |json: &str, expect: &str| {
            let d: PublishDefinition = serde_json::from_str(json).unwrap();
            let err = d.validate().unwrap_err().to_string();
            assert!(err.contains(expect), "{err}");
        };
        check(r#"{"filters": ["nope"]}"#, "unknown filter");
        check(r#"{"filters": ["custom"]}"#, "requires a prompt");
        check(
            r#"{"filters": ["pii", {"name": "pii", "prompt": "x"}]}"#,
            "more than once",
        );
        check(
            r#"{"filters": [], "text": [[1, "", "pii", "u", true]], "next_text_id": 2}"#,
            "redaction 1: literal is empty",
        );
        check(
            r#"{"filters": [], "text": [[1, "x", "nope", "u", true]], "next_text_id": 2}"#,
            "redaction 1: unknown reason",
        );
        check(
            r#"{"filters": [], "text": [[1, "x", "pii", "u", true]]}"#,
            "not below next_text_id",
        );
        check(
            r#"{"filters": [], "text": [[1, "x", "pii", "u", true], [1, "y", "pii", "u", true]], "next_text_id": 3}"#,
            "not unique",
        );
        check(
            r#"{"filters": [], "segments": [[1, "d", [[5, 2]], "u"]], "next_segment_id": 2}"#,
            "segment 1: lines are 1-based",
        );
        let pending: PublishDefinition = serde_json::from_str(
            r#"{"filters": [], "segments": [[1, "d", [], "u"]], "next_segment_id": 2}"#,
        )
        .unwrap();
        pending.validate().unwrap();
    }
}
