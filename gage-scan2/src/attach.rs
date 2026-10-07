//! The attach phase: run the attach tasks of compiled scanners against
//! a dataset, in preparation for a scan by those scanners.
//!
//! An attach task is a task declaring `attaches`, the attachment
//! names it adds. It runs under an [`AttachContext`] and an output
//! sink, on a fresh VM, in scanner order then task order; a scan never
//! runs it. Everything a task prints or logs and every attachment it
//! writes reaches the caller as an [`AttachEvent`] as it happens. A
//! task that writes a name it did not declare, or declares one it did
//! not write, is reported as a warning. The first task that fails ends
//! the phase: later tasks do not run, and the attachments linked
//! before the failure stay linked.

use std::collections::BTreeSet;
use std::fmt;
use std::path::Path;
use std::sync::Arc;

use gage_registry::scanner::{TaskDef, TaskKind};
use gage_runtime2::{
    ATTACH_CTX, AttachContext, Attached, OUTPUT_SINK, OutputSink, ScanDatasetRef, TaskOutput,
};
use gage_session::Driver;
use gage_store::{DatasetStore, Store, StoreError};
use tokio::sync::mpsc;

use crate::{CompiledScanner, execute};

/// Something the attach phase reports as it runs.
#[derive(Debug, PartialEq, Eq)]
pub enum AttachEvent {
    /// An attach task is about to run
    Started { scanner: String, task: String },
    /// A line the task printed or logged
    Output(TaskOutput),
    /// An attachment the task wrote
    Attached { scanner: String, attached: Attached },
    /// The task's `attaches` and what it wrote disagree
    Warning {
        scanner: String,
        task: String,
        message: String,
    },
}

/// A failure of the attach phase.
#[derive(Debug)]
pub enum AttachError {
    /// The dataset could not be read or the store could not be opened
    Store(StoreError),
    /// An attach task returned an error or faulted; the message is the
    /// rendered diagnostic
    Task {
        scanner: String,
        task: String,
        message: String,
    },
}

impl fmt::Display for AttachError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AttachError::Store(e) => write!(f, "{e}"),
            AttachError::Task {
                scanner,
                task,
                message,
            } => write!(f, "task {scanner}:{task} failed\n{message}"),
        }
    }
}

impl std::error::Error for AttachError {}

impl From<StoreError> for AttachError {
    fn from(e: StoreError) -> Self {
        AttachError::Store(e)
    }
}

/// Run every attach task of `scanners` against the dataset
/// `dataset_id`, which is read at its current commit. `driver` reopens
/// native sources for `session.native()`.
pub async fn attach(
    store: &Store,
    dataset_id: &str,
    scanners: &[CompiledScanner],
    driver: Arc<dyn Driver>,
    mut on_event: impl FnMut(AttachEvent),
) -> Result<(), AttachError> {
    let record = DatasetStore::from(store).get(dataset_id)?;
    let dataset = ScanDatasetRef {
        id: record.id,
        commit_sha: record.commit_sha,
    };
    for scanner in scanners {
        for task in scanner.attach_tasks() {
            on_event(AttachEvent::Started {
                scanner: scanner.name.clone(),
                task: task.name.clone(),
            });
            run_one(
                store.path(),
                &dataset,
                scanner,
                task,
                Arc::clone(&driver),
                &mut on_event,
            )
            .await?;
        }
    }
    Ok(())
}

impl CompiledScanner {
    /// The scanner's attach tasks, in task order.
    pub(crate) fn attach_tasks(&self) -> impl Iterator<Item = &TaskDef> {
        self.tasks.values().filter(|t| t.kind() == TaskKind::Attach)
    }
}

/// Run one attach task, delivering its output and the attachments it
/// writes as they arrive, then compare what it wrote with what it
/// declared.
async fn run_one(
    store_path: &Path,
    dataset: &ScanDatasetRef,
    scanner: &CompiledScanner,
    task: &TaskDef,
    driver: Arc<dyn Driver>,
    on_event: &mut impl FnMut(AttachEvent),
) -> Result<(), AttachError> {
    let (attached_tx, mut attached_rx) = mpsc::unbounded_channel();
    let (output_tx, mut output_rx) = mpsc::unbounded_channel();
    let ctx = AttachContext::new(
        dataset.clone(),
        scanner.name.clone(),
        store_path,
        attached_tx,
        driver,
    )?;
    let sink = OutputSink {
        scanner: scanner.name.clone(),
        task: task.name.clone(),
        tx: output_tx,
    };
    let unit = scanner.task_unit();
    let call = task.call.clone();
    let run = ATTACH_CTX.scope(
        ctx,
        OUTPUT_SINK.scope(sink, async move { execute(&unit, &call).await }),
    );
    let mut run = std::pin::pin!(run);
    let mut written: BTreeSet<String> = BTreeSet::new();
    let result = loop {
        tokio::select! {
            biased;
            Some(output) = output_rx.recv() => on_event(AttachEvent::Output(output)),
            Some(attached) = attached_rx.recv() => {
                deliver_attached(&scanner.name, attached, &mut written, on_event)
            }
            result = &mut run => break result,
        }
    };
    // The task has returned; deliver what it sent before it did
    while let Ok(output) = output_rx.try_recv() {
        on_event(AttachEvent::Output(output));
    }
    while let Ok(attached) = attached_rx.try_recv() {
        deliver_attached(&scanner.name, attached, &mut written, on_event);
    }
    result.map_err(|message| AttachError::Task {
        scanner: scanner.name.clone(),
        task: task.name.clone(),
        message,
    })?;

    let declared: BTreeSet<&str> = task.attaches.iter().map(String::as_str).collect();
    for name in written.iter().filter(|n| !declared.contains(n.as_str())) {
        on_event(AttachEvent::Warning {
            scanner: scanner.name.clone(),
            task: task.name.clone(),
            message: format!("attached '{name}', which it does not declare in `attaches`"),
        });
    }
    for name in declared.iter().filter(|n| !written.contains(**n)) {
        on_event(AttachEvent::Warning {
            scanner: scanner.name.clone(),
            task: task.name.clone(),
            message: format!("declares '{name}' in `attaches` but attached nothing by that name"),
        });
    }
    Ok(())
}

/// Report one written attachment and record its name for the
/// declaration check.
fn deliver_attached(
    scanner: &str,
    attached: Attached,
    written: &mut BTreeSet<String>,
    on_event: &mut impl FnMut(AttachEvent),
) {
    if let Some(name) = &attached.name {
        written.insert(name.clone());
    }
    on_event(AttachEvent::Attached {
        scanner: scanner.to_string(),
        attached,
    });
}
