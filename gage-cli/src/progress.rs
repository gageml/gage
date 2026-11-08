//! The progress indicator: one row on standard error that shows what
//! a command is doing while it does it.
//!
//! The data crates report work as tracing spans on
//! `gage_session::progress::TARGET`. [`layer`] subscribes to that
//! target and drives the row: a span opening starts it, each `done`
//! record moves it, and the span closing ends it. A command that runs
//! work which reports nothing wraps it in [`busy`] or [`busy_async`],
//! which shows the row with a label of the command's choosing until
//! the work returns. The row is a spinner and a label; once a span has
//! run for [`BAR_DELAY`] it gains a short bar and a count, so work that
//! finishes quickly never flashes a bar.
//!
//! Log lines share standard error with the row. [`LogWriter`] is the
//! writer for the log layer: it takes the row down, writes the line,
//! and puts the row back, so a warning raised mid-work lands above the
//! row instead of over it.

use std::future::Future;
use std::io::{self, Write};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use cliclack as cli;
use gage_session::progress;
use tracing::span::{Attributes, Id, Record};
use tracing::{Subscriber, field};
use tracing_subscriber::Layer;
use tracing_subscriber::filter::filter_fn;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;

use crate::style::INDICATOR_TEMPLATE;

/// How long a span runs before the row shows its bar.
const BAR_DELAY: Duration = Duration::from_millis(200);

/// The subscriber layer that drives the process's indicator row from
/// the progress spans the data crates emit.
pub fn layer<S>() -> impl Layer<S>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    ProgressLayer {
        indicator: Arc::clone(&INDICATOR),
    }
    .with_filter(filter_fn(|meta| meta.target() == progress::TARGET))
}

/// Show the row with `label` while `f` runs. Work under `f` that
/// reports progress takes the row over for its duration.
pub fn busy<T>(label: &str, f: impl FnOnce() -> T) -> T {
    let _busy = Busy::begin(label);
    f()
}

/// As [`busy`], over a future.
pub async fn busy_async<T>(label: &str, f: impl Future<Output = T>) -> T {
    let _busy = Busy::begin(label);
    f.await
}

/// The writer for the log layer: a line is held until the event is
/// written, then printed above the indicator row.
pub struct LogWriter;

impl<'a> MakeWriter<'a> for LogWriter {
    type Writer = LogLine;

    fn make_writer(&'a self) -> LogLine {
        LogLine::default()
    }
}

#[derive(Default)]
pub struct LogLine(Vec<u8>);

impl Write for LogLine {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for LogLine {
    fn drop(&mut self) {
        if !self.0.is_empty() {
            lock(&INDICATOR).print(&self.0);
        }
    }
}

static INDICATOR: LazyLock<Arc<Mutex<Indicator>>> =
    LazyLock::new(|| Arc::new(Mutex::new(Indicator::new(true))));

fn lock(indicator: &Mutex<Indicator>) -> MutexGuard<'_, Indicator> {
    indicator.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A busy label on the indicator for the lifetime of the value.
struct Busy {
    token: u64,
}

impl Busy {
    fn begin(label: &str) -> Self {
        let token = lock(&INDICATOR).push_busy(label);
        Self { token }
    }
}

impl Drop for Busy {
    fn drop(&mut self) {
        lock(&INDICATOR).pop_busy(self.token);
    }
}

struct ProgressLayer {
    indicator: Arc<Mutex<Indicator>>,
}

impl<S> Layer<S> for ProgressLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, _ctx: Context<'_, S>) {
        let mut fields = Fields::default();
        attrs.record(&mut fields);
        lock(&self.indicator).begin(
            id.into_u64(),
            attrs.metadata().name(),
            fields.total.unwrap_or(0),
        );
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, _ctx: Context<'_, S>) {
        let mut fields = Fields::default();
        values.record(&mut fields);
        lock(&self.indicator).update(id.into_u64(), fields.done, fields.total);
    }

    fn on_close(&self, id: Id, _ctx: Context<'_, S>) {
        lock(&self.indicator).end(id.into_u64());
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
            progress::TOTAL => self.total = Some(value),
            progress::DONE => self.done = Some(value),
            _ => {}
        }
    }

    fn record_debug(&mut self, _field: &field::Field, _value: &dyn std::fmt::Debug) {}
}

/// The indicator: what is in progress, and the row showing it.
struct Indicator {
    tracker: Tracker,
    row: Option<Row>,
    /// Whether the row is drawn; tests track without drawing.
    render: bool,
}

impl Indicator {
    fn new(render: bool) -> Self {
        Self {
            tracker: Tracker::default(),
            row: None,
            render,
        }
    }

