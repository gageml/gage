use std::fmt;
use std::io::{self, IsTerminal};
use std::sync::{Arc, Mutex};

use anyhow::Context;
use clap::{Args, Subcommand};
use cliclack as cli;
use datafusion::arrow::array::{Int64Array, StringArray, TimestampMillisecondArray};
use gage_core::datetime::ms_to_iso8601;
use gage_core::uuid::short_uuid;
use gage_query::ContextBuilder;
use gage_session::SourceOptions;
use gage_store::{
    AddStep, DatasetOrigin, DatasetSessionAddOutcome, DatasetStore, SessionOutcome, Store,
    StoreError, TagStore,
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
use crate::cmd_tag::TAG_HELP;
use crate::dialog::{self, DialogError, DialogResult};
use crate::error::CliError;
use crate::human::{count, format_elapsed_ms};
use crate::session_filter::{FILTER_ARG_NAMES, SessionFilterArgs, select_sessions};
use crate::size_limit::SizeLimitArgs;
use crate::source::SOURCE_HELP;
use crate::style::{self, IdKind, styled_id};

#[derive(Subcommand)]
pub enum DatasetCommand {
    /// Add a dataset
    ///
    /// Without session-selection options, adds an empty dataset. With
    /// options, adds a dataset populated with the selected native
    /// sessions in a single step.
    Add(DatasetAddArgs),

    /// Re-add each dataset session from its source
    ///
    /// Every member is re-read from the source and re-added: a
    /// session whose native content grew is updated in place.
    /// Equivalent to running gage session add -d DATASET on every
    /// member's native id.
    Refresh(DatasetRefreshArgs),

    /// List datasets
    List(DatasetListArgs),

    /// Show a dataset
    Show(DatasetShowArgs),

    /// Delete datasets
    ///
    /// Member sessions and scan runs that used a dataset are kept
    /// unless --sessions is given.
    Delete(DatasetDeleteArgs),
}

#[derive(Args)]
pub struct DatasetAddArgs {
    /// Session source
    #[arg(short, long, value_name = "SOURCE", long_help = format!("Session source\n\n{SOURCE_HELP}"))]
    pub source: Option<String>,

    /// Sessions to add (ID or prefix)
    #[arg(value_name = "SESSION", conflicts_with_all = FILTER_ARG_NAMES)]
    pub sessions: Vec<String>,

    #[command(flatten)]
    pub filter: SessionFilterArgs,

    #[command(flatten)]
    pub size: SizeLimitArgs,

    /// Tag new dataset
    #[arg(long, value_name = "NAME", long_help = format!("Tag new dataset\n\n{TAG_HELP}"))]
    pub tag: Option<String>,

    /// Move tag if it exists
    #[arg(long, requires = "tag")]
    pub force_tag: bool,

    /// Skip confirmation prompt
    #[arg(short, long)]
    pub yes: bool,
}

#[derive(Args)]
pub struct DatasetRefreshArgs {
    /// Dataset to refresh (ID or prefix)
    pub dataset: String,

    /// Session source
    #[arg(short, long, value_name = "SOURCE", long_help = format!("Session source\n\n{SOURCE_HELP}"))]
    pub source: Option<String>,

    #[command(flatten)]
    pub size: SizeLimitArgs,

    /// Skip confirmation prompt
    #[arg(short, long)]
    pub yes: bool,
}

#[derive(Args)]
pub struct DatasetListArgs {
    #[command(flatten)]
    limit: crate::limit::LimitArgs,

    /// Show full dataset ID
    #[arg(long)]
    full_id: bool,
}

#[derive(Args)]
pub struct DatasetShowArgs {
    /// Dataset to show (ID or prefix)
    dataset: String,
}

#[derive(Args)]
pub struct DatasetDeleteArgs {
    /// Datasets to delete (ID or prefix)
    #[arg(required = true)]
    ids: Vec<String>,

    /// Also remove member sessions from store
    ///
    /// A session that another dataset holds is kept. Scan runs that
    /// used the dataset are not affected.
    #[arg(long)]
    sessions: bool,

    /// Skip confirmation prompt
    #[arg(short, long)]
    yes: bool,
}

pub async fn add(args: DatasetAddArgs) -> Result<(), CliError> {
    let store = open_store("gage dataset add")?;
    dialog::run_async("Add dataset", || add_dialog(&args, &store)).await
}

async fn add_dialog(args: &DatasetAddArgs, store: &Store) -> Result<DialogResult, DialogError> {
    let tags = TagStore::from(store);
    // A tag that cannot be added is reported before the dataset
    // exists, so the failure leaves the store untouched
    if let Some(name) = &args.tag {
        tags.check_available(name, args.force_tag)
            .map_err(|e| match e {
                StoreError::TagExists { .. } => {
                    DialogError::Failed(format!("{e} (use --force-tag to move it)"))
                }
                e => DialogError::Failed(e.to_string()),
            })?;
        dialog::step_value("Tag", name)?;
    }
    // The selection is resolved before the dataset exists, so a bad
    // selection leaves no empty dataset behind
    let selecting = !args.sessions.is_empty() || !args.filter.is_empty();
    let selected = if selecting {
        let registry = crate::source::driver_registry();
        let (driver, spec) = crate::source::resolve_source(&registry, args.source.as_deref())
            .map_err(DialogError::Failed)?;
        let selected = select_sessions(args.source.as_deref(), &args.sessions, &args.filter)
            .await
            .map_err(|e| DialogError::Failed(format!("{e}")))?;
        dialog::step_value(
            "Source",
            crate::source::describe_source(driver.as_ref(), &spec),
        )?;
        dialog::step_value("Sessions", count(selected.len(), "session"))?;
        selected
    } else {
        Vec::new()
    };
    if !args.yes && !cli::confirm("Continue?").initial_value(true).interact()? {
        return Err(DialogError::Canceled);
    }

    let dataset_id = create_dataset(store, None).map_err(|e| DialogError::Failed(e.to_string()))?;
    if let Some(name) = &args.tag {
        let added = tags
            .add(name, &dataset_id, args.force_tag)
            .map_err(|e| DialogError::Failed(e.to_string()))?;
        if let Some(previous) = added.previous_id {
            cli::log::info(format!("Moved tag {name} from {}", short_uuid(&previous)))?;
        }
    }
    let shown = short_uuid(&dataset_id);
    if selected.is_empty() {
        if selecting {
            cli::log::info("No sessions matched the selection")?;
        }
        return Ok(format!("Added dataset {shown}").into());
    }

    let mut view = AddView::new();
    add_native_to_dataset(
        store,
        &dataset_id,
        args.source.as_deref(),
        &selected,
        Some(args.size.max_bytes().context("user config")?),
        &mut |e| view.on(e),
    )
    .map_err(DialogError::Failed)?;
    let placed = view.finish()?;
    Ok(format!("Added dataset {shown} with {}", count(placed, "session")).into())
}

pub fn refresh(args: DatasetRefreshArgs) -> Result<(), CliError> {
    let store = open_store("gage dataset refresh")?;
    dialog::run("Refresh dataset", || {
        let datasets = DatasetStore::from(&store);
        let record = datasets
            .get(&args.dataset)
            .map_err(|e| DialogError::Failed(format!("{}: {e}", args.dataset)))?;
        let members = datasets
            .sessions_list(&record.id)
            .map_err(|e| DialogError::Failed(e.to_string()))?;
        let shown = short_uuid(&record.id);
        dialog::step_value(
            "Dataset",
            format!("{shown} ({})", count(members.len(), "session")),
        )?;
        if members.is_empty() {
            return Ok(format!("Nothing to refresh in dataset {shown}").into());
        }
        if !args.yes && !cli::confirm("Continue?").initial_value(true).interact()? {
            return Err(DialogError::Canceled);
        }

        // Each member's native id is looked back up in the source by
        // the add; a member whose native session is gone fails the
        // whole add and the dataset stays untouched.
        let selected: Vec<String> = members.iter().map(|m| m.native_id.clone()).collect();
        let mut view = AddView::new();
        add_native_to_dataset(
            &store,
            &record.id,
            args.source.as_deref(),
            &selected,
            Some(args.size.max_bytes().context("user config")?),
            &mut |e| view.on(e),
        )
        .map_err(DialogError::Failed)?;
        let placed = view.finish()?;
        Ok(format!("Refreshed dataset {shown} ({})", count(placed, "session")).into())
    })
}

/// Create an empty dataset in `store`, recording `origin` when given.
pub(crate) fn create_dataset(
    store: &Store,
    origin: Option<&DatasetOrigin>,
) -> Result<String, StoreError> {
    let datasets = DatasetStore::from(store);
    match origin {
        Some(origin) => datasets.create_with_origin(origin),
        None => datasets.create(),
    }
}

/// Renders a session add inside a dialog: a progress bar on a
/// terminal, a `Sessions` step listing every outcome otherwise, and
/// the closing line held for the outro.
pub(crate) struct AddView {
    progress: bool,
    bar: Option<cli::ProgressBar>,
    lines: Vec<String>,
    placed: usize,
    done: String,
}

impl AddView {
    pub(crate) fn new() -> Self {
        Self::with_progress(io::stdout().is_terminal())
    }

    /// `progress` shows the bar; without it every outcome is a line
    /// of the step
    pub(crate) fn with_progress(progress: bool) -> Self {
        Self {
            progress,
            bar: None,
            lines: Vec::new(),
            placed: 0,
            done: String::new(),
        }
    }

    pub(crate) fn on(&mut self, event: AddEvent<'_>) {
        match &event {
            AddEvent::Starting { total, .. } => {
                if self.progress {
                    let message = event.to_string();
                    self.bar
                        .get_or_insert_with(|| {
                            let bar = cli::progress_bar(*total as u64);
                            bar.start(&message);
                            bar
                        })
                        .set_message(message);
                }
            }
            AddEvent::Added { .. } => {
                self.placed += 1;
                match &self.bar {
                    Some(bar) => bar.inc(1),
                    None => self.lines.push(event.to_string()),
                }
            }
            AddEvent::Done { .. } => {
                if let Some(bar) = self.bar.take() {
                    bar.clear();
                }
                self.done = event.to_string();
            }
        }
    }

    /// The number of sessions placed, after writing the outcome lines
    /// as a step when no bar showed them
    pub(crate) fn finish(self) -> io::Result<usize> {
        if !self.progress {
            dialog::step_list("Sessions", self.lines)?;
        }
        Ok(self.placed)
    }

    /// The closing line, for an outro that reports the add itself
    pub(crate) fn done(&self) -> &str {
        &self.done
    }
}

/// Progress of a session add, reported to the caller as it happens.
/// Each event renders as the line a plain listing prints for it.
#[derive(Debug)]
pub(crate) enum AddEvent<'a> {
    /// A session, one of `total`, is about to be written
    Starting { total: usize, native_id: &'a str },
    /// A session was written and placed
    Added {
        outcome: SessionOutcome,
        id: &'a str,
        native_id: &'a str,
        dataset: Option<&'a str>,
    },
    /// Every session is placed. `placed` counts them all, whether
    /// written anew, updated, or unchanged.
    Done {
        placed: usize,
        dataset: Option<&'a str>,
    },
}

impl fmt::Display for AddEvent<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AddEvent::Starting { native_id, .. } => {
                write!(f, "Adding session {}", short_uuid(native_id))
            }
            AddEvent::Added {
                outcome,
                id,
                native_id,
                dataset,
            } => {
                let verb = match outcome {
                    SessionOutcome::Added => "Added",
                    SessionOutcome::Updated => "Updated",
                    SessionOutcome::Unchanged => "Unchanged",
                };
                write!(
                    f,
                    "{verb} session {} (native {})",
                    short_uuid(id),
                    short_uuid(native_id)
                )?;
                match dataset {
                    Some(dataset) => write!(f, " to dataset {}", short_uuid(dataset)),
                    None => Ok(()),
                }
            }
            AddEvent::Done { placed, dataset } => {
                let noun = if *placed == 1 { "session" } else { "sessions" };
                write!(f, "Added {placed} {noun}")?;
                match dataset {
                    Some(dataset) => write!(f, " to dataset {}", short_uuid(dataset)),
                    None => Ok(()),
                }
            }
        }
    }
}

