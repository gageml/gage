//! The publish engine: the definition a pending publish is derived
//! from, the named filters, and the derivation of the staged session
//! from the original.
//!
//! gage-store stages the directory and knows where the definition file
//! lives; this crate reads and writes the definition, expands it to
//! instances over the original's text values, and writes the staged
//! content and `filtered.json` through the session's driver. The
//! definition is edited by hand, and a refresh rebuilds the staged
//! session from the original plus the full definition. The original
//! is named by the staged `attrs.json`.

pub mod candidate;
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
    Allow, FilterEntry, FilterSpec, InitArgs, NAMED_FILTERS, PublishDefinition, REASONS, Reason,
    Segment, TextEntry, format_lines, parse_lines, requires_prompt,
};
pub use derive::{Built, Candidate, Instance, InstanceKind, Span};
pub use filter::{Audience, Filter, Pattern, Use};

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
            PublishError::Input(m) | PublishError::Refused(m) => {
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

/// What a refresh produced: the spans the text redactions produced,
/// the rows that took the refresh's author, and the rows that matched
/// nothing, each by 1-based position with its literal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refreshed {
    pub instances: usize,
    pub authored: usize,
    pub unmatched: Vec<(usize, String)>,
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
    /// per description in `segments` attributed to `author` and
    /// `init` recorded for the reader: stage the directory, run every
    /// filter, write the definition, and build the staged session. A
    /// segment starts without lines and drops nothing until it has
    /// them. A filter given twice is an error.
    pub fn init(
        &self,
        original: &SessionRecord,
        filters: &[FilterSpec],
        segments: &[String],
        author: &str,
        init: &InitArgs,
    ) -> Result<PublishInit, PublishError> {
        let mut definition = PublishDefinition::new(&original.id, filters);
        definition.init = init.clone();
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

    /// Rebuild the pending publish `id` from its definition as it is on
    /// disk, for a definition edited by hand. A redaction written
    /// without an author takes `author`. The definition is checked
    /// first and an invalid one is an error naming the row; nothing
    /// changes. Every named filter runs again, so a filter's row
    /// deleted by hand comes back unless its literal is allowed. The
    /// file is rewritten with what the refresh changed.
    pub fn refresh(&self, id: &str, author: &str) -> Result<Refreshed, PublishError> {
        let mut definition = self.definition(id)?;
        let authored = definition.fill_authors(author);
        definition.validate()?;
        let original = self.original(id)?;
        if definition.session != original.record.id {
            return Err(PublishError::Input(format!(
                "session {:?} is not the original {} the publish was staged from",
                definition.session, original.record.id
            )));
        }
        let session = self.open_original(id)?;
        let names = definition.named_filters();
        derive::run_filters(&mut definition, &session, &names)?;
        let built = self.build(id, &definition, &session)?;
        self.write_definition(id, &definition)?;
        let unmatched = definition
            .redact
            .iter()
            .enumerate()
            .filter(|(i, _)| built.counts.get(i).copied().unwrap_or(0) == 0)
            .map(|(i, t)| (i + 1, t.literal.clone()))
            .collect();
        Ok(Refreshed {
            instances: built.counts.values().sum(),
            authored,
            unmatched,
        })
    }

    /// Every text value the pending publish `id` discloses, with the
    /// spans redacted in each. Reads only.
    pub fn candidates(&self, id: &str) -> Result<Vec<Candidate>, PublishError> {
        let definition = self.definition(id)?;
        let session = self.open_original(id)?;
        derive::candidates(&definition, &session)
    }

    /// Each distinct text the pending publish `id` discloses, once, in
    /// order of first occurrence: the candidate text as the redaction
    /// agent reads it. Reads only.
    pub fn candidate_texts(&self, id: &str) -> Result<Vec<String>, PublishError> {
        Ok(candidate::unique_texts(&self.candidates(id)?))
    }

    /// The number of spans each text redaction of the pending publish
    /// `id` produces, by its index in `redact`. Reads only.
    pub fn instances(&self, id: &str) -> Result<BTreeMap<usize, usize>, PublishError> {
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
    /// definition is valid, names its original, and has a filter or a
    /// segment, its staged session passes [`check`], and the
    /// definition changed the session. A definition with no filter and
    /// no segment is refused regardless of `force`; an unchanged
    /// session is refused unless `force` is set. Reads only.
    ///
    /// [`check`]: Publisher::check
    pub fn verify(&self, id: &str, force: bool) -> Result<(), PublishError> {
        let definition = self.definition(id)?;
        definition.validate()?;
        let original = self.original(id)?;
        if definition.session != original.record.id {
            return Err(PublishError::Refused(format!(
                "definition names session {:?}, not the original {} the publish was staged from",
                definition.session, original.record.id
            )));
        }
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

    /// The definition of the pending publish `id`, as the file holds
    /// it. A row written without an author has an empty one.
    pub fn definition(&self, id: &str) -> Result<PublishDefinition, PublishError> {
        let path = self.publish.definition_file(id);
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(PublishError::Store(StoreError::ObjectNotFound(
                    id.to_string(),
                )));
            }
            Err(e) => return Err(PublishError::Io(e)),
        };
        PublishDefinition::from_jsonc(&text)
    }

    /// Write the definition of `id` with the original's title beside
    /// the session.
    fn write_definition(
        &self,
        id: &str,
        definition: &PublishDefinition,
    ) -> Result<(), PublishError> {
        let original = self.original(id)?;
        let title = original.record.attrs.summary.title.as_deref();
        fs::write(
            self.publish.definition_file(id),
            definition.to_jsonc_text(title),
        )?;
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
