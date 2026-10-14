//! `gage blame`: the sessions that wrote the lines of a file.
//!
//! Each line of the file is matched against the `tool_use` rows that
//! wrote the file: the latest call whose input carries the line's
//! text wins, a call the driver attributes to the file preferred over
//! one it could not judge; with no such call the latest write the
//! driver attributes to the file stands in, marked as a file-level
//! match. The span reported with a match runs from the human prompt
//! before the call to the call itself: the input to the line.
//!
//! Under git, `git blame` names the commit behind each line, and the
//! commit bounds the calls considered: those between the previous
//! commit touching the file and the commit itself. Without git, or
//! for a file git does not track, every write to the file is a
//! candidate. The sessions read are those of the file's directory and
//! its ancestors, as the driver names projects, that wrote the file.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use anyhow::{Context, anyhow, bail};
use chrono::{DateTime, Utc};
use clap::Args;
use datafusion::arrow::array::{
    Array, Int64Array, Int64Builder, StringArray, StringBuilder, TimestampMillisecondArray,
    TimestampMillisecondBuilder,
};
use datafusion::arrow::datatypes::{DataType, Field, Schema, TimeUnit};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::common::ScalarValue;
use datafusion::prelude::SessionContext;
use gage_query::ContextBuilder;
use gage_session::Source;
use gage_store::Store;
use gage_store::url::LineSelection;
use serde_json::json;

use crate::error::CliError;
use crate::source::{self, SOURCE_HELP};

#[derive(Args)]
pub struct BlameArgs {
    /// File to blame
    path: PathBuf,

    /// Lines to blame (default all)
    ///
    /// A line ("42"), a range ("10-20"), or a comma-separated list of
    /// them ("10-20,42"), 1-based and inclusive: the selection a
    /// session target carries
    #[arg(short, long, value_name = "LINES")]
    lines: Option<LineSelection>,

    /// Session source
    #[arg(short, long, value_name = "SOURCE", long_help = format!("Session source\n\n{SOURCE_HELP}"))]
    source: Option<String>,

    /// Use stored sessions
    #[arg(short = 'S', long, conflicts_with = "source")]
    stored: bool,

    /// Output JSON
    #[arg(long)]
    json: bool,
}

pub async fn run(args: BlameArgs) -> Result<(), CliError> {
    let path = std::fs::canonicalize(&args.path)
        .with_context(|| format!("gage blame: {}", args.path.display()))?;
    let lines = blamed_lines(&path, args.lines.as_ref()).context("gage blame")?;

    // The stored scope names projects as the default source's driver
    // does, since that is the source stored sessions were added from
    let spec = match args.stored {
        true => String::new(),
        false => {
            let registry = source::driver_registry();
            source::resolve_source(&registry, args.source.as_deref())
                .map_err(|e| anyhow!("gage blame: {e}"))?
                .1
        }
    };
    let projects = {
        let source = source::open_source("gage blame", &spec)?;
        project_names(source.as_ref(), &path).context("gage blame")?
    };
    let (ctx, tables) = if args.stored {
        let store = Store::open(&gage_store::store_path()).context("gage blame")?;
        let ctx = ContextBuilder::new(Some(Arc::new(Mutex::new(store))))
            .build()
            .await;
        (ctx, Tables::stored())
    } else {
        (
            ContextBuilder::new(None).build().await,
            Tables::native(&spec),
        )
    };

    let rows = blame(&ctx, &tables, &path, &projects, lines)
        .await
        .context("gage blame")?;
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json_rows(&rows)).context("gage blame")?
        );
    } else {
        print_rows(&rows);
    }
    Ok(())
}

/// The lines to attribute, with the window each line's call must
/// fall in. Git supplies the windows when the file is in a repository
/// it tracks; otherwise every line's window is open-ended, and the
/// selection is applied here.
fn blamed_lines(path: &Path, selection: Option<&LineSelection>) -> anyhow::Result<Vec<BlamedLine>> {
    match GitFile::open(path).and_then(|file| file.blame(selection)) {
        Ok(lines) => Ok(lines),
        // Not a repository, a file git does not track, or no git at
        // all: the windows are the only loss
        Err(e) => {
            tracing::debug!(path = %path.display(), "blame without git: {e:#}");
            plain_lines(path, selection)
        }
    }
}

