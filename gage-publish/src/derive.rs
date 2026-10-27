//! Derivation of the staged session: the original's text values,
//! the expansion of a definition to instances over them, and the
//! write of the staged content and `filtered.json`.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;

use gage_session::{
    Derivation, EntryJson, FilterValues, StoredSession, TextDescriptor, TextValue, delete_values,
    edit_strings,
};
use gage_store::{
    DirContentSink, FilteredKind, FilteredRecord, PublishDirLayout, SummaryAttrs, format_filtered,
};
use regex::{Regex, RegexBuilder};
use serde_json::{Value, json};

use crate::definition::{PublishDefinition, Segment, TextEntry};
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
    /// The original's `project` attribute, when the caller supplies it
    pub project: Option<String>,
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
            project: None,
            descriptor: descriptor.clone(),
        })
    }

    fn entry(&self, line: u32) -> Option<&EntryText> {
        self.entries.iter().find(|e| e.line == line)
    }

    /// Whether the original carries the attribute `name`.
    fn has_attr(&self, name: &str) -> bool {
        match name {
            "project" => self.project.is_some(),
            _ => false,
        }
    }

    /// The last line of the original.
    fn last_line(&self) -> u32 {
        self.entries.last().map_or(0, |e| e.line)
    }
}

/// One effect of the definition on the session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instance {
    /// The text redaction that produced a span; none for a dropped
    /// line, a left-out value, or an attribute
    pub entry: Option<u64>,
    pub reason: String,
    pub author: String,
    pub kind: InstanceKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstanceKind {
    /// The lines `line` through `line_end` are left out
    Line { line: u32, line_end: u32 },
    Span {
        line: u32,
        pointer: String,
        /// Zero-based byte offsets in the original value, `end`
        /// inclusive
        start: usize,
        end: usize,
        /// Whether the marker replaces the span; otherwise it is removed
        mark: bool,
    },
    /// The value at `pointer` is left out of the entry
    Value { line: u32, pointer: String },
    /// The attribute `name` is left out of `attrs.json`
    Attr { name: String },
}

impl Instance {
    /// The `filtered.json` record. `published` is the span's position
    /// in the published value, `end` exclusive, for a `Span` instance.
    /// A left-out value has no record of its own: the path that
    /// selected it is recorded once as a value rule.
    fn record(&self, published: Option<(usize, usize)>) -> Option<FilteredRecord> {
        let record =
            |kind, line, line_end, reference: Option<&str>, span: Option<(usize, usize)>| {
                FilteredRecord {
                    kind,
                    line,
                    line_end,
                    reference: reference.map(String::from),
                    span_start: span.map(|(start, _)| start as u64),
                    span_end: span.map(|(_, end)| end as u64),
                    reason: self.reason.clone(),
                    author: self.author.clone(),
                }
            };
        match &self.kind {
            InstanceKind::Line { line, line_end } => Some(record(
                FilteredKind::Line,
                *line,
                Some(*line_end),
                None,
                None,
            )),
            InstanceKind::Span { line, pointer, .. } => {
                let span =
                    published.expect("a span instance is on a kept line, which line_edits visits");
                Some(record(
                    FilteredKind::Text,
                    *line,
                    None,
                    Some(pointer),
                    Some(span),
                ))
            }
            InstanceKind::Value { .. } => None,
            InstanceKind::Attr { name } => {
                Some(record(FilteredKind::Attr, 0, None, Some(name), None))
            }
        }
    }

    /// The line the instance is on; 0 for an attribute, which is on
    /// none.
    fn line(&self) -> u32 {
        match &self.kind {
            InstanceKind::Line { line, .. }
            | InstanceKind::Span { line, .. }
            | InstanceKind::Value { line, .. } => *line,
            InstanceKind::Attr { .. } => 0,
        }
    }

