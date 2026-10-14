//! `gage attachment`: file trees in the Gage store, for scanners to
//! read.
//!
//! An attachment is added from a directory and a pattern list,
//! optionally named, keyed, and targeting objects, listed on its own
//! or as a dataset's, shown file by file, and removed. A dataset
//! holds attachments the way it holds sessions: `add --dataset`
//! stores the attachment and adds it to the dataset in one step,
//! `remove --dataset` takes it out of the dataset without touching
//! the object.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Context;
use clap::{Args, Subcommand};
use cliclack as cli;
use datafusion::arrow::array::{
    Array, Int64Array, ListArray, StringArray, TimestampMillisecondArray,
};
use gage_core::path::shorten_home;
use gage_core::uuid::short_uuid;
use gage_query::ContextBuilder;
use gage_registry::scanner::ScannerRegistry;
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

use crate::cmd_note::{count_rows, target_cell};
use crate::cmd_session::{column, run_query};
use crate::dialog::{self, DialogError, DialogResult};
use crate::error::CliError;
use crate::human::{count, format_elapsed_ms, format_size};
use crate::scanner_attach::{self, describe};
use crate::scanner_spec;
use crate::source;
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
    /// Adding again under the same key updates the attachment, with
    /// any new targets added to it; a named add without --key is
    /// keyed by its name and
    /// selection, and an unnamed add without --key always creates a
    /// new attachment. With --stored, an attachment already in the
    /// store is added to a dataset without reading the file system.
    Add(AttachmentAddArgs),

    /// List attachments
    List(AttachmentListArgs),

    /// Show an attachment and its files
    Show(AttachmentShowArgs),

    /// Remove attachments from store or from a dataset
    ///
    /// Without --dataset, each attachment is removed from every
    /// dataset that holds it, then marked removed in the store. With
    /// --dataset, each attachment is removed from that dataset only
    /// and stays in the store.
    Remove(AttachmentRemoveArgs),
}

#[derive(Args)]
pub struct AttachmentAddArgs {
    /// Files to include (glob)
    ///
    /// At least one is required; `**/*` selects every file
    #[arg(required_unless_present_any = ["stored", "scanners"], value_name = "INCLUDE")]
    pub includes: Vec<String>,

    /// Where to select files from (default current directory)
    ///
    /// Stored file paths are relative to it
    #[arg(short, long, value_name = "DIR")]
    pub root: Option<PathBuf>,

    /// Files to exclude (glob; repeatable)
    #[arg(short, long, value_name = "PATTERN")]
    pub exclude: Vec<String>,

    /// Add stored attachment (name, ID, or prefix)
    ///
    /// Attachment name, ID, or ID prefix. Takes the attachment from
    /// the store instead of the file system. Requires --dataset
    #[arg(
        short = 'S',
        long,
        value_name = "ATTACHMENT",
        requires = "dataset",
        conflicts_with_all = ["includes", "name", "key", "root", "exclude", "targets", "scanners"]
    )]
    pub stored: Option<String>,

    /// Attach scanner inputs (name or path; repeatable)
    ///
    /// A scanner name, or when no scanner has that name, a path: a
    /// directory holding a scanner.rn, or a scanner file. Each scanner
    /// attaches the files its scan reads, preparing the dataset for a
    /// scan by those scanners. Requires --dataset
    #[arg(
        short,
        long = "scanner",
        value_name = "SCANNER",
        requires = "dataset",
        conflicts_with_all = ["includes", "name", "key", "root", "exclude", "targets"]
    )]
    pub scanners: Vec<String>,

    /// Add attachment to dataset (ID or prefix)
    ///
    /// Dataset ID (or prefix). The attachment is stored and added to
    /// the dataset in one step
    #[arg(short, long, value_name = "DATASET")]
    pub dataset: Option<String>,

    /// Target object (ID or prefix; repeatable)
    ///
    /// ID (or prefix) of any stored object, such as a session or note
    #[arg(short, long = "target", value_name = "OBJECT")]
    pub targets: Vec<String>,

    /// Attachment name
    ///
    /// Scanners select attachments by name. A name is letters,
    /// digits, `-`, `_`, and `.`
    #[arg(short, long, value_name = "NAME")]
    pub name: Option<String>,

    /// Attachment key
    ///
    /// A later add under the same key updates this attachment instead
    /// of creating one. Without --key, a named add is keyed by its
    /// name, root, and patterns
    #[arg(short, long, value_name = "KEY")]
    pub key: Option<String>,

    /// Skip confirmation prompt
    #[arg(short, long)]
    pub yes: bool,
}

