//! The publish engine: the definition a pending publish is derived
//! from, the named filters, and the derivation of the staged session
//! from the original.
//!
//! gage-store stages the directory and knows where the definition file
//! lives; this crate reads and writes the definition, expands it to
//! instances over the original's text values, and writes the staged
//! content and `filtered.json` through the session's driver. Every
//! mutation rebuilds the staged session from the original plus the
//! full definition. The original is named by the staged `attrs.json`.

mod definition;
mod derive;
mod filter;

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::sync::Arc;

use gage_registry::driver::DriverRegistry;
use gage_session::{Driver, DriverError, EditError, StoredSession, TextDescriptor};
use gage_store::{
    DirContentSource, PublishDirLayout, PublishInit, PublishStore, SessionAttrsRecord,
    SessionRecord, SessionStore, Store, StoreError,
};

pub use definition::{
    Allow, FilterEntry, FilterSpec, NAMED_FILTERS, PublishDefinition, REASONS, Reason,
    RedactionInput, Segment, TextEntry, requires_prompt,
};
pub use derive::{Applied, Built, Candidate, Instance, InstanceKind, Span};
pub use filter::{Filter, Pattern};

/// The marker a redacted span or value is replaced with.
pub const MARKER: &str = "[REDACTED]";

#[derive(Debug)]
pub enum PublishError {
    Store(StoreError),
    Driver(DriverError),
    Io(io::Error),
    Json(serde_json::Error),
    Edit(EditError),
    /// A spec or argument the caller gave is malformed
    Input(String),
    /// A literal matched nothing, or named a value it is not in
    NoMatch(String),
    /// No driver is registered under the name the original records
    NoDriver(String),
    /// The definition has no filter or segment, or the staged content
    /// is not a removal-only derivation of the original, so `apply`
    /// refuses to commit it
    Refused(String),
    /// The definition has a filter or segment but none of them changed
    /// the session, so `apply` refuses to commit it unless forced
    Unchanged,
}

impl fmt::Display for PublishError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PublishError::Store(e) => write!(f, "{e}"),
            PublishError::Driver(e) => write!(f, "{e}"),
            PublishError::Io(e) => write!(f, "{e}"),
            PublishError::Json(e) => write!(f, "{e}"),
            PublishError::Edit(e) => write!(f, "{e}"),
            PublishError::Input(m) | PublishError::NoMatch(m) | PublishError::Refused(m) => {
                write!(f, "{m}")
            }
            PublishError::NoDriver(name) => write!(f, "no driver named {name:?} is registered"),
            PublishError::Unchanged => write!(f, "no filter or segment changed the session"),
        }
    }
}

impl Error for PublishError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            PublishError::Store(e) => Some(e),
            PublishError::Driver(e) => Some(e),
            PublishError::Io(e) => Some(e),
            PublishError::Json(e) => Some(e),
            PublishError::Edit(e) => Some(e),
            _ => None,
        }
    }
}

impl From<StoreError> for PublishError {
    fn from(e: StoreError) -> Self {
        PublishError::Store(e)
    }
}

impl From<DriverError> for PublishError {
    fn from(e: DriverError) -> Self {
        PublishError::Driver(e)
    }
}

impl From<io::Error> for PublishError {
    fn from(e: io::Error) -> Self {
        PublishError::Io(e)
    }
}

impl From<serde_json::Error> for PublishError {
    fn from(e: serde_json::Error) -> Self {
        PublishError::Json(e)
    }
}

impl From<EditError> for PublishError {
    fn from(e: EditError) -> Self {
        PublishError::Edit(e)
    }
}

/// The publish operations over an opened store.
pub struct Publisher<'a> {
    store: &'a Store,
    publish: PublishStore<'a>,
    drivers: DriverRegistry,
}

impl<'a> Publisher<'a> {
    pub fn new(store: &'a Store) -> Self {
        Self {
            store,
            publish: PublishStore::from(store),
            drivers: DriverRegistry::builtin(),
        }
    }

    /// Stage the publish under `root` instead of Gage home. For tests.
    pub fn with_root(store: &'a Store, root: impl Into<std::path::PathBuf>) -> Self {
        Self {
            store,
            publish: PublishStore::with_root(store, root),
            drivers: DriverRegistry::builtin(),
        }
    }