    /// The order instances are recorded in: by line, then pointer,
    /// then offset, with an attribute first and a dropped run before
    /// anything on its first line.
    fn order(&self) -> (u32, &str, usize) {
        match &self.kind {
            InstanceKind::Attr { name } => (0, name, 0),
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

/// A text redaction as `redact` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    pub id: u64,
    pub literal: String,
    pub reason: String,
    pub mark: bool,
    pub instances: usize,
    /// Whether the call added the redaction. False when the literal was
    /// already a redaction, which the call left as it stood, and on an
    /// edit.
    pub added: bool,
}

/// What a build produced: the instances, and the number of spans each
/// text redaction produced, by id.
pub struct Built {
    pub instances: Vec<Instance>,
    pub counts: BTreeMap<u64, usize>,
}

/// Run the named filters `names` over the original and record what
/// their patterns and typed literal paths find as the filter's text
/// redactions, replacing the filter's earlier ones. A literal on the
/// allow list is not recorded.
pub fn run_filters(
    definition: &mut PublishDefinition,
    session: &SessionText,
    names: &[String],
) -> Result<(), PublishError> {
    for name in names {
        let author = format!("filter:{name}");
        definition.text.retain(|t| t.author != author);
        let Some(filter) = Filter::named(name) else {
            continue;
        };
        let mut literals: BTreeSet<String> = BTreeSet::new();
        for entry in &session.entries {
            for value in &entry.values {
                for (_, literal) in filter.matches(&value.text) {
                    literals.insert(literal.to_string());
                }
            }
            let found = session
                .descriptor
                .filter_values(name, &entry.json)
                .map_err(|e| PublishError::Input(format!("line {}: {e}", entry.line)))?;
            literals.extend(found.literals);
        }
        for literal in literals {
            if definition.is_allowed(&literal) {
                continue;
            }
            definition.push_text(&literal, filter.reason, &author, true);
        }
    }
    Ok(())
}

/// Expand `definition` over `session`, write the staged content
/// through `stored` and `filtered.json` into `layout`, and count the
/// spans of each text redaction. The staged `attrs.json` is the
/// caller's to update.
pub fn build(
    definition: &PublishDefinition,
    session: &SessionText,
    stored: &mut dyn StoredSession,
    layout: &PublishDirLayout,
) -> Result<Built, PublishError> {
    let Expanded {
        mut instances,
        rules,
    } = expand(definition, session)?;
    instances.sort_by(|a, b| a.order().cmp(&b.order()));
    let dropped = dropped_of(&instances);
    let counts = counts_of(definition, &instances);

    let mut replaced = BTreeMap::new();
    let mut published = HashMap::new();
    for entry in &session.entries {
        if dropped.contains(&entry.line) {
            continue;
        }
        let edits = line_edits(entry, &instances, &mut published)?;
        if let Some(json) = edits.apply(&entry.json)? {
            replaced.insert(entry.line, json);
        }
    }
    let derivation = Derivation { dropped, replaced };
    let mut sink = DirContentSink::new(layout.files_dir());
    stored.write_derived(&derivation, &mut sink)?;

    let records: Vec<FilteredRecord> = rules
        .into_iter()
        .chain(
            instances
                .iter()
                .enumerate()
                .filter_map(|(i, instance)| instance.record(published.get(&i).copied())),
        )
        .collect();
    fs::write(layout.filtered_file(), format_filtered(&records))?;
    Ok(Built { instances, counts })
}

/// A byte span `start..=end` of a text value and whether the marker
/// replaces it.
pub type Span = (usize, usize, bool);

/// One text value of the published session as a candidate for
/// redaction: its original line and pointer, its text as the original
/// holds it, and the spans the definition redacts in it, merged, as
/// byte offsets into that text with `end` inclusive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub line: u32,
    pub pointer: String,
    pub text: String,
    pub spans: Vec<Span>,
}

/// Every text value the published session discloses, in document
/// order: the values of the kept lines that no filter leaves out, with
/// the spans the definition redacts in each. Reads only.
pub fn candidates(
    definition: &PublishDefinition,
    session: &SessionText,
) -> Result<Vec<Candidate>, PublishError> {
    let Expanded { instances, .. } = expand(definition, session)?;
    let dropped = dropped_of(&instances);
    let mut deleted: HashSet<(u32, &str)> = HashSet::new();
    let mut spans: HashMap<(u32, &str), Vec<Span>> = HashMap::new();
    for i in &instances {
        match &i.kind {
            InstanceKind::Value { line, pointer } => {
                deleted.insert((*line, pointer.as_str()));
            }
            InstanceKind::Span {
                line,
                pointer,
                start,
                end,
                mark,
            } => spans
                .entry((*line, pointer.as_str()))
                .or_default()
                .push((*start, *end, *mark)),
            InstanceKind::Line { .. } | InstanceKind::Attr { .. } => {}
        }
    }
    let mut out = Vec::new();
    for entry in &session.entries {
        if dropped.contains(&entry.line) {
            continue;
        }
        for value in &entry.values {
            let key = (entry.line, value.pointer.as_str());
            if deleted.contains(&key) {
                continue;
            }
            let ranges = spans.remove(&key).unwrap_or_default();
            out.push(Candidate {
                line: entry.line,
                pointer: value.pointer.clone(),
                text: value.text.clone(),
                spans: merge_spans(ranges),
            });
        }
    }
    Ok(out)
}

/// `ranges` in order with overlapping and adjacent spans merged; a
/// merged span is marked when any part of it was.
fn merge_spans(mut ranges: Vec<Span>) -> Vec<Span> {
    ranges.sort_by_key(|(start, _, _)| *start);
    let mut merged: Vec<Span> = Vec::new();
    for (start, end, mark) in ranges {
        match merged.last_mut() {
            Some(last) if start <= last.1 + 1 => {
                last.1 = last.1.max(end);
                last.2 |= mark;
            }
            _ => merged.push((start, end, mark)),
        }
    }
    merged
}

/// The number of spans each text redaction of `definition` produces
/// over `session`, by id. Reads only.
pub fn counts(
    definition: &PublishDefinition,
    session: &SessionText,
) -> Result<BTreeMap<u64, usize>, PublishError> {
    let Expanded { instances, .. } = expand(definition, session)?;
    Ok(counts_of(definition, &instances))
}

/// The number of instances `definition` produces over `session`.
/// Reads only.
pub fn change_count(
    definition: &PublishDefinition,
    session: &SessionText,
) -> Result<usize, PublishError> {
    let Expanded { instances, .. } = expand(definition, session)?;
    Ok(instances.len())
}

/// Every line a `Line` instance drops.
fn dropped_of(instances: &[Instance]) -> BTreeSet<u32> {
    let mut dropped = BTreeSet::new();
    for i in instances {
        if let InstanceKind::Line { line, line_end } = &i.kind {
            dropped.extend(*line..=*line_end);
        }
    }
    dropped
}

/// The spans per text redaction, with every redaction of the
/// definition present, at zero when it produced none.
fn counts_of(definition: &PublishDefinition, instances: &[Instance]) -> BTreeMap<u64, usize> {
    let mut counts: BTreeMap<u64, usize> = definition.text.iter().map(|t| (t.id, 0)).collect();
    for i in instances {
        if let Some(id) = i.entry {
            *counts.entry(id).or_default() += 1;
        }
    }
    counts
}

/// Check that `staged` is a removal-only derivation of `original`
/// under `definition`: the staged entries are the original's undropped
/// entries in order, each with the same JSON apart from the values
/// the named filters leave out and its text values, and each text
/// value is the original's with substrings removed or replaced by the
/// marker. An error names the original line and, for a value, its
/// pointer.
pub fn check_removal_only(
    definition: &PublishDefinition,
    original: &SessionText,
    staged: &SessionText,
) -> Result<(), PublishError> {
    let refuse =
        |m: String| PublishError::Refused(format!("staged session is not removal-only: {m}"));
    let Expanded { instances, .. } = expand(definition, original)?;
    let dropped = dropped_of(&instances);
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
        // The original with its left-out values gone is what the
        // staged entry derives from; the pointers of what remains are
        // the staged entry's
        let deleted: Vec<&str> = instances
            .iter()
            .filter_map(|i| match &i.kind {
                InstanceKind::Value { line, pointer } if *line == source.line => {
                    Some(pointer.as_str())
                }
                _ => None,
            })
            .collect();
        let expected_json = delete_values(&source.json, &deleted)?;
        let expected_values = original
            .descriptor
            .text_values(&expected_json)
            .map_err(|e| at(e.to_string()))?;
        for value in &derived.values {
            let Some(source_value) = expected_values.iter().find(|v| v.pointer == value.pointer)
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
        let pointers: Vec<&str> = expected_values.iter().map(|v| v.pointer.as_str()).collect();
        if blank_values(&expected_json, &pointers)? != blank_values(&derived.json, &pointers)? {
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

/// What the definition does to the session: the instances, and the
/// value rules of the named filters, one per path that selected a
/// value.
struct Expanded {
    instances: Vec<Instance>,
    rules: Vec<FilteredRecord>,
}

/// Expand `definition` over `session` in order of precedence: the
/// entries the named filters leave out, then the lines outside every
/// segment, then the attributes and values the named filters leave
/// out, then the spans of the text redactions. A match inside a
/// dropped line or a left-out value records nothing, and a value is
/// left out once however many paths select it, so the record holds
/// only what changed the derived session.
fn expand(definition: &PublishDefinition, session: &SessionText) -> Result<Expanded, PublishError> {
    let filters: Vec<&Filter> = definition
        .named_filters()
        .iter()
        .filter_map(|name| Filter::named(name))
        .collect();
    let mut instances = Vec::new();
    let mut dropped = BTreeSet::new();
    // What each filter's typed paths select in each entry, read once
    let mut found: Vec<Vec<(u32, FilterValues)>> = Vec::with_capacity(filters.len());
    for filter in &filters {
        let mut mine = Vec::with_capacity(session.entries.len());
        let mut lines = BTreeSet::new();
        for entry in &session.entries {
            let values = session
                .descriptor
                .filter_values(filter.name, &entry.json)
                .map_err(|e| PublishError::Input(format!("line {}: {e}", entry.line)))?;
            if values.dropped {
                lines.insert(entry.line);
            }
            mine.push((entry.line, values));
        }
        for (line, line_end) in runs(lines.iter().copied()) {
            instances.push(Instance {
                entry: None,
                reason: filter.reason.to_string(),
                author: format!("filter:{}", filter.name),
                kind: InstanceKind::Line { line, line_end },
            });
        }
        dropped.extend(lines);
        found.push(mine);
    }
    for (line, line_end, author) in segment_gaps(&definition.segments, session.last_line()) {
        let outside = (line..=line_end).filter(|l| !dropped.contains(l));
        for (line, line_end) in runs(outside) {
            instances.push(Instance {
                entry: None,
                reason: "scope".to_string(),
                author: author.clone(),
                kind: InstanceKind::Line { line, line_end },
            });
        }
    }
    dropped = dropped_of(&instances);

    let mut deleted: HashSet<(u32, String)> = HashSet::new();
    let mut rules = Vec::new();
    for (filter, mine) in filters.iter().zip(found) {
        let author = format!("filter:{}", filter.name);
        let instance = |kind| Instance {
            entry: None,
            reason: filter.reason.to_string(),
            author: author.clone(),
            kind,
        };
        for attr in filter.attrs {
            if session.has_attr(attr) {
                instances.push(instance(InstanceKind::Attr {
                    name: attr.to_string(),
                }));
            }
        }
        let mut paths: BTreeSet<String> = BTreeSet::new();
        for (line, values) in mine {
            if dropped.contains(&line) {
                continue;
            }
            let Some(entry) = session.entry(line) else {
                continue;
            };
            for hit in values.values {
                if !entry.values.iter().any(|v| v.pointer == hit.pointer) {
                    continue;
                }
                paths.insert(hit.path);
                if deleted.insert((line, hit.pointer.clone())) {
                    instances.push(instance(InstanceKind::Value {
                        line,
                        pointer: hit.pointer,
                    }));
                }
            }
        }
        rules.extend(paths.into_iter().map(|path| FilteredRecord {
            kind: FilteredKind::Value,
            line: 0,
            line_end: None,
            reference: Some(path),
            span_start: None,
            span_end: None,
            reason: filter.reason.to_string(),
            author: author.clone(),
        }));
    }

    for text in &definition.text {
        let regex = literal_regex(&text.literal);
        for entry in &session.entries {
            if dropped.contains(&entry.line) {
                continue;
            }
            for value in &entry.values {
                if deleted.contains(&(entry.line, value.pointer.clone())) {
                    continue;
                }
                instances.extend(
                    regex
                        .find_iter(&value.text)
                        .map(|m| span_of(text, entry.line, &value.pointer, m.start(), m.end() - 1)),
                );
            }
        }
    }
    Ok(Expanded { instances, rules })
}

/// The runs of consecutive lines in `lines`, which come in order.
fn runs(lines: impl IntoIterator<Item = u32>) -> Vec<(u32, u32)> {
    let mut runs: Vec<(u32, u32)> = Vec::new();
    for line in lines {
        match runs.last_mut() {
            Some(last) if line == last.1 + 1 => last.1 = line,
            _ => runs.push((line, line)),
        }
    }
    runs
}

/// The span `start..=end` of `value` on `line` as `text` redacts it.
fn span_of(text: &TextEntry, line: u32, pointer: &str, start: usize, end: usize) -> Instance {
    Instance {
        entry: Some(text.id),
        reason: text.reason.clone(),
        author: text.author.clone(),
        kind: InstanceKind::Span {
            line,
            pointer: pointer.to_string(),
            start,
            end,
            mark: text.mark,
        },
    }
}

/// The runs of lines outside every segment, `(line, line_end,
/// author)`, when at least one segment has lines. A run is attributed
/// to the segment that follows it, and the run after the last
/// segment to that segment.
fn segment_gaps(segments: &[Segment], last_line: u32) -> Vec<(u32, u32, String)> {
    let mut ranges: Vec<(u32, u32, &str)> = segments
        .iter()
        .flat_map(|s| s.lines.iter().map(|&(a, b)| (a, b, s.author.as_str())))
        .collect();
    if ranges.is_empty() {
        return Vec::new();
    }
    ranges.sort_by_key(|&(start, end, _)| (start, end));
    let mut merged: Vec<(u32, u32, &str)> = Vec::new();
    for (start, end, author) in ranges {
        match merged.last_mut() {
            Some(last) if start <= last.1.saturating_add(1) => last.1 = last.1.max(end),
            _ => merged.push((start, end, author)),
        }
    }
    let mut gaps = Vec::new();
    let mut next = 1;
    for &(start, end, author) in &merged {
        if next < start {
            gaps.push((next, start - 1, author.to_string()));
        }
        next = end.saturating_add(1);
    }
    if let Some(&(_, _, author)) = merged.last()
        && next <= last_line
    {
        gaps.push((next, last_line, author.to_string()));
    }
    gaps
}

/// Case-insensitive matcher for a literal.
fn literal_regex(literal: &str) -> Regex {
    RegexBuilder::new(&regex::escape(literal))
        .case_insensitive(true)
        .build()
        .expect("an escaped literal is a valid regex")
}

/// The edits of one entry: the text values rewritten and the values
/// left out.
struct LineEdits {
    replaced: Vec<(String, String)>,
    deleted: Vec<String>,
}

impl LineEdits {
    /// `json` with the edits applied, or `None` when there are none.
    /// The replacements go first, so every pointer names a position
    /// in the entry as written.
    fn apply(&self, json: &str) -> Result<Option<String>, PublishError> {
        if self.replaced.is_empty() && self.deleted.is_empty() {
            return Ok(None);
        }
        let pairs: Vec<(&str, &str)> = self
            .replaced
            .iter()
            .map(|(p, t)| (p.as_str(), t.as_str()))
            .collect();
        let replaced = edit_strings(json, &pairs)?;
        let deleted: Vec<&str> = self.deleted.iter().map(String::as_str).collect();
        Ok(Some(delete_values(&replaced, &deleted)?))
    }
}

/// The edits of one entry from the instances on its line. A value
/// with a `Value` instance is left out; spans are merged where they
/// overlap, a merged span is marked when any part of it was, and they
/// are applied in order. Every `Span` instance on the line records in
/// `published`, by its index in `instances`, the position of its
/// merged span in the published value.
fn line_edits(
    entry: &EntryText,
    instances: &[Instance],
    published: &mut HashMap<usize, (usize, usize)>,
) -> Result<LineEdits, PublishError> {
    let mut deleted: Vec<String> = Vec::new();
    let mut spans: BTreeMap<&str, Vec<(usize, usize, bool, usize)>> = BTreeMap::new();
    for (index, i) in instances
        .iter()
        .enumerate()
        .filter(|(_, i)| i.line() == entry.line)
    {
        match &i.kind {
            InstanceKind::Value { pointer, .. } => deleted.push(pointer.clone()),
            InstanceKind::Span {
                pointer,
                start,
                end,
                mark,
                ..
            } => spans
                .entry(pointer)
                .or_default()
                .push((*start, *end, *mark, index)),
            InstanceKind::Line { .. } | InstanceKind::Attr { .. } => {}
        }
    }
    let mut replaced = Vec::new();
    for (pointer, mut ranges) in spans {
        let value = entry
            .values
            .iter()
            .find(|v| v.pointer == pointer)
            .ok_or_else(|| {
                PublishError::Input(format!("line {}: no value at {pointer}", entry.line))
            })?;
        ranges.sort_by_key(|(start, _, _, _)| *start);
        let mut merged: Vec<(usize, usize, bool)> = Vec::new();
        let mut members: Vec<Vec<usize>> = Vec::new();
        for (start, end, mark, index) in ranges {
            match merged.last_mut() {
                Some(last) if start <= last.1 + 1 => {
                    last.1 = last.1.max(end);
                    last.2 |= mark;
                    members
                        .last_mut()
                        .expect("one member list per merged span")
                        .push(index);
                }
                _ => {
                    merged.push((start, end, mark));
                    members.push(vec![index]);
                }
            }
        }
        let (text, positions) = apply_spans(&value.text, &merged);
        for (indexes, position) in members.iter().zip(positions) {
            for index in indexes {
                published.insert(*index, position);
            }
        }
        replaced.push((pointer.to_string(), text));
    }
    Ok(LineEdits { replaced, deleted })
}

/// `text` with each byte span `start..=end` of `spans`, which are in
/// order and disjoint, replaced by the marker, or removed when `mark`
/// is false. Returns the new text and the position of each span in
/// it, `end` exclusive: the marker, or an empty span where text was
/// removed. A removal bounded by a space on both sides also removes
/// the space that follows, so the words around it keep one space
/// between them.
fn apply_spans(text: &str, spans: &[Span]) -> (String, Vec<(usize, usize)>) {
    let mut out = String::with_capacity(text.len());
    let mut positions = Vec::with_capacity(spans.len());
    let mut cursor = 0;
    for &(start, end, mark) in spans {
        let mut end = end + 1;
        let mut start = start;
        // A span must sit on character boundaries; a literal match always does
        while !text.is_char_boundary(start) {
            start -= 1;
        }
        while end < text.len() && !text.is_char_boundary(end) {
            end += 1;
        }
        let start = start.max(cursor);
        out.push_str(&text[cursor..start]);
        let at = out.len();
        if mark {
            out.push_str(MARKER);
        }
        positions.push((at, out.len()));
        cursor = end.max(start);
        if !mark && out.ends_with(' ') && text[cursor..].starts_with(' ') {
            cursor += 1;
        }
    }
    out.push_str(&text[cursor..]);
    (out, positions)
}

/// Rewrite `size`, `project`, and the driver summary in the staged
/// `attrs.json` from the content written. `staged` reads the staged
/// content, not the original. `project` is the original's unless a
/// redaction leaves it out. The title is kept from the original: the
/// driver takes it from the first prompt, and dropping that line does
/// not retitle the publication.
pub fn update_attrs(
    layout: &PublishDirLayout,
    staged: &mut dyn StoredSession,
    project: Option<&str>,
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
        match project {
            Some(project) => object.insert("project".to_string(), json!(project)),
            None => object.remove("project"),
        };
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
    use gage_store::parse_filtered;

    use super::*;

    /// Every record the engine produces reads back through the store's
    /// parser with the same fields, so the two sides agree on the
    /// record layout.
    #[test]
    fn records_round_trip_through_the_store_parser() {
        let instance = |kind| Instance {
            entry: Some(1),
            reason: "pii".to_string(),
            author: "filter:pii".to_string(),
            kind,
        };
        // A left-out value has no record of its own
        assert!(
            instance(InstanceKind::Value {
                line: 7,
                pointer: "/toolUseResult/stdout".to_string(),
            })
            .record(None)
            .is_none()
        );
        let records = [
            instance(InstanceKind::Line {
                line: 1,
                line_end: 4,
            })
            .record(None)
            .unwrap(),
            instance(InstanceKind::Attr {
                name: "project".to_string(),
            })
            .record(None)
            .unwrap(),
            instance(InstanceKind::Span {
                line: 5,
                pointer: "/message/content".to_string(),
                start: 4,
                end: 23,
                mark: true,
            })
            .record(Some((4, 14)))
            .unwrap(),
        ];
        let parsed = parse_filtered(format_filtered(&records).as_bytes()).unwrap();
        // The parser returns list by list: lines, values, text, attrs
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0].kind, FilteredKind::Line);
        assert_eq!((parsed[0].line, parsed[0].line_end), (1, Some(4)));
        assert_eq!(parsed[1].kind, FilteredKind::Text);
        assert_eq!(parsed[1].reference.as_deref(), Some("/message/content"));
        assert_eq!(
            (parsed[1].span_start, parsed[1].span_end),
            (Some(4), Some(14))
        );
        assert_eq!(parsed[2].kind, FilteredKind::Attr);
        assert_eq!(parsed[2].reference.as_deref(), Some("project"));
        assert!(
            parsed
                .iter()
                .all(|r| r.reason == "pii" && r.author == "filter:pii")
        );
    }

    fn segment(id: u64, lines: &[(u32, u32)], author: &str) -> Segment {
        Segment {
            id,
            description: String::new(),
            lines: lines.to_vec(),
            author: author.to_string(),
        }
    }

    /// The lines outside every segment are the runs between and
    /// around the merged ranges, each attributed to the segment that
    /// follows it; no segment means no gap.
    #[test]
    fn segment_gaps_are_the_runs_outside_the_merged_ranges() {
        assert!(segment_gaps(&[], 10).is_empty());
        let gaps = segment_gaps(
            &[
                segment(1, &[(4, 5), (9, 9)], "a"),
                segment(2, &[(2, 3)], "b"),
            ],
            12,
        );
        assert_eq!(
            gaps,
            [
                (1, 1, "b".to_string()),
                (6, 8, "a".to_string()),
                (10, 12, "a".to_string()),
            ]
        );
        let whole = segment_gaps(&[segment(1, &[(1, 12)], "a")], 12);
        assert!(whole.is_empty(), "{whole:?}");
        let past = segment_gaps(&[segment(1, &[(1, 20)], "a")], 12);
        assert!(past.is_empty(), "{past:?}");
    }

    #[test]
    fn spans_merge_where_they_overlap_or_touch() {
        assert_eq!(
            merge_spans(vec![
                (10, 12, false),
                (0, 3, true),
                (4, 6, false),
                (11, 20, true)
            ]),
            [(0, 6, true), (10, 20, true)]
        );
        assert!(merge_spans(Vec::new()).is_empty());
    }

    #[test]
    fn runs_group_consecutive_lines() {
        assert_eq!(runs([1, 2, 3, 5, 7, 8]), [(1, 3), (5, 5), (7, 8)]);
        assert!(runs([]).is_empty());
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
            apply_spans("What are you fucking asking me?", &[(13, 19, false)]),
            ("What are you asking me?".to_string(), vec![(13, 13)])
        );
        assert_eq!(
            apply_spans("What are you fucking asking me?", &[(13, 19, true)]),
            (
                "What are you [REDACTED] asking me?".to_string(),
                vec![(13, 23)]
            )
        );
        assert_eq!(
            apply_spans("fucking hell", &[(0, 6, false)]),
            (" hell".to_string(), vec![(0, 0)])
        );
    }

    /// A span's published position accounts for every span before it
    /// in the same value: a marker lengthens the text and a removal
    /// shortens it.
    #[test]
    fn published_positions_follow_earlier_spans_in_the_value() {
        let text = "key AAAA then BBBB then CCCC end";
        let (out, positions) = apply_spans(text, &[(4, 7, true), (14, 17, false), (24, 27, true)]);
        assert_eq!(out, "key [REDACTED] then then [REDACTED] end");
        assert_eq!(positions, [(4, 14), (20, 20), (25, 35)]);
        assert_eq!(&out[4..14], MARKER);
        assert_eq!(&out[25..35], MARKER);
    }
}
