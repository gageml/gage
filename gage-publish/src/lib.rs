//! The publish engine: the definition a pending publish is derived
//! from, the filters it names, and the derivation of the staged
//! session from the original.
//!
//! [`staging`] stages the directory and knows where the definition
//! file lives; the engine reads and writes the definition, runs the
//! Rune filters it names for their contributions, expands both to
//! instances over the original's text values, and writes the staged
//! content and `changes.json` through the session's driver. The
//! definition is edited by hand, and a refresh rebuilds the staged
//! session from the original plus the full definition. The original
//! is named by the staged `attrs.json`. The filters run at every
//! rebuild, so every operation that expands the definition is async.

pub mod candidate;
mod definition;
mod derive;
mod filter;
pub mod staging;

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::sync::Arc;

use gage_registry::driver::DriverRegistry;
use gage_registry::filter::FilterRegistry;
use gage_session::{Driver, DriverError, EditError, StoredSession, TextDescriptor};
use gage_store::{
    DirContentSource, SessionAttrsRecord, SessionDirLayout, SessionRecord, SessionStore, Store,
    StoreError,
};

pub use definition::{
    Allow, FilterEntry, InitArgs, Problem, PublishDefinition, Segment, TextEntry, TitleEntry,
    format_lines, parse_lines,
};
pub use derive::{Built, Candidate, Instance, InstanceKind, Span};
pub use filter::{FilterOutput, instructions_markdown};
pub use staging::{PublishInit, Staging};

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
    /// The definition file has these problems, each at its position
    Definition(Vec<Problem>),
    /// A filter the definition names does not compile or a task of it
    /// failed; the message names the filter
    Filter(String),
    /// No driver is registered under the name the original records
    NoDriver(String),
    /// The staged content is not a removal-only derivation of the
    /// original, so `apply` refuses to commit it
    Refused(String),
    /// The definition changed nothing in the session, so `apply`
    /// refuses to commit it unless forced
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
            PublishError::Input(m) | PublishError::Refused(m) | PublishError::Filter(m) => {
                write!(f, "{m}")
            }
            PublishError::Definition(problems) => {
                for (i, p) in problems.iter().enumerate() {
                    if i > 0 {
                        f.write_str("\n")?;
                    }
                    write!(f, "{p}")?;
                }
                Ok(())
            }
            PublishError::NoDriver(name) => write!(f, "no driver named {name:?} is registered"),
            PublishError::Unchanged => write!(f, "definition changed nothing in the session"),
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

/// The publish operations over an opened store, with the filter
/// registry the definitions' filters resolve through.
pub struct Publisher<'a> {
    store: &'a Store,
    staging: Staging<'a>,
    drivers: DriverRegistry,
    filters: FilterRegistry,
}

/// What a refresh produced: the spans the text redactions produced,
/// the rows that took the refresh's author, and the rows that matched
/// nothing, each by 1-based position with its literal. A filter's own
/// row is not reported: the filter read its literal from the original,
/// and a miss means another filter left the source entry out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refreshed {
    pub instances: usize,
    pub authored: usize,
    pub unmatched: Vec<(usize, String)>,
}

impl<'a> Publisher<'a> {
    pub fn new(store: &'a Store, filters: FilterRegistry) -> Self {
        Self {
            store,
            staging: Staging::from(store),
            drivers: DriverRegistry::builtin(),
            filters,
        }
    }

    /// Stage the publish under `root` instead of Gage home. For tests.
    pub fn with_root(
        store: &'a Store,
        root: impl Into<std::path::PathBuf>,
        filters: FilterRegistry,
    ) -> Self {
        Self {
            store,
            staging: Staging::with_root(store, root),
            drivers: DriverRegistry::builtin(),
            filters,
        }
    }

    pub fn filters(&self) -> &FilterRegistry {
        &self.filters
    }

