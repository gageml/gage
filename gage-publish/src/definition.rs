//! The publish definition, `publish/<id>.publish.json`: what the
//! staged session is derived from. The filters, every redaction
//! instruction with its author, and the suppressions. It may hold the
//! redacted literals, because it is instructions and is never
//! committed.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::PublishError;

/// The filters with a built-in definition. Any other filter text is
/// an instruction to the agent.
pub const NAMED_FILTERS: &[&str] = &["pii", "language", "secret"];

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
];

/// One filter of a publish: a named filter, or an instruction to the
/// agent describing what to remove.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilterSpec {
    Named(String),
    Prompt(String),
}

impl FilterSpec {
    /// The filter `text` denotes: a named filter when `text` is one of
    /// [`NAMED_FILTERS`], else an instruction.
    pub fn from_text(text: &str) -> Self {
        if NAMED_FILTERS.contains(&text) {
            FilterSpec::Named(text.to_string())
        } else {
            FilterSpec::Prompt(text.to_string())
        }
    }

    /// The filter's name when it is a named filter.
    pub fn name(&self) -> Option<&str> {
        match self {
            FilterSpec::Named(name) => Some(name),
            FilterSpec::Prompt(_) => None,
        }
    }
}

impl fmt::Display for FilterSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FilterSpec::Named(name) => f.write_str(name),
            FilterSpec::Prompt(prompt) => f.write_str(prompt),
        }
    }
}

/// A filter as the definition records it, with the Gage version its
/// deterministic rules last ran under.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilterEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    pub version: String,
}

impl FilterEntry {
    pub fn new(spec: &FilterSpec, version: &str) -> Self {
        match spec {
            FilterSpec::Named(name) => FilterEntry {
                name: Some(name.clone()),
                prompt: None,
                version: version.to_string(),
            },
            FilterSpec::Prompt(prompt) => FilterEntry {
                name: None,
                prompt: Some(prompt.clone()),
                version: version.to_string(),
            },
        }
    }

    pub fn spec(&self) -> FilterSpec {
        match (&self.name, &self.prompt) {
            (Some(name), _) => FilterSpec::Named(name.clone()),
            (None, Some(prompt)) => FilterSpec::Prompt(prompt.clone()),
            (None, None) => FilterSpec::Prompt(String::new()),
        }
    }
}

/// How a span redaction replaces its span.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SpanMode {
    /// Remove the span and normalize the whitespace it leaves
    Remove,
    /// Replace the span with the marker
    Mark,
}

impl FromStr for SpanMode {
    type Err = PublishError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "remove" => Ok(SpanMode::Remove),
            "mark" => Ok(SpanMode::Mark),
            other => Err(PublishError::Input(format!(
                "mode must be \"remove\" or \"mark\", not {other:?}"
            ))),
        }
    }
}

impl fmt::Display for SpanMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            SpanMode::Remove => "remove",
            SpanMode::Mark => "mark",
        })
    }
}

/// What one redaction instruction does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum RedactionKind {
    /// Drop the lines `line` through `line_end`
    Lines {
        line: u32,
        line_end: u32,
        reason: String,
    },
    /// Redact `literal` wherever it appears in the session
    Text { reason: String, literal: String },
    /// Redact `literal` within the one text value at `pointer` of
    /// `line`
    Span {
        line: u32,
        pointer: String,
        reason: String,
        mode: SpanMode,
        literal: String,
    },
    /// Replace the whole text value at `pointer` of `line`
    Value {
        line: u32,
        pointer: String,
        reason: String,
    },
}

impl RedactionKind {
    pub fn reason(&self) -> &str {
        match self {
            RedactionKind::Lines { reason, .. }
            | RedactionKind::Text { reason, .. }
            | RedactionKind::Span { reason, .. }
            | RedactionKind::Value { reason, .. } => reason,
        }
    }

    /// The kind's name as `status` shows it.
    pub fn kind_name(&self) -> &'static str {
        match self {
            RedactionKind::Lines { .. } => "lines",
            RedactionKind::Text { .. } => "text",
            RedactionKind::Span { .. } => "span",
            RedactionKind::Value { .. } => "value",
        }
    }

    /// What the redaction targets, as `status` shows it.
    pub fn detail(&self) -> String {
        match self {
            RedactionKind::Lines { line, line_end, .. } if line == line_end => line.to_string(),
            RedactionKind::Lines { line, line_end, .. } => format!("{line}-{line_end}"),
            RedactionKind::Text { literal, .. } => literal.clone(),
            RedactionKind::Span {
                line,
                pointer,
                mode,
                literal,
                ..
            } => format!("{line} {pointer} {mode} {literal}"),
            RedactionKind::Value { line, pointer, .. } => format!("{line} {pointer}"),
        }
    }
}

