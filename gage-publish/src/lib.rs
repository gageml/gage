//! The publish engine: the definition a pending publish is derived
//! from, the named filters, and the derivation of the staged session
//! from the original.
//!
//! gage-store stages the directory and knows where the definition file
//! lives; this crate reads and writes the definition, expands its
//! redactions to instances over the original's text values, and
//! writes the staged content and `redacted.json` through the
//! session's driver. Every mutation rebuilds the staged session from
//! the original plus the full definition.

mod definition;
mod derive;
mod filter;

use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::sync::Arc;

use gage_registry::driver::DriverRegistry;
use gage_session::{Driver, DriverError, EditError, StoredSession, TextDescriptor};
use gage_store::{
    DirContentSource, PublishDirLayout, PublishInit, PublishStore, SessionRecord, SessionStore,
    Store, StoreError,
};

pub use definition::{
    FilterEntry, FilterSpec, NAMED_FILTERS, PublishDefinition, REASONS, Reason, Redaction,
    RedactionInput, RedactionKind, SpanMode, Suppression,
};
pub use derive::{Applied, Instance, InstanceKind};
pub use filter::{Filter, Pattern};

/// The Gage version whose filter logic this build carries.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

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
}

impl fmt::Display for PublishError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PublishError::Store(e) => write!(f, "{e}"),
            PublishError::Driver(e) => write!(f, "{e}"),
            PublishError::Io(e) => write!(f, "{e}"),
            PublishError::Json(e) => write!(f, "{e}"),
            PublishError::Edit(e) => write!(f, "{e}"),
            PublishError::Input(m) | PublishError::NoMatch(m) => write!(f, "{m}"),
            PublishError::NoDriver(name) => write!(f, "no driver named {name:?} is registered"),
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

    /// Start a publish of `original` under `filters`: stage the
    /// directory, write the definition, run the deterministic rules of
    /// every named filter, and build the staged session.
    pub fn init(
        &self,
        original: &SessionRecord,
        filters: &[FilterSpec],
    ) -> Result<PublishInit, PublishError> {
        let init = self.publish.init(original)?;
        let mut definition = PublishDefinition::new(&init.origin, filters, VERSION);
        self.run_filters(&mut definition)?;
        self.build(&init.id, &mut definition)?;
        Ok(init)
    }

    /// Replace the filters of the pending publish `id`: the old
    /// filters' redactions go, the new filters run, and the staged
    /// session is rebuilt.
    pub fn reinit(
        &self,
        id: &str,
        filters: &[FilterSpec],
    ) -> Result<PublishDefinition, PublishError> {
        let mut definition = self.definition(id)?;
        definition.filters = filters
            .iter()
            .map(|f| FilterEntry::new(f, VERSION))
            .collect();
        definition
            .redactions
            .retain(|r| !r.author.starts_with("filter:"));
        self.run_filters(&mut definition)?;
        self.build(id, &mut definition)?;
        Ok(definition)
    }

    /// Add `inputs` to the pending publish `id` as `author` and rebuild.
    /// Every input is resolved before any is recorded, so one bad input
    /// leaves the definition unchanged.
    pub fn redact(
        &self,
        id: &str,
        inputs: &[RedactionInput],
        author: &str,
    ) -> Result<Vec<Applied>, PublishError> {
        let mut definition = self.definition(id)?;
        let session = self.open_original(&definition)?;
        let kinds = derive::resolve_inputs(inputs, &session)?;
        let ids: Vec<u64> = kinds
            .into_iter()
            .map(|kind| definition.push(author, None, kind))
            .collect();
        let instances = self.build_with(id, &mut definition, &session)?;
        for redaction_id in &ids {
            let count = instances
                .iter()
                .filter(|i| i.redaction == *redaction_id)
                .count();
            if count == 0 {
                let r = definition
                    .redactions
                    .iter()
                    .find(|r| r.id == *redaction_id)
                    .expect("the redaction was pushed above");
                return Err(PublishError::NoMatch(format!(
                    "{} {:?} matches nothing in the session",
                    r.kind.kind_name(),
                    r.kind.detail()
                )));
            }
        }
        self.write_definition(id, &definition)?;
        Ok(definition
            .redactions
            .iter()
            .filter(|r| ids.contains(&r.id))
            .map(Applied::from)
            .collect())
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
        let mut bytes = serde_json::to_vec_pretty(definition)?;
        bytes.push(b'\n');
        fs::write(self.publish.definition_file(id), &bytes)?;
        Ok(())
    }

    /// Run every named filter's deterministic rules over the original
    /// and record their redactions, replacing the filter's earlier
    /// ones.
    fn run_filters(&self, definition: &mut PublishDefinition) -> Result<(), PublishError> {
        let session = self.open_original(definition)?;
        derive::run_filters(definition, &session)
    }

    /// Rebuild the staged session of `id` from the original plus
    /// `definition`, record instance counts, and write the definition.
    fn build(&self, id: &str, definition: &mut PublishDefinition) -> Result<(), PublishError> {
        let session = self.open_original(definition)?;
        self.build_with(id, definition, &session)?;
        self.write_definition(id, definition)
    }

    fn build_with(
        &self,
        id: &str,
        definition: &mut PublishDefinition,
        session: &derive::SessionText,
    ) -> Result<Vec<Instance>, PublishError> {
        let layout = self.publish.layout(id);
        let original = self.original(definition)?;
        let instances = derive::build(definition, session, original.stored()?.as_mut(), &layout)?;
        derive::update_attrs(&layout, original.staged(&layout)?.as_mut())?;
        Ok(instances)
    }

    /// The original's text values, read once per operation.
    fn open_original(
        &self,
        definition: &PublishDefinition,
    ) -> Result<derive::SessionText, PublishError> {
        let original = self.original(definition)?;
        derive::SessionText::read(original.stored()?.as_mut(), &original.descriptor)
    }

    /// The original's record, driver, and descriptor.
    fn original(&self, definition: &PublishDefinition) -> Result<Original<'_>, PublishError> {
        let (_, commit) = definition.original()?;
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