/// Add each already-resolved native session record to `dataset_id`
/// through the opened source in a single commit, reporting each
/// session to `on_event` as it is written. Performs no output of its
/// own. `sessions_add` is idempotent per session — a member whose
/// native content is unchanged is a no-op, a member whose content
/// grew is updated in its slot, and a non-member is appended.
pub(crate) fn add_native_to_dataset(
    store: &Store,
    dataset_id: &str,
    source: Option<&str>,
    selected: &[String],
    max_bytes: Option<u64>,
    on_event: &mut dyn FnMut(AddEvent<'_>),
) -> Result<Vec<DatasetSessionAddOutcome>, String> {
    let registry = crate::source::driver_registry();
    let (driver, spec) = crate::source::resolve_source(&registry, source)?;
    let source_handle = driver
        .open_source(&SourceOptions::new(&spec))
        .map_err(|e| format!("{spec}: {e}"))?;

    // Confirm every selected session is present in the opened
    // source before writing anything, so one missing session leaves
    // the dataset unchanged rather than partially updated.
    let mut ids: Vec<String> = Vec::with_capacity(selected.len());
    let mut errors = Vec::new();
    for native_id in selected {
        match source_handle.find_native(native_id) {
            Ok(id) => ids.push(id),
            Err(e) => errors.push(e.to_string()),
        }
    }
    if !errors.is_empty() {
        return Err(errors.join("\n"));
    }

    let datasets = match max_bytes {
        Some(max) => DatasetStore::from(store).with_max_session_size(max),
        None => DatasetStore::from(store),
    };
    let total = ids.len();
    // The sessions are written in `ids` order; the index of the
    // session being written names its native id for the outcome event
    let mut current = 0;
    let outcomes = datasets
        .sessions_add_opening(
            dataset_id,
            driver.as_ref(),
            &ids,
            &mut |native_id| {
                if dialog::interrupted() {
                    return Ok(None);
                }
                source_handle
                    .open_native(native_id)
                    .map(Some)
                    .map_err(|e| StoreError::Parse(format!("{native_id}: {e}")))
            },
            &mut |step| match step {
                AddStep::Starting { index, native_id } => {
                    current = index;
                    on_event(AddEvent::Starting { total, native_id });
                }
                AddStep::Added(outcome) => on_event(AddEvent::Added {
                    outcome: outcome.outcome,
                    id: &outcome.id,
                    native_id: ids.get(current).map_or("", String::as_str),
                    dataset: Some(dataset_id),
                }),
            },
        )
        .map_err(|e| e.to_string())?;
    let placed = outcomes.len();
    on_event(AddEvent::Done {
        placed,
        dataset: Some(dataset_id),
    });

    source_handle.close().map_err(|e| format!("{spec}: {e}"))?;
    if placed < total {
        return Err(format!(
            "Interrupted after {placed} of {}",
            count(total, "session")
        ));
    }
    Ok(outcomes)
}

pub async fn list(args: DatasetListArgs) -> Result<(), CliError> {
    let store = open_store("gage dataset list")?;
    let ctx = ContextBuilder::new(Some(Arc::new(Mutex::new(store))))
        .build()
        .await;
    let total = count_rows(&ctx, "SELECT COUNT(*) FROM dataset")
        .await
        .context("gage dataset list")?;
    if total == 0 {
        println!("No datasets found");
        return Ok(());
    }
    let show = args.limit.show_count(total);
    let sql = format!(
        "SELECT d.id, d.id_prefix, d.created, \
         COALESCE(a.n, 0) AS attachments, COALESCE(m.n, 0) AS sessions, \
         COALESCE(sc.n, 0) AS scans, \
         arrow_cast(COALESCE(t.names, ''), 'Utf8') AS tags \
         FROM dataset d \
         LEFT JOIN (SELECT dataset_id, COUNT(*) AS n FROM dataset_attachment \
                    GROUP BY dataset_id) a ON a.dataset_id = d.id \
         LEFT JOIN (SELECT dataset_id, COUNT(*) AS n FROM dataset_session \
                    GROUP BY dataset_id) m ON m.dataset_id = d.id \
         LEFT JOIN (SELECT dataset, COUNT(*) AS n FROM scan \
                    WHERE dataset IS NOT NULL GROUP BY dataset) sc ON sc.dataset = d.id \
         LEFT JOIN (SELECT id, string_agg(name, ', ' ORDER BY name) AS names FROM tag \
                    GROUP BY id) t ON t.id = d.id \
         ORDER BY d.modified DESC LIMIT {show}"
    );
    let batches = run_query(&ctx, &sql).await.context("gage dataset list")?;

    let header: Vec<String> = ["Id", "Sessions", "Attachments", "Scans", "Tags", "Created"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let mut rows: Vec<Vec<String>> = Vec::new();
    for batch in &batches {
        let ids = column::<StringArray>(batch, 0);
        let prefixes = column::<StringArray>(batch, 1);
        let createds = column::<TimestampMillisecondArray>(batch, 2);
        let attachments = column::<Int64Array>(batch, 3);
        let sessions = column::<Int64Array>(batch, 4);
        let scans = column::<Int64Array>(batch, 5);
        let tags = column::<StringArray>(batch, 6);
        for i in 0..batch.num_rows() {
            let id = ids.value(i);
            let shown = if args.full_id { id } else { short_uuid(id) };
            rows.push(vec![
                styled_id(shown, prefixes.value(i), IdKind::Gage),
                sessions.value(i).to_string(),
                attachments.value(i).to_string(),
                scans.value(i).to_string(),
                tags.value(i).to_string(),
                format_elapsed_ms(createds.value(i)),
            ]);
        }
    }
    let shown = rows.len();
    let table = Table::from_iter(std::iter::once(header).chain(rows))
        .with(Style::rounded())
        .modify(Rows::first(), style::tty(Color::FG_BRIGHT_YELLOW))
        .modify(Columns::new(1..4), Alignment::right())
        .modify(
            Columns::one(4).not(Rows::first()),
            style::tty(Color::FG_BRIGHT_CYAN),
        )
        .modify(Columns::new(1..4).not(Rows::first()), style::dim())
        .modify(Columns::one(5).not(Rows::first()), style::dim())
        .to_string();
    println!("{table}");
    args.limit.print_summary(shown, total, "dataset");
    Ok(())
}

pub async fn show(args: DatasetShowArgs) -> Result<(), CliError> {
    let store = Arc::new(Mutex::new(open_store("gage dataset show")?));
    let (record, sessions, attachments, tags, header) = {
        let store = store.lock().unwrap();
        let datasets = DatasetStore::from(&*store);
        let record = datasets
            .get(&args.dataset)
            .with_context(|| format!("gage dataset show: {}", args.dataset))?;
        let sessions = datasets.sessions(&record.id).context("gage dataset show")?;
        let attachments = datasets
            .attachments(&record.id)
            .context("gage dataset show")?;
        let tags = TagStore::from(&*store)
            .targets()
            .context("gage dataset show")?;
        let header = store
            .read_header(&record.commit_sha)
            .context("gage dataset show")?;
        (record, sessions, attachments, tags, header)
    };
    let ctx = ContextBuilder::new(Some(store)).build().await;
    let sql = format!(
        "SELECT id FROM scan WHERE dataset = '{}' ORDER BY modified DESC",
        record.id
    );
    let batches = run_query(&ctx, &sql).await.context("gage dataset show")?;
    let scans: Vec<String> = batches
        .iter()
        .flat_map(|batch| {
            let ids = column::<StringArray>(batch, 0);
            (0..batch.num_rows())
                .map(|i| ids.value(i).to_string())
                .collect::<Vec<_>>()
        })
        .collect();
    let iso = |ms: Option<i64>| ms.map(ms_to_iso8601).unwrap_or_default();

    let sessions_cell = sessions
        .iter()
        .map(|s| s.id.clone())
        .collect::<Vec<_>>()
        .join("\n");
    let attachments_cell = attachments
        .iter()
        .map(|a| format!("{} {}", a.id, a.attrs.name.as_deref().unwrap_or_default()))
        .collect::<Vec<_>>()
        .join("\n");
    let scans_cell = scans.join("\n");
    let origin_cell = record
        .origin
        .as_ref()
        .map(|o| o.to_string())
        .unwrap_or_default();
    let tags_cell = tags
        .iter()
        .filter(|t| t.id == record.id)
        .map(|t| t.name.clone())
        .collect::<Vec<_>>()
        .join("\n");
    let rows = [
        ("id", record.id.clone()),
        ("sessions", sessions_cell),
        ("attachments", attachments_cell),
        ("scans", scans_cell),
        ("origin", origin_cell),
        ("tags", tags_cell),
        ("created", iso(Some(record.created_ms))),
        ("modified", iso(header.modified_ms)),
    ];
    let table = Table::from_iter(rows.iter().map(|(k, v)| [k.to_string(), v.clone()]))
        .with(Style::rounded())
        .modify(Columns::first(), style::tty(Color::FG_BRIGHT_YELLOW))
        .to_string();
    println!("{table}");
    Ok(())
}

pub fn delete(args: DatasetDeleteArgs) -> Result<(), CliError> {
    let store = open_store("gage dataset delete")?;
    let datasets = DatasetStore::from(&store);

    // Resolve every argument before writing anything, so one bad
    // argument leaves the store untouched
    let mut records = Vec::with_capacity(args.ids.len());
    let mut errors = 0;
    for prefix in &args.ids {
        match datasets.get(prefix) {
            Ok(record) => records.push(record),
            Err(e) => {
                eprintln!("gage dataset delete: {e}");
                errors += 1;
            }
        }
    }
    if errors > 0 {
        return Err(CliError::exit(1));
    }

    let total = records.len();
    let members: usize = records.iter().map(|r| r.session_count).sum();
    dialog::run("Delete datasets", || {
        dialog::step_list(
            "Datasets",
            records.iter().map(|r| {
                format!(
                    "{} ({})",
                    short_uuid(&r.id),
                    count(r.session_count, "session")
                )
            }),
        )?;
        if args.sessions {
            cli::log::remark(format!("{} to remove", count(members, "session")))?;
        }

        cli::log::remark("This cannot be undone")?;
        if !args.yes {
            let confirmed = cli::confirm("Continue?").initial_value(false).interact()?;
            if !confirmed {
                return Err(DialogError::Canceled);
            }
        }

        let mut deleted = 0;
        let mut removed = 0;
        let mut kept = 0;
        for record in &records {
            let result = if args.sessions {
                datasets.delete_cascade(&record.id).map(|d| {
                    removed += d.removed.len();
                    kept += d.kept.len();
                })
            } else {
                datasets.delete(&record.id).map(|_| ())
            };
            match result {
                Ok(()) => deleted += 1,
                Err(e) => cli::log::warning(format!("{}: {e}", short_uuid(&record.id)))?,
            }
        }
        if deleted < total {
            return Err(dialog::partial("Deleted", deleted, total, "dataset"));
        }

        let mut outro = format!("Deleted {}", count(deleted, "dataset"));
        if args.sessions {
            outro.push_str(&format!(", removed {}", count(removed, "session")));
            if kept > 0 {
                outro.push_str(&format!(", kept {kept} held by other datasets"));
            }
        }
        Ok(outro.into())
    })
}

/// Open the default store, or `command: <error>`
fn open_store(command: &str) -> Result<Store, CliError> {
    let store = Store::open(&gage_store::store_path()).context(command.to_string())?;
    Ok(store)
}
