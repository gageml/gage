//! The publish definition, `publish/<id>/publish.jsonc`: what the
//! staged session is derived from. The original session, the
//! arguments `init` was given, the title the published session takes,
//! the filters with their params, the segments, every text redaction
//! with its author, and the literals a filter may not record again.
//! It may hold the redacted literals, because it is instructions and
//! is never committed. The file is edited by hand, so it is JSON with
//! comments and trailing commas, written with the annotations an
//! editor needs and read back without them.

use std::collections::HashSet;
use std::fmt;
use std::sync::LazyLock;

use gage_registry::filter::Filter;
use gage_runtime::Reason;
use gage_store::url::LineSelection;
use jsonc_parser::ast;
use jsonc_parser::common::Ranged;
use jsonc_parser::{CollectOptions, ParseOptions};
use jsonschema::Validator;
use jsonschema::error::ValidationErrorKind;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::PublishError;

/// A filter as the definition records it: a registry name, with the
/// params the filter ran under. A filter without params is written as
/// a bare string, and one with params as `[name, {..}]`. The params
/// are the resolved values of the init, so a later rebuild runs the
/// filter the same way whatever the config says by then.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "FilterRepr", into = "FilterRepr")]
pub struct FilterEntry {
    pub name: String,
    pub params: Option<Map<String, Value>>,
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum FilterRepr {
    Name(String),
    WithParams(String, Map<String, Value>),
}

impl From<FilterRepr> for FilterEntry {
    fn from(repr: FilterRepr) -> Self {
        match repr {
            FilterRepr::Name(name) => FilterEntry { name, params: None },
            FilterRepr::WithParams(name, params) => FilterEntry {
                name,
                params: Some(params),
            },
        }
    }
}

impl From<FilterEntry> for FilterRepr {
    fn from(entry: FilterEntry) -> Self {
        match entry.params {
            None => FilterRepr::Name(entry.name),
            Some(params) => FilterRepr::WithParams(entry.name, params),
        }
    }
}

impl FilterEntry {
    /// The entry for a resolved filter: its name and its params, when
    /// it declares any.
    pub fn from_filter(filter: &Filter<'_>) -> Self {
        FilterEntry {
            name: filter.def.name.clone(),
            params: filter.params.as_ref().and_then(Value::as_object).cloned(),
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
    /// The filter groups that selected the filters
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<String>,
}

/// The title the published session takes, set by an entry the driver
/// adds after the session's last line. Recorded as `[text, author]`.
/// A row written by hand may leave the author out, `[text]`; the
/// author is empty until a refresh fills it in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Vec<String>", into = "Vec<String>")]
pub struct TitleEntry {
    pub text: String,
    /// `user:<name>` or `session:<id>`
    pub author: String,
}

impl TryFrom<Vec<String>> for TitleEntry {
    type Error = PublishError;

    fn try_from(row: Vec<String>) -> Result<Self, PublishError> {
        let mut row = row.into_iter();
        let (Some(text), author, None) = (row.next(), row.next(), row.next()) else {
            return Err(PublishError::Input(TITLE_SHAPE.to_string()));
        };
        Ok(TitleEntry {
            text,
            author: author.unwrap_or_default(),
        })
    }
}

impl From<TitleEntry> for Vec<String> {
    fn from(t: TitleEntry) -> Self {
        vec![t.text, t.author]
    }
}

/// The definition of one pending publish.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishDefinition {
    /// The id of the original session
    pub session: String,
    #[serde(default)]
    pub init: InitArgs,
    #[serde(default)]
    pub title: Option<TitleEntry>,
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
    pub fn new(session: &str, filters: &[FilterEntry]) -> Self {
        Self {
            session: session.to_string(),
            init: InitArgs::default(),
            title: None,
            filters: filters.to_vec(),
            segments: Vec::new(),
            allow: Vec::new(),
            redact: Vec::new(),
        }
    }

    /// The definition as the file holds it, or every problem the file
    /// has, each at its line and column. `session` is the id of the
    /// original the publish was staged from; a file naming another
    /// session is a problem. A redaction written without an author
    /// has an empty one and is not a problem.
    pub fn read(text: &str, session: &str) -> Result<Self, Vec<Problem>> {
        Self::read_checked(text, Some(session))
    }

    /// The definition as the file holds it, with no original to check
    /// the session against. The problems are one error.
    pub fn from_jsonc(text: &str) -> Result<Self, PublishError> {
        Self::read_checked(text, None).map_err(PublishError::Definition)
    }

    /// A syntax error is the only problem reported. The schema's
    /// problems are reported without reading further, since a row the
    /// schema refused has no meaning to the rules.
    fn read_checked(text: &str, session: Option<&str>) -> Result<Self, Vec<Problem>> {
        let doc = Doc::parse(text)?;
        let problems = doc.schema_problems();
        if !problems.is_empty() {
            return Err(problems);
        }
        let head: Head = serde_json::from_value(doc.value.clone())
            .map_err(|e| vec![doc.problem("", e.to_string())])?;
        let mut problems = Vec::new();
        if let Some(expected) = session
            && head.session != expected
        {
            problems.push(doc.problem(
                "/session",
                format!(
                    "session {:?} is not the original {expected} the publish was staged from",
                    head.session
                ),
            ));
        }
        let filters: Vec<FilterEntry> = doc.rows("filters", &mut problems);
        let segments: Vec<Segment> = doc.rows("segments", &mut problems);
        let allow: Vec<Allow> = doc.rows("allow", &mut problems);
        let redact: Vec<TextEntry> = doc.rows("redact", &mut problems);
        let mut seen = HashSet::new();
        for (i, f) in filters.iter().enumerate() {
            if let Some(message) = filter_problem(f, &mut seen) {
                problems.push(doc.problem(&format!("/filters/{i}"), message));
            }
        }
        for (i, s) in segments.iter().enumerate() {
            if let Some(message) = segment_problem(s) {
                problems.push(doc.problem(&format!("/segments/{i}"), message));
            }
        }
        for (i, t) in redact.iter().enumerate() {
            if let Some(message) = redaction_problem(t) {
                problems.push(doc.problem(&format!("/redact/{i}"), message));
            }
        }
        if !problems.is_empty() {
            problems.sort_by_key(|p| (p.line, p.column));
            return Err(problems);
        }
        Ok(Self {
            session: head.session,
            init: head.init,
            title: head.title,
            filters,
            segments,
            allow,
            redact,
        })
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

    /// Set the title, replacing any existing one.
    pub fn set_title(&mut self, text: &str, author: &str) {
        self.title = Some(TitleEntry {
            text: text.to_string(),
            author: author.to_string(),
        });
    }

    /// Append each redaction not already recorded, and return the
    /// number added. A row equal to one already in `redact` on text,
    /// reason, author, and mark together is an exact duplicate and is
    /// skipped; a difference in any field is a distinct record kept.
    pub fn add_redactions(&mut self, rows: &[TextEntry]) -> usize {
        let mut added = 0;
        for row in rows {
            if !self.redact.contains(row) {
                self.redact.push(row.clone());
                added += 1;
            }
        }
        added
    }

    /// Give every redaction, and the title, written without an author
    /// `author`. Returns the number of rows filled.
    pub fn fill_authors(&mut self, author: &str) -> usize {
        let mut filled = 0;
        for t in self.redact.iter_mut().filter(|t| t.author.is_empty()) {
            t.author = author.to_string();
            filled += 1;
        }
        if let Some(title) = self.title.as_mut().filter(|t| t.author.is_empty()) {
            title.author = author.to_string();
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

    /// Check a definition built in code or filled in after a read: the
    /// rules the reader applies, and every redaction and the title
    /// have an author. A row is named by its 1-based position.
    pub fn validate(&self) -> Result<(), PublishError> {
        if self.title.as_ref().is_some_and(|t| t.author.is_empty()) {
            return Err(PublishError::Input("title: no author".to_string()));
        }
        let mut seen = HashSet::new();
        for f in &self.filters {
            if let Some(message) = filter_problem(f, &mut seen) {
                return Err(PublishError::Input(message));
            }
        }
        for (i, s) in self.segments.iter().enumerate() {
            if let Some(message) = segment_problem(s) {
                return Err(PublishError::Input(format!("segment {}: {message}", i + 1)));
            }
        }
        for (i, t) in self.redact.iter().enumerate() {
            let message =
                redaction_problem(t).or_else(|| t.author.is_empty().then(|| "no author".into()));
            if let Some(message) = message {
                return Err(PublishError::Input(format!(
                    "redaction {}: {message}",
                    i + 1
                )));
            }
        }
        Ok(())
    }

    /// The definition as the file holds it: the annotations an editor
    /// needs as comments, the original's `title` beside the session,
    /// the title row or `null`, and one filter, segment, allowed
    /// literal, or redaction per line with a trailing comma, so a row
    /// is added or deleted without touching another.
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
        out.push_str(TITLE_COMMENT);
        let title_row = match &self.title {
            Some(t) => row(&json!([t.text, t.author])),
            None => "null".to_string(),
        };
        out.push_str(&format!("  \"title\": {title_row},\n\n"));
        out.push_str(FILTERS_COMMENT);
        rows(&mut out, "filters", &self.filters);
        out.push('\n');
        out.push_str(SEGMENTS_COMMENT);
        rows(&mut out, "segments", &self.segments);
        out.push('\n');
        out.push_str(ALLOW_COMMENT);
        rows(&mut out, "allow", &self.allow);
        out.push('\n');
        out.push_str(&redact_comment());
        rows(&mut out, "redact", &self.redact);
        out.push_str("}\n");
        out
    }
}

/// A problem a read found, at a 1-based line and column of the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    pub line: usize,
    pub column: usize,
    pub message: String,
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}: {}", self.line, self.column, self.message)
    }
}

/// The document fields beside the row sections.
#[derive(Deserialize)]
struct Head {
    session: String,
    #[serde(default)]
    init: InitArgs,
    #[serde(default)]
    title: Option<TitleEntry>,
}

/// The shape of the title row, for a problem at it.
const TITLE_SHAPE: &str = "title is [text, author], [text], or null";

/// The shape of the file. The names a filter or reason may take, and
/// what rows may say about each other, are checked in code.
static SCHEMA: LazyLock<Validator> = LazyLock::new(|| {
    let schema: Value = serde_json::from_str(include_str!("../schema/definition.json"))
        .expect("schema/definition.json is fixed and valid JSON");
    jsonschema::validator_for(&schema).expect("schema/definition.json is fixed and valid")
});

/// The shape of a row of each section, for a problem at the row.
const ROW_SHAPES: [(&str, &str); 4] = [
    ("filters", "a filters row is a name or [name, params]"),
    ("segments", "a segments row is [lines, description, author]"),
    ("allow", "an allow row is [text, author]"),
    (
        "redact",
        "a redact row is [text, reason, author, mark] or [text, reason, mark]",
    ),
];

fn row_shape(section: &str) -> Option<&'static str> {
    ROW_SHAPES
        .iter()
        .find(|(name, _)| *name == section)
        .map(|(_, shape)| *shape)
}

