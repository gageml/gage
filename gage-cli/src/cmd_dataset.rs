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
    ATTACHMENT_TYPE, AddStep, DATASET_TYPE, DatasetOrigin, DatasetSessionAddOutcome, DatasetStore,
    SESSION_TYPE, SessionOutcome, SessionRecord, Store, StoreError, TagStore,
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
use crate::session_filter::{
    FILTER_ARG_NAMES, SessionFilterArgs, SessionSelect, SessionSelectError, native_session_lines,
    preview_sessions, select_sessions,
};
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

    /// Add stored sessions and attachments to a dataset
    ///
    /// Each object is already in store. A session takes a member slot;
    /// an object already a member advances to its current commit.
    Link(DatasetLinkArgs),

    /// Remove sessions and attachments from a dataset
    ///
    /// Objects stay in store.
    Unlink(DatasetUnlinkArgs),
}

#[derive(Args)]
pub struct DatasetLinkArgs {
    /// Dataset to add objects to (tag, ID, or prefix)
    pub dataset: String,

    /// Sessions and attachments to add (tag, ID, or prefix)
    #[arg(required = true, value_name = "OBJECT")]
    pub objects: Vec<String>,

    /// Skip confirmation prompt
    #[arg(short, long)]
    pub yes: bool,
}

#[derive(Args)]
pub struct DatasetUnlinkArgs {
    /// Dataset to remove objects from (tag, ID, or prefix)
    pub dataset: String,

    /// Sessions and attachments to remove (tag, ID, or prefix)
    #[arg(required = true, value_name = "OBJECT")]
    pub objects: Vec<String>,

    /// Skip confirmation prompt
    #[arg(short, long)]
    pub yes: bool,
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
    #[arg(short, long, value_name = "NAME", long_help = format!("Tag new dataset\n\n{TAG_HELP}"))]
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
        let selected = select_sessions(
            SessionSelect::Native(args.source.as_deref()),
            &args.sessions,
            &args.filter,
        )
        .await
        .map_err(SessionSelectError::into_dialog_error)?;
        dialog::step_value(
            "Source",
            crate::source::describe_source(driver.as_ref(), &spec),
        )?;
        let lines = native_session_lines(args.source.as_deref(), &selected)
            .await
            .map_err(|e| DialogError::Failed(format!("{e}")))?;
        preview_sessions(&lines, IdKind::Native, None)?;
        selected
    } else {
        Vec::new()
    };
    dialog::confirm(args.yes, true)?;

    let dataset_id = create_dataset(store, None).map_err(|e| DialogError::Failed(e.to_string()))?;
    if let Some(name) = &args.tag {
        let added = tags
            .add(name, &dataset_id, args.force_tag)
            .map_err(|e| DialogError::Failed(e.to_string()))?;
        if let Some(previous) = added.previous_id {
            cli::log::info(format!("Moved tag {name} from {}", short_uuid(&previous)))?;
        }
    }
    let shown = style::outro_id(store, DATASET_TYPE, &dataset_id);
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
        dialog::confirm(args.yes, true)?;

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
        let done = view.done().to_string();
        view.finish()?;
        Ok(done.into())
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
            dialog::step_list("Outcome", self.lines)?;
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
    Starting { total: usize },
    /// A session was written and placed
    Added {
        outcome: SessionOutcome,
        id: &'a str,
        native_id: &'a str,
        dataset: Option<&'a str>,
    },
    /// Every session is placed, counted by outcome
    Done {
        counts: AddCounts,
        dataset: Option<&'a str>,
    },
}

/// The sessions of an add, counted by outcome.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AddCounts {
    pub(crate) added: usize,
    pub(crate) updated: usize,
    pub(crate) unchanged: usize,
}

