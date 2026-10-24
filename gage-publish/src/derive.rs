//! Derivation of the staged session: the original's text values,
//! the expansion of a definition's redactions to instances over them,
//! and the write of the staged content and `redacted.json`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;

use gage_session::{Derivation, EntryJson, StoredSession, TextDescriptor, TextValue, edit_strings};
use gage_store::{DirContentSink, PublishDirLayout, SummaryAttrs};
use regex::{Regex, RegexBuilder};
use serde_json::{Value, json};

use crate::definition::{PublishDefinition, Redaction, RedactionInput, RedactionKind};
use crate::filter::Filter;
use crate::{MARKER, PublishError};

/// One entry of the original with its text values.
pub struct EntryText {
    pub line: u32,
    pub json: String,
    pub values: Vec<TextValue>,
}

/// The original session's entries and text values, read once.
pub struct SessionText {
    pub entries: Vec<EntryText>,
    descriptor: TextDescriptor,
}

impl SessionText {
    pub fn read(
        stored: &mut dyn StoredSession,
        descriptor: &TextDescriptor,
    ) -> Result<Self, PublishError> {
        let mut entries = Vec::new();
        for item in stored.entry_json() {
            let EntryJson { line, json } = item?;
            let values = descriptor
                .text_values(&json)
                .map_err(|e| PublishError::Input(format!("line {line}: {e}")))?;
            entries.push(EntryText { line, json, values });
        }
        Ok(Self {
            entries,
            descriptor: descriptor.clone(),
        })
    }

    fn entry(&self, line: u32) -> Option<&EntryText> {
        self.entries.iter().find(|e| e.line == line)
    }
}

/// One effect of a redaction on the session, as `redacted.json`
/// records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instance {
    /// The id of the redaction that produced it
    pub redaction: u64,
    pub reason: String,
    pub author: String,
    pub kind: InstanceKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstanceKind {
    Line {
        line: u32,
        line_end: u32,
    },
    Span {
        line: u32,
        pointer: String,
        /// Zero-based byte offsets, `end` inclusive
        start: usize,
        end: usize,
        /// Whether the marker replaces the span; otherwise it is removed
        mark: bool,
    },
    Value {
        line: u32,
        pointer: String,
    },
}

impl Instance {
    /// The `redacted.json` entry.
    fn record(&self) -> Value {
        match &self.kind {
            InstanceKind::Line { line, line_end } => {
                json!(["line", line, line_end, self.reason, self.author])
            }
            InstanceKind::Span {
                line,
                pointer,
                start,
                end,
                ..
            } => json!(["span", line, pointer, start, end, self.reason, self.author]),
            InstanceKind::Value { line, pointer } => {
                json!(["value", line, pointer, self.reason, self.author])
            }
        }
    }

    fn line(&self) -> u32 {
        match &self.kind {
            InstanceKind::Line { line, .. }
            | InstanceKind::Span { line, .. }
            | InstanceKind::Value { line, .. } => *line,
        }
    }

    /// The order instances are recorded in: by line, then pointer,
    /// then offset, with a dropped line before anything on it.
    fn order(&self) -> (u32, &str, usize) {
        match &self.kind {
            InstanceKind::Line { line, .. } => (*line, "", 0),
            InstanceKind::Value { line, pointer } => (*line, pointer, 0),
            InstanceKind::Span {
                line,
                pointer,
                start,
                ..
            } => (*line, pointer, *start),
        }
    }
}

/// A redaction as `redact` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    pub id: u64,
    pub kind: String,
    pub reason: String,
    pub detail: String,
    pub instances: usize,
}

impl From<&Redaction> for Applied {
    fn from(r: &Redaction) -> Self {
        Applied {
            id: r.id,
            kind: r.kind.kind_name().to_string(),
            reason: r.kind.reason().to_string(),
            detail: r.kind.detail(),
            instances: r.instances,
        }
    }
}