/// The shape message when `pointer` names a whole row, `/section/N`.
fn row_shape_at(pointer: &str) -> Option<&'static str> {
    let mut steps = pointer.split('/').skip(1);
    let section = steps.next()?;
    let index = steps.next()?;
    if steps.next().is_some() || index.parse::<usize>().is_err() {
        return None;
    }
    row_shape(section)
}

/// A definition file parsed to its tree, which places each problem,
/// and to the JSON value the schema checks and the rows are read from.
struct Doc<'a> {
    text: &'a str,
    root: ast::Value<'a>,
    value: Value,
}

impl<'a> Doc<'a> {
    fn parse(text: &'a str) -> Result<Self, Vec<Problem>> {
        let parsed =
            jsonc_parser::parse_to_ast(text, &CollectOptions::default(), &ParseOptions::default())
                .map_err(|e| {
                    vec![Problem {
                        line: e.line_display(),
                        column: e.column_display(),
                        message: e.kind().to_string(),
                    }]
                })?;
        let root = parsed.value.ok_or_else(|| {
            vec![Problem {
                line: 1,
                column: 1,
                message: "file has no value".into(),
            }]
        })?;
        let value = Value::from(root.clone());
        Ok(Doc { text, root, value })
    }

    /// Every way the value departs from the schema, in file order. A
    /// problem with a whole row names the row's shape. An unknown key
    /// is a problem at the key.
    fn schema_problems(&self) -> Vec<Problem> {
        let mut problems = Vec::new();
        for e in SCHEMA.iter_errors(&self.value) {
            let pointer = e.instance_path().to_string();
            match e.kind() {
                ValidationErrorKind::AdditionalProperties { unexpected } => {
                    for key in unexpected {
                        problems.push(
                            self.problem(
                                &format!("{pointer}/{key}"),
                                format!("unknown key {key:?}"),
                            ),
                        );
                    }
                }
                _ => {
                    let message = match row_shape_at(&pointer) {
                        Some(shape) => shape.to_string(),
                        None if pointer == "/title" || pointer.starts_with("/title/") => {
                            TITLE_SHAPE.to_string()
                        }
                        None => e.to_string(),
                    };
                    problems.push(self.problem(&pointer, message));
                }
            }
        }
        problems.sort_by_key(|p| (p.line, p.column));
        problems
    }