/// One redaction instruction of the definition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Redaction {
    pub id: u64,
    /// `user:<name>`, `session:<id>`, or `filter:<name>`
    pub author: String,
    /// For a filter's redaction, the rule that produced it: a pattern
    /// name or a typed path
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule: Option<String>,
    /// How many instances the last build recorded for it
    #[serde(default)]
    pub instances: usize,
    #[serde(flatten)]
    pub kind: RedactionKind,
}

/// A filter's redaction that `undo` removed and a re-run of the
/// filter must not restore.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Suppression {
    pub author: String,
    pub rule: String,
    pub line: u32,
    pub pointer: String,
}

/// The definition of one pending publish.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishDefinition {
    /// The original, `session:<id>@<commit>`
    pub origin: String,
    pub filters: Vec<FilterEntry>,
    #[serde(default)]
    pub redactions: Vec<Redaction>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub suppressions: Vec<Suppression>,
    /// The id the next redaction takes
    #[serde(default = "first_id")]
    pub next_id: u64,
}

fn first_id() -> u64 {
    1
}

impl PublishDefinition {
    pub fn new(origin: &str, filters: &[FilterSpec], version: &str) -> Self {
        Self {
            origin: origin.to_string(),
            filters: filters
                .iter()
                .map(|f| FilterEntry::new(f, version))
                .collect(),
            redactions: Vec::new(),
            suppressions: Vec::new(),
            next_id: 1,
        }
    }

    /// Append a redaction under the next id and return its id.
    pub fn push(&mut self, author: &str, rule: Option<String>, kind: RedactionKind) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.redactions.push(Redaction {
            id,
            author: author.to_string(),
            rule,
            instances: 0,
            kind,
        });
        id
    }

    /// The `(id, commit)` of the original the `origin` names.
    pub fn original(&self) -> Result<(&str, &str), PublishError> {
        self.origin
            .strip_prefix("session:")
            .and_then(|rest| rest.split_once('@'))
            .ok_or_else(|| {
                PublishError::Input(format!(
                    "origin {:?} is not session:<id>@<commit>",
                    self.origin
                ))
            })
    }
}

/// A redaction as the user or agent specifies it, before the engine
/// resolves it against the session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RedactionInput {
    Lines {
        line: u32,
        line_end: u32,
        reason: String,
    },
    Text {
        reason: String,
        literal: String,
    },
    /// `seq` names the value within `line` as `text_block` numbers it
    Span {
        line: u32,
        seq: u32,
        reason: String,
        mode: SpanMode,
        literal: String,
    },
}

impl RedactionInput {
    /// `<LINE> [<LINE_END>]` under `reason`. A missing end is the line
    /// itself.
    pub fn parse_lines(spec: &str, reason: &str) -> Result<Self, PublishError> {
        let mut parts = spec.split_whitespace();
        let bad = |why: String| PublishError::Input(format!("lines {spec:?}: {why}"));
        let shape = || bad("expected <LINE> [<LINE_END>]".to_string());
        let line: u32 = parts
            .next()
            .ok_or_else(shape)?
            .parse()
            .map_err(|e| bad(format!("line: {e}")))?;
        let line_end: u32 = match parts.next() {
            Some(end) => end.parse().map_err(|e| bad(format!("line end: {e}")))?,
            None => line,
        };
        if parts.next().is_some() {
            return Err(shape());
        }
        if line == 0 || line_end < line {
            return Err(bad(
                "lines are 1-based and the end is not before the start".to_string()
            ));
        }
        check_reason(reason)?;
        Ok(RedactionInput::Lines {
            line,
            line_end,
            reason: reason.to_string(),
        })
    }

    /// A literal redacted everywhere under `reason`.
    pub fn parse_text(literal: &str, reason: &str) -> Result<Self, PublishError> {
        if literal.is_empty() {
            return Err(PublishError::Input("text: literal is empty".to_string()));
        }
        check_reason(reason)?;
        Ok(RedactionInput::Text {
            reason: reason.to_string(),
            literal: literal.to_string(),
        })
    }