/// Resolve the caller's inputs against the session: a span input's
/// `(line, seq)` becomes the value's JSON Pointer.
pub fn resolve_inputs(
    inputs: &[RedactionInput],
    session: &SessionText,
) -> Result<Vec<RedactionKind>, PublishError> {
    inputs
        .iter()
        .map(|input| match input {
            RedactionInput::Lines {
                line,
                line_end,
                reason,
            } => Ok(RedactionKind::Lines {
                line: *line,
                line_end: *line_end,
                reason: reason.clone(),
            }),
            RedactionInput::Text {
                reason,
                literal,
                mark,
            } => Ok(RedactionKind::Text {
                reason: reason.clone(),
                literal: literal.clone(),
                mark: *mark,
            }),
            RedactionInput::Span {
                line,
                seq,
                reason,
                literal,
                mark,
            } => {
                let value = session
                    .entry(*line)
                    .and_then(|e| e.values.get(*seq as usize))
                    .ok_or_else(|| {
                        PublishError::NoMatch(format!(
                            "span {line} {seq}: no text value at that line and seq"
                        ))
                    })?;
                if !literal_regex(literal).is_match(&value.text) {
                    return Err(PublishError::NoMatch(format!(
                        "span {line} {seq}: {literal:?} is not in the value"
                    )));
                }
                Ok(RedactionKind::Span {
                    line: *line,
                    pointer: value.pointer.clone(),
                    reason: reason.clone(),
                    literal: literal.clone(),
                    mark: *mark,
                })
            }
        })
        .collect()
}

/// Case-insensitive matcher for a literal.
fn literal_regex(literal: &str) -> Regex {
    RegexBuilder::new(&regex::escape(literal))
        .case_insensitive(true)
        .build()
        .expect("an escaped literal is a valid regex")
}

/// Run the deterministic rules of the named filters `names` over the
/// original and record their redactions as the filter's, replacing
/// the filter's earlier ones.
pub fn run_filters(
    definition: &mut PublishDefinition,
    session: &SessionText,
    names: &[String],
) -> Result<(), PublishError> {
    for name in names {
        let author = format!("filter:{name}");
        definition.redactions.retain(|r| r.author != author);
        let Some(filter) = Filter::named(name) else {
            continue;
        };
        // One text redaction per distinct literal; the first rule to
        // find it is the one recorded
        let mut literals: BTreeMap<String, String> = BTreeMap::new();
        let mut values: Vec<(u32, String, String)> = Vec::new();
        for entry in &session.entries {
            for value in &entry.values {
                for (rule, literal) in filter.matches(&value.text) {
                    literals
                        .entry(literal.to_string())
                        .or_insert_with(|| rule.to_string());
                }
            }
            let found = session
                .descriptor
                .filter_values(name, &entry.json)
                .map_err(|e| PublishError::Input(format!("line {}: {e}", entry.line)))?;
            for literal in found.literals {
                literals
                    .entry(literal)
                    .or_insert_with(|| "typed".to_string());
            }
            for pointer in found.values {
                values.push((entry.line, pointer, "typed".to_string()));
            }
        }
        let mut candidates: Vec<(String, RedactionKind)> = literals
            .into_iter()
            .map(|(literal, rule)| {
                (
                    rule,
                    RedactionKind::Text {
                        reason: filter.reason.to_string(),
                        literal,
                        mark: true,
                    },
                )
            })
            .collect();
        candidates.extend(values.into_iter().map(|(line, pointer, rule)| {
            (
                rule,
                RedactionKind::Value {
                    line,
                    pointer,
                    reason: filter.reason.to_string(),
                },
            )
        }));
        for (rule, kind) in candidates {
            if definition.is_suppressed(&author, &kind) {
                continue;
            }
            definition.push(&author, Some(rule), kind);
        }
    }
    Ok(())
}

/// Expand every redaction of `definition` to instances over `session`,
/// write the staged content through `stored` and `redacted.json`
/// into `layout`, and record each redaction's instance count. Returns
/// the instances.
pub fn build(
    definition: &mut PublishDefinition,
    session: &SessionText,
    stored: &mut dyn StoredSession,
    layout: &PublishDirLayout,
) -> Result<Vec<Instance>, PublishError> {
    let dropped = dropped_lines(definition);
    let mut instances = Vec::new();
    for redaction in &definition.redactions {
        expand(redaction, session, &dropped, &mut instances);
    }
    for redaction in &mut definition.redactions {
        redaction.instances = instances
            .iter()
            .filter(|i| i.redaction == redaction.id)
            .count();
    }
    instances.sort_by(|a, b| a.order().cmp(&b.order()));

    let mut replaced = BTreeMap::new();
    for entry in &session.entries {
        if dropped.contains(&entry.line) {
            continue;
        }
        let edits = line_edits(entry, &instances)?;
        if !edits.is_empty() {
            let pairs: Vec<(&str, &str)> = edits
                .iter()
                .map(|(p, t)| (p.as_str(), t.as_str()))
                .collect();
            replaced.insert(entry.line, edit_strings(&entry.json, &pairs)?);
        }
    }
    let derivation = Derivation { dropped, replaced };
    let mut sink = DirContentSink::new(layout.files_dir());
    stored.write_derived(&derivation, &mut sink)?;

    // One instance per line, so the record reads as the design shows it
    let mut text = String::from("[\n");
    for (i, instance) in instances.iter().enumerate() {
        text.push_str("  ");
        text.push_str(&serde_json::to_string(&instance.record())?);
        text.push_str(if i + 1 < instances.len() { ",\n" } else { "\n" });
    }
    text.push_str("]\n");
    fs::write(layout.redacted_file(), text)?;
    Ok(instances)
}

