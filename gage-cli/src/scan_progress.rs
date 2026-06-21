//! `gage scan` progress UI built on `indicatif::MultiProgress`.
//!
//! Bars are created lazily so the UI only shows what's actually moving:
//!
//! - An "Initializing" spinner appears immediately and is removed when
//!   the first `Status` event arrives (i.e. the plan is built).
//! - The "Tasks" bar is added on the first `Status` with `total > 0`.
//! - The "Session cache" bar is added the first time the polled cached
//!   session count is nonzero.
//!
//! Scanner `print`/`println` events route through `MultiProgress::println`
//! so output appears above any active bars without corrupting them.
//! `Print` lines (no terminating newline from the scanner) buffer until
//! a `\n` arrives.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use console::style;
use gage_scan::event::ScanEvent;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

pub struct ProgressUi {
    multi: MultiProgress,
    total_sessions: u64,
    sessions: Arc<Mutex<Option<ProgressBar>>>,
    init: Option<ProgressBar>,
    summary: Option<ProgressBar>,
    print_buf: String,
}

impl ProgressUi {
    pub fn new(total_sessions: usize) -> Self {
        let multi = MultiProgress::new();

        let init = multi.add(ProgressBar::new_spinner());
        init.set_style(ProgressStyle::with_template("{spinner:.magenta} {msg}").unwrap());
        init.set_message("Initializing");
        init.enable_steady_tick(Duration::from_millis(120));

        Self {
            multi,
            total_sessions: total_sessions as u64,
            sessions: Arc::new(Mutex::new(None)),
            init: Some(init),
            summary: None,
            print_buf: String::new(),
        }
    }

    /// Returns a handle the session-cache poller uses to publish counts.
    /// The "Session cache" bar is created lazily on the first nonzero
    /// set, and removed once the count reaches the total.
    pub fn sessions_setter(&self) -> SessionsSetter {
        SessionsSetter {
            multi: self.multi.clone(),
            bar: self.sessions.clone(),
            total: self.total_sessions,
        }
    }

    pub fn handle(&mut self, event: ScanEvent) {
        match event {
            ScanEvent::Status(status) => {
                self.clear_init();
                let total = status.total as u64;
                if total == 0 {
                    return;
                }
                let multi = &self.multi;
                let bar = self.summary.get_or_insert_with(|| {
                    let bar = multi.add(ProgressBar::new(total));
                    bar.set_style(
                        ProgressStyle::with_template(
                            "{spinner:.magenta} {msg} [{elapsed_precise}] {bar:30.white/bright.black} ({pos}/{len})",
                        )
                        .unwrap()
                        .progress_chars("▬▬"),
                    );
                    bar.set_message("Tasks");
                    bar.enable_steady_tick(Duration::from_millis(120));
                    bar
                });
                bar.set_length(total);
                bar.set_position(status.progress as u64);
            }
            ScanEvent::Println { s } => {
                self.flush_print_buf();
                self.println(&s);
            }
            ScanEvent::Print { s } => {
                self.print_buf.push_str(&s);
                while let Some(i) = self.print_buf.find('\n') {
                    let line: String = self.print_buf.drain(..=i).collect();
                    self.println(line.trim_end_matches('\n'));
                }
            }
            ScanEvent::TaskFailed {
                scanner,
                task,
                message,
            } => {
                self.flush_print_buf();
                self.task_failed(&scanner, &task, &message);
            }
            ScanEvent::Warning {
                scanner,
                task,
                message,
            } => {
                self.flush_print_buf();
                self.task_warning(&scanner, &task, &message);
            }
        }
    }

    fn clear_init(&mut self) {
        if let Some(init) = self.init.take() {
            init.finish_and_clear();
            self.multi.remove(&init);
        }
    }

    fn task_failed(&self, scanner: &str, task: &str, message: &str) {
        let header = format!("error: {scanner}::{task}");
        self.println(&style(header).red().bold().to_string());
        for line in message.lines() {
            self.println(&style(line).red().to_string());
        }
    }

    fn task_warning(&self, scanner: &str, task: &str, message: &str) {
        let header = format!("warning: {scanner}::{task}: {message}");
        self.println(&style(header).yellow().to_string());
    }

    fn println(&self, line: &str) {
        #[allow(clippy::let_underscore_must_use)]
        let _ = self.multi.println(line);
    }

    fn flush_print_buf(&mut self) {
        if !self.print_buf.is_empty() {
            let tail = std::mem::take(&mut self.print_buf);
            self.println(&tail);
        }
    }

    pub fn finish(mut self) {
        self.flush_print_buf();
        self.clear_init();
        if let Some(bar) = self.summary.take() {
            bar.finish_and_clear();
        }
        if let Some(bar) = self.sessions.lock().unwrap().take() {
            bar.finish_and_clear();
        }
        #[allow(clippy::let_underscore_must_use)]
        let _ = self.multi.clear();
    }
}

/// Handle to the lazily-created "Session cache" bar. Calls to `set` are
/// no-ops until `n > 0`; the bar is then inserted above the Tasks bar
/// and updated. Once `n >= total` the bar finishes and is removed; later
/// calls are no-ops.
#[derive(Clone)]
pub struct SessionsSetter {
    multi: MultiProgress,
    bar: Arc<Mutex<Option<ProgressBar>>>,
    total: u64,
}

impl SessionsSetter {
    pub fn set(&self, n: u64) {
        if n == 0 {
            return;
        }
        let mut guard = self.bar.lock().unwrap();
        if guard.is_none() {
            let bar = self.multi.insert(0, ProgressBar::new(self.total));
            bar.set_style(
                ProgressStyle::with_template(
                    "{spinner:.cyan} {msg} {bar:30.white/bright.black} ({pos}/{len})",
                )
                .unwrap()
                .progress_chars("▬▬"),
            );
            bar.set_message("Session cache");
            bar.enable_steady_tick(Duration::from_millis(120));
            *guard = Some(bar);
        }
        let bar = guard.as_ref().unwrap();
        bar.set_position(n);
        if n >= self.total {
            bar.finish_and_clear();
            self.multi.remove(bar);
            *guard = None;
        }
    }
}
