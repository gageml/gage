//! `gage scan` progress UI built on `indicatif::MultiProgress`.
//!
//! A single summary progress bar tracks 0 → total. Scanner
//! `print`/`println` events route through `MultiProgress::println` so
//! output appears above the bar without corrupting it. `Print` lines
//! (no terminating newline from the scanner) buffer until a `\n`
//! arrives.

use std::time::Duration;

use console::style;
use gage_scan::event::ScanEvent;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

pub struct ProgressUi {
    multi: MultiProgress,
    sessions: ProgressBar,
    summary: ProgressBar,
    print_buf: String,
}

impl ProgressUi {
    pub fn new(total_sessions: usize) -> Self {
        let multi = MultiProgress::new();

        let sessions = multi.add(ProgressBar::new(total_sessions as u64));
        sessions.set_style(
            ProgressStyle::with_template(
                "{spinner:.cyan} {msg} {bar:30.white/bright.black} ({pos}/{len})",
            )
            .unwrap()
            .progress_chars("▬▬"),
        );
        sessions.set_message("Session cache");
        sessions.enable_steady_tick(Duration::from_millis(120));

        let summary = multi.add(ProgressBar::new(0));
        summary.set_style(
            ProgressStyle::with_template(
                "{spinner:.magenta} {msg} [{elapsed_precise}] {bar:30.white/bright.black} ({pos}/{len})",
            )
            .unwrap()
            .progress_chars("▬▬"),
        );
        summary.set_message("Tasks");
        summary.enable_steady_tick(Duration::from_millis(120));

        Self {
            multi,
            sessions,
            summary,
            print_buf: String::new(),
        }
    }

    /// Handle to the "Sessions read" bar. The bar is removed when it
    /// reaches its length; further sets after that are no-ops on a
    /// finished bar (cheap, safe to call from a polling task).
    pub fn sessions_bar(&self) -> ProgressBar {
        self.sessions.clone()
    }

    pub fn handle(&mut self, event: ScanEvent) {
        match event {
            ScanEvent::Status(status) => {
                self.summary.set_length(status.total as u64);
                self.summary.set_position(status.progress as u64);
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
        self.sessions.finish_and_clear();
        self.summary.finish_and_clear();
        #[allow(clippy::let_underscore_must_use)]
        let _ = self.multi.clear();
    }
}