/// Check that `staged` is a removal-only derivation of `original`
/// under `dropped`: the staged entries are the original's undropped
/// entries in order, each with the same JSON apart from its text
/// values, and each text value is the original's with substrings
/// removed or replaced by the marker. An error names the original line
/// and, for a value, its pointer.
pub fn check_removal_only(
    original: &SessionText,
    staged: &SessionText,
    dropped: &BTreeSet<u32>,
) -> Result<(), PublishError> {
    let refuse =
        |m: String| PublishError::Refused(format!("staged session is not removal-only: {m}"));
    let kept: Vec<&EntryText> = original
        .entries
        .iter()
        .filter(|e| !dropped.contains(&e.line))
        .collect();
    if kept.len() != staged.entries.len() {
        return Err(refuse(format!(
            "{} entries staged, {} expected from the original",
            staged.entries.len(),
            kept.len()
        )));
    }
    for (source, derived) in kept.iter().zip(&staged.entries) {
        let at = |m: String| refuse(format!("line {}: {m}", source.line));
        let expected_line =
            source.line - dropped.iter().filter(|&&d| d < source.line).count() as u32;
        if derived.line != expected_line {
            return Err(at(format!(
                "staged at line {}, expected line {expected_line}",
                derived.line
            )));
        }
        for value in &derived.values {
            let Some(source_value) = source.values.iter().find(|v| v.pointer == value.pointer)
            else {
                return Err(at(format!("{} has no original value", value.pointer)));
            };
            if !is_removal_only(&source_value.text, &value.text) {
                return Err(at(format!(
                    "{} is not the original value with removals",
                    value.pointer
                )));
            }
        }
        let pointers: Vec<&str> = source.values.iter().map(|v| v.pointer.as_str()).collect();
        if blank_values(&source.json, &pointers)? != blank_values(&derived.json, &pointers)? {
            return Err(at("entry differs outside its text values".to_string()));
        }
    }
    Ok(())
}

/// Whether `derived` is `original` with zero or more substrings removed
/// or replaced by the marker: with the markers taken out, `derived` is
/// a subsequence of `original`.
fn is_removal_only(original: &str, derived: &str) -> bool {
    let mut rest = original.chars();
    derived
        .split(MARKER)
        .flat_map(str::chars)
        .all(|c| rest.by_ref().any(|o| o == c))
}

/// The entry `json` with the string at each of `pointers` set to the
/// empty string, as a value, for comparison outside the text values.
fn blank_values(json: &str, pointers: &[&str]) -> Result<Value, PublishError> {
    let mut value: Value = serde_json::from_str(json)?;
    for pointer in pointers {
        if let Some(slot) = value.pointer_mut(pointer) {
            *slot = Value::String(String::new());
        }
    }
    Ok(value)
}

/// Every line a `Lines` redaction drops.
pub fn dropped_lines(definition: &PublishDefinition) -> BTreeSet<u32> {
    let mut dropped = BTreeSet::new();
    for r in &definition.redactions {
        if let RedactionKind::Lines { line, line_end, .. } = &r.kind {
            dropped.extend(*line..=*line_end);
        }
    }
    dropped
}

