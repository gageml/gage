//! The attach phase: run the attachment functions of compiled
//! scanners against a dataset, in preparation for a scan by those
//! scanners.
//!
//! Each function runs under an [`AttachContext`] and an output sink,
//! on a fresh VM, in scanner order then key order. Everything a
//! function prints or logs and every attachment it writes reaches the
//! caller as an [`AttachEvent`] as it happens. The first function
//! that fails ends the phase: later functions do not run, and the
//! attachments linked before the failure stay linked.

use std::fmt;
use std::path::Path;

use gage_runtime2::{
    ATTACH_CTX, AttachContext, Attached, OUTPUT_SINK, OutputSink, ScanDatasetRef, TaskOutput,
};
use gage_store::{DatasetStore, Store, StoreError};
use tokio::sync::mpsc;

use crate::{CompiledScanner, execute};

/// Something the attach phase reports as it runs.
#[derive(Debug, PartialEq, Eq)]
pub enum AttachEvent {
    /// An attachment function is about to run
    Started { scanner: String, key: String },
    /// A line the function printed or logged, with the function named
    /// as the task
    Output(TaskOutput),
    /// An attachment the function wrote
    Attached { scanner: String, attached: Attached },
}

/// A failure of the attach phase.
#[derive(Debug)]
pub enum AttachError {
    /// The dataset could not be read or the store could not be opened
    Store(StoreError),
    /// An attachment function returned an error or faulted; the
    /// message is the rendered diagnostic
    Function {
        scanner: String,
        key: String,
        message: String,
    },
}

impl fmt::Display for AttachError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AttachError::Store(e) => write!(f, "{e}"),
            AttachError::Function {
                scanner,
                key,
                message,
            } => write!(f, "attachment {scanner}:{key} failed\n{message}"),
        }
    }
}

impl std::error::Error for AttachError {}

impl From<StoreError> for AttachError {
    fn from(e: StoreError) -> Self {
        AttachError::Store(e)
    }
}

/// Run every attachment function of `scanners` against the dataset
/// `dataset_id`, which is read at its current commit.
pub async fn attach(
    store: &Store,
    dataset_id: &str,
    scanners: &[CompiledScanner],
    mut on_event: impl FnMut(AttachEvent),
) -> Result<(), AttachError> {
    let record = DatasetStore::from(store).get(dataset_id)?;
    let dataset = ScanDatasetRef {
        id: record.id,
        commit_sha: record.commit_sha,
    };
    for scanner in scanners {
        for (key, function) in &scanner.attachments {
            on_event(AttachEvent::Started {
                scanner: scanner.name.clone(),
                key: key.clone(),
            });
            run_one(
                store.path(),
                &dataset,
                scanner,
                key,
                function,
                &mut on_event,
            )
            .await?;
        }
    }
    Ok(())
}

/// Run one attachment function, delivering its output and the
/// attachments it writes as they arrive.
async fn run_one(
    store_path: &Path,
    dataset: &ScanDatasetRef,
    scanner: &CompiledScanner,
    key: &str,
    function: &str,
    on_event: &mut impl FnMut(AttachEvent),
) -> Result<(), AttachError> {
    let (attached_tx, mut attached_rx) = mpsc::unbounded_channel();
    let (output_tx, mut output_rx) = mpsc::unbounded_channel();
    let ctx = AttachContext::new(
        dataset.clone(),
        scanner.name.clone(),
        store_path,
        attached_tx,
    )?;
    let sink = OutputSink {
        scanner: scanner.name.clone(),
        task: key.to_string(),
        tx: output_tx,
    };
    let unit = scanner.task_unit();
    let function = function.to_string();
    let run = ATTACH_CTX.scope(
        ctx,
        OUTPUT_SINK.scope(sink, async move { execute(&unit, &function).await }),
    );
    let mut run = std::pin::pin!(run);
    let result = loop {
        tokio::select! {
            biased;
            Some(output) = output_rx.recv() => on_event(AttachEvent::Output(output)),
            Some(attached) = attached_rx.recv() => on_event(AttachEvent::Attached {
                scanner: scanner.name.clone(),
                attached,
            }),
            result = &mut run => break result,
        }
    };
    // The function has returned; deliver what it sent before it did
    while let Ok(output) = output_rx.try_recv() {
        on_event(AttachEvent::Output(output));
    }
    while let Ok(attached) = attached_rx.try_recv() {
        on_event(AttachEvent::Attached {
            scanner: scanner.name.clone(),
            attached,
        });
    }
    result.map_err(|message| AttachError::Function {
        scanner: scanner.name.clone(),
        key: key.to_string(),
        message,
    })
}
