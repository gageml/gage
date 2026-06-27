//! Baseline indicatif prototype: MultiProgress with overall bar plus one
//! spinner line per active worker. Per-task elapsed and stdout byte
//! counter under each worker's current task. Mirrors what
//! `gage-cli::scan_progress` does today plus per-worker visibility.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::Result;
use console::style;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use tokio::sync::mpsc;

use gage_scan::event::TaskRef;

use crate::scan::{RunSummary, Setup, run_scan};
use crate::sink::UiEvent;

pub async fn run(setup: Setup) -> Result<RunSummary> {
    let (tx, mut rx) = mpsc::unbounded_channel::<UiEvent>();

    let multi = MultiProgress::new();
    let overall = multi.add(ProgressBar::new(0));
    overall.set_style(
        ProgressStyle::with_template(
            "{spinner:.magenta} {msg} [{elapsed_precise}] {bar:30.white/bright.black} ({pos}/{len})",
        )
        .unwrap()
        .progress_chars("▬▬"),
    );
    overall.set_message("Tasks");
    overall.enable_steady_tick(Duration::from_millis(120));

    let mut workers: HashMap<usize, (ProgressBar, TaskRef, Instant, u64)> = HashMap::new();
    let mut total_bytes: u64 = 0;

    let mut total: u64 = 0;
    let mut progress: u64 = 0;

    let multi_clone = multi.clone();
    let scan_task = tokio::spawn(async move { run_scan(setup, tx).await });

    loop {
        let ev = match rx.recv().await {
            Some(ev) => ev,
            None => break,
        };
        match ev {
            UiEvent::Status(s) => {
                total = s.total as u64;
                progress = s.progress as u64;
                overall.set_length(total);
                overall.set_position(progress);

                // Reconcile per-worker bars with the snapshot.
                let mut seen = Vec::new();
                for w in &s.workers {
                    seen.push(w.id);
                    match (&w.current, workers.get_mut(&w.id)) {
                        (Some(task), Some((bar, current, _started, _bytes)))
                            if task.scanner == current.scanner && task.task == current.task =>
                        {
                            // Same task still running — update elapsed via tick.
                            update_worker_msg(bar, current, _started.elapsed(), *_bytes);
                        }
                        (Some(task), Some((bar, current, started, bytes))) => {
                            // Worker switched tasks.
                            *current = task.clone();
                            *started = Instant::now();
                            *bytes = 0;
                            update_worker_msg(bar, current, started.elapsed(), *bytes);
                        }
                        (Some(task), None) => {
                            let bar = multi_clone.add(ProgressBar::new_spinner());
                            bar.set_style(
                                ProgressStyle::with_template("  {spinner:.cyan} {msg}").unwrap(),
                            );
                            bar.enable_steady_tick(Duration::from_millis(120));
                            let started = Instant::now();
                            update_worker_msg(&bar, task, started.elapsed(), 0);
                            workers.insert(w.id, (bar, task.clone(), started, 0));
                        }
                        (None, Some(_)) => {
                            let (bar, _, _, _) = workers.remove(&w.id).unwrap();
                            bar.finish_and_clear();
                            multi_clone.remove(&bar);
                        }
                        (None, None) => {}
                    }
                }
                // Drop any workers not present in this snapshot.
                workers.retain(|id, (bar, _, _, _)| {
                    if seen.contains(id) {
                        true
                    } else {
                        bar.finish_and_clear();
                        multi_clone.remove(bar);
                        false
                    }
                });

                overall.set_message(format!(
                    "Tasks  ({} bytes from scanners)",
                    style(total_bytes).dim()
                ));
            }
            UiEvent::Bytes(n) => {
                total_bytes += n;
                // Charge to whichever worker is currently active. We
                // don't know which one — Status doesn't tell us which
                // worker emitted — so we just bump the overall counter
                // and let the per-worker bars update on the next Status.
                for (_, (_, _, _, bytes)) in workers.iter_mut() {
                    let _ = bytes;
                }
            }
            UiEvent::Log(line) => {
                let _ = multi.println(line);
            }
            UiEvent::Warning {
                scanner,
                task,
                message,
            } => {
                let _ = multi.println(
                    style(format!("warning: {scanner}::{task}: {message}"))
                        .yellow()
                        .to_string(),
                );
            }
            UiEvent::Failed {
                scanner,
                task,
                message,
            } => {
                let _ = multi.println(
                    style(format!("error: {scanner}::{task}"))
                        .red()
                        .bold()
                        .to_string(),
                );
                for line in message.lines() {
                    let _ = multi.println(style(line).red().to_string());
                }
            }
            UiEvent::Finished => break,
        }
    }

    for (_, (bar, _, _, _)) in workers.drain() {
        bar.finish_and_clear();
        multi.remove(&bar);
    }
    overall.finish_and_clear();
    let _ = multi.clear();

    let _ = (total, progress);
    scan_task.await.expect("scan task joins")
}

fn update_worker_msg(bar: &ProgressBar, task: &TaskRef, elapsed: Duration, bytes: u64) {
    let bytes_part = if bytes > 0 {
        format!(" {} B", bytes)
    } else {
        String::new()
    };
    bar.set_message(format!(
        "{}::{} {}{}",
        style(&task.scanner).cyan(),
        task.task,
        style(format!("{:.0}s", elapsed.as_secs_f64())).dim(),
        style(bytes_part).dim(),
    ));
}