    /// The rows of the section `key`, each read as `T`. A row that
    /// does not read is a problem at the row: after the schema, a
    /// segment row fails only on its lines text, which the error
    /// quotes, and a redaction row only on an author slot that does
    /// not fit its length, which the row's shape explains.
    fn rows<T: DeserializeOwned>(&self, key: &str, problems: &mut Vec<Problem>) -> Vec<T> {
        let mut rows = Vec::new();
        let Some(Value::Array(items)) = self.value.get(key) else {
            return rows;
        };
        for (i, item) in items.iter().enumerate() {
            match serde_json::from_value(item.clone()) {
                Ok(row) => rows.push(row),
                Err(e) => {
                    let message = match (key, row_shape(key)) {
                        ("segments", _) | (_, None) => e.to_string(),
                        (_, Some(shape)) => shape.to_string(),
                    };
                    problems.push(self.problem(&format!("/{key}/{i}"), message));
                }
            }
        }
        rows
    }

    /// A problem at the node `pointer` names: the key of an object
    /// property, or the element of an array. The root, and a pointer
    /// that resolves no further, place the problem at the last node
    /// reached.
    fn problem(&self, pointer: &str, message: String) -> Problem {
        let mut node = &self.root;
        let mut start = node.range().start;
        for step in pointer.split('/').skip(1) {
            let next = match node {
                ast::Value::Object(o) => o.get(step).map(|prop| (prop.range.start, &prop.value)),
                ast::Value::Array(a) => match step.parse::<usize>() {
                    Ok(i) => a.elements.get(i).map(|v| (v.range().start, v)),
                    Err(_) => None,
                },
                _ => None,
            };
            match next {
                Some((at, value)) => {
                    start = at;
                    node = value;
                }
                None => break,
            }
        }
        let before = self.text.get(..start).unwrap_or_default();
        let line = before.matches('\n').count() + 1;
        let column = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
        Problem {
            line,
            column,
            message,
        }
    }
}