    fn begin(&mut self, id: u64, work: &'static str, total: u64) {
        self.tracker.begin(id, work, total, Instant::now());
        self.refresh();
    }

    fn update(&mut self, id: u64, done: Option<u64>, total: Option<u64>) {
        self.tracker.update(id, done, total);
        self.refresh();
    }

    fn end(&mut self, id: u64) {
        self.tracker.end(id);
        self.refresh();
    }

    fn push_busy(&mut self, label: &str) -> u64 {
        let token = self.tracker.push_busy(label);
        self.refresh();
        token
    }

    fn pop_busy(&mut self, token: u64) {
        self.tracker.pop_busy(token);
        self.refresh();
    }

    /// Write a log line above the row: the row comes down, the line
    /// goes out, and the row is drawn again from the tracked state.
    fn print(&mut self, line: &[u8]) {
        if let Some(row) = self.row.take() {
            row.clear();
        }
        // Standard error is the last channel there is; a failure to
        // write it has nowhere further to go.
        drop(io::stderr().write_all(line));
        self.refresh();
    }

    fn refresh(&mut self) {
        if !self.render {
            return;
        }
        match self.tracker.view(Instant::now()) {
            View::Idle => {
                if let Some(row) = self.row.take() {
                    row.clear();
                }
            }
            View::Spinner(label) => self.row_mut(&label).set_spinner(&label),
            View::Bar { label, done, total } => self.row_mut(&label).set_bar(&label, done, total),
        }
    }

    fn row_mut(&mut self, label: &str) -> &mut Row {
        self.row.get_or_insert_with(|| Row::new(label))
    }
}

/// What is in progress: the open spans, oldest first, and the busy
/// labels, innermost last.
#[derive(Default)]
struct Tracker {
    tasks: Vec<Task>,
    busy: Vec<(u64, String)>,
    next_token: u64,
}

struct Task {
    id: u64,
    work: &'static str,
    total: u64,
    done: u64,
    started: Instant,
}

/// What the row shows.
#[derive(Debug, PartialEq, Eq)]
enum View {
    Idle,
    Spinner(String),
    Bar {
        label: String,
        done: u64,
        total: u64,
    },
}

impl Tracker {
    fn begin(&mut self, id: u64, work: &'static str, total: u64, now: Instant) {
        self.tasks.push(Task {
            id,
            work,
            total,
            done: 0,
            started: now,
        });
    }

    fn update(&mut self, id: u64, done: Option<u64>, total: Option<u64>) {
        if let Some(task) = self.tasks.iter_mut().find(|t| t.id == id) {
            if let Some(done) = done {
                task.done = done;
            }
            if let Some(total) = total {
                task.total = total;
            }
        }
    }

    fn end(&mut self, id: u64) {
        self.tasks.retain(|t| t.id != id);
    }

    fn push_busy(&mut self, label: &str) -> u64 {
        let token = self.next_token;
        self.next_token += 1;
        self.busy.push((token, label.to_string()));
        token
    }

    fn pop_busy(&mut self, token: u64) {
        self.busy.retain(|(t, _)| *t != token);
    }

    /// The oldest open span's work, summed over every open span of
    /// that work; with none open, the innermost busy label.
    fn view(&self, now: Instant) -> View {
        let Some(first) = self.tasks.first() else {
            return match self.busy.last() {
                Some((_, label)) => View::Spinner(label.clone()),
                None => View::Idle,
            };
        };
        let same = self.tasks.iter().filter(|t| t.work == first.work);
        let total: u64 = same.clone().map(|t| t.total).sum();
        let done: u64 = same.map(|t| t.done).sum();
        let label = label(first.work);
        if total > 0 && now.duration_since(first.started) >= BAR_DELAY {
            View::Bar { label, done, total }
        } else {
            View::Spinner(label)
        }
    }
}

/// What the user reads for each kind of work the data crates report.
fn label(work: &str) -> String {
    match work {
        progress::READ_SESSIONS => "Reading sessions",
        progress::INDEX_TEXT => "Indexing sessions",
        other => other,
    }
    .to_string()
}

/// The one drawn row: a dialog-themed spinner with a label, or the
/// same with a bar and count.
struct Row {
    bar: cli::ProgressBar,
    mode: Mode,
    label: String,
}

#[derive(PartialEq, Eq)]
enum Mode {
    Spinner,
    Bar,
}

impl Row {
    /// A started spinner showing `label`. The row is started at once
    /// so that every later change draws with the theme's style; a bar
    /// whose count changes before it is started draws in indicatif's
    /// default style.
    fn new(label: &str) -> Self {
        let bar = cli::spinner();
        bar.start(label);
        Self {
            bar,
            mode: Mode::Spinner,
            label: label.to_string(),
        }
    }

