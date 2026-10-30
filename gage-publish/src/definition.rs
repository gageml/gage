//! The publish definition, `publish/<id>.publish.jsonc`: what the
//! staged session is derived from. The original session, the
//! arguments `init` was given, the filters with their prompts, the
//! segments, every text redaction with its author, and the literals a
//! filter may not record again. It may hold the redacted literals,
//! because it is instructions and is never committed. The file is
//! edited by hand, so it is JSON with comments and trailing commas,
//! written with the annotations an editor needs and read back without
//! them.

use std::collections::HashSet;
use std::fmt;
use std::str::FromStr;
use std::sync::LazyLock;

use gage_store::url::LineSelection;
use jsonc_parser::ParseOptions;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::PublishError;
use crate::filter::Filter;

/// The filters with a built-in definition, in listing order.
pub const NAMED_FILTERS: &[&str] = &[
    "pii",
    "language",
    "secret",
    "system",
    "harness-context",
    "harness-records",
    "custom",
];

/// Whether the filter `name` has no instructions of its own and takes
/// its prompt as the whole instruction.
pub fn requires_prompt(name: &str) -> bool {
    name == "custom"
}

/// A reason code a redaction may carry.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Reason {
    pub code: String,
    /// One line on what the code covers, for a listing
    pub description: String,
}

impl Reason {
    /// The reason with `code`, when there is one.
    pub fn find(code: &str) -> Option<&'static Reason> {
        REASONS.iter().find(|r| r.code == code)
    }
}

/// The reason codes, in listing order. A filter's prose cites codes
/// from this list; the list is not defined by the filters.
pub static REASONS: LazyLock<Vec<Reason>> = LazyLock::new(|| {
    serde_json::from_str(include_str!("../filters/reasons.json"))
        .expect("filters/reasons.json is fixed and valid")
});

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
/// without a prompt is written as a bare string, and one with a
/// prompt as `[name, prompt]`.
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
    WithPrompt(String, String),
}

impl From<FilterRepr> for FilterEntry {
    fn from(repr: FilterRepr) -> Self {
        match repr {
            FilterRepr::Name(name) => FilterEntry { name, prompt: None },
            FilterRepr::WithPrompt(name, prompt) => FilterEntry {
                name,
                prompt: Some(prompt),
            },
        }
    }
}

