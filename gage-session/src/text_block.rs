//! The text values of a stored session's entries, in document order
//! within each entry. A driver's codec decodes each entry into an
//! [`EntryValue`] and supplies a [`TextDescriptor`] that names the
//! fields whose type is not text; the walk here emits the rest. The
//! publish engine and the candidate listing read it.

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::path::Path;

use regex::{Regex, RegexBuilder};
use serde_json::Value;
use serde_json_path::JsonPath;

use crate::codec::EntryValue;

/// What a driver knows about the strings of one content format: the
/// fields whose type is not text, per typed set the fields whose type
/// says their value belongs to it, and the literals a set's paths
/// select that are too generic to redact. The paths are lists of
/// JSONPath (RFC 9535) patterns; a pattern selects a node and covers
/// every string under it. The allow list is regexes over the literal.
#[derive(Debug, Clone)]
pub struct TextDescriptor {
    safe: PathSet,
    typed: Vec<TypedPaths>,
    allow: Vec<Regex>,
}

/// A named set of typed-sensitive paths a driver knows: the fields
/// whose type, not whose content, says what they hold. A publish
/// filter removes or redacts a set by name; the driver owns which
/// paths of its content format belong to each.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum TypedSet {
    /// What the harness wrote into the model's context: the request
    /// shape and blobs, attachments, and injected user entries
    HarnessContext,
    /// The harness's own bookkeeping: run records, descriptive fields,
    /// and the tags it rides inside a user turn
    HarnessRecord,
    /// Fields that identify the person: a username, an email field
    Pii,
    /// Fields that describe the machine: paths, hosts, OS versions
    System,
}

impl TypedSet {
    pub const ALL: [TypedSet; 4] = [
        TypedSet::HarnessContext,
        TypedSet::HarnessRecord,
        TypedSet::Pii,
        TypedSet::System,
    ];

    /// The set's name in diagnostics and listings.
    pub fn name(self) -> &'static str {
        match self {
            TypedSet::HarnessContext => "harness-context",
            TypedSet::HarnessRecord => "harness-record",
            TypedSet::Pii => "pii",
            TypedSet::System => "system",
        }
    }
}

impl fmt::Display for TypedSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A set of path patterns matched against a string's JSON Pointer by
/// prefix. A plain pattern, member names and `[*]` only, is held as a
/// normalized pointer prefix in a prefix-free ordered set, so one
/// predecessor lookup decides it in O(log n) comparisons. A pattern
/// JSONPath alone can express, a filter or a descendant, is evaluated
/// against the parsed entry.
#[derive(Debug, Clone, Default)]
struct PathSet {
    prefixes: BTreeSet<String>,
    irregular: Vec<JsonPath>,
}

impl PathSet {
    fn compile(patterns: &[&str]) -> std::result::Result<Self, DescriptorError> {
        let mut set = Self::default();
        for pattern in patterns {
            match plain_prefix(pattern) {
                Some(prefix) => set.insert(prefix),
                None => {
                    set.irregular
                        .push(JsonPath::parse(pattern).map_err(|e| DescriptorError {
                            pattern: pattern.to_string(),
                            message: e.to_string(),
                        })?)
                }
            }
        }
        Ok(set)
    }

    /// Insert `prefix`, keeping the set prefix-free: a prefix already
    /// covered by an ancestor is redundant, and descendants of a new
    /// prefix are removed.
    fn insert(&mut self, prefix: String) {
        if self.covers_normalized(&prefix) {
            return;
        }
        let below: Vec<String> = self
            .prefixes
            .range(prefix.clone()..)
            .take_while(|p| covered_by(p, &prefix))
            .cloned()
            .collect();
        for p in below {
            self.prefixes.remove(&p);
        }
        self.prefixes.insert(prefix);
    }

    /// Whether a normalized pointer is at or under one of the plain
    /// prefixes. The set is prefix-free, so the greatest prefix at or
    /// below `key` is the only candidate.
    fn covers_normalized(&self, key: &str) -> bool {
        self.prefixes
            .range(..=key.to_string())
            .next_back()
            .is_some_and(|p| covered_by(key, p))
    }