/// Why `f` is not a filter the list may hold: a name already in
/// `seen`. Whether the name is a filter the registry knows is found
/// when the filters run.
fn filter_problem(f: &FilterEntry, seen: &mut HashSet<String>) -> Option<String> {
    if !seen.insert(f.name.clone()) {
        return Some(format!("filter {} is listed more than once", f.name));
    }
    None
}

fn segment_problem(s: &Segment) -> Option<String> {
    s.lines
        .iter()
        .any(|(start, end)| *start == 0 || end < start)
        .then(|| "lines are 1-based and a range's end is not before its start".into())
}

/// Why `t` is not a redaction: an empty literal or an unknown reason.
/// A missing author is not a problem here; a refresh fills it.
fn redaction_problem(t: &TextEntry) -> Option<String> {
    if t.literal.is_empty() {
        return Some("literal is empty".into());
    }
    check_reason(&t.reason).err().map(|e| e.to_string())
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

const TITLE_COMMENT: &str = "  // [text, author]: the title the published session takes, set by an
  // entry added after its last line. null keeps the title the content
  // yields. A row without an author, [text], takes the author of the
  // refresh that reads it.
";

const FILTERS_COMMENT: &str =
    "  // A filter name, or [name, params]. Each name once. Params are the
  // filter's values as init resolved them; gage publish init
  // --list-filters names the filters.
";

/// The `redact` annotation, with the reason codes of the runtime.
fn redact_comment() -> String {
    comment(&format!(
        "[text, reason, author, mark]. Text is matched without case \
         everywhere it appears. Reasons: {}. mark false removes \
         the text with no marker. A row without an author, [text, \
         reason, mark], takes the author of the refresh that reads it.",
        Reason::codes()
    ))
}

/// `text` wrapped into `  // ` comment lines no wider than 70 columns.
fn comment(text: &str) -> String {
    const WIDTH: usize = 70;
    let mut out = String::new();
    let mut line = String::from("  //");
    for word in text.split_whitespace() {
        if line.len() + 1 + word.len() > WIDTH {
            out.push_str(&line);
            out.push('\n');
            line = String::from("  //");
        }
        line.push(' ');
        line.push_str(word);
    }
    out.push_str(&line);
    out.push('\n');
    out
}

const SEGMENTS_COMMENT: &str =
    "  // [lines, description, author]. Lines are 1-based inclusive ranges
  // of the session, \"100-125,130\", or null for a segment that drops
  // nothing yet. Once any segment has lines, every line outside every
  // segment is dropped.
";

const ALLOW_COMMENT: &str = "  // [text, author]: text a filter found that stays. A filter row
  // deleted from redact comes back on refresh unless its text is here.
";

/// `reason` is a known code.
fn check_reason(reason: &str) -> Result<(), PublishError> {
    reason
        .parse::<Reason>()
        .map(|_| ())
        .map_err(PublishError::Input)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filter(name: &str) -> FilterEntry {
        FilterEntry {
            name: name.into(),
            params: None,
        }
    }

    fn filter_with(name: &str, params: Value) -> FilterEntry {
        FilterEntry {
            name: name.into(),
            params: params.as_object().cloned(),
        }
    }

    /// The redact annotation names every reason code, so it does not
    /// drift from the runtime's list.
    #[test]
    fn the_redact_annotation_names_every_code() {
        let redact = redact_comment();
        for r in Reason::ALL {
            assert!(redact.contains(r.code()), "redact omits {}", r.code());
        }
    }

    /// The file holds the annotations, the original's title beside the
    /// session, the init arguments, the title row, and one filter,
    /// segment, allowed literal, or redaction per line, and reads back
    /// equal.
    #[test]
    fn definition_writes_one_row_per_line_and_round_trips() {
        let mut d = PublishDefinition::new(
            "abc",
            &[
                filter("pii"),
                filter_with("language", json!({ "strict": true })),
            ],
        );
        d.init = InitArgs {
            groups: vec!["public".into()],
        };
        d.filters.push(filter_with(
            "system",
            json!({ "prompt": "Watch for hosts." }),
        ));
        d.title = Some(TitleEntry {
            text: "Fixing the duck".into(),
            author: "user:g".into(),
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
        let mut expected = String::from(concat!(
            "// A pending publish. Edit and save; `gage publish edit` rebuilds the\n",
            "// staged session on exit, and `gage publish refresh` rebuilds after an\n",
            "// edit made elsewhere.\n",
            "{\n",
            "  \"session\": \"abc\", // title: Fix the duck\n",
            "\n",
            "  // Recorded by init. Not read by publish.\n",
            "  \"init\": {\"groups\":[\"public\"]},\n",
            "\n",
            "  // [text, author]: the title the published session takes, set by an\n",
            "  // entry added after its last line. null keeps the title the content\n",
            "  // yields. A row without an author, [text], takes the author of the\n",
            "  // refresh that reads it.\n",
            "  \"title\": [\"Fixing the duck\", \"user:g\"],\n",
            "\n",
        ));
        expected.push_str(FILTERS_COMMENT);
        expected.push_str(concat!(
            "  \"filters\": [\n",
            "    \"pii\",\n",
            "    [\"language\", {\"strict\":true}],\n",
            "    [\"system\", {\"prompt\":\"Watch for hosts.\"}],\n",
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
        ));
        expected.push_str(&redact_comment());
        expected.push_str(concat!(
            "  \"redact\": [\n",
            "    [\"abc\", \"secret\", \"user:g\", true],\n",
            "    [\"Sam\", \"pii\", \"filter:pii\", false],\n",
            "  ],\n",
            "}\n",
        ));
        assert_eq!(text, expected);
        let back = PublishDefinition::from_jsonc(&text).unwrap();
        assert_eq!(back, d);
        assert_eq!(back.filters[1].params.as_ref().unwrap()["strict"], true);
        assert_eq!(back.filters[1].author(), "filter:language");
        assert_eq!(back.named_filters(), ["pii", "language", "system"]);
        assert!(back.is_allowed("HUNTER2"));
        assert!(!back.is_allowed("hunter"));
        let empty = PublishDefinition::new("abc", &[]).to_jsonc_text(None);
        assert!(empty.contains("  \"session\": \"abc\",\n"), "{empty}");
        assert!(empty.contains("  \"init\": {},\n"), "{empty}");
        assert!(empty.contains("  \"title\": null,\n"), "{empty}");
        assert!(empty.contains("  \"filters\": [],\n"), "{empty}");
        assert!(empty.contains("  \"redact\": [],\n}\n"), "{empty}");
        assert_eq!(
            PublishDefinition::from_jsonc(&empty).unwrap(),
            PublishDefinition::new("abc", &[])
        );
    }

    #[test]
    fn add_redactions_skips_exact_duplicates_only() {
        let row = |literal: &str, reason: &str, author: &str, mark: bool| TextEntry {
            literal: literal.into(),
            reason: reason.into(),
            author: author.into(),
            mark,
        };
        let mut d = PublishDefinition::new("abc", &[]);
        let added = d.add_redactions(&[
            row("secret", "secret", "session:a", true),
            row("secret", "secret", "session:a", true),
            row("secret", "secret", "session:b", true),
            row("secret", "pii", "session:a", true),
            row("secret", "secret", "session:a", false),
        ]);
        assert_eq!(added, 4);
        assert_eq!(d.redact.len(), 4);
        assert_eq!(
            d.add_redactions(&[row("secret", "secret", "session:a", true)]),
            0
        );
        assert_eq!(d.redact.len(), 4);
    }

    #[test]
    fn set_title_replaces_any_existing_title() {
        let mut d = PublishDefinition::new("abc", &[]);
        d.set_title("First", "user:a");
        assert_eq!(d.title.as_ref().unwrap().text, "First");
        d.set_title("Second", "user:b");
        let t = d.title.as_ref().unwrap();
        assert_eq!(t.text, "Second");
        assert_eq!(t.author, "user:b");
    }

    /// The `init` record holds groups alone; a file that records any
    /// other field there is refused, naming the field.
    #[test]
    fn init_refuses_a_field_other_than_groups() {
        let err = PublishDefinition::from_jsonc(
            r#"{"session": "abc", "init": {"groups": ["public"], "notes": "For the blog"}}"#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("notes"), "{err}");
    }

    /// A missing list reads as empty, and a redaction or title without
    /// an author is filled by `fill_authors` and refused by `validate`
    /// until then.
    #[test]
    fn validate_requires_an_author() {
        let d = PublishDefinition::from_jsonc(r#"{"session": "abc", "filters": ["pii"]}"#).unwrap();
        assert_eq!(d, PublishDefinition::new("abc", &[filter("pii")]));
        d.validate().unwrap();

        let mut d = PublishDefinition::from_jsonc(
            r#"{"session": "abc", "redact": [["x", "pii", true], ["y", "pii", "user:me", false]]}"#,
        )
        .unwrap();
        let err = d.validate().unwrap_err().to_string();
        assert!(err.contains("redaction 1: no author"), "{err}");
        assert_eq!(d.fill_authors("session:s"), 1);
        assert_eq!(d.redact[0].author, "session:s");
        assert_eq!(d.redact[1].author, "user:me");
        d.validate().unwrap();

        let mut d =
            PublishDefinition::from_jsonc(r#"{"session": "abc", "title": ["Ducks"]}"#).unwrap();
        let err = d.validate().unwrap_err().to_string();
        assert!(err.contains("title: no author"), "{err}");
        assert_eq!(d.fill_authors("session:s"), 1);
        assert_eq!(
            d.title,
            Some(TitleEntry {
                text: "Ducks".into(),
                author: "session:s".into(),
            })
        );
        d.validate().unwrap();
        assert_eq!(
            PublishDefinition::from_jsonc(r#"{"session": "abc", "title": null}"#)
                .unwrap()
                .title,
            None
        );

        let mut d = PublishDefinition::new("abc", &[]);
        d.push_segment("d", &[(9, 2)], "u");
        let err = d.validate().unwrap_err().to_string();
        assert!(err.contains("segment 1: lines are 1-based"), "{err}");
    }

    /// The problems of a file are reported together, each at the line
    /// and column of the node it concerns, in file order. The schema's
    /// problems come first and alone: an unknown key at the key, a row
    /// of the wrong shape at the row with the shape named, a title of
    /// the wrong shape, and a wrong element at the element. A file the schema accepts then reports
    /// what the rules find: a session other than the original, a
    /// filter listed twice, bad segment lines, an unknown reason, and
    /// a redaction whose author slot does not fit its length.
    #[test]
    fn reader_reports_each_problem_at_its_position() {
        let problems = |text: &str, session: &str| -> Vec<(usize, usize, String)> {
            PublishDefinition::read(text, session)
                .unwrap_err()
                .into_iter()
                .map(|p| (p.line, p.column, p.message))
                .collect()
        };
        let shape = concat!(
            "// top\n",
            "{\n",
            "  \"session\": \"abc\",\n",
            "  \"filter\": [\"pii\"],\n",
            "  \"filters\": [\"pii\", [\"custom\"]],\n",
            "  \"segments\": [\n",
            "    [\"1-3\", \"d\"],\n",
            "  ],\n",
            "  \"title\": \"Ducks\",\n",
            "  \"redact\": [\n",
            "    [\"\", \"pii\", \"u\", true],\n",
            "    [\"x\", \"pii\", \"u\", \"yes\"],\n",
            "  ],\n",
            "}\n",
        );
        let found = problems(shape, "abc");
        let expect = [
            (4, 3, "unknown key \"filter\""),
            (5, 22, "a filters row is a name or [name, params]"),
            (7, 5, "a segments row is [lines, description, author]"),
            (9, 3, "title is [text, author], [text], or null"),
            (11, 6, "shorter than 1 character"),
            (12, 23, "not of type \"boolean\""),
        ];
        assert_eq!(found.len(), expect.len(), "{found:?}");
        for ((line, column, message), (l, c, m)) in found.iter().zip(expect) {
            assert_eq!((*line, *column), (l, c), "{message}");
            assert!(message.contains(m), "{message}");
        }

        let rules = concat!(
            "{\n",
            "  \"session\": \"abc\",\n",
            "  \"filters\": [\"nope\", [\"custom\", {}], \"pii\", \"pii\"],\n",
            "  \"segments\": [[\"9-2\", \"d\", \"u\"]],\n",
            "  \"redact\": [\n",
            "    [\"x\", \"nope\", \"u\", true],\n",
            "    [\"x\", \"pii\", true, true],\n",
            "  ],\n",
            "}\n",
        );
        let found = problems(rules, "xyz");
        let expect = [
            (2, 3, "session \"abc\" is not the original xyz"),
            (3, 46, "filter pii is listed more than once"),
            (4, 16, "9-2"),
            (6, 5, "unknown reason \"nope\""),
            (
                7,
                5,
                "a redact row is [text, reason, author, mark] or [text, reason, mark]",
            ),
        ];
        assert_eq!(found.len(), expect.len(), "{found:?}");
        for ((line, column, message), (l, c, m)) in found.iter().zip(expect) {
            assert_eq!((*line, *column), (l, c), "{message}");
            assert!(message.contains(m), "{message}");
        }

        assert_eq!(
            problems(r#"{"filters": []}"#, "abc"),
            [(1, 1, "\"session\" is a required property".to_string())]
        );
        assert_eq!(
            problems("", "abc"),
            [(1, 1, "file has no value".to_string())]
        );
        let d = PublishDefinition::read(rules.replace("xyz", "abc").as_str(), "abc");
        assert!(d.is_err());
        let d = PublishDefinition::read(
            r#"{"session": "abc", "segments": [[null, "d", "u"]], "redact": [["x", "pii", true]]}"#,
            "abc",
        )
        .unwrap();
        assert!(d.segments[0].lines.is_empty());
        assert_eq!(d.redact[0].author, "");
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
        assert!(err.to_string().starts_with("4:1: "), "{err}");
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
}
