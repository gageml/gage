//! Progress reporting for long-running work in the data crates.
//!
//! A unit of work is a tracing span on [`TARGET`]. The span's name is
//! the kind of work, its `total` field is the number of steps, and
//! [`advance`] records the `done` field as steps complete. Closing the
//! span ends the work. A consumer that renders progress subscribes to
//! the target and maps each name to what the user sees; the data
//! crates carry no rendering and no label text. A process without a
//! consumer pays the disabled-span check and nothing more. Work with
//! no steps returns a disabled span, so a consumer never sees an
//! empty bar.

use tracing::field::Empty;
use tracing::{Level, Span};

/// The tracing target every progress span carries.
pub const TARGET: &str = "gage::progress";

/// Deriving session summaries into a driver's cache.
pub const READ_SESSIONS: &str = "read_sessions";

/// Bringing a text index up to date with its corpus.
pub const INDEX_TEXT: &str = "index_text";

/// The span field holding the number of steps.
pub const TOTAL: &str = "total";

/// The span field recorded with the number of steps completed.
pub const DONE: &str = "done";

/// Begin deriving `total` session summaries.
pub fn read_sessions(total: usize) -> Span {
    if total == 0 {
        return Span::none();
    }
    tracing::span!(
        target: TARGET,
        Level::INFO,
        READ_SESSIONS,
        total = total as u64,
        done = Empty
    )
}

/// Begin indexing `total` sessions into a text index.
pub fn index_text(total: usize) -> Span {
    if total == 0 {
        return Span::none();
    }
    tracing::span!(
        target: TARGET,
        Level::INFO,
        INDEX_TEXT,
        total = total as u64,
        done = Empty
    )
}

/// Record that `done` steps of `span` are complete.
pub fn advance(span: &Span, done: usize) {
    span.record(DONE, done as u64);
}

/// A subscriber layer that records every progress report, for tests
/// of the code that emits them.
#[cfg(feature = "test-util")]
pub mod recorder {
    use std::sync::{Arc, Mutex};

    use tracing::span::{Attributes, Id, Record};
    use tracing::{Subscriber, field};
    use tracing_subscriber::Layer;
    use tracing_subscriber::layer::{Context, SubscriberExt};
    use tracing_subscriber::registry::LookupSpan;

    /// One report: the work's name, steps done, and steps in total.
    pub type Report = (&'static str, u64, u64);

    /// Run `f` under a subscriber that records every progress report,
    /// and return the reports in order. A span's opening reports zero
    /// steps done.
    pub fn record(f: impl FnOnce()) -> Vec<Report> {
        let reports = Arc::new(Mutex::new(Vec::new()));
        let layer = Recorder {
            reports: Arc::clone(&reports),
        };
        let subscriber = tracing_subscriber::registry().with(layer);
        tracing::subscriber::with_default(subscriber, f);
        Arc::try_unwrap(reports)
            .expect("the subscriber holding the reports is dropped above")
            .into_inner()
            .unwrap()
    }

    struct Recorder {
        reports: Arc<Mutex<Vec<Report>>>,
    }

    struct Total(u64);

    impl<S> Layer<S> for Recorder
    where
        S: Subscriber + for<'a> LookupSpan<'a>,
    {
        fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
            if attrs.metadata().target() != super::TARGET {
                return;
            }
            let mut fields = Fields::default();
            attrs.record(&mut fields);
            let total = fields.total.unwrap_or(0);
            let span = ctx.span(id).expect("a new span is in the registry");
            span.extensions_mut().insert(Total(total));
            self.reports
                .lock()
                .unwrap()
                .push((attrs.metadata().name(), 0, total));
        }

        fn on_record(&self, id: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
            let span = ctx.span(id).expect("a recorded span is in the registry");
            let Some(total) = span.extensions().get::<Total>().map(|t| t.0) else {
                return;
            };
            let mut fields = Fields::default();
            values.record(&mut fields);
            if let Some(done) = fields.done {
                self.reports
                    .lock()
                    .unwrap()
                    .push((span.metadata().name(), done, total));
            }
        }
    }

    #[derive(Default)]
    struct Fields {
        total: Option<u64>,
        done: Option<u64>,
    }

    impl field::Visit for Fields {
        fn record_u64(&mut self, field: &field::Field, value: u64) {
            match field.name() {
                super::TOTAL => self.total = Some(value),
                super::DONE => self.done = Some(value),
                _ => {}
            }
        }

        fn record_debug(&mut self, _field: &field::Field, _value: &dyn std::fmt::Debug) {}
    }
}