    /// `<LINE> <SEQ> <MODE> <TEXT>` under `reason`; the literal runs to
    /// the end.
    pub fn parse_span(spec: &str, reason: &str) -> Result<Self, PublishError> {
        let bad = |why: String| PublishError::Input(format!("span {spec:?}: {why}"));
        let shape = || bad("expected <LINE> <SEQ> <MODE> <TEXT>".to_string());
        let mut parts = spec.trim_start().splitn(4, ' ');
        let line: u32 = parts
            .next()
            .ok_or_else(shape)?
            .parse()
            .map_err(|e| bad(format!("line: {e}")))?;
        let seq: u32 = parts
            .next()
            .ok_or_else(shape)?
            .parse()
            .map_err(|e| bad(format!("seq: {e}")))?;
        let mode: SpanMode = parts.next().ok_or_else(shape)?.parse()?;
        let literal = parts.next().ok_or_else(shape)?.trim_start();
        if literal.is_empty() {
            return Err(shape());
        }
        check_reason(reason)?;
        Ok(RedactionInput::Span {
            line,
            seq,
            reason: reason.to_string(),
            mode,
            literal: literal.to_string(),
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
    fn lines_spec_defaults_the_end() {
        assert_eq!(
            RedactionInput::parse_lines("7", "scope").unwrap(),
            RedactionInput::Lines {
                line: 7,
                line_end: 7,
                reason: "scope".into()
            }
        );
        assert_eq!(
            RedactionInput::parse_lines("10 15", "opaque").unwrap(),
            RedactionInput::Lines {
                line: 10,
                line_end: 15,
                reason: "opaque".into()
            }
        );
        assert!(RedactionInput::parse_lines("15 10", "scope").is_err());
        assert!(RedactionInput::parse_lines("1 1 scope", "scope").is_err());
        assert!(RedactionInput::parse_lines("1", "nope").is_err());
    }

    #[test]
    fn text_takes_any_reason_and_a_non_empty_literal() {
        assert_eq!(
            RedactionInput::parse_text("sk-ant api key", "secret").unwrap(),
            RedactionInput::Text {
                reason: "secret".into(),
                literal: "sk-ant api key".into()
            }
        );
        assert!(RedactionInput::parse_text("fucking", "privacy").is_ok());
        assert!(RedactionInput::parse_text("", "secret").is_err());
        assert!(RedactionInput::parse_text("x", "nope").is_err());
    }

    #[test]
    fn span_spec_names_the_value_the_mode_and_the_literal() {
        assert_eq!(
            RedactionInput::parse_span("12 0 remove fucking hell", "privacy").unwrap(),
            RedactionInput::Span {
                line: 12,
                seq: 0,
                reason: "privacy".into(),
                mode: SpanMode::Remove,
                literal: "fucking hell".into()
            }
        );
        assert!(RedactionInput::parse_span("12 0 mark x", "secret").is_ok());
        assert!(RedactionInput::parse_span("12 0 drop x", "privacy").is_err());
    }

    #[test]
    fn every_reason_has_a_description() {
        for r in REASONS {
            assert!(!r.description.is_empty(), "{}", r.code);
        }
        assert!(Reason::find("scope").is_some());
        assert!(Reason::find("nope").is_none());
    }

    #[test]
    fn definition_round_trips_and_numbers_redactions() {
        let mut d = PublishDefinition::new(
            "session:abc@def",
            &[
                FilterSpec::from_text("pii"),
                FilterSpec::from_text("Ducks only"),
            ],
            "0.2.0",
        );
        assert_eq!(d.original().unwrap(), ("abc", "def"));
        let first = d.push(
            "user:g",
            None,
            RedactionKind::Text {
                reason: "secret".into(),
                literal: "abc".into(),
            },
        );
        let second = d.push(
            "filter:pii",
            Some("email".into()),
            RedactionKind::Value {
                line: 3,
                pointer: "/x".into(),
                reason: "pii".into(),
            },
        );
        assert_eq!((first, second), (1, 2));
        let text = serde_json::to_string_pretty(&d).unwrap();
        assert!(text.contains("\"type\": \"text\""), "{text}");
        assert!(text.contains("\"name\": \"pii\""), "{text}");
        assert!(text.contains("\"prompt\": \"Ducks only\""), "{text}");
        let back: PublishDefinition = serde_json::from_str(&text).unwrap();
        assert_eq!(back, d);
        assert_eq!(
            back.filters[1].spec(),
            FilterSpec::Prompt("Ducks only".into())
        );
    }
}
