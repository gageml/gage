//! `gage attachment`: named file trees in the Gage store, for
//! scanners to read.
//!
//! An attachment is added from a directory and a pattern list, listed
//! on its own or as a dataset's, shown file by file, and removed. A
//! dataset links attachments the way it links sessions:
//! `add --dataset` stores and links in one step, `remove --dataset`
//! unlinks without touching the object.

use std::path::PathBuf;

use clap::{Args, Subcommand};
use gage_core::uuid::short_uuid;
use gage_store::{
    AttachmentLinkOutcome, AttachmentOutcome, AttachmentRecord, AttachmentSpec, AttachmentStore,
    DatasetStore, Store, StoreError,
};
use tabled::{
    Table,
    settings::{
        Alignment, Color, Style,
        object::{Columns, Object, Rows},
    },
};

use crate::human::{format_elapsed_ms, format_size};
use crate::style;

#[derive(Subcommand)]
pub enum AttachmentCommand {
    /// Add an attachment from a directory
    ///
    /// Files are selected by path globs relative to the root, as in
    /// a shell: `settings.json` is the one file at the root,
    /// `**/settings.json` is every file by that name, `*` and `?` do
    /// not cross `/`, and `{a,b}` and `[a-z]` are supported. Excludes
    /// use the same form, and a directory an exclude matches is not
    /// entered. Adding under an existing name updates the attachment
    /// when its files changed and is otherwise a no-op.
    Add(AttachmentAddArgs),

    /// List attachments
    List(AttachmentListArgs),

    /// Show an attachment and its files
    Show(AttachmentShowArgs),

    /// Remove attachments from the store or from a dataset
    ///
    /// Without --dataset, each attachment is unlinked from every
    /// dataset that holds it, then marked removed in the store. With
    /// --dataset, each attachment is unlinked from that dataset only
    /// and stays in the store.
    Remove(AttachmentRemoveArgs),
}

#[derive(Args)]
pub struct AttachmentAddArgs {
    /// Attachment name
    ///
    /// Letters, digits, `-`, `_`, and `.`. Names the attachment in
    /// every other command and in scanners
    pub name: String,

    /// Files to include (path globs relative to the root)
    ///
    /// At least one is required; `**/*` selects every file
    #[arg(required = true, value_name = "INCLUDE")]
    pub includes: Vec<String>,

    /// Files to exclude (path glob relative to the root, repeatable)
    #[arg(short, long, value_name = "PATTERN")]
    pub exclude: Vec<String>,

    /// Directory to select files under (default: current directory)
    ///
    /// Stored file paths are relative to it
    #[arg(short, long, value_name = "DIR")]
    pub root: Option<PathBuf>,

    /// Link the attachment to a dataset
    ///
    /// Dataset ID (or prefix). The attachment is stored and linked in
    /// one step
    #[arg(short, long, value_name = "DATASET")]
    pub dataset: Option<String>,
}

#[derive(Args)]
pub struct AttachmentListArgs {
    #[command(flatten)]
    pub limit: crate::limit::LimitArgs,

    /// List the attachments linked to a dataset
    ///
    /// Dataset ID (or prefix). Rows are in link order
    #[arg(short, long, value_name = "DATASET")]
    pub dataset: Option<String>,

    /// Show the full attachment ID
    #[arg(long)]
    pub full_id: bool,
}

#[derive(Args)]
pub struct AttachmentShowArgs {
    /// Attachment name, ID, or ID prefix
    pub attachment: String,
}

#[derive(Args)]
pub struct AttachmentRemoveArgs {
    /// Attachment names, IDs, or ID prefixes
    #[arg(required = true)]
    pub attachments: Vec<String>,

    /// Remove the attachments from a dataset only
    ///
    /// Dataset ID (or prefix). The attachments stay in the store
    #[arg(short, long, value_name = "DATASET")]
    pub dataset: Option<String>,
}

pub fn add(args: AttachmentAddArgs) {
    let store = open_store("gage attachment add");
    let attachments = AttachmentStore::from(&store);

    let root = args.root.unwrap_or_else(|| PathBuf::from("."));
    let root = match root.canonicalize() {
        Ok(root) => root,
        Err(e) => {
            eprintln!("gage attachment add: {}: {e}", root.display());
            std::process::exit(1);
        }
    };
    let spec = AttachmentSpec {
        name: &args.name,
        root: &root,
        includes: &args.includes,
        excludes: &args.exclude,
    };
    let added = match attachments.add(&spec) {
        Ok(outcome) => outcome,
        Err(e) => {
            eprintln!("gage attachment add: {e}");
            std::process::exit(1);
        }
    };
    let files = plural(added.file_count as usize, "file");
    match added.outcome {
        AttachmentOutcome::Added => {
            println!(
                "Added attachment {} ({}) with {files}",
                args.name,
                short_uuid(&added.id)
            );
        }
        AttachmentOutcome::Updated => {
            println!(
                "Updated attachment {} ({}) with {files}",
                args.name,
                short_uuid(&added.id)
            );
        }
        AttachmentOutcome::Unchanged => {
            println!(
                "Attachment {} ({}) is unchanged with {files}",
                args.name,
                short_uuid(&added.id)
            );
        }
    }

    if let Some(prefix) = args.dataset.as_deref() {
        link_to_dataset(&store, prefix, &added.id, &args.name);
    }
}