impl From<FilterEntry> for FilterRepr {
    fn from(entry: FilterEntry) -> Self {
        match entry.prompt {
            None => FilterRepr::Name(entry.name),
            Some(prompt) => FilterRepr::WithPrompt(entry.name, prompt),
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
/// a description awaiting its cut and drops nothing. Recorded as
/// `[lines, description, author]`, with the lines as `"12-20,31"` or
/// `null`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "SegmentRepr", into = "SegmentRepr")]
pub struct Segment {
    pub description: String,
    /// Inclusive 1-based line ranges
    pub lines: Vec<(u32, u32)>,
    pub author: String,
}

type SegmentRepr = (Option<String>, String, String);

impl TryFrom<SegmentRepr> for Segment {
    type Error = PublishError;

    fn try_from((lines, description, author): SegmentRepr) -> Result<Self, PublishError> {
        let lines = match lines {
            Some(text) => parse_lines(&text)?,
            None => Vec::new(),
        };
        Ok(Segment {
            description,
            lines,
            author,
        })
    }
}

impl From<Segment> for SegmentRepr {
    fn from(s: Segment) -> Self {
        let lines = (!s.lines.is_empty()).then(|| format_lines(&s.lines));
        (lines, s.description, s.author)
    }
}

/// The ranges `text` selects, `"12-20,31"`, as the segment holds
/// them.
pub fn parse_lines(text: &str) -> Result<Vec<(u32, u32)>, PublishError> {
    let at = |e: &dyn fmt::Display| PublishError::Input(format!("segment lines {text:?}: {e}"));
    let selection: LineSelection = text.parse().map_err(|e| at(&e))?;
    selection
        .ranges()
        .iter()
        .map(|&(start, end)| {
            Ok((
                u32::try_from(start).map_err(|e| at(&e))?,
                u32::try_from(end).map_err(|e| at(&e))?,
            ))
        })
        .collect()
}

/// The ranges as the definition writes them, `"12-20,31"`.
pub fn format_lines(lines: &[(u32, u32)]) -> String {
    lines
        .iter()
        .map(|&(start, end)| {
            if start == end {
                start.to_string()
            } else {
                format!("{start}-{end}")
            }
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// One text redaction: `literal` is redacted wherever it appears in a
/// text value, replaced by the marker when `mark` is set and removed
/// otherwise. Recorded as `[text, reason, author, mark]`. A row
/// written by hand may leave the author out, `[text, reason, mark]`;
/// the author is empty until a refresh fills it in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "TextRepr", into = "TextRepr")]
pub struct TextEntry {
    pub literal: String,
    pub reason: String,
    /// `user:<name>`, `session:<id>`, or `filter:<name>`
    pub author: String,
    pub mark: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum TextRepr {
    Full(String, String, String, bool),
    NoAuthor(String, String, bool),
}

impl From<TextRepr> for TextEntry {
    fn from(repr: TextRepr) -> Self {
        match repr {
            TextRepr::Full(literal, reason, author, mark) => TextEntry {
                literal,
                reason,
                author,
                mark,
            },
            TextRepr::NoAuthor(literal, reason, mark) => TextEntry {
                literal,
                reason,
                author: String::new(),
                mark,
            },
        }
    }
}

impl From<TextEntry> for TextRepr {
    fn from(t: TextEntry) -> Self {
        TextRepr::Full(t.literal, t.reason, t.author, t.mark)
    }
}

/// A literal a filter found that `author` removed. A filter run does
/// not record it again. Recorded as `[text, author]`.
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

/// What `init` was given, kept for the record. The publish does not
/// read it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitArgs {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audience: Option<String>,
    #[serde(default, rename = "use", skip_serializing_if = "Option::is_none")]
    pub uses: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

/// The definition of one pending publish.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishDefinition {
    /// The id of the original session
    pub session: String,
    #[serde(default)]
    pub init: InitArgs,
    #[serde(default)]
    pub filters: Vec<FilterEntry>,
    #[serde(default)]
    pub segments: Vec<Segment>,
    #[serde(default)]
    pub allow: Vec<Allow>,
    #[serde(default)]
    pub redact: Vec<TextEntry>,
}

impl PublishDefinition {
    pub fn new(session: &str, filters: &[FilterSpec]) -> Self {
        Self {
            session: session.to_string(),
            init: InitArgs::default(),
            filters: filters.iter().map(FilterEntry::new).collect(),
            segments: Vec::new(),
            allow: Vec::new(),
            redact: Vec::new(),
        }
    }

    /// The definition as the file holds it, comments and trailing
    /// commas included.
    pub fn from_jsonc(text: &str) -> Result<Self, PublishError> {
        let value: Value = jsonc_parser::parse_to_serde_value(text, &ParseOptions::default())
            .map_err(|e| PublishError::Input(format!("definition: {e}")))?;
        Ok(serde_json::from_value(value)?)
    }

    /// The names of the filters on the list, in list order.
    pub fn named_filters(&self) -> Vec<String> {
        self.filters.iter().map(|f| f.name.clone()).collect()
    }

    /// Append a text redaction.
    pub fn push_redact(&mut self, literal: &str, reason: &str, author: &str, mark: bool) {
        self.redact.push(TextEntry {
            literal: literal.to_string(),
            reason: reason.to_string(),
            author: author.to_string(),
            mark,
        });
    }

    /// Append a segment.
    pub fn push_segment(&mut self, description: &str, lines: &[(u32, u32)], author: &str) {
        self.segments.push(Segment {
            description: description.to_string(),
            lines: lines.to_vec(),
            author: author.to_string(),
        });
    }

    /// Give every redaction written without an author `author`.
    /// Returns the number of rows filled.
    pub fn fill_authors(&mut self, author: &str) -> usize {
        let mut filled = 0;
        for t in self.redact.iter_mut().filter(|t| t.author.is_empty()) {
            t.author = author.to_string();
            filled += 1;
        }
        filled
    }

    /// Whether a filter may not record `literal`: it was removed
    /// before. Literals match the way redaction does, without case.
    pub fn is_allowed(&self, literal: &str) -> bool {
        self.allow
            .iter()
            .any(|a| a.literal.eq_ignore_ascii_case(literal))
    }

    /// The redaction instructions of every filter as markdown: per
    /// filter a level-three heading with its name in code font, then
    /// its instructions with the prompt given for it in place of the
    /// placeholder. Sections are separated by a blank line.
    pub fn instructions_markdown(&self) -> Result<String, PublishError> {
        let mut out = String::new();
        for entry in &self.filters {
            let filter = Filter::named(&entry.name)
                .ok_or_else(|| PublishError::Input(format!("unknown filter {:?}", entry.name)))?;
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&format!(
                "### `{}`\n\n{}\n",
                filter.name,
                filter.instructions_with_prompt(entry.prompt.as_deref())
            ));
        }
        Ok(out)
    }

    /// Check a definition as read from disk, which may have been
    /// edited by hand: every filter is known and on the list once, a
    /// filter that requires a prompt has one, every segment has
    /// ordered 1-based ranges, and every redaction has a non-empty
    /// literal, a known reason, and an author. A row is named by its
    /// 1-based position.
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
        for (i, s) in self.segments.iter().enumerate() {
            if s.lines
                .iter()
                .any(|(start, end)| *start == 0 || end < start)
            {
                return Err(PublishError::Input(format!(
                    "segment {}: lines are 1-based and a range's end is not before its start",
                    i + 1
                )));
            }
        }
        for (i, t) in self.redact.iter().enumerate() {
            let at = |why: String| PublishError::Input(format!("redaction {}: {why}", i + 1));
            if t.literal.is_empty() {
                return Err(at("literal is empty".into()));
            }
            check_reason(&t.reason).map_err(|e| at(e.to_string()))?;
            if t.author.is_empty() {
                return Err(at("no author".into()));
            }
        }
        Ok(())
    }

    /// The definition as the file holds it: the annotations an editor
    /// needs as comments, `title` beside the session, and one filter,
    /// segment, allowed literal, or redaction per line with a trailing
    /// comma, so a row is added or deleted without touching another.
    pub fn to_jsonc_text(&self, title: Option<&str>) -> String {
        fn rows<T: Clone + Into<Value>>(out: &mut String, key: &str, items: &[T]) {
            out.push_str(&format!("  \"{key}\": ["));
            for item in items {
                out.push_str("\n    ");
                out.push_str(&row(&item.clone().into()));
                out.push(',');
            }
            if !items.is_empty() {
                out.push_str("\n  ");
            }
            out.push_str("],\n");
        }
        let mut out = String::from(HEADER);
        out.push_str("{\n");
        out.push_str(&format!("  \"session\": {},", json!(self.session)));
        if let Some(title) = title {
            out.push_str(&format!(
                " // title: {}",
                title.split_whitespace().collect::<Vec<_>>().join(" ")
            ));
        }
        out.push_str("\n\n");
        out.push_str(INIT_COMMENT);
        out.push_str(&format!(
            "  \"init\": {},\n\n",
            serde_json::to_string(&self.init).expect("init args are a JSON object")
        ));
        out.push_str(FILTERS_COMMENT);
        rows(&mut out, "filters", &self.filters);
        out.push('\n');
        out.push_str(SEGMENTS_COMMENT);
        rows(&mut out, "segments", &self.segments);
        out.push('\n');
        out.push_str(ALLOW_COMMENT);
        rows(&mut out, "allow", &self.allow);
        out.push('\n');
        out.push_str(REDACT_COMMENT);
        rows(&mut out, "redact", &self.redact);
        out.push_str("}\n");
        out
    }
}

/// A row as one line: the elements of an array separated by a comma
/// and a space, or a bare scalar.
fn row(value: &Value) -> String {
    match value {
        Value::Array(items) => format!(
            "[{}]",
            items
                .iter()
                .map(|v| v.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ),
        other => other.to_string(),
    }
}

impl From<FilterEntry> for Value {
    fn from(entry: FilterEntry) -> Self {
        json!(FilterRepr::from(entry))
    }
}

impl From<Segment> for Value {
    fn from(segment: Segment) -> Self {
        json!(SegmentRepr::from(segment))
    }
}

impl From<Allow> for Value {
    fn from(allow: Allow) -> Self {
        json!(AllowRepr::from(allow))
    }
}

impl From<TextEntry> for Value {
    fn from(entry: TextEntry) -> Self {
        json!(TextRepr::from(entry))
    }
}

const HEADER: &str = "\
// A pending publish. Edit and save; `gage publish edit` rebuilds the
// staged session on exit, and `gage publish refresh` rebuilds after an
// edit made elsewhere.
";

const INIT_COMMENT: &str = "  // Recorded by init. Not read by publish.
";

const FILTERS_COMMENT: &str =
    "  // A name, or [name, prompt]. Each name once. Names: pii, language,
  // secret, system, harness-context, harness-records, custom. custom
  // requires a prompt; a prompt on any other filter adds to its
  // instructions.
";

const SEGMENTS_COMMENT: &str =
    "  // [lines, description, author]. Lines are 1-based inclusive ranges
  // of the session, \"100-125,130\", or null for a segment that drops
  // nothing yet. Once any segment has lines, every line outside every
  // segment is dropped.
";

const ALLOW_COMMENT: &str = "  // [text, author]: text a filter found that stays. A filter row
  // deleted from redact comes back on refresh unless its text is here.
";

const REDACT_COMMENT: &str = "  // [text, reason, author, mark]. Text is matched without case
  // everywhere it appears. Reasons: pii, secret, language, system,
  // scope, custom. mark false removes the text with no marker. A row
  // without an author, [text, reason, mark], takes the author of the
  // refresh that reads it.
";

/// `reason` is a known code.
fn check_reason(reason: &str) -> Result<(), PublishError> {
    if Reason::find(reason).is_some() {
        return Ok(());
    }
    Err(PublishError::Input(format!(
        "unknown reason {reason:?}; reasons are {}",
        REASONS
            .iter()
            .map(|r| r.code.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// The reason file lists each code once with a description.
    #[test]
    fn every_reason_is_listed_once_with_a_description() {
        let mut codes = HashSet::new();
        for r in REASONS.iter() {
            assert!(!r.description.is_empty(), "{}", r.code);
            assert!(codes.insert(r.code.as_str()), "{}", r.code);
        }
        assert!(Reason::find("scope").is_some());
        assert!(Reason::find("opaque").is_none());
        assert!(Reason::find("nope").is_none());
    }

    /// The file holds the annotations, the title beside the session,
    /// the init arguments, and one filter, segment, allowed literal,
    /// or redaction per line, and reads back equal.
    #[test]
    fn definition_writes_one_row_per_line_and_round_trips() {
        let mut d = PublishDefinition::new(
            "abc",
            &[
                "pii".parse().unwrap(),
                FilterSpec::new("custom", Some("Ducks only".into())).unwrap(),
            ],
        );
        d.init = InitArgs {
            audience: Some("public".into()),
            uses: None,
            notes: Some("For the blog".into()),
        };
        d.filters.push(FilterEntry {
            name: "system".into(),
            prompt: Some("Watch for hosts.".into()),
        });
        d.push_redact("abc", "secret", "user:g", true);
        d.push_redact("Sam", "pii", "filter:pii", false);
        d.push_segment("The reply", &[(100, 125), (130, 130)], "session:s");
        d.push_segment("The question", &[], "session:s");
        d.allow.push(Allow {
            literal: "hunter2".into(),
            author: "user:g".into(),
        });
        let text = d.to_jsonc_text(Some("Fix  the\nduck"));
        assert_eq!(
            text,
            concat!(
                "// A pending publish. Edit and save; `gage publish edit` rebuilds the\n",
                "// staged session on exit, and `gage publish refresh` rebuilds after an\n",
                "// edit made elsewhere.\n",
                "{\n",
                "  \"session\": \"abc\", // title: Fix the duck\n",
                "\n",
                "  // Recorded by init. Not read by publish.\n",
                "  \"init\": {\"audience\":\"public\",\"notes\":\"For the blog\"},\n",
                "\n",
                "  // A name, or [name, prompt]. Each name once. Names: pii, language,\n",
                "  // secret, system, harness-context, harness-records, custom. custom\n",
                "  // requires a prompt; a prompt on any other filter adds to its\n",
                "  // instructions.\n",
                "  \"filters\": [\n",
                "    \"pii\",\n",
                "    [\"custom\", \"Ducks only\"],\n",
                "    [\"system\", \"Watch for hosts.\"],\n",
                "  ],\n",
                "\n",
                "  // [lines, description, author]. Lines are 1-based inclusive ranges\n",
                "  // of the session, \"100-125,130\", or null for a segment that drops\n",
                "  // nothing yet. Once any segment has lines, every line outside every\n",
                "  // segment is dropped.\n",
                "  \"segments\": [\n",
                "    [\"100-125,130\", \"The reply\", \"session:s\"],\n",
                "    [null, \"The question\", \"session:s\"],\n",
                "  ],\n",
                "\n",
                "  // [text, author]: text a filter found that stays. A filter row\n",
                "  // deleted from redact comes back on refresh unless its text is here.\n",
                "  \"allow\": [\n",
                "    [\"hunter2\", \"user:g\"],\n",
                "  ],\n",
                "\n",
                "  // [text, reason, author, mark]. Text is matched without case\n",
                "  // everywhere it appears. Reasons: pii, secret, language, system,\n",
                "  // scope, custom. mark false removes the text with no marker. A row\n",
                "  // without an author, [text, reason, mark], takes the author of the\n",
                "  // refresh that reads it.\n",
                "  \"redact\": [\n",
                "    [\"abc\", \"secret\", \"user:g\", true],\n",
                "    [\"Sam\", \"pii\", \"filter:pii\", false],\n",
                "  ],\n",
                "}\n",
            )
        );
        let back = PublishDefinition::from_jsonc(&text).unwrap();
        assert_eq!(back, d);
        assert_eq!(
            back.filters[1].spec(),
            FilterSpec::new("custom", Some("Ducks only".into())).unwrap()
        );
        assert_eq!(back.named_filters(), ["pii", "custom", "system"]);
        assert!(back.is_allowed("HUNTER2"));
        assert!(!back.is_allowed("hunter"));
        let empty = PublishDefinition::new("abc", &[]).to_jsonc_text(None);
        assert!(empty.contains("  \"session\": \"abc\",\n"), "{empty}");
        assert!(empty.contains("  \"init\": {},\n"), "{empty}");
        assert!(empty.contains("  \"filters\": [],\n"), "{empty}");
        assert!(empty.contains("  \"redact\": [],\n}\n"), "{empty}");
        assert_eq!(
            PublishDefinition::from_jsonc(&empty).unwrap(),
            PublishDefinition::new("abc", &[])
        );
    }

    /// A missing list reads as empty, a redaction without an author is
    /// filled by `fill_authors` and refused by `validate` until then,
    /// and the checks name the row that is wrong by position.
    #[test]
    fn validate_names_the_bad_row() {
        let d = PublishDefinition::from_jsonc(r#"{"session": "abc", "filters": ["pii"]}"#).unwrap();
        assert_eq!(d, PublishDefinition::new("abc", &["pii".parse().unwrap()]));
        d.validate().unwrap();
        let check = |json: &str, expect: &str| {
            let d = PublishDefinition::from_jsonc(json).unwrap();
            let err = d.validate().unwrap_err().to_string();
            assert!(err.contains(expect), "{err}");
        };
        check(
            r#"{"session": "abc", "filters": ["nope"]}"#,
            "unknown filter",
        );
        check(
            r#"{"session": "abc", "filters": ["custom"]}"#,
            "requires a prompt",
        );
        check(
            r#"{"session": "abc", "filters": ["pii", ["pii", "x"]]}"#,
            "more than once",
        );
        check(
            r#"{"session": "abc", "redact": [["x", "pii", "u", true], ["", "pii", "u", true]]}"#,
            "redaction 2: literal is empty",
        );
        check(
            r#"{"session": "abc", "redact": [["x", "nope", "u", true]]}"#,
            "redaction 1: unknown reason",
        );
        check(
            r#"{"session": "abc", "redact": [["x", "pii", true]]}"#,
            "redaction 1: no author",
        );

        let mut d = PublishDefinition::from_jsonc(
            r#"{"session": "abc", "redact": [["x", "pii", true], ["y", "pii", "user:me", false]]}"#,
        )
        .unwrap();
        assert_eq!(d.fill_authors("session:s"), 1);
        assert_eq!(d.redact[0].author, "session:s");
        assert_eq!(d.redact[1].author, "user:me");
        d.validate().unwrap();

        let bad_lines =
            PublishDefinition::from_jsonc(r#"{"session": "abc", "segments": [["9-2", "d", "u"]]}"#)
                .unwrap_err();
        assert!(bad_lines.to_string().contains("9-2"), "{bad_lines}");
        let pending =
            PublishDefinition::from_jsonc(r#"{"session": "abc", "segments": [[null, "d", "u"]]}"#)
                .unwrap();
        pending.validate().unwrap();
        assert!(pending.segments[0].lines.is_empty());
        let no_session = PublishDefinition::from_jsonc(r#"{"filters": []}"#).unwrap_err();
        assert!(no_session.to_string().contains("session"), "{no_session}");
    }

    /// The reader takes comments and trailing commas and reports a
    /// syntax error with its position.
    #[test]
    fn reader_takes_comments_and_trailing_commas() {
        let d = PublishDefinition::from_jsonc(
            "// top\n{\n  \"session\": \"abc\", // title: x\n  \"redact\": [\n    [\"a\", \"pii\", \"u\", true],\n  ],\n}\n",
        )
        .unwrap();
        assert_eq!(d.redact.len(), 1);
        let err = PublishDefinition::from_jsonc("{\n  \"session\": \"abc\",\n  \"redact\": [\n}")
            .unwrap_err();
        assert!(err.to_string().contains("line 4"), "{err}");
    }

    /// Lines round trip through the `"12-20,31"` form.
    #[test]
    fn lines_round_trip_through_the_selection_form() {
        assert_eq!(parse_lines("12-20,31").unwrap(), [(12, 20), (31, 31)]);
        assert_eq!(format_lines(&[(12, 20), (31, 31)]), "12-20,31");
        assert!(parse_lines("0-3").is_err());
        assert!(parse_lines("").is_err());
        assert!(parse_lines("99999999999").is_err());
    }

    /// The instructions are one markdown section per filter: a heading
    /// with the name, then the prose with the prompt at its placeholder.
    #[test]
    fn instructions_are_markdown_sections_with_the_prompt_in_place() {
        let filters = [
            FilterSpec::new("language", None).unwrap(),
            FilterSpec::new("custom", Some("Remove the project codename".into())).unwrap(),
        ];
        let definition = PublishDefinition::new("abc", &filters);
        let markdown = definition.instructions_markdown().unwrap();
        let language = Filter::named("language").unwrap();
        let custom = Filter::named("custom").unwrap();
        assert_eq!(
            markdown,
            format!(
                "### `language`\n\n{}\n\n### `custom`\n\n{}\n",
                language.instructions.trim_end(),
                custom.instructions_with_prompt(Some("Remove the project codename"))
            )
        );
        assert!(
            markdown.ends_with("\n\nRemove the project codename\n"),
            "{markdown}"
        );
    }
}