/// The file's own lines, every window open at the start and ending
/// now.
fn plain_lines(path: &Path, selection: Option<&LineSelection>) -> anyhow::Result<Vec<BlamedLine>> {
    let text = std::fs::read(path).with_context(|| path.display().to_string())?;
    let text = String::from_utf8_lossy(&text);
    let now = Utc::now().timestamp_millis();
    Ok(text
        .lines()
        .enumerate()
        .map(|(i, line)| (i as u64 + 1, line))
        .filter(|(n, _)| selection.is_none_or(|s| s.contains(*n)))
        .map(|(number, line)| BlamedLine {
            number,
            text: line.to_string(),
            window_start: None,
            window_end: now,
        })
        .collect())
}

/// A file in a git working tree.
struct GitFile {
    root: PathBuf,
    path: PathBuf,
}

/// One line of the file with the window its call must fall in.
#[derive(Debug, Clone, PartialEq, Eq)]
struct BlamedLine {
    number: u64,
    text: String,
    /// Epoch milliseconds, exclusive, or `None` when nothing bounds
    /// the start
    window_start: Option<i64>,
    /// Epoch milliseconds, inclusive
    window_end: i64,
}

impl GitFile {
    fn open(path: &Path) -> anyhow::Result<Self> {
        let dir = path
            .parent()
            .ok_or_else(|| anyhow!("{} has no parent directory", path.display()))?;
        let root = git(dir, &["rev-parse", "--show-toplevel"])?;
        Ok(Self {
            root: PathBuf::from(root.trim_end()),
            path: path.to_path_buf(),
        })
    }

    /// The file's lines with their commits' windows. Each range of
    /// the selection is one `-L` to git.
    fn blame(&self, selection: Option<&LineSelection>) -> anyhow::Result<Vec<BlamedLine>> {
        let mut args = vec!["blame".to_string(), "--porcelain".to_string()];
        for &(start, end) in selection.map(LineSelection::ranges).unwrap_or_default() {
            args.push("-L".to_string());
            args.push(format!("{start},{end}"));
        }
        args.push("--".to_string());
        let path = self.path.to_str().context("path is not UTF-8")?;
        args.push(path.to_string());
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let porcelain = git(&self.root, &args)?;
        let entries = parse_porcelain(&porcelain)?;

        let mut lines = Vec::with_capacity(entries.len());
        let mut windows: Vec<(String, Option<i64>, i64)> = Vec::new();
        for entry in entries {
            let window = match windows.iter().find(|(sha, _, _)| *sha == entry.sha) {
                Some((_, start, end)) => (*start, *end),
                None => {
                    let window = self.window(&entry)?;
                    windows.push((entry.sha.clone(), window.0, window.1));
                    window
                }
            };
            lines.push(BlamedLine {
                number: entry.line,
                text: entry.text,
                window_start: window.0,
                window_end: window.1,
            });
        }
        Ok(lines)
    }

    /// The window of an entry's commit: after the previous commit
    /// touching the file, through the commit's own time. An
    /// uncommitted line's window runs from the file's last commit to
    /// now.
    fn window(&self, entry: &PorcelainEntry) -> anyhow::Result<(Option<i64>, i64)> {
        let path = self.path.to_str().context("path is not UTF-8")?;
        let (previous_of, end) = if is_uncommitted(&entry.sha) {
            ("HEAD".to_string(), Utc::now().timestamp_millis())
        } else {
            let end = entry
                .committer_time
                .context("porcelain entry without committer-time")?;
            (format!("{}^", entry.sha), end * 1000)
        };
        // `<sha>^` fails on a root commit, which has no earlier
        // commit; the window is then open at the start
        let start = Command::new("git")
            .current_dir(&self.root)
            .args(["log", "-1", "--format=%at", &previous_of, "--", path])
            .output()?;
        let start = if start.status.success() {
            String::from_utf8_lossy(&start.stdout)
                .trim()
                .parse::<i64>()
                .ok()
                .map(|s| s * 1000)
        } else {
            None
        };
        Ok((start, end))
    }
}

fn is_uncommitted(sha: &str) -> bool {
    sha.bytes().all(|b| b == b'0')
}