#[derive(Args)]
pub struct AttachmentListArgs {
    /// List attachments in dataset (ID or prefix)
    ///
    /// Dataset ID (or prefix). Rows are in dataset order
    #[arg(short, long, value_name = "DATASET")]
    pub dataset: Option<String>,

    #[command(flatten)]
    pub limit: crate::limit::LimitArgs,

    /// Show full attachment ID
    #[arg(long)]
    pub full_id: bool,
}

#[derive(Args)]
pub struct AttachmentShowArgs {
    /// Attachment to show (name, ID, or prefix)
    pub attachment: String,
}

#[derive(Args)]
pub struct AttachmentRemoveArgs {
    /// Attachments to remove (name, ID, or prefix)
    #[arg(required = true)]
    pub attachments: Vec<String>,

    /// Remove attachments from dataset only (ID or prefix)
    ///
    /// Dataset ID (or prefix). The attachments stay in the store
    #[arg(short, long, value_name = "DATASET")]
    pub dataset: Option<String>,

    /// Skip confirmation prompt
    #[arg(short, long)]
    pub yes: bool,
}

pub async fn add(args: AttachmentAddArgs) -> Result<(), CliError> {
    let store = open_store("gage attachment add")?;
    dialog::run_async("Add attachment", || add_dialog(&args, &store)).await
}

async fn add_dialog(args: &AttachmentAddArgs, store: &Store) -> Result<DialogResult, DialogError> {
    let attachments = AttachmentStore::from(store);

    if !args.scanners.is_empty() {
        let dataset = args
            .dataset
            .as_deref()
            .expect("clap requires --dataset with --scanner");
        return add_from_scanners(store, dataset, &args.scanners, args.yes).await;
    }

    if let Some(stored) = args.stored.as_deref() {
        let record = resolve(&attachments, stored).map_err(DialogError::Failed)?;
        let dataset = args
            .dataset
            .as_deref()
            .expect("clap requires --dataset with --stored");
        let dataset_id = DatasetStore::from(store)
            .resolve_id(dataset)
            .map_err(|e| DialogError::Failed(format!("--dataset {dataset}: {e}")))?;
        let shown = describe(record.attrs.name.as_deref(), &record.id);
        dialog::step_value("Attachment", &shown)?;
        dialog::step_value("Dataset", short_uuid(&dataset_id))?;
        confirm(args.yes)?;
        let (dataset_id, linked) =
            link_to_dataset(store, &dataset_id, &record.id).map_err(DialogError::Failed)?;
        let verb = match linked {
            AttachmentLinkOutcome::Linked => "Added",
            AttachmentLinkOutcome::Updated => "Updated",
            AttachmentLinkOutcome::Unchanged => "Unchanged",
        };
        return Ok(format!(
            "{verb} attachment {} to dataset {}",
            describe(record.attrs.name.as_deref(), &record.id),
            short_uuid(&dataset_id)
        )
        .into());
    }

    let root = args.root.clone().unwrap_or_else(|| PathBuf::from("."));
    let root = root
        .canonicalize()
        .map_err(|e| DialogError::Failed(format!("{}: {e}", root.display())))?;
    let mut targets = Vec::with_capacity(args.targets.len());
    for input in &args.targets {
        targets.push(resolve_target(store, input).map_err(DialogError::Failed)?);
    }
    if let Some(name) = &args.name {
        dialog::step_value("Name", name)?;
    }
    dialog::step_value("Root", shorten_home(&root))?;
    dialog::step_list("Include", args.includes.iter().cloned())?;
    if !args.exclude.is_empty() {
        dialog::step_list("Exclude", args.exclude.iter().cloned())?;
    }
    if !targets.is_empty() {
        dialog::step_list("Targets", targets.iter().map(|url| target_cell(url)))?;
    }
    let dataset_id = match args.dataset.as_deref() {
        Some(prefix) => {
            let id = DatasetStore::from(store)
                .resolve_id(prefix)
                .map_err(|e| DialogError::Failed(format!("--dataset {prefix}: {e}")))?;
            dialog::step_value("Dataset", short_uuid(&id))?;
            Some(id)
        }
        None => None,
    };
    confirm(args.yes)?;

    let spec = AttachmentSpec {
        name: args.name.as_deref(),
        key: args.key.as_deref(),
        targets: &targets,
        root: &root,
        includes: &args.includes,
        excludes: &args.exclude,
    };
    let spinner = cli::spinner();
    spinner.start("Reading files");
    let added = attachments.add(&spec);
    spinner.clear();
    let added = added.map_err(|e| DialogError::Failed(e.to_string()))?;
    let files = count(added.file_count as usize, "file");
    let shown = describe(args.name.as_deref(), &added.id);
    let verb = match added.outcome {
        AttachmentOutcome::Added => "Added",
        AttachmentOutcome::Updated => "Updated",
        AttachmentOutcome::Unchanged => "Unchanged",
    };
    match dataset_id {
        Some(dataset_id) => {
            // The line reports the store write, as `gage dataset add`
            // does; the dataset suffix names where the attachment
            // was placed
            let (dataset_id, _) =
                link_to_dataset(store, &dataset_id, &added.id).map_err(DialogError::Failed)?;
            Ok(format!(
                "{verb} attachment {shown} with {files} to dataset {}",
                short_uuid(&dataset_id)
            )
            .into())
        }
        None => Ok(format!("{verb} attachment {shown} with {files}").into()),
    }
}