/// The instances of one redaction. Text instances inside a dropped
/// line are not recorded, since the line is not in the derived
/// session.
fn expand(
    redaction: &Redaction,
    session: &SessionText,
    dropped: &BTreeSet<u32>,
    out: &mut Vec<Instance>,
) {
    let instance = |kind: InstanceKind| Instance {
        redaction: redaction.id,
        reason: redaction.kind.reason().to_string(),
        author: redaction.author.clone(),
        kind,
    };
    match &redaction.kind {
        RedactionKind::Lines { line, line_end, .. } => out.push(instance(InstanceKind::Line {
            line: *line,
            line_end: *line_end,
        })),
        RedactionKind::Text { literal, mark, .. } => {
            let regex = literal_regex(literal);
            for entry in &session.entries {
                if dropped.contains(&entry.line) {
                    continue;
                }
                for value in &entry.values {
                    out.extend(
                        matches_in(&regex, value, entry.line, *mark)
                            .into_iter()
                            .map(&instance),
                    );
                }
            }
        }
        RedactionKind::Span {
            line,
            pointer,
            literal,
            mark,
            ..
        } => {
            if dropped.contains(line) {
                return;
            }
            let regex = literal_regex(literal);
            if let Some(value) = session
                .entry(*line)
                .and_then(|e| e.values.iter().find(|v| v.pointer == *pointer))
            {
                out.extend(
                    matches_in(&regex, value, *line, *mark)
                        .into_iter()
                        .map(&instance),
                );
            }
        }
        RedactionKind::Value { line, pointer, .. } => {
            if dropped.contains(line) {
                return;
            }
            if session
                .entry(*line)
                .is_some_and(|e| e.values.iter().any(|v| v.pointer == *pointer))
            {
                out.push(instance(InstanceKind::Value {
                    line: *line,
                    pointer: pointer.clone(),
                }));
            }
        }
    }
}

/// The instances a literal produces in one value: one `Value` when the
/// value is atomic and matches, else one `Span` per match.
fn matches_in(regex: &Regex, value: &TextValue, line: u32, mark: bool) -> Vec<InstanceKind> {
    let mut found = regex.find_iter(&value.text).peekable();
    if found.peek().is_none() {
        return Vec::new();
    }
    if value.atomic {
        return vec![InstanceKind::Value {
            line,
            pointer: value.pointer.clone(),
        }];
    }
    found
        .map(|m| InstanceKind::Span {
            line,
            pointer: value.pointer.clone(),
            start: m.start(),
            end: m.end() - 1,
            mark,
        })
        .collect()
}

/// The `(pointer, new text)` edits of one entry from the instances on
/// its line. A value with a `Value` instance becomes the marker; spans
/// are merged where they overlap, a merged span is marked when any
/// part of it was, and they are applied from the end.
fn line_edits(
    entry: &EntryText,
    instances: &[Instance],
) -> Result<Vec<(String, String)>, PublishError> {
    let mut whole: BTreeSet<&str> = BTreeSet::new();
    let mut spans: BTreeMap<&str, Vec<(usize, usize, bool)>> = BTreeMap::new();
    for i in instances.iter().filter(|i| i.line() == entry.line) {
        match &i.kind {
            InstanceKind::Value { pointer, .. } => {
                whole.insert(pointer);
            }
            InstanceKind::Span {
                pointer,
                start,
                end,
                mark,
                ..
            } => spans
                .entry(pointer)
                .or_default()
                .push((*start, *end, *mark)),
            InstanceKind::Line { .. } => {}
        }
    }
    let mut edits = Vec::new();
    for pointer in &whole {
        edits.push((pointer.to_string(), MARKER.to_string()));
    }
    for (pointer, mut ranges) in spans {
        if whole.contains(pointer) {
            continue;
        }
        let value = entry
            .values
            .iter()
            .find(|v| v.pointer == pointer)
            .ok_or_else(|| {
                PublishError::Input(format!("line {}: no value at {pointer}", entry.line))
            })?;
        ranges.sort_by_key(|(start, _, _)| *start);
        let mut merged: Vec<(usize, usize, bool)> = Vec::new();
        for (start, end, mark) in ranges {
            match merged.last_mut() {
                Some(last) if start <= last.1 + 1 => {
                    last.1 = last.1.max(end);
                    last.2 |= mark;
                }
                _ => merged.push((start, end, mark)),
            }
        }
        let mut text = value.text.clone();
        for (start, end, mark) in merged.into_iter().rev() {
            text = apply_span(&text, start, end, mark);
        }
        edits.push((pointer.to_string(), text));
    }
    Ok(edits)
}

/// `text` with the byte span `start..=end` replaced by the marker, or
/// removed when `mark` is false. A removal bounded by a space on both
/// sides also removes the space that follows, so the words around it
/// keep one space between them.
fn apply_span(text: &str, start: usize, end: usize, mark: bool) -> String {
    let mut end = end + 1;
    let mut start = start;
    // A span must sit on character boundaries; a literal match always does
    while !text.is_char_boundary(start) {
        start -= 1;
    }
    while end < text.len() && !text.is_char_boundary(end) {
        end += 1;
    }
    if mark {
        return format!("{}{MARKER}{}", &text[..start], &text[end..]);
    }
    let before = &text[..start];
    let after = &text[end..];
    let bounded = before.ends_with(' ') && after.starts_with(' ');
    let after = if bounded { &after[1..] } else { after };
    format!("{before}{after}")
}