    /// The pointers the irregular patterns select in `value`; empty
    /// when there are none.
    fn irregular_pointers(&self, value: Option<&Value>) -> Vec<String> {
        let Some(value) = value else {
            return Vec::new();
        };
        self.irregular
            .iter()
            .flat_map(|p| {
                p.query_located(value)
                    .locations()
                    .map(|l| l.to_json_pointer())
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    /// Whether a string at `pointer` (and its normalized form `key`)
    /// is covered, by a plain prefix or by a node an irregular pattern
    /// selected.
    fn covers(&self, pointer: &str, key: &str, irregular: &[String]) -> bool {
        self.covers_normalized(key) || irregular.iter().any(|root| covered_by(pointer, root))
    }

    fn has_irregular(&self) -> bool {
        !self.irregular.is_empty()
    }
}

/// Compile JSONPath patterns, naming the one that fails.
fn compile(patterns: &[&str]) -> std::result::Result<Vec<JsonPath>, DescriptorError> {
    patterns.iter().map(|p| compile_one(p)).collect()
}

fn compile_one(pattern: &str) -> std::result::Result<JsonPath, DescriptorError> {
    JsonPath::parse(pattern).map_err(|e| DescriptorError {
        pattern: pattern.to_string(),
        message: e.to_string(),
    })
}

/// Whether `key` equals `prefix` or continues it at a segment boundary.
fn covered_by(key: &str, prefix: &str) -> bool {
    key.strip_prefix(prefix)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// The normalized pointer prefix of a plain pattern: `$`, then `.name`
/// and `[*]` steps, with a trailing `.*` or `[*]` selecting the parent
/// subtree. `None` for any other JSONPath, which is evaluated as such.
fn plain_prefix(pattern: &str) -> Option<String> {
    let mut rest = pattern.strip_prefix('$')?;
    let mut prefix = String::new();
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix("[*]") {
            prefix.push_str("/*");
            rest = after;
        } else if let Some(after) = rest.strip_prefix('.') {
            if after.starts_with('.') {
                return None;
            }
            let end = after.find(['.', '[']).unwrap_or(after.len());
            let name = &after[..end];
            if name.is_empty() || name.contains('?') {
                return None;
            }
            if name == "*" {
                return if after[end..].is_empty() {
                    Some(prefix)
                } else {
                    None
                };
            }
            prefix.push('/');
            prefix.push_str(&name.replace('~', "~0").replace('/', "~1"));
            rest = &after[end..];
        } else {
            return None;
        }
    }
    Some(prefix)
}

/// A pointer with every array index replaced by `*`, the form the
/// plain prefixes take.
fn normalize(pointer: &str) -> String {
    pointer
        .split('/')
        .map(|seg| {
            if !seg.is_empty() && seg.bytes().all(|b| b.is_ascii_digit()) {
                "*"
            } else {
                seg
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// The typed-sensitive paths of one set.
#[derive(Debug, Clone)]
struct TypedPaths {
    set: TypedSet,
    /// Fields whose value is itself the sensitive literal
    literal: Vec<JsonPath>,
    /// Fields whose value is a path inside the sensitive directory;
    /// the literal is the path's parent
    literal_parent: Vec<JsonPath>,
    /// Fields whose whole value is left out, each with its pattern text
    value: Vec<(String, JsonPath)>,
    /// Selectors over the entry; one that selects anything marks the
    /// whole entry as the set's to leave out. A selector tests the
    /// entry's own fields through the root, as in
    /// `$[?$.type == 'progress']`.
    entries: Vec<JsonPath>,
    /// Regexes whose every match in a text value is a span the set
    /// removes, for out-of-band content that rides inside a value.
    removes: Vec<Regex>,
}

/// What a set's typed-sensitive paths select in one entry.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TypedValues {
    /// The string values at the set's `literal` paths and the parent
    /// directories of the values at its `literal_parent` paths
    pub literals: Vec<String>,
    /// The string values at the set's `value` paths
    pub values: Vec<ValueHit>,
    /// The spans the set's `removes` regexes match in text values
    pub spans: Vec<SpanHit>,
    /// Whether one of the set's entry selectors selected the entry
    pub dropped: bool,
}

/// One value a set's `value` path selects, of any JSON type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValueHit {
    /// The pattern that selected it
    pub path: String,
    /// JSON Pointer (RFC 6901) of the value from the entry root
    pub pointer: String,
}

/// One span a set's `removes` regex matches in a text value, as
/// zero-based byte offsets into that value's text with `end` inclusive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpanHit {
    /// JSON Pointer (RFC 6901) of the value from the entry root
    pub pointer: String,
    pub start: usize,
    pub end: usize,
}

/// A text value of one entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextValue {
    /// JSON Pointer (RFC 6901) of the value from the entry root
    pub pointer: String,
    pub text: String,
}

/// A descriptor pattern that is not JSONPath, or an allow pattern
/// that is not a regex.
#[derive(Debug)]
pub struct DescriptorError {
    pub pattern: String,
    pub message: String,
}

impl fmt::Display for DescriptorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "descriptor pattern {:?}: {}", self.pattern, self.message)
    }
}

