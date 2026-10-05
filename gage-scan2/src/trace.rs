//! Runtime records and panics into the scan record.
//!
//! [`layer`] is a `tracing` layer the CLI installs. Every event it
//! admits is appended as one record to the running scan's
//! `logs/records` in scan_dir. A runtime record carries the Rust
//! target as its origin, `… INFO gage_store: …`, where a scanner's
//! own record carries `<scanner>:<task>`; a runtime record raised
//! while a task runs ends with `task=<scanner>:<task>`. Outside a
//! scan the layer drops events; the CLI's stderr layer still shows
//! them.
//!
//! The scan sets the destination through a task-local [`LogScope`],
//! so no handle is shared with the layer. A span created inside a
//! scope carries the scope with it, so an event raised outside any
//! scope but under such a span is still recorded: a scanner tool
//! handler runs on the MCP host's own tasks under a span parented to
//! the calling task's. [`install_panic_hook`] uses the task-local
//! scope to append a panic and its backtrace to the scan's `logs/err`
//! before the process dies, leaving the scan directory for recovery.

use std::fmt::Write as _;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Once};

use gage_core::datetime::{ms_to_iso8601, now_ms};
use gage_runtime2::LOG_TARGET;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id};
use tracing::{Event, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;

use crate::scan_dir;

tokio::task_local! {
    /// Where runtime records of the current code go
    pub(crate) static LOG_SCOPE: LogScope;
}

/// The running scan's `scan/` subtree in its scan directory and, while a task
/// runs, the task, which runtime records name.
#[derive(Clone)]
pub(crate) struct LogScope {
    pub object_dir: PathBuf,
    /// `(scanner, task)` while a task runs
    pub task: Option<(String, String)>,
    /// The first failed append, surfaced by the scan at its end since
    /// the layer has no caller to return it to
    pub failure: Arc<Mutex<Option<io::Error>>>,
}

impl LogScope {
    fn append(&self, name: &str, bytes: &[u8]) {
        if let Err(e) = scan_dir::append(&scan_dir::scan_logs_dir(&self.object_dir), name, bytes) {
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

impl<S> Layer<S> for RecordsLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, _attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        if let Ok(scope) = LOG_SCOPE.try_with(|s| s.clone())
            && let Some(span) = ctx.span(id)
        {
            span.extensions_mut().insert(scope);
        }
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        if event.metadata().target() == LOG_TARGET {
            return;
        }
        let Some(scope) = log_scope(event, &ctx) else {
            return;
        };
        let mut message = MessageVisitor::default();
        event.record(&mut message);
        let meta = event.metadata();
        let mut line = format!(
            "{} {} {}: {}",
            ms_to_iso8601(now_ms()),
            meta.level(),
            meta.target(),
            message.text
        );
        if let Some((scanner, task)) = &scope.task {
            write!(line, " task={scanner}:{task}").unwrap();
        }
        line.push('\n');
        scope.append(scan_dir::RECORDS_LOG, line.as_bytes());
    }
}

/// The scope `event` is recorded under: the task-local scope where
/// it is raised, or the scope the nearest enclosing span was created
/// in.
fn log_scope<S>(event: &Event<'_>, ctx: &Context<'_, S>) -> Option<LogScope>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    if let Ok(scope) = LOG_SCOPE.try_with(|s| s.clone()) {
        return Some(scope);
    }
    ctx.event_span(event)?
        .scope()
        .find_map(|span| span.extensions().get::<LogScope>().cloned())
}

/// Collects an event's `message` field, then any other fields as
/// `key=value` pairs.
#[derive(Default)]
pub(crate) struct MessageVisitor {
    pub(crate) text: String,
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
                scope.append(scan_dir::ERR_LOG, text.as_bytes());
            }
            previous(info);
        }));
    });
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tracing_subscriber::layer::SubscriberExt;

    use super::*;

    /// An event raised outside any log scope is recorded when it
    /// runs under a span created inside one, and dropped otherwise.
    #[test]
    fn events_under_a_scoped_span_are_recorded_outside_the_scope() {
        let dir = tempfile::tempdir().unwrap();
        let object_dir = dir.path().join("scan");
        let scope = LogScope {
            object_dir: object_dir.clone(),
            task: Some(("s".into(), "t".into())),
            failure: Arc::new(Mutex::new(None)),
        };
        let subscriber = tracing_subscriber::registry().with(layer());
        tracing::subscriber::with_default(subscriber, || {
            let span = LOG_SCOPE.sync_scope(scope.clone(), || tracing::info_span!("task"));
            tracing::info!("dropped: no scope");
            span.in_scope(|| tracing::info!("recorded: under the span"));
            let child = span.in_scope(|| tracing::info_span!("tool"));
            child.in_scope(|| tracing::info!("recorded: under a child span"));
        });
        let records =
            fs::read_to_string(scan_dir::scan_logs_dir(&object_dir).join(scan_dir::RECORDS_LOG))
                .unwrap();
        let lines: Vec<&str> = records.lines().collect();
        assert_eq!(lines.len(), 2, "{records}");
        assert!(
            lines[0].ends_with("INFO gage_scan2::trace::tests: recorded: under the span task=s:t"),
            "{records}"
        );
        assert!(
            lines[1].ends_with("recorded: under a child span task=s:t"),
            "{records}"
        );
        assert!(scope.failure.lock().unwrap().is_none());
    }
}