    pub fn store(&self) -> &PublishStore<'a> {
        &self.publish
    }

    /// Start a publish of `original` under `filters`, with a segment
    /// per description in `segments` attributed to `author`: stage the
    /// directory, run every filter, write the definition, and build
    /// the staged session. A segment starts without lines and drops
    /// nothing until it has them. A filter given twice is an error.
    pub fn init(
        &self,
        original: &SessionRecord,
        filters: &[FilterSpec],
        segments: &[String],
        author: &str,
    ) -> Result<PublishInit, PublishError> {
        let mut definition = PublishDefinition::new(filters);
        for description in segments {
            definition.push_segment(description, &[], author);
        }
        definition.validate()?;
        let init = self.publish.init(original)?;
        let session = self.open_original(&init.id)?;
        let names = definition.named_filters();
        derive::run_filters(&mut definition, &session, &names)?;
        self.build(&init.id, &definition, &session)?;
        self.write_definition(&init.id, &definition)?;
        Ok(init)
    }

    /// Add `filter` to the pending publish `id`, run it, and rebuild.
    /// A filter already on the list is an error.
    pub fn filter_add(
        &self,
        id: &str,
        filter: &FilterSpec,
    ) -> Result<PublishDefinition, PublishError> {
        let mut definition = self.definition(id)?;
        if definition.filters.iter().any(|f| f.name == filter.name) {
            return Err(PublishError::Input(format!(
                "filter {filter} is already on the list"
            )));
        }
        definition.filters.push(FilterEntry::new(filter));
        definition.validate()?;
        let session = self.open_original(id)?;
        derive::run_filters(
            &mut definition,
            &session,
            std::slice::from_ref(&filter.name),
        )?;
        self.build(id, &definition, &session)?;
        self.write_definition(id, &definition)?;
        Ok(definition)
    }

    /// Remove the filter `name` from the pending publish `id` with its
    /// text redactions, and rebuild. The allow list stays, so adding
    /// the filter back does not record a literal that was removed.
    /// Returns the removed filter.
    pub fn filter_remove(&self, id: &str, name: &str) -> Result<FilterEntry, PublishError> {
        let mut definition = self.definition(id)?;
        let (index, _) = definition.find_filter(name)?;
        let removed = definition.filters.remove(index);
        let author = removed.author();
        definition.text.retain(|t| t.author != author);
        let session = self.open_original(id)?;
        self.build(id, &definition, &session)?;
        self.write_definition(id, &definition)?;
        Ok(removed)
    }

    /// Set the prompt of the filter `name` on the pending publish `id`.
    /// A prompt is instructions, not rules, so nothing is rebuilt.
    /// Returns the filter as it now stands.
    pub fn filter_edit(
        &self,
        id: &str,
        name: &str,
        prompt: Option<String>,
    ) -> Result<FilterEntry, PublishError> {
        let mut definition = self.definition(id)?;
        let (index, _) = definition.find_filter(name)?;
        let entry = definition
            .filters
            .get_mut(index)
            .expect("find_filter returned the index");
        entry.prompt = prompt;
        let edited = entry.clone();
        definition.validate()?;
        self.write_definition(id, &definition)?;
        Ok(edited)
    }

    /// Add a segment to the pending publish `id` as `author` and
    /// rebuild. Returns its id.
    pub fn segment_add(
        &self,
        id: &str,
        description: &str,
        lines: &[(u32, u32)],
        author: &str,
    ) -> Result<u64, PublishError> {
        let mut definition = self.definition(id)?;
        let segment = definition.push_segment(description, lines, author);
        definition.validate()?;
        let session = self.open_original(id)?;
        self.build(id, &definition, &session)?;
        self.write_definition(id, &definition)?;
        Ok(segment)
    }

    /// Change the description or the lines of segment `segment` of the
    /// pending publish `id` and rebuild. Returns the segment as it now
    /// stands.
    pub fn segment_edit(
        &self,
        id: &str,
        segment: u64,
        description: Option<&str>,
        lines: Option<&[(u32, u32)]>,
    ) -> Result<Segment, PublishError> {
        let mut definition = self.definition(id)?;
        let target = definition
            .segments
            .iter_mut()
            .find(|s| s.id == segment)
            .ok_or_else(|| PublishError::Input(format!("no segment with id {segment}")))?;
        if let Some(description) = description {
            target.description = description.to_string();
        }
        if let Some(lines) = lines {
            target.lines = lines.to_vec();
        }
        let edited = target.clone();
        definition.validate()?;
        let session = self.open_original(id)?;
        self.build(id, &definition, &session)?;
        self.write_definition(id, &definition)?;
        Ok(edited)
    }

    /// Remove segment `segment` from the pending publish `id` and
    /// rebuild. Returns the removed segment.
    pub fn segment_remove(&self, id: &str, segment: u64) -> Result<Segment, PublishError> {
        let mut definition = self.definition(id)?;
        let index = definition
            .segments
            .iter()
            .position(|s| s.id == segment)
            .ok_or_else(|| PublishError::Input(format!("no segment with id {segment}")))?;
        let removed = definition.segments.remove(index);
        let session = self.open_original(id)?;
        self.build(id, &definition, &session)?;
        self.write_definition(id, &definition)?;
        Ok(removed)
    }

    /// Re-run the named filter `name`, or every named filter on the
    /// list when `name` is `None`, and rebuild. The filter's earlier
    /// text redactions go, and an allowed literal is not recorded.
    pub fn filter_run(
        &self,
        id: &str,
        name: Option<&str>,
    ) -> Result<PublishDefinition, PublishError> {
        let mut definition = self.definition(id)?;
        let names = match name {
            Some(name) => {
                if !definition.named_filters().iter().any(|n| n == name) {
                    return Err(PublishError::Input(format!(
                        "no named filter {name:?} is on the list"
                    )));
                }
                vec![name.to_string()]
            }
            None => definition.named_filters(),
        };
        let session = self.open_original(id)?;
        derive::run_filters(&mut definition, &session, &names)?;
        self.build(id, &definition, &session)?;
        self.write_definition(id, &definition)?;
        Ok(definition)
    }

    /// Add `inputs` to the pending publish `id` as `author` and rebuild.
    /// An input that matches nothing is an error, and nothing changes.
    pub fn redact(
        &self,
        id: &str,
        inputs: &[RedactionInput],
        author: &str,
    ) -> Result<Vec<Applied>, PublishError> {
        let mut definition = self.definition(id)?;
        let session = self.open_original(id)?;
        let ids: Vec<u64> = inputs
            .iter()
            .map(|input| definition.push_text(&input.literal, &input.reason, author, input.mark))
            .collect();
        let counts = derive::counts(&definition, &session)?;
        for text in definition.text.iter().filter(|t| ids.contains(&t.id)) {
            if counts.get(&text.id).copied().unwrap_or(0) == 0 {
                return Err(PublishError::NoMatch(format!(
                    "text {:?} matches nothing in the session",
                    text.literal
                )));
            }
        }
        self.build(id, &definition, &session)?;
        self.write_definition(id, &definition)?;
        Ok(definition
            .text
            .iter()
            .filter(|t| ids.contains(&t.id))
            .map(|t| Applied {
                id: t.id,
                literal: t.literal.clone(),
                reason: t.reason.clone(),
                mark: t.mark,
                instances: counts.get(&t.id).copied().unwrap_or(0),
            })
            .collect())
    }

    /// Change the literal, the reason, or the mark of the text
    /// redaction `text` of the pending publish `id` and rebuild. A
    /// literal that matches nothing is an error, and nothing changes.
    pub fn redact_edit(
        &self,
        id: &str,
        text: u64,
        literal: Option<&str>,
        reason: Option<&str>,
        mark: Option<bool>,
    ) -> Result<Applied, PublishError> {
        let mut definition = self.definition(id)?;
        let target = definition
            .text
            .iter_mut()
            .find(|t| t.id == text)
            .ok_or_else(|| PublishError::Input(format!("no redaction with id {text}")))?;
        if let Some(literal) = literal {
            target.literal = literal.to_string();
        }
        if let Some(reason) = reason {
            target.reason = reason.to_string();
        }
        if let Some(mark) = mark {
            target.mark = mark;
        }
        let edited = target.clone();
        definition.validate()?;
        let session = self.open_original(id)?;
        let counts = derive::counts(&definition, &session)?;
        let instances = counts.get(&text).copied().unwrap_or(0);
        if instances == 0 {
            return Err(PublishError::NoMatch(format!(
                "text {:?} matches nothing in the session",
                edited.literal
            )));
        }
        self.build(id, &definition, &session)?;
        self.write_definition(id, &definition)?;
        Ok(Applied {
            id: edited.id,
            literal: edited.literal,
            reason: edited.reason,
            mark: edited.mark,
            instances,
        })
    }

    /// Remove the text redactions `ids` from the pending publish `id`
    /// as `author` and rebuild. Every id must exist, or nothing
    /// changes. A filter's literal goes on the allow list so a re-run
    /// of the filter does not record it again. Returns the removed
    /// redactions.
    pub fn redact_remove(
        &self,
        id: &str,
        ids: &[u64],
        author: &str,
    ) -> Result<Vec<TextEntry>, PublishError> {
        let mut definition = self.definition(id)?;
        let missing: Vec<String> = ids
            .iter()
            .filter(|i| !definition.text.iter().any(|t| t.id == **i))
            .map(u64::to_string)
            .collect();
        if !missing.is_empty() {
            return Err(PublishError::Input(format!(
                "no redaction with id {}",
                missing.join(", ")
            )));
        }
        let (removed, kept): (Vec<TextEntry>, Vec<TextEntry>) =
            definition.text.drain(..).partition(|t| ids.contains(&t.id));
        definition.text = kept;
        for t in &removed {
            if t.author.starts_with("filter:") && !definition.is_allowed(&t.literal) {
                definition.allow.push(Allow {
                    literal: t.literal.clone(),
                    author: author.to_string(),
                });
            }
        }
        let session = self.open_original(id)?;
        self.build(id, &definition, &session)?;
        self.write_definition(id, &definition)?;
        Ok(removed)
    }

    /// Rebuild the pending publish `id` from its definition as it is on
    /// disk, for a definition edited by hand. The definition is checked
    /// first and an invalid one is an error naming the entry. Returns
    /// the number of spans the text redactions produced.
    pub fn refresh(&self, id: &str) -> Result<usize, PublishError> {
        let definition = self.definition(id)?;
        definition.validate()?;
        let session = self.open_original(id)?;
        let built = self.build(id, &definition, &session)?;
        Ok(built.counts.values().sum())
    }

    /// Every text value the pending publish `id` discloses, with the
    /// spans redacted in each. Reads only.
    pub fn candidates(&self, id: &str) -> Result<Vec<Candidate>, PublishError> {
        let definition = self.definition(id)?;
        let session = self.open_original(id)?;
        derive::candidates(&definition, &session)
    }

    /// The number of spans each text redaction of the pending publish
    /// `id` produces, by id. Reads only.
    pub fn instances(&self, id: &str) -> Result<BTreeMap<u64, usize>, PublishError> {
        let definition = self.definition(id)?;
        let session = self.open_original(id)?;
        derive::counts(&definition, &session)
    }

    /// Check that the staged session of the pending publish `id` is a
    /// removal-only derivation of the original: every staged line is an
    /// original line the definition keeps, a value is absent only
    /// where a named filter leaves it out, and every staged text value
    /// is its original with substrings removed or replaced by the
    /// marker. Reads only.
    pub fn check(&self, id: &str) -> Result<(), PublishError> {
        let definition = self.definition(id)?;
        let original = self.original(id)?;
        let layout = self.publish.layout(id);
        let source = derive::SessionText::read(original.stored()?.as_mut(), &original.descriptor)?;
        let staged =
            derive::SessionText::read(original.staged(&layout)?.as_mut(), &original.descriptor)?;
        derive::check_removal_only(&definition, &source, &staged)
    }

    /// Verify that the pending publish `id` can be published: its
    /// definition has a filter or a segment, its staged session passes
    /// [`check`], and the definition changed the session. A definition
    /// with no filter and no segment is refused regardless of `force`;
    /// an unchanged session is refused unless `force` is set. Reads
    /// only.
    ///
    /// [`check`]: Publisher::check
    pub fn verify(&self, id: &str, force: bool) -> Result<(), PublishError> {
        let definition = self.definition(id)?;
        if definition.filters.is_empty() && definition.segments.is_empty() {
            return Err(PublishError::Refused(
                "definition has no filter or segment; push the session directly to \
                 publish it unmodified"
                    .into(),
            ));
        }
        self.check(id)?;
        if !force && self.changes(id)? == 0 {
            return Err(PublishError::Unchanged);
        }
        Ok(())
    }

    /// The number of changes the pending publish `id` makes to its
    /// original: every dropped line, dropped value, dropped attribute,
    /// and redacted span. Reads only.
    pub fn changes(&self, id: &str) -> Result<usize, PublishError> {
        let definition = self.definition(id)?;
        let session = self.open_original(id)?;
        derive::change_count(&definition, &session)
    }

    /// Publish the pending publish `id`: verify it, commit the staged
    /// directory as a new session object, and delete the staging
    /// directory and the definition. Returns the new object's commit.
    pub fn apply(&self, id: &str, force: bool) -> Result<String, PublishError> {
        self.verify(id, force)?;
        Ok(self.publish.apply(id)?)
    }

    /// The definition of the pending publish `id`.
    pub fn definition(&self, id: &str) -> Result<PublishDefinition, PublishError> {
        let path = self.publish.definition_file(id);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(PublishError::Store(StoreError::ObjectNotFound(
                    id.to_string(),
                )));
            }
            Err(e) => return Err(PublishError::Io(e)),
        };
        Ok(serde_json::from_slice(&bytes)?)
    }

    fn write_definition(
        &self,
        id: &str,
        definition: &PublishDefinition,
    ) -> Result<(), PublishError> {
        fs::write(self.publish.definition_file(id), definition.to_json_text())?;
        Ok(())
    }

    /// Rebuild the staged session of `id` from the original plus
    /// `definition`, and refresh the staged `attrs.json`.
    fn build(
        &self,
        id: &str,
        definition: &PublishDefinition,
        session: &derive::SessionText,
    ) -> Result<Built, PublishError> {
        let layout = self.publish.layout(id);
        let original = self.original(id)?;
        let built = derive::build(definition, session, original.stored()?.as_mut(), &layout)?;
        let project_dropped = built
            .instances
            .iter()
            .any(|i| matches!(&i.kind, InstanceKind::Attr { name } if name == "project"));
        let project = if project_dropped {
            None
        } else {
            original.record.attrs.project.as_deref()
        };
        derive::update_attrs(&layout, original.staged(&layout)?.as_mut(), project)?;
        Ok(built)
    }

    /// The original's text values and `project` attribute, read once
    /// per operation.
    fn open_original(&self, id: &str) -> Result<derive::SessionText, PublishError> {
        let original = self.original(id)?;
        let mut session =
            derive::SessionText::read(original.stored()?.as_mut(), &original.descriptor)?;
        session.project = original.record.attrs.project.clone();
        Ok(session)
    }

    /// The original of the pending publish `id`, named by the staged
    /// `attrs.json`: its record, driver, and descriptor.
    fn original(&self, id: &str) -> Result<Original<'_>, PublishError> {
        let path = self.publish.layout(id).attrs_file();
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(PublishError::Store(StoreError::ObjectNotFound(
                    id.to_string(),
                )));
            }
            Err(e) => return Err(PublishError::Io(e)),
        };
        let attrs: SessionAttrsRecord = serde_json::from_slice(&bytes)?;
        let (_, commit) = attrs
            .origin
            .strip_prefix("session:")
            .and_then(|rest| rest.split_once('@'))
            .ok_or_else(|| {
                PublishError::Input(format!(
                    "origin {:?} is not session:<id>@<commit>",
                    attrs.origin
                ))
            })?;
        let record = SessionStore::from(self.store).at_commit(commit)?;
        let driver: Arc<dyn Driver> = self
            .drivers
            .for_name(&record.driver_name)
            .ok_or_else(|| PublishError::NoDriver(record.driver_name.clone()))?;
        let descriptor = driver
            .text_descriptor(&record.attrs.content_format)
            .ok_or_else(|| {
                PublishError::Input(format!(
                    "driver {:?} describes no text for content format {:?}",
                    record.driver_name, record.attrs.content_format
                ))
            })?;
        Ok(Original {
            store: self.store,
            record,
            driver,
            descriptor,
        })
    }
}

/// The original of a pending publish: its record, the driver that
/// wrote it, and the driver's descriptor.
struct Original<'a> {
    store: &'a Store,
    record: SessionRecord,
    driver: Arc<dyn Driver>,
    descriptor: TextDescriptor,
}

impl Original<'_> {
    /// A reader over the original's content at its commit.
    fn stored(&self) -> Result<Box<dyn StoredSession>, PublishError> {
        let content = SessionStore::from(self.store).content(&self.record.commit_sha);
        Ok(self.driver.read_stored(
            self.record.attrs.native_id().to_string(),
            &self.record.attrs.content_format,
            content,
        )?)
    }

    /// A reader over the staged content under `layout`.
    fn staged(&self, layout: &PublishDirLayout) -> Result<Box<dyn StoredSession>, PublishError> {
        let content = Box::new(DirContentSource::new(layout.files_dir()));
        Ok(self.driver.read_stored(
            self.record.attrs.native_id().to_string(),
            &self.record.attrs.content_format,
            content,
        )?)
    }
}