impl Error for DescriptorError {}

impl TextDescriptor {
    /// Compile the `safe` pattern list.
    pub fn new(safe: &[&str]) -> std::result::Result<Self, DescriptorError> {
        Ok(Self {
            safe: PathSet::compile(safe)?,
            typed: Vec::new(),
            allow: Vec::new(),
        })
    }

    /// Add the regexes a literal a set's `literal` path selects is
    /// checked against: a literal one of them matches is not reported.
    /// Matching is case-insensitive, as redaction is.
    pub fn with_allow(mut self, allow: &[&str]) -> std::result::Result<Self, DescriptorError> {
        for pattern in allow {
            let regex = RegexBuilder::new(pattern)
                .case_insensitive(true)
                .build()
                .map_err(|e| DescriptorError {
                    pattern: pattern.to_string(),
                    message: e.to_string(),
                })?;
            self.allow.push(regex);
        }
        Ok(self)
    }

    /// Add the typed-sensitive paths of `set`. `literal_parent` paths
    /// hold a path whose parent directory is the literal. `removes`
    /// are regexes whose matches in a text value are spans the set
    /// removes.
    pub fn with_typed(
        mut self,
        set: TypedSet,
        literal: &[&str],
        literal_parent: &[&str],
        value: &[&str],
        entries: &[&str],
        removes: &[&str],
    ) -> std::result::Result<Self, DescriptorError> {
        self.typed.push(TypedPaths {
            set,
            literal: compile(literal)?,
            literal_parent: compile(literal_parent)?,
            value: value
                .iter()
                .map(|p| compile_one(p).map(|c| (p.to_string(), c)))
                .collect::<std::result::Result<_, _>>()?,
            entries: compile(entries)?,
            removes: removes
                .iter()
                .map(|p| {
                    Regex::new(p).map_err(|e| DescriptorError {
                        pattern: (*p).to_string(),
                        message: e.to_string(),
                    })
                })
                .collect::<std::result::Result<_, _>>()?,
        });
        Ok(self)
    }

