//! `gage attachment`: file trees in the Gage store, for scanners to
//! read.
//!
//! An attachment is added from a directory and a pattern list,
//! optionally named and optionally targeting a session, listed on its
//! own or as a dataset's, shown file by file, and removed. A dataset
//! holds attachments the way it holds sessions: `add --dataset`
//! stores the attachment and adds it to the dataset in one step,
//! `remove --dataset` takes it out of the dataset without touching
//! the object.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use clap::{Args, Subcommand};
use datafusion::arrow::array::{Array, Int64Array, StringArray, TimestampMillisecondArray};
use gage_core::path::shorten_home;
use gage_core::uuid::short_uuid;
use gage_query2::ContextBuilder;
use gage_registry::scanner::{Scanner, ScannerRegistry};
use gage_runtime2::{Output, TaskOutput};
use gage_scan2::attach::{AttachEvent, attach};
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

use crate::cmd_note::count_rows;
use crate::cmd_session::{column, run_query};
use crate::human::{format_elapsed_ms, format_size};
use crate::style::{self, IdKind, styled_id};
use crate::target::resolve_target;

#[derive(Subcommand)]
pub enum AttachmentCommand {
    /// Add an attachment from a directory
    ///
    /// Files are selected by path globs relative to the root, as in
    /// a shell: `settings.json` is the one file at the root,
    /// `**/settings.json` is every file by that name, `*` and `?` do
    /// not cross `/`, and `{a,b}` and `[a-z]` are supported. Excludes
    /// use the same form, and a directory an exclude matches is not
    /// entered. An attachment holds at most 10 MiB and 1000 files.
    /// Adding again under the same name and target updates the
    /// attachment when its files changed and is otherwise a no-op; an
    /// unnamed add always creates a new attachment. With --stored, an
    /// attachment already in the store is added to a dataset without
    /// reading the file system.
    Add(AttachmentAddArgs),

    /// List attachments
    List(AttachmentListArgs),

    /// Show an attachment and its files
    Show(AttachmentShowArgs),

    /// Remove attachments from the store or from a dataset
    ///
    /// Without --dataset, each attachment is removed from every
    /// dataset that holds it, then marked removed in the store. With
    /// --dataset, each attachment is removed from that dataset only
    /// and stays in the store.
    Remove(AttachmentRemoveArgs),
}

#[derive(Args)]
pub struct AttachmentAddArgs {
    /// Files to include (path globs relative to the root)
    ///
    /// At least one is required; `**/*` selects every file
    #[arg(required_unless_present_any = ["stored", "scanners"], value_name = "INCLUDE")]
    pub includes: Vec<String>,

    /// Name the attachment
    ///
    /// Scanners select attachments by name. Adding again under the
    /// same name and target updates the attachment; an unnamed add
    /// always creates a new one. A name is letters, digits, `-`, `_`,
    /// and `.`
    #[arg(short, long, value_name = "NAME")]
    pub name: Option<String>,

    /// Files to exclude (path glob relative to the root, repeatable)
    #[arg(short, long, value_name = "PATTERN")]
    pub exclude: Vec<String>,

    /// Directory to select files under (default: current directory)
    ///
    /// Stored file paths are relative to it
    #[arg(short, long, value_name = "DIR")]
    pub root: Option<PathBuf>,

    /// Object the files are about
    ///
    /// ID (or prefix) of any stored object, such as a session or note
    #[arg(short, long, value_name = "OBJECT")]
    pub target: Option<String>,

    /// Add the attachment to a dataset
    ///
    /// Dataset ID (or prefix). The attachment is stored and added to
    /// the dataset in one step
    #[arg(short, long, value_name = "DATASET")]
    pub dataset: Option<String>,

    /// Add an attachment already in the store to a dataset
    ///
    /// Attachment name, ID, or ID prefix. Takes the attachment from
    /// the store instead of the file system. Requires --dataset
    #[arg(
        short = 'S',
        long,
        value_name = "ATTACHMENT",
        requires = "dataset",
        conflicts_with_all = ["includes", "name", "root", "exclude", "target", "scanners"]
    )]
    pub stored: Option<String>,

    /// Run a scanner's attachment functions against a dataset (repeatable)
    ///
    /// Each named scanner attaches what its tasks read, preparing the
    /// dataset for a scan by those scanners. Requires --dataset
    #[arg(
        short,
        long = "scanner",
        value_name = "SCANNER",
        requires = "dataset",
        conflicts_with_all = ["includes", "name", "root", "exclude", "target"]
    )]
    pub scanners: Vec<String>,
}

#[derive(Args)]
pub struct AttachmentListArgs {
    #[command(flatten)]
    pub limit: crate::limit::LimitArgs,