/// The confirmation before a write; `yes` skips it
fn confirm(yes: bool) -> Result<(), DialogError> {
    if !yes && !cli::confirm("Continue?").initial_value(true).interact()? {
        return Err(DialogError::Canceled);
    }
    Ok(())
}

/// Run the attach tasks of each named scanner against the dataset.
/// The first failing task ends the command; attachments linked
/// before it stay linked.
async fn add_from_scanners(
    store: &Store,
    dataset_prefix: &str,
    names: &[String],
    yes: bool,
) -> Result<DialogResult, DialogError> {
    let dataset_id = DatasetStore::from(store)
        .resolve_id(dataset_prefix)
        .map_err(|e| DialogError::Failed(format!("--dataset {dataset_prefix}: {e}")))?;
    let registry = ScannerRegistry::load();
    let selected = scanner_spec::resolve(&registry, names).map_err(DialogError::Failed)?;
    let mut compiled = Vec::with_capacity(selected.len());
    for resolved in &selected {
        let scanner = resolved.scanner().map_err(DialogError::Failed)?;
        compiled
            .push(gage_scan::compile(&scanner).map_err(|e| DialogError::Failed(e.to_string()))?);
    }
    dialog::step_value("Dataset", short_uuid(&dataset_id))?;
    dialog::step_list(
        "Scanners",
        selected.iter().map(|r| r.source.def().name.clone()),
    )?;
    confirm(yes)?;
    let driver = source::driver_registry()
        .default()
        .ok_or_else(|| DialogError::Failed("no default session driver registered".to_string()))?;
    let placed = scanner_attach::attach_for_scanners(store, &dataset_id, &compiled, driver).await?;
    Ok(format!(
        "Added {} to dataset {}",
        count(placed, "attachment"),
        short_uuid(&dataset_id)
    )
    .into())
}

/// Link one stored attachment into `dataset_id` and return the id
/// with the link outcome.
fn link_to_dataset(
    store: &Store,
    dataset_id: &str,
    id: &str,
) -> Result<(String, AttachmentLinkOutcome), String> {
    let datasets = DatasetStore::from(store);
    let dataset_id = dataset_id.to_string();
    let mut outcomes = datasets
        .attachments_link(&dataset_id, &[id.to_string()])
        .map_err(|e| e.to_string())?;
    let linked = outcomes
        .pop()
        .expect("attachments_link should yield one outcome per id")
        .outcome;
    Ok((dataset_id, linked))
}