/// Run git in `dir` and return its stdout; a non-zero exit is an error
/// carrying stderr.
fn git(dir: &Path, args: &[&str]) -> anyhow::Result<String> {
    let output = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .with_context(|| format!("git {}", args.join(" ")))?;
    if !output.status.success() {
        bail!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PorcelainEntry {
    sha: String,
    line: u64,
    text: String,
    committer_time: Option<i64>,
}

/// Parse `git blame --porcelain`. Each line opens with
/// `<sha> <orig> <final> [<count>]`, carries the commit's attributes
/// the first time the commit appears, and ends with the tab-prefixed
/// content line. The attributes are carried to every later line of
/// the same commit.
fn parse_porcelain(text: &str) -> anyhow::Result<Vec<PorcelainEntry>> {
    let mut entries = Vec::new();
    let mut times: Vec<(String, i64)> = Vec::new();
    let mut lines = text.lines();
    while let Some(header) = lines.next() {
        let mut parts = header.split(' ');
        let sha = parts.next().unwrap_or_default();
        let (Some(_orig), Some(line)) = (parts.next(), parts.next()) else {
            bail!("unexpected blame header {header:?}");
        };
        let line: u64 = line
            .parse()
            .with_context(|| format!("blame header {header:?}"))?;
        let mut committer_time = None;
        let text = loop {
            let Some(next) = lines.next() else {
                bail!("blame output ends inside an entry");
            };
            if let Some(content) = next.strip_prefix('\t') {
                break content.to_string();
            }
            if let Some(t) = next.strip_prefix("committer-time ") {
                committer_time = Some(t.trim().parse::<i64>()?);
            }
        };
        match committer_time {
            Some(t) => times.push((sha.to_string(), t)),
            None => committer_time = times.iter().find(|(s, _)| s == sha).map(|(_, t)| *t),
        }
        entries.push(PorcelainEntry {
            sha: sha.to_string(),
            line,
            text,
            committer_time,
        });
    }
    Ok(entries)
}

/// The session tables the blame reads, as the SQL names them in the
/// native or the stored scope.
struct Tables {
    session: String,
    tool_use: String,
    message: String,
}

impl Tables {
    fn native(spec: &str) -> Self {
        let quoted = spec.replace('\'', "''");
        Self {
            session: format!("native_session('{quoted}')"),
            tool_use: format!("native_tool_use('{quoted}')"),
            message: format!("native_message('{quoted}')"),
        }
    }

    fn stored() -> Self {
        Self {
            session: "session".to_string(),
            tool_use: "tool_use".to_string(),
            message: "message".to_string(),
        }
    }
}

/// The project names of the file's directory and every ancestor, as
/// `source` names them: the projects whose sessions could have
/// written the file.
fn project_names(source: &dyn Source, path: &Path) -> anyhow::Result<Vec<String>> {
    let mut names = Vec::new();
    for dir in path.ancestors().skip(1) {
        names.push(source.project_name(dir)?);
    }
    Ok(names)
}

/// Literal ids for an `IN` list. Literal ids reach the session walk
/// as path pruning; a subquery over the session table does not.
fn id_list(ids: &[String]) -> String {
    ids.iter()
        .map(|id| format!("'{}'", id.replace('\'', "''")))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The sessions among `ids` with a write the driver attributes to the
/// file. The `array_has` literal reaches the walk, which skips every
/// session whose summary does not list the file before parsing it.
async fn writers_of(
    ctx: &SessionContext,
    tables: &Tables,
    ids: &[String],
    wrote_path: &str,
) -> anyhow::Result<Vec<String>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let sql = format!(
        "SELECT DISTINCT t.session_id FROM {tool_use} t \
         WHERE t.session_id IN ({ids}) AND {wrote_path}",
        tool_use = tables.tool_use,
        ids = id_list(ids),
    );
    let batches = ctx.sql(&sql).await?.collect().await?;
    let mut out = Vec::new();
    for batch in &batches {
        let col = str_col(batch, 0);
        out.extend((0..batch.num_rows()).map(|i| col.value(i).to_string()));
    }
    Ok(out)
}

/// The ids of the sessions in `projects`, read through the session
/// table so the walk prunes by project.
async fn session_ids(
    ctx: &SessionContext,
    tables: &Tables,
    projects: &[String],
) -> anyhow::Result<Vec<String>> {
    if projects.is_empty() {
        return Ok(Vec::new());
    }
    let list = projects
        .iter()
        .map(|p| format!("'{}'", p.replace('\'', "''")))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "SELECT id FROM {session} WHERE project IN ({list})",
        session = tables.session
    );
    let batches = ctx.sql(&sql).await?.collect().await?;
    let mut ids = Vec::new();
    for batch in &batches {
        let col = str_col(batch, 0);
        ids.extend((0..batch.num_rows()).map(|i| col.value(i).to_string()));
    }
    Ok(ids)
}

/// How a blamed line was tied to a call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Match {
    /// The call's input carries the line's text
    Content,
    /// The latest write to the file in the window; the text was not
    /// found in any call
    File,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Attribution {
    session_id: String,
    /// Line of the call in the session
    call_line: i64,
    /// Line of the human prompt the span starts at; the call line
    /// when no prompt precedes it
    span_start: i64,
    /// Epoch milliseconds of the call's result
    timestamp: i64,
    matched: Match,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BlameRow {
    line: BlamedLine,
    attribution: Option<Attribution>,
}

/// A line's text as it appears inside a JSON string, for matching
/// against `input_raw`. Trimmed, since an edit's indentation may
/// differ from the file's after formatting.
fn match_text(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.chars().count() < MIN_MATCH_CHARS {
        return None;
    }
    let quoted = serde_json::to_string(trimmed).ok()?;
    Some(quoted[1..quoted.len() - 1].to_string())
}

/// A trimmed line shorter than this is not matched by content: `}`
/// and `);` appear in nearly every edit
const MIN_MATCH_CHARS: usize = 4;

/// Attribute `lines` through one SQL join per match kind over the
/// writes to the file by the sessions of `projects`, then one query
/// for the prompt spans.
async fn blame(
    ctx: &SessionContext,
    tables: &Tables,
    path: &Path,
    projects: &[String],
    lines: Vec<BlamedLine>,
) -> anyhow::Result<Vec<BlameRow>> {
    let path = path.to_str().context("path is not UTF-8")?;
    let quoted_path = path.replace('\'', "''");
    let wrote_path = format!("array_has(t.files_changed, '{quoted_path}')");
    let ids = session_ids(ctx, tables, projects).await?;
    let ids = writers_of(ctx, tables, &ids, &wrote_path).await?;
    if ids.is_empty() {
        return Ok(lines
            .into_iter()
            .map(|line| BlameRow {
                line,
                attribution: None,
            })
            .collect());
    }
    let id_list = id_list(&ids);
    ctx.register_batch("blame_line", blame_line_batch(&lines)?)?;
    // The union of the lines' windows as literal bounds on the scan,
    // which the session walk prunes on; the per-line windows in the
    // join condition reach no scan
    let ts = |ms: i64| ScalarValue::TimestampMillisecond(Some(ms), Some("UTC".into()));
    let before = lines.iter().map(|l| l.window_end).max().unwrap_or(0);
    let after = lines
        .iter()
        .map(|l| l.window_start)
        .reduce(|a, b| Some(a?.min(b?)))
        .flatten();
    let mut params = vec![ts(before)];
    let mut span = "t.result_timestamp <= $1".to_string();
    if let Some(after) = after {
        params.push(ts(after));
        span.push_str(" AND t.result_timestamp > $2");
    }
    // The winning call per line among the calls in the line's window
    // that satisfy `predicate`: a call the driver attributes to the
    // file over one it could not judge, then the latest
    let latest = |predicate: &str| {
        format!(
            "SELECT number, session_id, line, result_timestamp FROM ( \
               SELECT w.*, row_number() OVER ( \
                 PARTITION BY w.number \
                 ORDER BY w.judged DESC, w.result_timestamp DESC, w.line DESC \
               ) AS rank \
               FROM ( \
                 SELECT b.number, t.session_id, t.line, t.result_timestamp, \
                        coalesce({wrote_path}, false) AS judged \
                 FROM blame_line b JOIN {tool_use} t \
                   ON t.result_timestamp <= b.window_end \
                  AND (b.window_start IS NULL OR t.result_timestamp > b.window_start) \
                 WHERE t.session_id IN ({id_list}) AND {span} AND {predicate} \
               ) w \
             ) WHERE rank = 1",
            tool_use = tables.tool_use,
        )
    };
    // The text is the evidence for a content match, so a call the
    // driver could not judge (`files_changed` null, as for a shell
    // command whose result carries no diff) is admitted, within the
    // sessions known to have written the file. The file-level
    // fallback has only the path list to go on.
    let by_content = latest(&format!(
        "({wrote_path} OR t.files_changed IS NULL) \
         AND b.match_text IS NOT NULL AND strpos(t.input_raw, b.match_text) > 0"
    ));
    let by_file = latest(&wrote_path);

    let mut found = collect_matches(ctx, &by_content, &params, Match::Content).await?;
    let fallback = collect_matches(ctx, &by_file, &params, Match::File).await?;
    for (number, attribution) in fallback {
        found.entry(number).or_insert(attribution);
    }
    ctx.deregister_table("blame_line")?;

    let calls: Vec<(String, i64)> = found
        .values()
        .map(|a| (a.session_id.clone(), a.call_line))
        .collect();
    let spans = prompt_spans(ctx, tables, &calls).await?;
    for a in found.values_mut() {
        if let Some(start) = spans
            .iter()
            .find(|(id, call, _)| *id == a.session_id && *call == a.call_line)
            .map(|(_, _, start)| *start)
        {
            a.span_start = start;
        }
    }

    Ok(lines
        .into_iter()
        .map(|line| {
            let attribution = found.remove(&line.number);
            BlameRow { line, attribution }
        })
        .collect())
}

/// The `blame_line` table: one row per blamed line with its window
/// and the text to find in a call's input.
fn blame_line_batch(lines: &[BlamedLine]) -> anyhow::Result<RecordBatch> {
    let mut numbers = Int64Builder::new();
    let mut starts = TimestampMillisecondBuilder::new();
    let mut ends = TimestampMillisecondBuilder::new();
    let mut texts = StringBuilder::new();
    for l in lines {
        numbers.append_value(l.number as i64);
        starts.append_option(l.window_start);
        ends.append_value(l.window_end);
        texts.append_option(match_text(&l.text));
    }
    let ts = DataType::Timestamp(TimeUnit::Millisecond, Some("UTC".into()));
    let schema = Arc::new(Schema::new(vec![
        Field::new("number", DataType::Int64, false),
        Field::new("window_start", ts.clone(), true),
        Field::new("window_end", ts, false),
        Field::new("match_text", DataType::Utf8, true),
    ]));
    Ok(RecordBatch::try_new(
        schema,
        vec![
            Arc::new(numbers.finish()),
            Arc::new(starts.finish().with_timezone("UTC")),
            Arc::new(ends.finish().with_timezone("UTC")),
            Arc::new(texts.finish()),
        ],
    )?)
}

/// Run a `latest` query and read its rows as attributions by line
/// number.
async fn collect_matches(
    ctx: &SessionContext,
    sql: &str,
    params: &[ScalarValue],
    matched: Match,
) -> anyhow::Result<std::collections::BTreeMap<u64, Attribution>> {
    let batches = ctx
        .sql(sql)
        .await?
        .with_param_values(params.to_vec())?
        .collect()
        .await?;
    let mut out = std::collections::BTreeMap::new();
    for batch in &batches {
        let numbers = int_col(batch, 0);
        let session_ids = str_col(batch, 1);
        let call_lines = int_col(batch, 2);
        let timestamps = batch
            .column(3)
            .as_any()
            .downcast_ref::<TimestampMillisecondArray>()
            .context("result_timestamp column type")?;
        for i in 0..batch.num_rows() {
            out.insert(
                numbers.value(i) as u64,
                Attribution {
                    session_id: session_ids.value(i).to_string(),
                    call_line: call_lines.value(i),
                    span_start: call_lines.value(i),
                    timestamp: timestamps.value(i),
                    matched,
                },
            );
        }
    }
    Ok(out)
}

/// For each `(session_id, call_line)`, the line of the nearest human
/// prompt before the call: `(session_id, call_line, prompt_line)`.
async fn prompt_spans(
    ctx: &SessionContext,
    tables: &Tables,
    calls: &[(String, i64)],
) -> anyhow::Result<Vec<(String, i64, i64)>> {
    if calls.is_empty() {
        return Ok(Vec::new());
    }
    let mut ids = StringBuilder::new();
    let mut lines = Int64Builder::new();
    for (id, line) in calls {
        ids.append_value(id);
        lines.append_value(*line);
    }
    let schema = Arc::new(Schema::new(vec![
        Field::new("session_id", DataType::Utf8, false),
        Field::new("call_line", DataType::Int64, false),
    ]));
    let batch = RecordBatch::try_new(
        schema,
        vec![Arc::new(ids.finish()), Arc::new(lines.finish())],
    )?;
    ctx.register_batch("blame_call", batch)?;
    // The literal id list prunes the message walk to the calls'
    // sessions; the join condition alone does not reach the scan
    let mut ids: Vec<&str> = calls.iter().map(|(id, _)| id.as_str()).collect();
    ids.sort_unstable();
    ids.dedup();
    let id_list = ids
        .iter()
        .map(|id| format!("'{}'", id.replace('\'', "''")))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "SELECT c.session_id, c.call_line, max(m.line) \
         FROM blame_call c JOIN {message} m \
           ON m.session_id = c.session_id AND m.line < c.call_line \
         WHERE m.session_id IN ({id_list}) \
           AND m.type = 'user' AND m.subtype = 'text' \
         GROUP BY c.session_id, c.call_line",
        message = tables.message,
    );
    let batches = ctx.sql(&sql).await?.collect().await?;
    ctx.deregister_table("blame_call")?;
    let mut out = Vec::new();
    for batch in &batches {
        let ids = str_col(batch, 0);
        let calls = int_col(batch, 1);
        let prompts = int_col(batch, 2);
        for i in 0..batch.num_rows() {
            out.push((ids.value(i).to_string(), calls.value(i), prompts.value(i)));
        }
    }
    Ok(out)
}

fn str_col(batch: &RecordBatch, i: usize) -> &StringArray {
    batch
        .column(i)
        .as_any()
        .downcast_ref::<StringArray>()
        .expect("blame query projects a string column here")
}

fn int_col(batch: &RecordBatch, i: usize) -> &Int64Array {
    batch
        .column(i)
        .as_any()
        .downcast_ref::<Int64Array>()
        .expect("blame query projects an integer column here")
}

/// `<session> <span> (<time> <n>) <text>`, in the layout of `git
/// blame`. The session is its first eight characters. A file-level
/// match marks its span with `?`; a line no session wrote shows
/// dashes.
fn print_rows(rows: &[BlameRow]) {
    let width = rows.iter().map(|r| r.line.number).max().unwrap_or(0);
    let width = width.to_string().len();
    let span_width = rows
        .iter()
        .filter_map(|r| r.attribution.as_ref().map(|a| span_text(a).len()))
        .max()
        .unwrap_or(1);
    for row in rows {
        match &row.attribution {
            Some(a) => println!(
                "{} {:<sw$} ({} {:>w$}) {}",
                session_prefix(&a.session_id),
                span_text(a),
                format_time(a.timestamp),
                row.line.number,
                row.line.text,
                sw = span_width,
                w = width,
            ),
            None => println!(
                "{:<8} {:<sw$} ({:<16} {:>w$}) {}",
                "-",
                "-",
                "-",
                row.line.number,
                row.line.text,
                sw = span_width,
                w = width,
            ),
        }
    }
}

fn session_prefix(id: &str) -> String {
    id.chars().take(8).collect()
}

fn span_text(a: &Attribution) -> String {
    let mark = match a.matched {
        Match::Content => "",
        Match::File => "?",
    };
    if a.span_start == a.call_line {
        format!("{}{mark}", a.call_line)
    } else {
        format!("{}-{}{mark}", a.span_start, a.call_line)
    }
}

fn format_time(ms: i64) -> String {
    DateTime::<Utc>::from_timestamp_millis(ms)
        .map(|t| t.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "-".to_string())
}

fn json_rows(rows: &[BlameRow]) -> serde_json::Value {
    rows.iter()
        .map(|r| {
            json!({
                "line": r.line.number,
                "text": r.line.text,
                "session_id": r.attribution.as_ref().map(|a| &a.session_id),
                "session_prefix": r.attribution.as_ref().map(|a| session_prefix(&a.session_id)),
                "span": r.attribution.as_ref().map(|a| [a.span_start, a.call_line]),
                "timestamp": r.attribution.as_ref().map(|a| format_time(a.timestamp)),
                "match": r.attribution.as_ref().map(|a| match a.matched {
                    Match::Content => "content",
                    Match::File => "file",
                }),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gage_claude::session::encode_project_dir;
    use gage_session::SourceOptions;

    #[test]
    fn porcelain_carries_commit_times_to_later_lines() {
        let text = "\
aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 1 1 2
author A
author-time 1000
committer A
committer-time 1100
summary first
filename f.rs
\tfn main() {
aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 2 2
\t}
0000000000000000000000000000000000000000 3 3 1
author Not Committed Yet
committer-time 0
\t// new
";
        let entries = parse_porcelain(text).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].line, 1);
        assert_eq!(entries[0].text, "fn main() {");
        assert_eq!(entries[0].committer_time, Some(1100));
        assert_eq!(entries[1].committer_time, Some(1100));
        assert_eq!(entries[1].text, "}");
        assert!(is_uncommitted(&entries[2].sha));
    }

    #[test]
    fn match_text_is_the_trimmed_line_as_json_text() {
        assert_eq!(
            match_text("    let s = \"a\\b\";").as_deref(),
            Some(r#"let s = \"a\\b\";"#)
        );
        assert_eq!(match_text("  }"), None);
        assert_eq!(match_text(""), None);
    }

    /// Commit a file at a controlled time; the returned path is the
    /// file inside the repository.
    fn commit(root: &Path, name: &str, content: &str, epoch: i64, message: &str) -> PathBuf {
        let path = root.join(name);
        std::fs::write(&path, content).unwrap();
        let date = format!("{epoch} +0000");
        for args in [vec!["add", name], vec!["commit", "-q", "-m", message]] {
            let status = Command::new("git")
                .current_dir(root)
                .env("GIT_AUTHOR_DATE", &date)
                .env("GIT_COMMITTER_DATE", &date)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@x")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@x")
                .args(&args)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?}");
        }
        path
    }

    /// An Edit call and its result, as session lines, at `ts`.
    fn edit(line: u32, id: &str, path: &Path, new_string: &str, ts: &str) -> [String; 2] {
        let path = path.display().to_string();
        [
            serde_json::json!({
                "type": "assistant", "uuid": format!("a{line}"), "timestamp": ts,
                "message": {"role": "assistant", "content": [{
                    "type": "tool_use", "id": id, "name": "Edit",
                    "input": {"file_path": path, "old_string": "x", "new_string": new_string}
                }]}
            })
            .to_string(),
            serde_json::json!({
                "type": "user", "uuid": format!("u{line}"), "timestamp": ts,
                "message": {"role": "user", "content": [{
                    "type": "tool_result", "tool_use_id": id, "content": "ok"
                }]},
                "toolUseResult": {"filePath": path}
            })
            .to_string(),
        ]
    }

    /// A Bash call and a result without a diff, as session lines.
    fn shell(line: u32, id: &str, command: &str, ts: &str) -> [String; 2] {
        [
            serde_json::json!({
                "type": "assistant", "uuid": format!("a{line}"), "timestamp": ts,
                "message": {"role": "assistant", "content": [{
                    "type": "tool_use", "id": id, "name": "Bash",
                    "input": {"command": command}
                }]}
            })
            .to_string(),
            serde_json::json!({
                "type": "user", "uuid": format!("u{line}"), "timestamp": ts,
                "message": {"role": "user", "content": [{
                    "type": "tool_result", "tool_use_id": id, "content": ""
                }]},
                "toolUseResult": {"stdout": "", "stderr": "", "interrupted": false}
            })
            .to_string(),
        ]
    }

    fn prompt(line: u32, text: &str, ts: &str) -> String {
        serde_json::json!({
            "type": "user", "uuid": format!("p{line}"), "timestamp": ts,
            "message": {"role": "user", "content": text}
        })
        .to_string()
    }

    /// Two commits of one file. The second commit's line came from a
    /// session edit inside its window; the first commit's lines
    /// predate every session.
    #[tokio::test]
    async fn blame_finds_the_session_call_that_wrote_a_line() {
        gage_core::config::use_test_cache_root();
        let repo = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(repo.path()).unwrap();
        assert!(
            Command::new("git")
                .current_dir(&root)
                .args(["init", "-q"])
                .status()
                .unwrap()
                .success()
        );
        // 2025-01-01T00:00:00Z and 2025-01-02T00:00:00Z
        let path = commit(&root, "a.rs", "fn one() {}\n", 1_735_689_600, "one");
        commit(
            &root,
            "a.rs",
            "fn one() {}\nfn two_added_later() {}\n",
            1_735_776_000,
            "two",
        );

        let claude_root = tempfile::tempdir().unwrap();
        let native_id = "11111111-2222-3333-4444-777777777777";
        let project_dir = claude_root
            .path()
            .join("projects")
            .join(encode_project_dir(&root));
        std::fs::create_dir_all(&project_dir).unwrap();
        let mut lines = vec![prompt(1, "add a second function", "2025-01-01T10:00:00Z")];
        lines.extend(edit(
            2,
            "t1",
            &path,
            "fn one() {}\nfn two_added_later() {}\n",
            "2025-01-01T10:00:05Z",
        ));
        // A later edit to the same file after the second commit; it
        // is outside the window and must not win
        lines.extend(edit(
            4,
            "t2",
            &path,
            "fn two_added_later() {}\n",
            "2025-01-03T00:00:00Z",
        ));
        // A shell write whose result carries no diff: unjudged, but
        // its command text names the line it wrote
        lines.extend(shell(
            6,
            "t3",
            "printf 'fn three_by_shell() {}\\n' >> a.rs",
            "2025-01-03T01:00:00Z",
        ));
        std::fs::write(
            project_dir.join(format!("{native_id}.jsonl")),
            lines.join("\n") + "\n",
        )
        .unwrap();

        commit(
            &root,
            "a.rs",
            "fn one() {}\nfn two_added_later() {}\nfn three_by_shell() {}\n",
            1_735_866_000,
            "three",
        );

        let spec = format!("claude:{}", claude_root.path().display());
        let ctx = ContextBuilder::new(None).build().await;
        let blamed = blamed_lines(&path, None).unwrap();
        assert_eq!(blamed[0].window_start, None, "root commit: open start");
        assert!(
            blamed[1..].iter().all(|l| l.window_start.is_some()),
            "git bounds a line of a later commit"
        );
        let picked: LineSelection = "1,3".parse().unwrap();
        let picked = blamed_lines(&path, Some(&picked)).unwrap();
        assert_eq!(
            picked.iter().map(|l| l.number).collect::<Vec<_>>(),
            [1, 3],
            "each range of the selection reaches git"
        );
        let projects = {
            let source = source::driver_registry()
                .open_source(&SourceOptions::new(&spec))
                .unwrap();
            project_names(source.as_ref(), &path).unwrap()
        };
        let rows = blame(&ctx, &Tables::native(&spec), &path, &projects, blamed)
            .await
            .unwrap();

        assert_eq!(rows.len(), 3);
        let shell_write = rows[2].attribution.as_ref().unwrap();
        assert_eq!(rows[2].line.text, "fn three_by_shell() {}");
        assert_eq!(shell_write.call_line, 6);
        assert_eq!(shell_write.matched, Match::Content);
        assert_eq!(rows[0].line.text, "fn one() {}");
        assert_eq!(rows[0].attribution, None, "{:?}", rows[0]);

        let a = rows[1].attribution.as_ref().unwrap();
        assert_eq!(a.session_id, native_id);
        assert_eq!(a.call_line, 2);
        assert_eq!(a.span_start, 1);
        assert_eq!(a.matched, Match::Content);
        assert_eq!(format_time(a.timestamp), "2025-01-01 10:00");
    }

    /// Outside a repository the lines come from the file, every
    /// window open at the start, and the range applies.
    #[test]
    fn plain_lines_serve_a_file_git_does_not_track() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.txt");
        std::fs::write(&path, "one\ntwo\nthree\n").unwrap();
        let selection: LineSelection = "2-3".parse().unwrap();
        let lines = blamed_lines(&path, Some(&selection)).unwrap();
        assert_eq!(
            lines
                .iter()
                .map(|l| (l.number, l.text.as_str()))
                .collect::<Vec<_>>(),
            [(2, "two"), (3, "three")]
        );
        assert!(lines.iter().all(|l| l.window_start.is_none()));
    }
}