    /// List the attachments in a dataset
    ///
    /// Dataset ID (or prefix). Rows are in dataset order
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

pub async fn add(args: AttachmentAddArgs) {
    let store = open_store("gage attachment add");
    let attachments = AttachmentStore::from(&store);

    if !args.scanners.is_empty() {
        let dataset = args
            .dataset
            .as_deref()
            .expect("clap requires --dataset with --scanner");
        add_from_scanners(&store, dataset, &args.scanners).await;
        return;
    }

    if let Some(stored) = args.stored.as_deref() {
        let record = resolve("gage attachment add", &attachments, stored);
        let dataset = args
            .dataset
            .as_deref()
            .expect("clap requires --dataset with --stored");
        add_to_dataset(&store, dataset, &record.id, record.attrs.name.as_deref());
        return;
    }

    let root = args.root.unwrap_or_else(|| PathBuf::from("."));
    let root = match root.canonicalize() {
        Ok(root) => root,
        Err(e) => {
            eprintln!("gage attachment add: {}: {e}", root.display());
            std::process::exit(1);
        }
    };
    let target = args
        .target
        .as_deref()
        .map(|input| match resolve_target(&store, input) {
            Ok(url) => url,
            Err(e) => {
                eprintln!("gage attachment add: {e}");
                std::process::exit(1);
            }
        });
    let spec = AttachmentSpec {
        name: args.name.as_deref(),
        target: target.as_deref(),
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
    let shown = describe(args.name.as_deref(), &added.id);
    match added.outcome {
        AttachmentOutcome::Added => println!("Added attachment {shown} with {files}"),
        AttachmentOutcome::Updated => println!("Updated attachment {shown} with {files}"),
        AttachmentOutcome::Unchanged => {
            println!("Attachment {shown} is unchanged with {files}")
        }
    }

    if let Some(prefix) = args.dataset.as_deref() {
        add_to_dataset(&store, prefix, &added.id, args.name.as_deref());
    }
}

/// Run the attachment functions of each named scanner against the
/// dataset, printing what the functions print and one line per
/// attachment they write. The first failing function ends the command
/// with exit 1; attachments linked before it stay linked.
async fn add_from_scanners(store: &Store, dataset_prefix: &str, names: &[String]) {
    let dataset_id = match DatasetStore::from(store).resolve_id(dataset_prefix) {
        Ok(id) => id,
        Err(e) => {
            eprintln!("gage attachment add: --dataset {dataset_prefix}: {e}");
            std::process::exit(1);
        }
    };
    let registry = ScannerRegistry::load();
    let mut compiled = Vec::with_capacity(names.len());
    for name in names {
        let Some(def) = registry.get_def(name) else {
            eprintln!("gage attachment add: unknown scanner {name}");
            std::process::exit(1);
        };
        let scanner = match Scanner::from_spec(def, None, name) {
            Ok(scanner) => scanner,
            Err(e) => {
                eprintln!("gage attachment add: {e}");
                std::process::exit(1);
            }
        };
        match gage_scan2::compile(&scanner) {
            Ok(c) => compiled.push(c),
            Err(e) => {
                eprintln!("gage attachment add: {e}");
                std::process::exit(1);
            }
        }
    }
    let dataset = short_uuid(&dataset_id).to_string();
    let result = attach(store, &dataset_id, &compiled, |event| match event {
        AttachEvent::Started { .. } => {}
        AttachEvent::Output(TaskOutput { output, .. }) => match output {
            Output::Print(s) => print!("{s}"),
            Output::Println(s) => println!("{s}"),
            Output::Log { level, message } => eprintln!("{}: {message}", level.as_str()),
            Output::Progress { .. } => {}
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
    .await;
    if let Err(e) = result {
        eprintln!("gage attachment add: {e}");
        std::process::exit(1);
    }
}

/// The attachment as messages show it: `name (short id)` when named,
/// the short id alone otherwise
fn describe(name: Option<&str>, id: &str) -> String {
    match name {
        Some(name) => format!("{name} ({})", short_uuid(id)),
        None => short_uuid(id).to_string(),
    }
}

/// Add one stored attachment to a dataset and report the outcome in
/// the shape `gage session add` uses.
fn add_to_dataset(store: &Store, dataset_prefix: &str, id: &str, name: Option<&str>) {
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
        let verb = match o.outcome {
            AttachmentLinkOutcome::Linked => "Added",
            AttachmentLinkOutcome::Updated => "Updated",
            AttachmentLinkOutcome::Unchanged => "Unchanged",
        };
        println!(
            "{verb} attachment {} to dataset {dataset}",
            describe(name, &o.id)
        );
    }
}

pub async fn list(args: AttachmentListArgs) {
    let store = open_store("gage attachment list");
    // With --dataset the rows are the dataset's attachments in dataset order;
    // otherwise every live attachment, newest modified first
    let (from, order) = match args.dataset.as_deref() {
        Some(prefix) => {
            let dataset = match DatasetStore::from(&store).get(prefix) {
                Ok(record) => record,
                Err(e) => {
                    eprintln!("gage attachment list: --dataset {prefix}: {e}");
                    std::process::exit(1);
                }
            };
            (
                format!(
                    "attachment a JOIN dataset_attachment m ON m.attachment_id = a.id \
                     AND m.dataset_id = '{}'",
                    dataset.id
                ),
                "m.attachment_num",
            )
        }
        None => ("attachment a".to_string(), "a.modified DESC"),
    };
    let ctx = ContextBuilder::new(Some(Arc::new(Mutex::new(store))))
        .build()
        .await;
    let total = count_rows(&ctx, &format!("SELECT COUNT(*) FROM {from}")).await;
    if total == 0 {
        println!("No attachments found");
        return;
    }
    let show = args.limit.show_count(total);
    let sql = format!(
        "SELECT a.id, a.id_prefix, a.name, a.target, a.file_count, a.size, a.root, a.modified \
         FROM {from} ORDER BY {order} LIMIT {show}"
    );
    let batches = run_query(&ctx, &sql).await;

    let header: Vec<String> = ["Id", "Name", "Target", "Files", "Size", "Root", "Modified"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let mut rows: Vec<Vec<String>> = Vec::new();
    for batch in &batches {
        let ids = column::<StringArray>(batch, 0);
        let prefixes = column::<StringArray>(batch, 1);
        let names = column::<StringArray>(batch, 2);
        let targets = column::<StringArray>(batch, 3);
        let files = column::<Int64Array>(batch, 4);
        let sizes = column::<Int64Array>(batch, 5);
        let roots = column::<StringArray>(batch, 6);
        let modifieds = column::<TimestampMillisecondArray>(batch, 7);
        for i in 0..batch.num_rows() {
            let id = ids.value(i);
            let shown = if args.full_id { id } else { short_uuid(id) };
            let name = if names.is_null(i) { "" } else { names.value(i) };
            let target = if targets.is_null(i) {
                String::new()
            } else {
                target_cell(targets.value(i))
            };
            rows.push(vec![
                styled_id(shown, prefixes.value(i), IdKind::Gage),
                name.to_string(),
                target,
                files.value(i).to_string(),
                format_size(sizes.value(i)),
                shorten_home(Path::new(roots.value(i))),
                format_elapsed_ms(modifieds.value(i)),
            ]);
        }
    }
    let shown = rows.len();
    let table = Table::from_iter(std::iter::once(header).chain(rows))
        .with(Style::rounded())
        .modify(Rows::first(), style::tty(Color::FG_BRIGHT_YELLOW))
        .modify(Columns::new(3..5), Alignment::right())
        .modify(Columns::new(3..).not(Rows::first()), style::dim())
        .to_string();
    println!("{table}");
    args.limit.print_summary(shown, total, "attachment");
}

/// A target URL as listings show it: the scheme and the short id
fn target_cell(url: &str) -> String {
    match url.split_once(':') {
        Some((scheme, id)) => format!("{scheme}:{}", short_uuid(id)),
        None => url.to_string(),
    }
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
        ("name", record.attrs.name.clone().unwrap_or_default()),
        ("target", record.attrs.target.clone().unwrap_or_default()),
        ("root", shorten_home(&record.attrs.root)),
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

/// Remove attachments from one dataset; the objects stay
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
            "Removed attachment {} from dataset {}",
            o.label,
            short_uuid(&dataset_id)
        );
    }
}

/// Remove attachments from every dataset holding them and tombstone
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
                .and_then(|r| r.attrs.name.as_deref());
            println!(
                "Removed attachment {} from dataset {}",
                describe(name, id),
                short_uuid(&held.dataset_id)
            );
        }
    }
    for record in &outcome.attachments {
        println!(
            "Removed attachment {}",
            describe(record.attrs.name.as_deref(), &record.id)
        );
    }
}

/// The live attachment an argument names: by name first, then by id
/// or prefix. A name shared by several attachments is an error that
/// lists them. Exits with the error when nothing resolves.
fn resolve(command: &str, attachments: &AttachmentStore<'_>, arg: &str) -> AttachmentRecord {
    let named: Vec<AttachmentRecord> = match attachments
        .query()
        .name(arg)
        .iter()
        .and_then(|records| records.collect())
    {
        Ok(records) => records,
        Err(e) => {
            eprintln!("{command}: {e}");
            std::process::exit(1);
        }
    };
    if named.len() > 1 {
        eprintln!("{command}: {arg} names {} attachments:", named.len());
        for record in &named {
            let target = record.attrs.target.as_deref().unwrap_or_default();
            eprintln!("  {} {target}", record.id);
        }
        std::process::exit(1);
    }
    if let Some(record) = named.into_iter().next() {
        return record;
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
