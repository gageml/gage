//! The attach phase as the CLI runs it: the attach tasks of compiled
//! scanners against a dataset, rendered inside the caller's dialog.
//! Shared by `gage attachment add --scanner` and the dataset
//! `gage scan` builds for a run.

use std::io;
use std::sync::Arc;

use cliclack as cli;
use gage_core::uuid::short_uuid;
use gage_runtime::{Level, Output, TaskOutput};
use gage_scan::CompiledScanner;
use gage_scan::attach::{AttachEvent, attach};
use gage_session::Driver;
use gage_store::Store;

use crate::dialog::{self, DialogError};

/// Run every attach task of `scanners` against the dataset
/// `dataset_id`. What the tasks print and log becomes info lines,
/// each declaration warning a warning line, and the attachments they
/// write one `Attachments` step. The first failing task ends the
/// phase with its error; attachments linked before it stay linked.
/// Returns the number of attachments the tasks placed.
pub(crate) async fn attach_for_scanners(
    store: &Store,
    dataset_id: &str,
    scanners: &[CompiledScanner],
    driver: Arc<dyn Driver>,
) -> Result<usize, DialogError> {
    let mut lines: Vec<String> = Vec::new();
    // The first terminal write failure, surfaced after the tasks end;
    // later events are not rendered
    let mut write: io::Result<()> = Ok(());
    attach(store, dataset_id, scanners, driver, |event| {
        if write.is_err() {
            return;
        }
        write = match event {
            AttachEvent::Started { .. } => Ok(()),
            AttachEvent::Warning {
                scanner,
                task,
                message,
            } => cli::log::warning(format!("task {scanner}:{task} {message}")),
            AttachEvent::Output(TaskOutput { output, .. }) => match output {
                Output::Print(s) | Output::Println(s) => cli::log::info(s.trim_end()),
                Output::Log { level, message } => match level {
                    Level::Warn | Level::Error => {
                        cli::log::warning(format!("{}: {message}", level.as_str()))
                    }
                    _ => cli::log::info(format!("{}: {message}", level.as_str())),
                },
                Output::Progress { .. } | Output::AgentPool(_) => Ok(()),
            },
            AttachEvent::Attached { attached, .. } => {
                lines.push(format!(
                    "{} {}",
                    describe(attached.name.as_deref(), &attached.id),
                    attached.outcome
                ));
                Ok(())
            }
        };
    })
    .await
    .map_err(|e| DialogError::Failed(e.to_string()))?;
    write?;
    let placed = lines.len();
    if placed > 0 {
        dialog::step_list("Attachments", lines)?;
    }
    Ok(placed)
}

/// The attachment as messages show it: `name (short id)` when named,
/// the short id alone otherwise
pub(crate) fn describe(name: Option<&str>, id: &str) -> String {
    match name {
        Some(name) => format!("{name} ({})", short_uuid(id)),
        None => short_uuid(id).to_string(),
    }
}
