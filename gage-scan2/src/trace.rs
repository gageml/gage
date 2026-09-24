//! Runtime records and panics into the scan record.
//!
//! [`layer`] is a `tracing` layer the CLI installs. Every event it
//! admits is appended as one record to the running scan's staging:
//! to the task's `logs/records` when a task is running, otherwise to
//! the scan's `logs/records`. A runtime record carries the Rust target
//! after the level, `… INFO gage_store: …`, which a scanner's own
//! records lack. Outside a scan the layer drops events; the CLI's
//! stderr layer still shows them.
//!
//! The scan sets the destination through a task-local [`LogScope`],
//! so no handle is shared with the layer. [`install_panic_hook`] uses
//! the same scope to append a panic and its backtrace to the scan's
//! `logs/err` before the process dies, leaving staging for recovery.

use std::fmt::Write as _;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Once};

use gage_core::datetime::{ms_to_iso8601, now_ms};
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;

use crate::staging;

tokio::task_local! {
    /// Where runtime records of the current code go
    pub(crate) static LOG_SCOPE: LogScope;
}

/// The running scan's staging `scan/` directory and, while a task
/// runs, the task.
#[derive(Clone)]
pub(crate) struct LogScope {
    pub scan_dir: PathBuf,
    /// `(scanner, task)` while a task runs
    pub task: Option<(String, String)>,
    /// The first failed append, surfaced by the scan at its end since
    /// the layer has no caller to return it to
    pub failure: Arc<Mutex<Option<io::Error>>>,
}

impl LogScope {
    fn logs_dir(&self) -> PathBuf {
        match &self.task {
            Some((scanner, task)) => staging::task_logs_dir(&self.scan_dir, scanner, task),
            None => staging::scan_logs_dir(&self.scan_dir),
        }
    }

    fn append(&self, name: &str, bytes: &[u8]) {
        if let Err(e) = staging::append(&self.logs_dir(), name, bytes) {
            let mut failure = self.failure.lock().unwrap();
            if failure.is_none() {
                *failure = Some(e);
            }
        }
    }
}

/// The layer that writes admitted runtime events to the running
/// scan. Filter it at installation.
pub fn layer() -> RecordsLayer {
    RecordsLayer
}

pub struct RecordsLayer;

impl<S: Subscriber> Layer<S> for RecordsLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let Ok(scope) = LOG_SCOPE.try_with(|s| s.clone()) else {
            return;
        };
        let mut message = MessageVisitor::default();
        event.record(&mut message);
        let meta = event.metadata();
        let line = format!(
            "{} {} {}: {}\n",
            ms_to_iso8601(now_ms()),
            meta.level(),
            meta.target(),
            message.text
        );
        scope.append(staging::RECORDS_LOG, line.as_bytes());
    }
}

/// Collects an event's `message` field, then any other fields as
/// `key=value` pairs.
#[derive(Default)]
struct MessageVisitor {
    text: String,
}

impl Visit for MessageVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            let rest = std::mem::take(&mut self.text);
            self.text = format!("{value:?}");
            if !rest.is_empty() {
                self.text.push(' ');
                self.text.push_str(&rest);
            }
        } else {
            if !self.text.is_empty() {
                self.text.push(' ');
            }
            write!(self.text, "{}={value:?}", field.name()).unwrap();
        }
    }
}

/// Install, once, a panic hook that appends the panic and its
/// backtrace to the running scan's `logs/err`. Chains to the hook
/// already installed.
pub fn install_panic_hook() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if let Ok(scope) = LOG_SCOPE.try_with(|s| s.clone()) {
                let backtrace = std::backtrace::Backtrace::force_capture();
                let text = format!("{} PANIC {info}\n{backtrace}\n", ms_to_iso8601(now_ms()));
                let scan_scope = LogScope {
                    task: None,
                    ..scope
                };
                scan_scope.append(staging::ERR_LOG, text.as_bytes());
            }
            previous(info);
        }));
    });
}