    pub fn staging(&self) -> &Staging<'a> {
        &self.staging
    }

    /// Start a publish of `original` under `filters`, with a segment
    /// per description in `segments` attributed to `author` and
    /// `init` recorded for the reader: stage the directory, run every
    /// filter, write the definition, and build the staged session. A
    /// segment starts without lines and drops nothing until it has
    /// them. A filter given twice is an error, and so is a filter the
    /// registry does not know; the filters run before anything is
    /// staged, so a filter that fails stages nothing.
    pub async fn init(
        &self,
        original: &SessionRecord,
        filters: &[FilterEntry],
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
        let outputs = self.outputs(&definition).await?;
        let init = self.staging.init(original)?;
        let session = self.open_original(&init.id)?;
        derive::run_filters(&mut definition, &session, &outputs)?;
        self.build(&init.id, &definition, &session, &outputs)?;
        self.write_definition(&init.id, &definition)?;
        Ok(init)
    }

    /// What the filters of `definition` contribute, run now.
    async fn outputs(
        &self,
        definition: &PublishDefinition,
    ) -> Result<Vec<FilterOutput>, PublishError> {
        filter::run_filters(&self.filters, &definition.filters).await
    }

    /// Rebuild the pending publish `id` from its definition as it is on
    /// disk, for a definition edited by hand. A redaction written
    /// without an author takes `author`. The definition is checked
    /// first and an invalid one is an error naming the row; nothing
    /// changes. Every named filter runs again, so a filter's row
    /// deleted by hand comes back unless its literal is allowed. The
    /// file is rewritten with what the refresh changed.
    pub async fn refresh(&self, id: &str, author: &str) -> Result<Refreshed, PublishError> {
        let definition = self.definition(id)?;
        self.rebuild(id, definition, author).await
    }

    /// Apply edits to the definition of the pending publish `id` and
    /// rebuild it as [`refresh`] does. The title is set when `title`
    /// is given, replacing any existing one. Each of `redactions` not
    /// already recorded is added. A row this edit writes without an
    /// author takes `author`.
    ///
    /// [`refresh`]: Publisher::refresh
    pub async fn edit(
        &self,
        id: &str,
        author: &str,
        title: Option<&str>,
        redactions: &[TextEntry],
    ) -> Result<Refreshed, PublishError> {
        let mut definition = self.definition(id)?;
        if let Some(text) = title {
            definition.set_title(text, author);
        }
        definition.add_redactions(redactions);
        self.rebuild(id, definition, author).await
    }

    /// Fill authors, check, run every named filter, build the staged
    /// session, and write the definition back. Shared by [`refresh`]
    /// and [`edit`].
    ///
    /// [`refresh`]: Publisher::refresh
    /// [`edit`]: Publisher::edit
    async fn rebuild(
        &self,
        id: &str,
        mut definition: PublishDefinition,
        author: &str,
    ) -> Result<Refreshed, PublishError> {
        let authored = definition.fill_authors(author);
        definition.validate()?;
        let outputs = self.outputs(&definition).await?;
        let session = self.open_original(id)?;
        derive::run_filters(&mut definition, &session, &outputs)?;
        let built = self.build(id, &definition, &session, &outputs)?;
        self.write_definition(id, &definition)?;
        let unmatched = definition
            .redact
            .iter()
            .enumerate()
            .filter(|(_, t)| !t.author.starts_with("filter:"))
            .filter(|(i, _)| built.counts.get(i).copied().unwrap_or(0) == 0)
            .map(|(i, t)| (i + 1, t.literal.clone()))
            .collect();
        Ok(Refreshed {
            instances: built.counts.values().sum(),
            authored,
            unmatched,
        })
    }

    /// Every text value the pending publish `id` discloses. The text
    /// is the full value; the spans mark the substrings redaction
    /// removes, carried beside it. Reads only.
    pub async fn candidates(&self, id: &str) -> Result<Vec<Candidate>, PublishError> {
        let definition = self.definition(id)?;
        let outputs = self.outputs(&definition).await?;
        let session = self.open_original(id)?;
        derive::candidates(&definition, &session, &outputs)
    }

    /// Each distinct text the pending publish `id` discloses, once, in
    /// order of first occurrence: the candidate text as the redaction
    /// agent reads it. Reads only.
    pub async fn candidate_texts(&self, id: &str) -> Result<Vec<String>, PublishError> {
        Ok(candidate::unique_texts(&self.candidates(id).await?))
    }

    /// The redaction instructions of the pending publish `id`'s
    /// filters as markdown, as [`instructions_markdown`] renders them.
    /// Reads only.
    pub async fn instructions(&self, id: &str) -> Result<String, PublishError> {
        let definition = self.definition(id)?;
        let outputs = self.outputs(&definition).await?;
        Ok(instructions_markdown(&outputs))
    }

    /// The number of spans each text redaction of the pending publish
    /// `id` produces, by its index in `redact`. Reads only.
    pub async fn instances(&self, id: &str) -> Result<BTreeMap<usize, usize>, PublishError> {
        let definition = self.definition(id)?;
        let outputs = self.outputs(&definition).await?;
        let session = self.open_original(id)?;
        derive::counts(&definition, &session, &outputs)
    }

    /// Check that the staged session of the pending publish `id` is a
    /// removal-only derivation of the original: every staged line is an
    /// original line the definition keeps, a value is absent only
    /// where a named filter leaves it out, and every staged text value
    /// is its original with substrings removed or replaced by the
    /// marker. Reads only.
    pub async fn check(&self, id: &str) -> Result<(), PublishError> {
        let definition = self.definition(id)?;
        let outputs = self.outputs(&definition).await?;
        let original = self.original(id)?;
        let layout = self.staging.layout(id).session();
        let source = derive::SessionText::read(original.stored()?.as_mut(), &original.descriptor)?;
        let staged =
            derive::SessionText::read(original.staged(&layout)?.as_mut(), &original.descriptor)?;
        let title_entry = original.title_entry(&definition)?;
        derive::check_removal_only(
            &definition,
            &source,
            &staged,
            &outputs,
            title_entry.as_deref(),
        )
    }

    /// Verify that the pending publish `id` can be published: its
    /// definition reads and has an author on every redaction and the
    /// title, its staged session passes [`check`], and the definition
    /// changed the session. An unchanged session is refused unless
    /// `force` is set. Reads only.
    ///
    /// [`check`]: Publisher::check
    pub async fn verify(&self, id: &str, force: bool) -> Result<(), PublishError> {
        let definition = self.definition(id)?;
        definition.validate()?;
        self.check(id).await?;
        if !force && self.changes(id).await? == 0 {
            return Err(PublishError::Unchanged);
        }
        Ok(())
    }

    /// The number of changes the pending publish `id` makes to its
    /// original: every dropped line, dropped value, dropped attribute,
    /// and redacted span, and the title when the definition sets one.
    /// Reads only.
    pub async fn changes(&self, id: &str) -> Result<usize, PublishError> {
        let definition = self.definition(id)?;
        let outputs = self.outputs(&definition).await?;
        let session = self.open_original(id)?;
        derive::change_count(&definition, &session, &outputs)
    }

    /// Publish the pending publish `id`: verify it, commit the staged
    /// session as a new session object with the definition's values
    /// attached, and delete the staging directory. Returns the new
    /// object's commit.
    pub async fn apply(&self, id: &str, force: bool) -> Result<String, PublishError> {
        let definition = self.definition(id)?;
        self.verify(id, force).await?;
        self.staging.apply(id, Some(&definition))
    }

    /// The definition of the pending publish `id`, as the file holds
    /// it, checked against the original it was staged from. A row
    /// written without an author has an empty one. A file with
    /// problems is [`PublishError::Definition`] with every problem at
    /// its position.
    pub fn definition(&self, id: &str) -> Result<PublishDefinition, PublishError> {
        let path = self.staging.definition_file(id);
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(PublishError::Store(StoreError::ObjectNotFound(
                    id.to_string(),
                )));
            }
            Err(e) => return Err(PublishError::Io(e)),
        };
        let original = self.original(id)?;
        PublishDefinition::read(&text, &original.record.id).map_err(PublishError::Definition)
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
            self.staging.definition_file(id),
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
        outputs: &[FilterOutput],
    ) -> Result<Built, PublishError> {
        let layout = self.staging.layout(id).session();
        let original = self.original(id)?;
        let title_entry = original.title_entry(definition)?;
        let built = derive::build(
            definition,
            session,
            outputs,
            original.stored()?.as_mut(),
            &layout,
            title_entry.as_deref(),
        )?;
        derive::update_attrs(&layout, original.staged(&layout)?.as_mut())?;
        Ok(built)
    }

    /// The original's text values, read once per operation.
    fn open_original(&self, id: &str) -> Result<derive::SessionText, PublishError> {
        let original = self.original(id)?;
        derive::SessionText::read(original.stored()?.as_mut(), &original.descriptor)
    }

    /// The original of the pending publish `id`, named by the staged
    /// `attrs.json`: its record, driver, and descriptor.
    fn original(&self, id: &str) -> Result<Original<'_>, PublishError> {
        let path = self.staging.layout(id).session().attrs_file();
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
    fn staged(&self, layout: &SessionDirLayout) -> Result<Box<dyn StoredSession>, PublishError> {
        let content = Box::new(DirContentSource::new(layout.files_dir()));
        Ok(self.driver.read_stored(
            self.record.attrs.native_id().to_string(),
            &self.record.attrs.content_format,
            content,
        )?)
    }

    /// The entry the driver adds after the last line to set the title
    /// `definition` names; `None` when it names none. A driver with no
    /// such entry for the content format cannot set a title.
    fn title_entry(&self, definition: &PublishDefinition) -> Result<Option<String>, PublishError> {
        let Some(title) = &definition.title else {
            return Ok(None);
        };
        let format = &self.record.attrs.content_format;
        self.driver
            .title_entry(format, &title.text)
            .map(Some)
            .ok_or_else(|| {
                PublishError::Input(format!(
                    "driver {:?} sets no title for content format {format:?}",
                    self.record.driver_name
                ))
            })
    }
}