/// Rewrite `size` and the driver summary in the staged `attrs.json`
/// from the content written. `staged` reads the staged content, not
/// the original. The title is kept from the original: the driver
/// takes it from the first prompt, and dropping that line does not
/// retitle the publication.
pub fn update_attrs(
    layout: &PublishDirLayout,
    staged: &mut dyn StoredSession,
) -> Result<(), PublishError> {
    let size = dir_size(&layout.files_dir())?;
    let mut attrs: Value = serde_json::from_slice(&fs::read(layout.attrs_file())?)?;
    let title = attrs
        .pointer("/summary/title")
        .and_then(Value::as_str)
        .map(String::from);
    let mut summary = SummaryAttrs::from_attrs(staged.attrs()?.as_ref());
    summary.title = title;
    if let Some(object) = attrs.as_object_mut() {
        object.insert("size".to_string(), json!(size));
        object.insert("summary".to_string(), serde_json::to_value(&summary)?);
    }
    let mut bytes = serde_json::to_vec(&attrs)?;
    bytes.push(b'\n');
    fs::write(layout.attrs_file(), &bytes)?;
    Ok(())
}

fn dir_size(dir: &std::path::Path) -> Result<u64, PublishError> {
    let mut total = 0;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let meta = entry.metadata()?;
        total += if meta.is_dir() {
            dir_size(&entry.path())?
        } else {
            meta.len()
        };
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use gage_store::{RedactedKind, parse_redacted};

    use super::*;

    /// Every record the writer produces reads back through the store's
    /// parser with the same fields, so the two sides agree on the
    /// record layout.
    #[test]
    fn records_round_trip_through_the_store_parser() {
        let instance = |kind| Instance {
            redaction: 1,
            reason: "pii".to_string(),
            author: "filter:pii".to_string(),
            kind,
        };
        let instances = [
            instance(InstanceKind::Line {
                line: 1,
                line_end: 4,
            }),
            instance(InstanceKind::Span {
                line: 5,
                pointer: "/message/content".to_string(),
                start: 4,
                end: 23,
                mark: true,
            }),
            instance(InstanceKind::Value {
                line: 7,
                pointer: "/toolUseResult/stdout".to_string(),
            }),
        ];
        let text = serde_json::to_string(&Value::Array(
            instances.iter().map(Instance::record).collect(),
        ))
        .unwrap();
        let records = parse_redacted(text.as_bytes()).unwrap();
        assert_eq!(records.len(), 3);
        assert_eq!(records[0].kind, RedactedKind::Line);
        assert_eq!((records[0].line, records[0].line_end), (1, Some(4)));
        assert_eq!(records[1].kind, RedactedKind::Span);
        assert_eq!(records[1].property.as_deref(), Some("/message/content"));
        assert_eq!(
            (records[1].span_start, records[1].span_end),
            (Some(4), Some(23))
        );
        assert_eq!(records[2].kind, RedactedKind::Value);
        assert_eq!(
            records[2].property.as_deref(),
            Some("/toolUseResult/stdout")
        );
        assert!(
            records
                .iter()
                .all(|r| r.reason == "pii" && r.author == "filter:pii")
        );
    }

    #[test]
    fn removal_only_accepts_removals_and_markers_and_rejects_additions() {
        assert!(is_removal_only("abc def ghi", "abc def ghi"));
        assert!(is_removal_only("abc def ghi", "abc ghi"));
        assert!(is_removal_only("abc def ghi", "abc [REDACTED] ghi"));
        assert!(is_removal_only("abc def ghi", "[REDACTED]"));
        assert!(is_removal_only("abc def ghi", ""));
        assert!(is_removal_only("aXbXc", "[REDACTED]b[REDACTED]"));
        assert!(!is_removal_only("abc def ghi", "abc def ghi!"));
        assert!(!is_removal_only("abc def ghi", "ghi abc"));
        assert!(!is_removal_only("abc", "abcabc"));
        assert!(!is_removal_only("abc def", "abc [REDACTED] def x"));
    }

    #[test]
    fn removal_takes_one_bounding_space_and_marking_keeps_both() {
        assert_eq!(
            apply_span("What are you fucking asking me?", 13, 19, false),
            "What are you asking me?"
        );
        assert_eq!(
            apply_span("What are you fucking asking me?", 13, 19, true),
            "What are you [REDACTED] asking me?"
        );
        assert_eq!(apply_span("fucking hell", 0, 6, false), " hell");
    }
}