/// Link one stored attachment to a dataset and report the outcome.
fn link_to_dataset(store: &Store, dataset_prefix: &str, id: &str, name: &str) {
    let datasets = DatasetStore::from(store);
    let dataset_id = match datasets.resolve_id(dataset_prefix) {
        Ok(id) => id,
        Err(e) => {
            eprintln!("gage attachment add: --dataset {dataset_prefix}: {e}");
            std::process::exit(1);
        }
    };
    let outcomes = match datasets.attachments_link(&dataset_id, &[id.to_string()]) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("gage attachment add: {e}");
            std::process::exit(1);
        }
    };
    let dataset = short_uuid(&dataset_id);
    for o in &outcomes {
        match o.outcome {
            AttachmentLinkOutcome::Linked => {
                println!("Linked attachment {name} to dataset {dataset}");
            }
            AttachmentLinkOutcome::Updated => {
                println!("Updated attachment {name} in dataset {dataset}");
            }
            AttachmentLinkOutcome::Unchanged => {
                println!("Attachment {name} is already linked to dataset {dataset}");
            }
        }
    }
}

pub fn list(args: AttachmentListArgs) {
    let store = open_store("gage attachment list");
    let records: Vec<AttachmentRecord> = match args.dataset.as_deref() {
        Some(prefix) => {
            let datasets = DatasetStore::from(&store);
            let dataset = match datasets.get(prefix) {
                Ok(record) => record,
                Err(e) => {
                    eprintln!("gage attachment list: --dataset {prefix}: {e}");
                    std::process::exit(1);
                }
            };
            match datasets.attachments(&dataset.id) {
                Ok(records) => records,
                Err(e) => {
                    eprintln!("gage attachment list: {e}");
                    std::process::exit(1);
                }
            }
        }
        None => match AttachmentStore::from(&store)
            .query()
            .order(gage_store::Order::ModifiedDesc)
            .iter()
            .and_then(|iter| iter.collect::<Result<Vec<_>, _>>())
        {
            Ok(records) => records,
            Err(e) => {
                eprintln!("gage attachment list: {e}");
                std::process::exit(1);
            }
        },
    };
    let total = records.len();
    if total == 0 {
        println!("No attachments found");
        return;
    }
    let show = args.limit.show_count(total);

    let header: Vec<String> = ["Id", "Name", "Files", "Size", "Root", "Modified"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let rows: Vec<Vec<String>> = records
        .iter()
        .take(show)
        .map(|r| {
            let id = if args.full_id {
                r.id.clone()
            } else {
                short_uuid(&r.id).to_string()
            };
            vec![
                id,
                r.attrs.name.clone(),
                r.attrs.file_count.to_string(),
                format_size(r.attrs.size as i64),
                r.attrs.root.display().to_string(),
                r.modified_ms.map(format_elapsed_ms).unwrap_or_default(),
            ]
        })
        .collect();
    let shown = rows.len();
    let table = Table::from_iter(std::iter::once(header).chain(rows))
        .with(Style::rounded())
        .modify(Rows::first(), style::tty(Color::FG_BRIGHT_YELLOW))
        .modify(Columns::new(2..4), Alignment::right())
        .modify(Columns::new(2..).not(Rows::first()), style::dim())
        .to_string();
    println!("{table}");
    args.limit.print_summary(shown, total, "attachment");
}

pub fn show(args: AttachmentShowArgs) {
    let store = open_store("gage attachment show");
    let attachments = AttachmentStore::from(&store);
    let record = resolve("gage attachment show", &attachments, &args.attachment);
    let files = match attachments.files(&record.commit_sha) {
        Ok(files) => files,
        Err(e) => {
            eprintln!("gage attachment show: {e}");
            std::process::exit(1);
        }
    };
    let iso = |ms: Option<i64>| {
        ms.map(gage_core::datetime::ms_to_iso8601)
            .unwrap_or_default()
    };
    let attrs = [
        ("id", record.id.clone()),
        ("name", record.attrs.name.clone()),
        ("root", record.attrs.root.display().to_string()),
        ("includes", record.attrs.includes.join(" ")),
        ("excludes", record.attrs.excludes.join(" ")),
        ("files", record.attrs.file_count.to_string()),
        ("size", format_size(record.attrs.size as i64)),
        ("created", iso(record.created_ms)),
        ("modified", iso(record.modified_ms)),
    ];
    let label_width = attrs.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
    for (label, value) in &attrs {
        println!("{label:<label_width$}  {value}");
    }
    if files.is_empty() {
        return;
    }
    println!();
    let header: Vec<String> = ["File", "Size"].iter().map(|s| s.to_string()).collect();
    let rows = files
        .iter()
        .map(|f| vec![f.key.clone(), format_size(f.size as i64)]);
    let table = Table::from_iter(std::iter::once(header).chain(rows))
        .with(Style::rounded())
        .modify(Rows::first(), style::tty(Color::FG_BRIGHT_YELLOW))
        .modify(Columns::one(1), Alignment::right())
        .modify(Columns::one(1).not(Rows::first()), style::dim())
        .to_string();
    println!("{table}");
}

pub fn remove(args: AttachmentRemoveArgs) {
    let store = open_store("gage attachment remove");
    let attachments = AttachmentStore::from(&store);

    // Resolve every argument before writing anything, so one bad
    // argument leaves the store untouched
    let mut ids: Vec<String> = Vec::with_capacity(args.attachments.len());
    for arg in &args.attachments {
        let record = resolve("gage attachment remove", &attachments, arg);
        if !ids.contains(&record.id) {
            ids.push(record.id);
        }
    }
    match args.dataset.as_deref() {
        Some(prefix) => remove_from_dataset(&store, prefix, &ids),
        None => remove_from_store(&store, &ids),
    }
}

/// Unlink attachments from one dataset; the objects stay
fn remove_from_dataset(store: &Store, dataset_prefix: &str, ids: &[String]) {
    let datasets = DatasetStore::from(store);
    let dataset_id = match datasets.resolve_id(dataset_prefix) {
        Ok(id) => id,
        Err(e) => {
            eprintln!("gage attachment remove: --dataset {dataset_prefix}: {e}");
            std::process::exit(1);
        }
    };
    let outcomes = match datasets.attachments_unlink(&dataset_id, ids) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("gage attachment remove: {e}");
            std::process::exit(1);
        }
    };
    for o in &outcomes {
        println!(
            "Removed attachment {} ({}) from dataset {}",
            o.name,
            short_uuid(&o.id),
            short_uuid(&dataset_id)
        );
    }
}

