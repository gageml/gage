//! The attach phase as the CLI runs it: the attach tasks of compiled
//! scanners against a dataset, with each outcome printed as it
//! happens. Shared by `gage attachment add --scanner` and the dataset
//! `gage scan2` builds for a run.

use std::sync::Arc;

use gage_core::uuid::short_uuid;
use gage_runtime2::{Output, TaskOutput};
use gage_scan2::CompiledScanner;
use gage_scan2::attach::{AttachError, AttachEvent, attach};
use gage_session::Driver;
use gage_store::Store;

/// Run every attach task of `scanners` against the dataset
/// `dataset_id`, printing what the tasks print, one line per
/// attachment they write, and each declaration warning on stderr. The
/// first failing task ends the phase with its error; attachments
/// linked before it stay linked.
pub(crate) async fn attach_for_scanners(
    store: &Store,
    dataset_id: &str,
    scanners: &[CompiledScanner],
    driver: Arc<dyn Driver>,
) -> Result<(), AttachError> {
    let dataset = short_uuid(dataset_id).to_string();
    attach(store, dataset_id, scanners, driver, |event| match event {
        AttachEvent::Started { .. } => {}
        AttachEvent::Warning {
            scanner,
            task,
            message,
        } => eprintln!("warning: task {scanner}:{task} {message}"),
        AttachEvent::Output(TaskOutput { output, .. }) => match output {
            Output::Print(s) => print!("{s}"),
            Output::Println(s) => println!("{s}"),
            Output::Log { level, message } => eprintln!("{}: {message}", level.as_str()),
            Output::Progress { .. } | Output::AgentPool(_) => {}
        },
        AttachEvent::Attached { attached, .. } => {
            let verb = match attached.outcome.as_str() {
                "added" => "Added",
                "updated" => "Updated",
                _ => "Unchanged",
            };
            println!(
                "{verb} attachment {} to dataset {dataset}",
                describe(attached.name.as_deref(), &attached.id)
            );
        }
    })
    .await
}

/// The attachment as messages show it: `name (short id)` when named,
/// the short id alone otherwise
pub(crate) fn describe(name: Option<&str>, id: &str) -> String {
    match name {
        Some(name) => format!("{name} ({})", short_uuid(id)),
        None => short_uuid(id).to_string(),
    }
}