    fn set_spinner(&mut self, label: &str) {
        if self.mode != Mode::Spinner {
            self.bar = self.bar.clone().with_spinner_template();
            self.mode = Mode::Spinner;
            self.restart(label);
        } else if self.label != label {
            self.set_label(label);
        }
    }

    fn set_bar(&mut self, label: &str, done: u64, total: u64) {
        // The count is set before the template is applied, since
        // applying it draws a frame.
        self.bar.set_length(total);
        self.bar.set_position(done);
        if self.mode != Mode::Bar {
            self.bar = self.bar.clone().with_template(INDICATOR_TEMPLATE);
            self.mode = Mode::Bar;
            self.restart(label);
        } else if self.label != label {
            self.set_label(label);
        }
    }

    /// Apply the current template and show `label`. `start` is how
    /// cliclack applies a template; it is repeatable.
    fn restart(&mut self, label: &str) {
        self.bar.start(label);
        self.label = label.to_string();
    }

    fn set_label(&mut self, label: &str) {
        self.bar.set_message(label);
        self.label = label.to_string();
    }

    fn clear(self) {
        self.bar.clear();
    }
}

#[cfg(test)]
mod tests {
    use tracing_subscriber::layer::SubscriberExt;

    use super::*;

    #[test]
    fn a_span_shows_a_spinner_then_a_bar_after_the_delay() {
        let t0 = Instant::now();
        let mut tracker = Tracker::default();
        tracker.begin(1, progress::READ_SESSIONS, 4, t0);
        assert_eq!(tracker.view(t0), View::Spinner("Reading sessions".into()));
        tracker.update(1, Some(1), None);
        assert_eq!(
            tracker.view(t0 + BAR_DELAY),
            View::Bar {
                label: "Reading sessions".into(),
                done: 1,
                total: 4
            }
        );
        tracker.end(1);
        assert_eq!(tracker.view(t0 + BAR_DELAY), View::Idle);
    }

    #[test]
    fn open_spans_of_one_work_are_summed() {
        let t0 = Instant::now();
        let mut tracker = Tracker::default();
        tracker.begin(1, progress::INDEX_TEXT, 10, t0);
        tracker.begin(2, progress::INDEX_TEXT, 5, t0);
        tracker.update(1, Some(3), None);
        tracker.update(2, Some(5), None);
        assert_eq!(
            tracker.view(t0 + BAR_DELAY),
            View::Bar {
                label: "Indexing sessions".into(),
                done: 8,
                total: 15
            }
        );
        tracker.end(2);
        assert_eq!(
            tracker.view(t0 + BAR_DELAY),
            View::Bar {
                label: "Indexing sessions".into(),
                done: 3,
                total: 10
            }
        );
    }

    #[test]
    fn busy_labels_show_when_no_span_is_open_and_nest() {
        let t0 = Instant::now();
        let mut tracker = Tracker::default();
        let outer = tracker.push_busy("Outer");
        let inner = tracker.push_busy("Inner");
        assert_eq!(tracker.view(t0), View::Spinner("Inner".into()));
        tracker.begin(1, progress::READ_SESSIONS, 2, t0);
        assert_eq!(tracker.view(t0), View::Spinner("Reading sessions".into()));
        tracker.end(1);
        tracker.pop_busy(inner);
        assert_eq!(tracker.view(t0), View::Spinner("Outer".into()));
        tracker.pop_busy(outer);
        assert_eq!(tracker.view(t0), View::Idle);
    }

    #[test]
    fn the_layer_tracks_a_progress_span_through_its_life() {
        let indicator = Arc::new(Mutex::new(Indicator::new(false)));
        let layer = ProgressLayer {
            indicator: Arc::clone(&indicator),
        }
        .with_filter(filter_fn(|meta| meta.target() == progress::TARGET));
        let subscriber = tracing_subscriber::registry().with(layer);
        tracing::subscriber::with_default(subscriber, || {
            let span = progress::read_sessions(3);
            progress::advance(&span, 2);
            {
                let tracker = &lock(&indicator).tracker;
                let task = tracker.tasks.first().expect("the open span is tracked");
                assert_eq!(
                    (task.work, task.total, task.done),
                    (progress::READ_SESSIONS, 3, 2)
                );
            }
            // A span of another target is not tracked.
            let other = tracing::info_span!("other", total = 9u64);
            assert_eq!(lock(&indicator).tracker.tasks.len(), 1);
            drop(other);
            drop(span);
            assert!(lock(&indicator).tracker.tasks.is_empty());
        });
    }
}