impl AddCounts {
    pub(crate) fn from_outcomes(outcomes: impl IntoIterator<Item = SessionOutcome>) -> Self {
        let mut counts = Self::default();
        for outcome in outcomes {
            match outcome {
                SessionOutcome::Added => counts.added += 1,
                SessionOutcome::Updated => counts.updated += 1,
                SessionOutcome::Unchanged => counts.unchanged += 1,
            }
        }
        counts
    }

    /// The closing line: the leading nonzero count in the order added,
    /// updated, unchanged, carrying the dataset when there is one, then
    /// each remaining nonzero count as a short clause.
    fn fmt_line(&self, f: &mut fmt::Formatter<'_>, dataset: Option<&str>) -> fmt::Result {
        let place = |preposition: &str| match dataset {
            Some(dataset) => format!(" {preposition} dataset {}", short_uuid(dataset)),
            None => String::new(),
        };
        let mut rest: Vec<String> = Vec::new();
        if self.added > 0 {
            write!(f, "Added {}{}", count(self.added, "session"), place("to"))?;
            if self.updated > 0 {
                rest.push(format!("updated {}", self.updated));
            }
            if self.unchanged > 0 {
                rest.push(format!("{} unchanged", self.unchanged));
            }
        } else if self.updated > 0 {
            write!(
                f,
                "Updated {}{}",
                count(self.updated, "session"),
                place("in")
            )?;
            if self.unchanged > 0 {
                rest.push(format!("{} unchanged", self.unchanged));
            }
        } else {
            write!(
                f,
                "{} unchanged{}",
                count(self.unchanged, "session"),
                place("in")
            )?;
        }
        for clause in rest {
            write!(f, ", {clause}")?;
        }
        Ok(())
    }
}