pub async fn list(args: AttachmentListArgs) -> Result<(), CliError> {
    let store = open_store("gage attachment list")?;
    // With --dataset the rows are the dataset's attachments in dataset order;
    // otherwise every live attachment, newest modified first
    let (from, order) = match args.dataset.as_deref() {
        Some(prefix) => {
            let dataset = DatasetStore::from(&store)
                .get(prefix)
                .with_context(|| format!("gage attachment list: --dataset {prefix}"))?;
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
    let total = count_rows(&ctx, &format!("SELECT COUNT(*) FROM {from}"))
        .await
        .context("gage attachment list")?;
    if total == 0 {
        println!("No attachments found");
        return Ok(());
    }
    let show = args.limit.show_count(total);
    let sql = format!(
        "SELECT a.id, a.id_prefix, a.name, a.targets, a.file_count, a.size, a.root, a.modified \
         FROM {from} ORDER BY {order} LIMIT {show}"
    );
    let batches = run_query(&ctx, &sql)
        .await
        .context("gage attachment list")?;

    let header: Vec<String> = ["Id", "Name", "Targets", "Files", "Size", "Root", "Modified"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let mut rows: Vec<Vec<String>> = Vec::new();
    for batch in &batches {
        let ids = column::<StringArray>(batch, 0);
        let prefixes = column::<StringArray>(batch, 1);
        let names = column::<StringArray>(batch, 2);
        let targets = column::<ListArray>(batch, 3);
        let files = column::<Int64Array>(batch, 4);
        let sizes = column::<Int64Array>(batch, 5);
        let roots = column::<StringArray>(batch, 6);
        let modifieds = column::<TimestampMillisecondArray>(batch, 7);
        for i in 0..batch.num_rows() {
            let id = ids.value(i);
            let shown = if args.full_id { id } else { short_uuid(id) };
            let name = if names.is_null(i) { "" } else { names.value(i) };
            rows.push(vec![
                styled_id(shown, prefixes.value(i), IdKind::Gage),
                name.to_string(),
                targets_cell(&targets.value(i)),
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
    Ok(())
}

/// The targets as listings show them: none as blank, one as its
/// scheme and short id, more as a count
fn targets_cell(targets: &datafusion::arrow::array::ArrayRef) -> String {
    let urls = targets
        .as_any()
        .downcast_ref::<StringArray>()
        .expect("attachment targets hold strings");
    match urls.len() {
        0 => String::new(),
        1 => match urls.value(0).split_once(':') {
            Some((scheme, id)) => format!("{scheme}:{}", short_uuid(id)),
            None => urls.value(0).to_string(),
        },
        n => format!("{n} targets"),
    }
}

pub fn show(args: AttachmentShowArgs) -> Result<(), CliError> {
    let store = open_store("gage attachment show")?;
    let attachments = AttachmentStore::from(&store);
    let record = resolve(&attachments, &args.attachment)
        .map_err(|e| CliError::message(format!("gage attachment show: {e}")))?;
    let files = attachments
        .files(&record.commit_sha)
        .context("gage attachment show")?;
    let iso = |ms: Option<i64>| {
        ms.map(gage_core::datetime::ms_to_iso8601)
            .unwrap_or_default()
    };
    let attrs = [
        ("id", record.id.clone()),
        ("name", record.attrs.name.clone().unwrap_or_default()),
        ("key", record.attrs.key.clone().unwrap_or_default()),
        ("targets", record.attrs.targets.join(" ")),
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
        return Ok(());
    }
    println!();
    let header: Vec<String> = ["File", "Size"].iter().map(|s| s.to_string()).collect();
    let rows = files
        .iter()
        .map(|f| vec![f.path.clone(), format_size(f.size as i64)]);
    let table = Table::from_iter(std::iter::once(header).chain(rows))
        .with(Style::rounded())
        .modify(Rows::first(), style::tty(Color::FG_BRIGHT_YELLOW))
        .modify(Columns::one(1), Alignment::right())
        .modify(Columns::one(1).not(Rows::first()), style::dim())
        .to_string();
    println!("{table}");
    Ok(())
}

pub fn remove(args: AttachmentRemoveArgs) -> Result<(), CliError> {
    let store = open_store("gage attachment remove")?;
    let attachments = AttachmentStore::from(&store);

    // Resolve every argument before writing anything, so one bad
    // argument leaves the store untouched
    let mut records: Vec<AttachmentRecord> = Vec::with_capacity(args.attachments.len());
    let mut errors = 0;
    for arg in &args.attachments {
        match resolve(&attachments, arg) {
            Ok(record) => {
                if !records.iter().any(|r| r.id == record.id) {
                    records.push(record);
                }
            }
            Err(e) => {
                eprintln!("gage attachment remove: {e}");
                errors += 1;
            }
        }
    }
    if errors > 0 {
        return Err(CliError::exit(1));
    }
    let dataset_id = match args.dataset.as_deref() {
        Some(prefix) => Some(
            DatasetStore::from(&store)
                .resolve_id(prefix)
                .with_context(|| format!("gage attachment remove: --dataset {prefix}"))?,
        ),
        None => None,
    };

    dialog::run("Remove attachments", || {
        dialog::step_list(
            "Attachments",
            records
                .iter()
                .map(|r| describe(r.attrs.name.as_deref(), &r.id)),
        )?;
        if let Some(id) = &dataset_id {
            dialog::step_value("Dataset", short_uuid(id))?;
        }

        if dataset_id.is_some() {
            cli::log::remark("Attachments stay in the store")?;
        }
        if !args.yes {
            let confirmed = cli::confirm("Continue?").initial_value(false).interact()?;
            if !confirmed {
                return Err(DialogError::Canceled);
            }
        }

        // Every argument resolved above, so the single commit below
        // takes the whole set or nothing
        let ids: Vec<String> = records.iter().map(|r| r.id.clone()).collect();
        match &dataset_id {
            Some(dataset_id) => {
                let outcomes = DatasetStore::from(&store)
                    .attachments_unlink(dataset_id, &ids)
                    .map_err(|e| DialogError::Failed(e.to_string()))?;
                Ok(format!(
                    "Removed {} from dataset {}",
                    count(outcomes.len(), "attachment"),
                    short_uuid(dataset_id)
                )
                .into())
            }
            None => {
                let outcome = attachments
                    .remove(&ids)
                    .map_err(|e| DialogError::Failed(e.to_string()))?;
                if !outcome.datasets.is_empty() {
                    dialog::step_list(
                        "Datasets",
                        outcome.datasets.iter().map(|held| {
                            format!(
                                "{} ({} removed)",
                                short_uuid(&held.dataset_id),
                                count(held.attachment_ids.len(), "attachment")
                            )
                        }),
                    )?;
                }
                Ok(format!("Removed {}", count(outcome.attachments.len(), "attachment")).into())
            }
        }
    })
}

/// The live attachment an argument names: by name first, then by id
/// or prefix. A name shared by several attachments is an error that
/// lists them, one per line after the first.
fn resolve(attachments: &AttachmentStore<'_>, arg: &str) -> Result<AttachmentRecord, String> {
    let named: Vec<AttachmentRecord> = attachments
        .query()
        .name(arg)
        .iter()
        .and_then(|records| records.collect())
        .map_err(|e| e.to_string())?;
    if named.len() > 1 {
        let mut message = format!("{arg} names {} attachments:", named.len());
        for record in &named {
            let key = record.attrs.key.as_deref().unwrap_or_default();
            message.push_str(&format!("\n  {} {key}", record.id));
        }
        return Err(message);
    }
    if let Some(record) = named.into_iter().next() {
        return Ok(record);
    }
    match attachments.get(arg) {
        Ok(record) => Ok(record),
        Err(StoreError::ObjectNotFound(_)) => {
            Err(format!("no attachment named or identified by {arg}"))
        }
        Err(e) => Err(e.to_string()),
    }
}

fn open_store(command: &str) -> Result<Store, CliError> {
    let store = Store::open(&gage_store::store_path()).context(command.to_string())?;
    Ok(store)
}