    /// The sets with typed-sensitive paths.
    pub fn typed_sets(&self) -> impl Iterator<Item = TypedSet> + '_ {
        self.typed.iter().map(|t| t.set)
    }

    /// What `set`'s typed-sensitive paths select in one entry. Empty
    /// for a set without paths. A literal on the allow list is left
    /// out.
    pub fn typed_values(&self, set: TypedSet, entry: &EntryValue) -> TypedValues {
        let Some(paths) = self.typed.iter().find(|t| t.set == set) else {
            return TypedValues::default();
        };
        // JSONPath evaluates on the unordered form; a pointer is the
        // same in both.
        let decoded = entry;
        let entry: Value = decoded.into();
        let strings_at = |paths: &[JsonPath]| {
            paths
                .iter()
                .flat_map(|p| {
                    p.query(&entry)
                        .iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>()
        };
        let parents = strings_at(&paths.literal_parent)
            .into_iter()
            .filter_map(|path| parent_dir(&path));
        let literals = strings_at(&paths.literal)
            .into_iter()
            .chain(parents)
            .filter(|literal| !self.allow.iter().any(|a| a.is_match(literal)))
            .collect();
        let values = paths
            .value
            .iter()
            .flat_map(|(path, p)| {
                p.query_located(&entry)
                    .iter()
                    .map(|n| ValueHit {
                        path: path.clone(),
                        pointer: n.location().to_json_pointer(),
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
        let dropped = paths.entries.iter().any(|p| !p.query(&entry).is_empty());
        let spans = if paths.removes.is_empty() {
            Vec::new()
        } else {
            self.text_values(decoded)
                .iter()
                .flat_map(|value| {
                    paths.removes.iter().flat_map(move |re| {
                        re.find_iter(&value.text)
                            .filter(|m| m.end() > m.start())
                            .map(|m| SpanHit {
                                pointer: value.pointer.clone(),
                                start: m.start(),
                                end: m.end() - 1,
                            })
                            .collect::<Vec<_>>()
                    })
                })
                .collect()
        };
        TypedValues {
            literals,
            values,
            spans,
            dropped,
        }
    }

    /// The text values of one entry, in document order. A string at or
    /// under a safe path is excluded.
    pub fn text_values(&self, entry: &EntryValue) -> Vec<TextValue> {
        let value: Option<Value> = self.safe.has_irregular().then(|| entry.into());
        let safe_roots = self.safe.irregular_pointers(value.as_ref());
        entry
            .strings()
            .into_iter()
            .filter_map(|(pointer, text)| {
                let key = normalize(&pointer);
                if self.safe.covers(&pointer, &key, &safe_roots) {
                    return None;
                }
                Some(TextValue {
                    pointer,
                    text: text.to_string(),
                })
            })
            .collect()
    }
}

/// The parent directory of `path`; `None` for a root or a bare name.
fn parent_dir(path: &str) -> Option<String> {
    Path::new(path)
        .parent()
        .and_then(Path::to_str)
        .filter(|parent| !parent.is_empty())
        .map(String::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(line: &str) -> EntryValue {
        serde_json::from_str(line).unwrap()
    }

    fn descriptor() -> TextDescriptor {
        TextDescriptor::new(&[
            "$.uuid",
            "$.type",
            "$.message.content[*].type",
            "$.message.content[*].id",
        ])
        .unwrap()
    }

    /// Strings come out in the order the line was written, not key
    /// order, with safe fields excluded.
    #[test]
    fn text_values_follow_document_order_and_the_descriptor() {
        let line = r#"{"zeta":"last key first","uuid":"u1","type":"assistant","message":{"content":[{"type":"text","text":"hi"},{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}]},"toolUseResult":{"stdout":"out"},"alpha":"first key last"}"#;
        let values = descriptor().text_values(&entry(line));
        let summary: Vec<(&str, &str)> = values
            .iter()
            .map(|v| (v.pointer.as_str(), v.text.as_str()))
            .collect();
        assert_eq!(
            summary,
            [
                ("/zeta", "last key first"),
                ("/message/content/0/text", "hi"),
                ("/message/content/1/name", "Bash"),
                ("/message/content/1/input/command", "ls"),
                ("/toolUseResult/stdout", "out"),
                ("/alpha", "first key last"),
            ]
        );
    }

    /// A member name with `/` or `~` is escaped in the pointer, and a
    /// string that resembles an identifier is still text when its
    /// field is not on the safe list.
    #[test]
    fn pointers_escape_member_names() {
        let line = r#"{"a/b":"x","c~d":"y","note":"11111111-2222-3333-4444-555555555555"}"#;
        let values = descriptor().text_values(&entry(line));
        let pointers: Vec<&str> = values.iter().map(|v| v.pointer.as_str()).collect();
        assert_eq!(pointers, ["/a~1b", "/c~0d", "/note"]);
    }

    /// A `literal_parent` path yields the parent directory of its
    /// value as the literal, subject to the allow list like any other.
    #[test]
    fn literal_parent_paths_yield_the_parent_directory() {
        let descriptor = TextDescriptor::new(&[])
            .unwrap()
            .with_allow(&["^/(tmp|home)/?$"])
            .unwrap()
            .with_typed(TypedSet::System, &[], &["$.scratch"], &[], &[], &[])
            .unwrap();
        let line = r#"{"scratch":"/tmp/claude/proj/s1/scratchpad"}"#;
        let found = descriptor.typed_values(TypedSet::System, &entry(line));
        assert_eq!(found.literals, ["/tmp/claude/proj/s1"]);
        for shallow in ["/tmp/s", "/", "scratchpad"] {
            let line = format!(r#"{{"scratch":"{shallow}"}}"#);
            let found = descriptor.typed_values(TypedSet::System, &entry(&line));
            assert!(found.literals.is_empty(), "{shallow}: {found:?}");
        }
    }

    /// A literal on the allow list is not reported, whatever set
    /// selects it; the allow list does not touch whole values.
    #[test]
    fn allowed_literals_are_not_reported() {
        let descriptor = descriptor()
            .with_allow(&["^.{0,2}$", "^/(tmp|home)/?$"])
            .unwrap()
            .with_typed(
                TypedSet::System,
                &["$.cwd", "$..home_dir"],
                &[],
                &["$.cwd"],
                &[],
                &[],
            )
            .unwrap();
        let line = r#"{"cwd":"/","params":{"home_dir":"/HOME/"}}"#;
        let found = descriptor.typed_values(TypedSet::System, &entry(line));
        assert!(found.literals.is_empty(), "{found:?}");
        assert_eq!(found.values.len(), 1);
        let line = r#"{"cwd":"/home/garrett","params":{"home_dir":"/tmp/x"}}"#;
        let found = descriptor.typed_values(TypedSet::System, &entry(line));
        assert_eq!(found.literals, ["/home/garrett", "/tmp/x"]);
        let err = TextDescriptor::new(&[])
            .unwrap()
            .with_allow(&["("])
            .unwrap_err();
        assert_eq!(err.pattern, "(");
    }

    #[test]
    fn typed_values_select_literals_and_whole_values() {
        let descriptor = descriptor()
            .with_typed(
                TypedSet::Pii,
                &["$..user_identity"],
                &[],
                &["$.context.email"],
                &[],
                &[],
            )
            .unwrap()
            .with_typed(
                TypedSet::HarnessRecord,
                &[],
                &[],
                &["$.n", "$.params.n"],
                &[],
                &[],
            )
            .unwrap();
        let line = r#"{"context":{"email":"The user is a@b.c"},"params":{"classifier":{"user_identity":"garrett"},"n":3},"n":{"user_identity":5}}"#;
        let found = descriptor.typed_values(TypedSet::Pii, &entry(line));
        assert_eq!(found.literals, ["garrett"]);
        assert_eq!(
            found.values,
            [ValueHit {
                path: "$.context.email".into(),
                pointer: "/context/email".into(),
            }]
        );
        // A value path selects a value of any type: an object and a number
        let record = descriptor.typed_values(TypedSet::HarnessRecord, &entry(line));
        let pointers: Vec<&str> = record.values.iter().map(|v| v.pointer.as_str()).collect();
        assert_eq!(pointers, ["/n", "/params/n"]);
        assert_eq!(
            descriptor.typed_values(TypedSet::System, &entry(line)),
            TypedValues::default()
        );
        assert_eq!(
            descriptor.typed_sets().collect::<Vec<_>>(),
            [TypedSet::Pii, TypedSet::HarnessRecord]
        );
    }

    /// A `removes` regex reports each match in a text value as a span
    /// hit, with the value's pointer and inclusive byte offsets.
    #[test]
    fn removes_regexes_report_span_hits_in_text_values() {
        let descriptor = descriptor()
            .with_typed(
                TypedSet::HarnessRecord,
                &[],
                &[],
                &[],
                &[],
                &["(?s)<tag>.*?</tag>"],
            )
            .unwrap();
        let line = r#"{"message":{"content":"<tag>drop me</tag> keep"}}"#;
        let found = descriptor.typed_values(TypedSet::HarnessRecord, &entry(line));
        let hit = found.spans.first().expect("one span");
        assert_eq!(found.spans.len(), 1);
        assert_eq!(hit.pointer, "/message/content");
        let text = "<tag>drop me</tag> keep";
        assert_eq!(text.get(hit.start..=hit.end), Some("<tag>drop me</tag>"));
    }

    /// An entry selector tests the entry's own fields through the root
    /// and marks the whole entry; a missing field compares as unequal.
    #[test]
    fn entry_selectors_mark_the_entry_through_the_root() {
        let descriptor = descriptor()
            .with_typed(
                TypedSet::HarnessRecord,
                &[],
                &[],
                &[],
                &[
                    "$[?$.type == 'progress']",
                    "$[?$.type == 'system' && $.subtype != 'informational']",
                ],
                &[],
            )
            .unwrap();
        let dropped = |line: &str| {
            descriptor
                .typed_values(TypedSet::HarnessRecord, &entry(line))
                .dropped
        };
        assert!(dropped(r#"{"type":"progress","data":{}}"#));
        assert!(dropped(r#"{"type":"system","subtype":"turn_duration"}"#));
        assert!(dropped(r#"{"type":"system","content":"no subtype"}"#));
        assert!(!dropped(r#"{"type":"system","subtype":"informational"}"#));
        assert!(!dropped(r#"{"type":"user","message":{"content":"hi"}}"#));
        assert!(
            !descriptor
                .typed_values(TypedSet::Pii, &entry(r#"{"type":"progress"}"#))
                .dropped
        );
    }

    /// A safe pattern covers the subtree under the node it selects, and
    /// a descendant pattern falls back to JSONPath evaluation.
    #[test]
    fn safe_covers_subtrees_and_irregular_patterns_still_work() {
        let descriptor = TextDescriptor::new(&[
            "$.message.usage",
            "$..git_state.error",
            "$.permissionDecision.*",
        ])
        .unwrap();
        let line = r#"{"message":{"usage":{"service_tier":"standard","iterations":[{"type":"message"}]},"text":"hi"},"permissionDecision":{"decision":"accept"},"params":{"git_state":{"error":"pending","root":"/w"}}}"#;
        let pointers: Vec<String> = descriptor
            .text_values(&entry(line))
            .into_iter()
            .map(|v| v.pointer)
            .collect();
        assert_eq!(pointers, ["/message/text", "/params/git_state/root"]);
    }

    #[test]
    fn plain_prefixes_normalize_and_the_set_stays_prefix_free() {
        assert_eq!(
            plain_prefix("$.message.content[*].type").as_deref(),
            Some("/message/content/*/type")
        );
        assert_eq!(
            plain_prefix("$.permissionDecision.*").as_deref(),
            Some("/permissionDecision")
        );
        assert_eq!(plain_prefix("$..git_state").as_deref(), None);
        assert_eq!(
            plain_prefix("$.message.content[?@.type == 'x']").as_deref(),
            None
        );
        assert_eq!(
            normalize("/message/content/12/text"),
            "/message/content/*/text"
        );

        let set = PathSet::compile(&["$.a.b", "$.a", "$.a.c", "$.ab"]).unwrap();
        assert_eq!(
            set.prefixes.iter().cloned().collect::<Vec<_>>(),
            ["/a", "/ab"]
        );
        assert!(set.covers_normalized("/a/c/d"));
        assert!(!set.covers_normalized("/abc"));
    }

    #[test]
    fn a_bad_pattern_is_an_error_naming_it() {
        let err = TextDescriptor::new(&["$.ok", "$[?"]).unwrap_err();
        assert_eq!(err.pattern, "$[?");
    }
}