impl fmt::Display for AddEvent<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AddEvent::Starting { .. } => f.write_str("Adding sessions"),
            AddEvent::Added {
                outcome,
                id,
                native_id,
                dataset,
            } => {
                let (verb, preposition) = match outcome {
                    SessionOutcome::Added => ("Added", "to"),
                    SessionOutcome::Updated => ("Updated", "in"),
                    SessionOutcome::Unchanged => ("Unchanged", "in"),
                };
                write!(
                    f,
                    "{verb} session {} (native {})",
                    short_uuid(id),
                    short_uuid(native_id)
                )?;
                match dataset {
                    Some(dataset) => {
                        write!(f, " {preposition} dataset {}", short_uuid(dataset))
                    }
                    None => Ok(()),
                }
            }
            AddEvent::Done { counts, dataset } => counts.fmt_line(f, *dataset),
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
                AddStep::Starting { index, .. } => {
                    current = index;
                    on_event(AddEvent::Starting { total });
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
        counts: AddCounts::from_outcomes(outcomes.iter().map(|o| o.outcome)),
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
    let mut table = Table::from_iter(std::iter::once(header).chain(rows));
    table
        .with(Style::rounded())
        .modify(Rows::first(), style::tty(Color::FG_BRIGHT_YELLOW))
        .modify(Columns::new(1..4), Alignment::right())
        .modify(
            Columns::one(4).not(Rows::first()),
            style::tty(Color::FG_BRIGHT_CYAN),
        )
        .modify(Columns::new(1..4).not(Rows::first()), style::dim())
        .modify(Columns::one(5).not(Rows::first()), style::dim());
    style::truncate_to(
        &mut table,
        style::stdout_width(),
        style::IdAwarePriority::new(args.full_id),
    );
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

        dialog::confirm_irreversible(args.yes)?;

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

pub fn link(args: DatasetLinkArgs) -> Result<(), CliError> {
    let command = "gage dataset link";
    let store = open_store(command)?;
    let datasets = DatasetStore::from(&store);
    let dataset_id = datasets
        .resolve_id(&args.dataset)
        .with_context(|| format!("{command}: {}", args.dataset))?;
    let members = resolve_members(command, &store, &args.objects)?;
    dialog::run("Link to dataset", || {
        dialog::step_value("Dataset", short_uuid(&dataset_id))?;
        members.steps()?;
        dialog::confirm(args.yes, true)?;
        let mut placed = 0;
        if !members.sessions.is_empty() {
            placed += datasets
                .sessions_link(&dataset_id, &members.sessions)
                .map_err(|e| DialogError::Failed(e.to_string()))?
                .len();
        }
        if !members.attachments.is_empty() {
            placed += datasets
                .attachments_link(&dataset_id, &members.attachments)
                .map_err(|e| DialogError::Failed(e.to_string()))?
                .len();
        }
        Ok(format!(
            "Added {} to dataset {}",
            count(placed, "object"),
            short_uuid(&dataset_id)
        )
        .into())
    })
}

pub fn unlink(args: DatasetUnlinkArgs) -> Result<(), CliError> {
    let command = "gage dataset unlink";
    let store = open_store(command)?;
    let datasets = DatasetStore::from(&store);
    let dataset_id = datasets
        .resolve_id(&args.dataset)
        .with_context(|| format!("{command}: {}", args.dataset))?;
    let members = resolve_members(command, &store, &args.objects)?;
    dialog::run("Unlink from dataset", || {
        dialog::step_value("Dataset", short_uuid(&dataset_id))?;
        members.steps()?;
        cli::log::remark("Objects stay in the store")?;
        dialog::confirm(args.yes, false)?;
        let mut removed = 0;
        if !members.sessions.is_empty() {
            removed += datasets
                .sessions_unlink(&dataset_id, &members.sessions)
                .map_err(|e| DialogError::Failed(e.to_string()))?
                .len();
        }
        if !members.attachments.is_empty() {
            removed += datasets
                .attachments_unlink(&dataset_id, &members.attachments)
                .map_err(|e| DialogError::Failed(e.to_string()))?
                .len();
        }
        Ok(format!(
            "Removed {} from dataset {}",
            count(removed, "object"),
            short_uuid(&dataset_id)
        )
        .into())
    })
}

/// The objects a link or unlink names, sorted by type. Every argument
/// resolves before anything is written, and a type a dataset does not
/// hold is an error naming it.
struct Members {
    sessions: Vec<String>,
    attachments: Vec<String>,
}

impl Members {
    fn steps(&self) -> io::Result<()> {
        if !self.sessions.is_empty() {
            dialog::step_list(
                "Sessions",
                self.sessions.iter().map(|id| short_uuid(id).to_string()),
            )?;
        }
        if !self.attachments.is_empty() {
            dialog::step_list(
                "Attachments",
                self.attachments.iter().map(|id| short_uuid(id).to_string()),
            )?;
        }
        Ok(())
    }
}

fn resolve_members(command: &str, store: &Store, objects: &[String]) -> Result<Members, CliError> {
    let mut members = Members {
        sessions: Vec::new(),
        attachments: Vec::new(),
    };
    for arg in objects {
        let found = store
            .resolve_in(arg, None)
            .with_context(|| format!("{command}: {arg}"))?;
        let list = match found.object_type.as_str() {
            SESSION_TYPE => &mut members.sessions,
            ATTACHMENT_TYPE => &mut members.attachments,
            other => {
                return Err(CliError::message(format!(
                    "{command}: {arg} is a {}; a dataset holds sessions and attachments",
                    other.strip_prefix("gage::").unwrap_or(other)
                )));
            }
        };
        if !list.contains(&found.id) {
            list.push(found.id);
        }
    }
    Ok(members)
}

/// Link sessions already in the store as members of `dataset_id`,
/// reporting each to `on_event`.
pub(crate) fn link_stored(
    store: &Store,
    dataset_id: &str,
    records: &[SessionRecord],
    on_event: &mut dyn FnMut(AddEvent<'_>),
) -> Result<(), DialogError> {
    let ids: Vec<String> = records.iter().map(|r| r.id.clone()).collect();
    let outcomes = DatasetStore::from(store)
        .sessions_link(dataset_id, &ids)
        .map_err(|e| DialogError::Failed(format!("{e}")))?;
    for (outcome, record) in outcomes.iter().zip(records) {
        on_event(AddEvent::Added {
            outcome: outcome.outcome,
            id: &outcome.id,
            native_id: record.attrs.native_id(),
            dataset: Some(dataset_id),
        });
    }
    on_event(AddEvent::Done {
        counts: AddCounts::from_outcomes(outcomes.iter().map(|o| o.outcome)),
        dataset: Some(dataset_id),
    });
    Ok(())
}

/// Open the default store, or `command: <error>`
fn open_store(command: &str) -> Result<Store, CliError> {
    let store = Store::open(&gage_store::store_path()).context(command.to_string())?;
    Ok(store)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The progress label is constant: the sessions pass too quickly
    /// for an id to be read.
    #[test]
    fn starting_line_names_no_session() {
        assert_eq!(
            AddEvent::Starting { total: 3 }.to_string(),
            "Adding sessions"
        );
    }

    /// A session's line names the outcome, and the dataset preposition
    /// follows the verb: a session is added to a dataset and updated or
    /// unchanged in it.
    #[test]
    fn added_line_names_the_outcome() {
        let line = |outcome, dataset| {
            AddEvent::Added {
                outcome,
                id: "01k2h4t9aaaabbbbccccddddee",
                native_id: "fb064932-0000-0000-0000-000000000000",
                dataset,
            }
            .to_string()
        };
        let dataset = Some("7zzzzzzzzzzzzzzzzzzzzzzzzz");
        assert_eq!(
            line(SessionOutcome::Added, None),
            "Added session 01k2h4t9 (native fb064932)"
        );
        assert_eq!(
            line(SessionOutcome::Added, dataset),
            "Added session 01k2h4t9 (native fb064932) to dataset 7zzzzzzz"
        );
        assert_eq!(
            line(SessionOutcome::Updated, dataset),
            "Updated session 01k2h4t9 (native fb064932) in dataset 7zzzzzzz"
        );
        assert_eq!(
            line(SessionOutcome::Unchanged, dataset),
            "Unchanged session 01k2h4t9 (native fb064932) in dataset 7zzzzzzz"
        );
    }

    /// The closing line leads with the first nonzero count in the
    /// order added, updated, unchanged, attaches the dataset to that
    /// clause, and appends each remaining nonzero count.
    #[test]
    fn done_line_counts_each_outcome() {
        let line = |outcomes: &[SessionOutcome], dataset| {
            AddEvent::Done {
                counts: AddCounts::from_outcomes(outcomes.iter().copied()),
                dataset,
            }
            .to_string()
        };
        let dataset = Some("7zzzzzzzzzzzzzzzzzzzzzzzzz");
        use SessionOutcome::{Added, Unchanged, Updated};
        assert_eq!(line(&[Added], None), "Added 1 session");
        assert_eq!(line(&[Added, Added], None), "Added 2 sessions");
        assert_eq!(line(&[Updated, Updated], None), "Updated 2 sessions");
        assert_eq!(line(&[Unchanged], None), "1 session unchanged");
        assert_eq!(
            line(&[Added, Added, Added, Updated, Unchanged, Unchanged], None),
            "Added 3 sessions, updated 1, 2 unchanged"
        );
        assert_eq!(
            line(&[Updated, Unchanged, Unchanged], None),
            "Updated 1 session, 2 unchanged"
        );
        assert_eq!(
            line(&[Added, Updated], dataset),
            "Added 1 session to dataset 7zzzzzzz, updated 1"
        );
        assert_eq!(
            line(&[Updated, Unchanged], dataset),
            "Updated 1 session in dataset 7zzzzzzz, 1 unchanged"
        );
        assert_eq!(
            line(&[Unchanged, Unchanged], dataset),
            "2 sessions unchanged in dataset 7zzzzzzz"
        );
    }
}