/// Unlink attachments from every dataset holding them and tombstone
/// them
fn remove_from_store(store: &Store, ids: &[String]) {
    let outcome = match AttachmentStore::from(store).remove(ids) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("gage attachment remove: {e}");
            std::process::exit(1);
        }
    };
    for held in &outcome.datasets {
        for id in &held.attachment_ids {
            let name = outcome
                .attachments
                .iter()
                .find(|r| &r.id == id)
                .map(|r| r.attrs.name.as_str())
                .unwrap_or_default();
            println!(
                "Removed attachment {name} ({}) from dataset {}",
                short_uuid(id),
                short_uuid(&held.dataset_id)
            );
        }
    }
    for record in &outcome.attachments {
        println!(
            "Removed attachment {} ({})",
            record.attrs.name,
            short_uuid(&record.id)
        );
    }
}

/// The live attachment an argument names: by name first, then by id
/// or prefix. Exits with the error when neither resolves.
fn resolve(command: &str, attachments: &AttachmentStore<'_>, arg: &str) -> AttachmentRecord {
    match attachments.get_by_name(arg) {
        Ok(record) => return record,
        Err(StoreError::ObjectDeleted(_)) => {
            eprintln!("{command}: attachment {arg} is removed");
            std::process::exit(1);
        }
        Err(StoreError::ObjectNotFound(_)) => {}
        Err(e) => {
            eprintln!("{command}: {e}");
            std::process::exit(1);
        }
    }
    match attachments.get(arg) {
        Ok(record) => record,
        Err(StoreError::ObjectNotFound(_)) => {
            eprintln!("{command}: no attachment named or identified by {arg}");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("{command}: {e}");
            std::process::exit(1);
        }
    }
}

fn plural(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("{n} {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

fn open_store(command: &str) -> Store {
    match Store::open(&gage_store::store_path()) {
        Ok(store) => store,
        Err(e) => {
            eprintln!("{command}: {e}");
            std::process::exit(1);
        }
    }
}
