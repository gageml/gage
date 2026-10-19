//! `blame(file [, lines])` and `native_blame(source, file [, lines])`
//! --- the session tool calls that wrote the lines of a file.
//!
//! Each line of the file is attributed to the earliest `tool_use`
//! call, among the calls to the file in the line's window, whose
//! input carries the line's text. A call the driver recorded as
//! changing the file is preferred over one it recorded no change
//! for, so a command that only read the text does not take the line
//! from the call that wrote it. Whitespace is collapsed on both sides
//! before the comparison, so a formatter's reflow of a line the call
//! wrote does not break the match. A line no call's input carries has
//! a null attribution. The `lines` column is the session selection to
//! read: from the human prompt before the call to the call itself.
//!
//! Under git, the commit behind each line bounds the window: after
//! the previous commit touching the file through the commit itself,
//! and an uncommitted line's window runs from the file's last commit
//! to now. A renamed file is followed, and each line is matched
//! against calls that wrote the file under the path it had at its
//! commit. Without git, or for a file git does not track, every call
//! to the file is a candidate. The sessions read are those of the
//! file's directory and its ancestors, as the driver names projects,
//! that wrote the file under any of its paths.

use std::any::Any;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use aho_corasick::AhoCorasick;
use async_trait::async_trait;
use datafusion::arrow::array::{
    Array, Int64Array, Int64Builder, ListArray, StringArray, StringBuilder,
    TimestampMillisecondArray, TimestampMillisecondBuilder,
};
use datafusion::arrow::datatypes::{DataType, Field, Schema, SchemaRef, TimeUnit};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::catalog::{Session, TableFunctionImpl};
use datafusion::common::ScalarValue;
use datafusion::datasource::{MemTable, TableProvider, TableType};
use datafusion::error::{DataFusionError, Result};
use datafusion::physical_plan::ExecutionPlan;
use datafusion::prelude::{Expr, SessionContext};
use gage_registry::driver::DriverRegistry;
use gage_session::{Source, SourceOptions};
use gage_store::url::LineSelection;
use gage_store::{Store, StoredSessionTable};

use crate::native::string_literal;
use crate::new_context;
use crate::patch;
use crate::scope::SessionSet;
use crate::stored_rows::{RowKind, StoredRowsTable};

/// `blame(file [, lines])` over the stored sessions. The source the
/// project names are resolved through keeps its derived state under
/// `cache_root`, or the default root when it is `None`.
pub fn blame_fn(
    store: Arc<Mutex<Store>>,
    cache_root: Option<PathBuf>,
) -> Arc<dyn TableFunctionImpl> {
    Arc::new(BlameFn {
        scope: Scope::Stored(store),
        cache_root,
    })
}

/// `native_blame(source, file [, lines])` over a native source, whose
/// derived state is kept under `cache_root`, or the default root when
/// it is `None`.
pub fn native_blame_fn(
    show_agents: bool,
    cache_root: Option<PathBuf>,
) -> Arc<dyn TableFunctionImpl> {
    Arc::new(BlameFn {
        scope: Scope::Native { show_agents },
        cache_root,
    })
}

pub fn blame_schema() -> SchemaRef {
    let ts = DataType::Timestamp(TimeUnit::Millisecond, Some("UTC".into()));
    Arc::new(Schema::new(vec![
        Field::new("line", DataType::Int64, false),
        Field::new("line_end", DataType::Int64, true),
        Field::new("session_id", DataType::Utf8, true),
        Field::new("session_lines", DataType::Utf8, true),
        Field::new("tool_use_id", DataType::Utf8, true),
        Field::new("tool_name", DataType::Utf8, true),
        Field::new("timestamp", ts, true),
        Field::new("rank", DataType::Int64, false),
        Field::new("match", DataType::Utf8, true),
    ]))
}

/// Which sessions a blame reads. The native scope's source is an
/// argument of the call; the stored scope names projects through the
/// default source's driver, the source stored sessions were added from.
#[derive(Debug, Clone)]
enum Scope {
    Native { show_agents: bool },
    Stored(Arc<Mutex<Store>>),
}

#[derive(Debug)]
struct BlameFn {
    scope: Scope,
    cache_root: Option<PathBuf>,
}

impl TableFunctionImpl for BlameFn {
    fn call(&self, args: &[Expr]) -> Result<Arc<dyn TableProvider>> {
        let (name, usage) = match self.scope {
            Scope::Native { .. } => ("native_blame", "native_blame(source, file [, lines])"),
            Scope::Stored(_) => ("blame", "blame(file [, lines])"),
        };
        let plan = |msg: String| DataFusionError::Plan(format!("{name}: {msg}"));
        let (source, rest) = match (&self.scope, args) {
            (Scope::Native { .. }, [source, rest @ ..]) => {
                let source = string_literal(source)
                    .ok_or_else(|| plan("source must be a string literal".into()))?;
                (Some(source), rest)
            }
            (Scope::Stored(_), rest) => (None, rest),
            _ => return Err(plan(format!("usage {usage}"))),
        };
        let (file, selection) = match rest {
            [file] => (file, None),
            [file, lines] => (file, Some(line_selection(lines).map_err(plan)?)),
            _ => return Err(plan(format!("usage {usage}"))),
        };
        let file =
            string_literal(file).ok_or_else(|| plan("file must be a string literal".into()))?;
        let path = std::fs::canonicalize(&file).map_err(|e| plan(format!("{file}: {e}")))?;
        Ok(Arc::new(BlameTable {
            scope: self.scope.clone(),
            source,
            cache_root: self.cache_root.clone(),
            path,
            selection,
        }))
    }
}

/// A line selection argument: an integer literal naming one line, or
/// a string literal in the selection syntax (`'42'`, `'10-20'`,
/// `'10-20,42'`).
fn line_selection(e: &Expr) -> std::result::Result<LineSelection, String> {
    let text = match e {
        Expr::Literal(ScalarValue::Int64(Some(n)), _) => n.to_string(),
        _ => string_literal(e)
            .ok_or_else(|| "lines must be an integer or a string literal".to_string())?,
    };
    text.parse().map_err(|e| format!("lines {text:?}: {e}"))
}

#[derive(Debug)]
struct BlameTable {
    scope: Scope,
    /// The native source spec; `None` in the stored scope
    source: Option<String>,
    cache_root: Option<PathBuf>,
    path: PathBuf,
    selection: Option<LineSelection>,
}

#[async_trait]
impl TableProvider for BlameTable {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn schema(&self) -> SchemaRef {
        blame_schema()
    }

    fn table_type(&self) -> TableType {
        TableType::Base
    }

    async fn scan(
        &self,
        state: &dyn Session,
        projection: Option<&Vec<usize>>,
        filters: &[Expr],
        limit: Option<usize>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        let lines = blamed_lines(&self.path, self.selection.as_ref())?;
        let (ctx, projects) = self.open(&lines).await?;
        let rows = blame(&ctx, &projects, lines, &self.scope).await?;
        MemTable::try_new(blame_schema(), vec![vec![blame_batch(&rows)?]])?
            .scan(state, projection, filters, limit)
            .await
    }
}

impl BlameTable {
    /// A context with the scope's `session`, `tool_use`, and `message`
    /// tables, and the names of the projects whose sessions could have
    /// written the file under any of its paths.
    async fn open(&self, lines: &[BlamedLine]) -> Result<(SessionContext, Vec<String>)> {
        let registry = DriverRegistry::builtin();
        let spec = self.source.clone().unwrap_or_default();
        let mut options = SourceOptions::new(&spec);
        let show_agents = match self.scope {
            Scope::Native { show_agents } => show_agents,
            Scope::Stored(_) => false,
        };
        options.show_agents(show_agents);
        if let Some(root) = &self.cache_root {
            options.cache_root(root);
        }
        let source = registry.open_source(&options).map_err(external)?;
        let projects = project_names(source.as_ref(), lines)?;
        let ctx = new_context(false, show_agents, self.cache_root.clone());
        let tables: [(&str, Arc<dyn TableProvider>); 3] = match &self.scope {
            Scope::Native { .. } => {
                let tables = source.tables().map_err(external)?;
                [
                    ("session", tables.session),
                    ("tool_use", tables.tool_use),
                    ("message", tables.message),
                ]
            }
            Scope::Stored(store) => {
                let sessions = Arc::new(SessionSet::all(Arc::clone(store)));
                [
                    (
                        "session",
                        Arc::new(StoredSessionTable::new(Arc::clone(store))),
                    ),
                    (
                        "tool_use",
                        Arc::new(StoredRowsTable::new(
                            RowKind::ToolUse,
                            Arc::clone(&sessions),
                        )),
                    ),
                    (
                        "message",
                        Arc::new(StoredRowsTable::new(RowKind::Message, sessions)),
                    ),
                ]
            }
        };
        for (name, table) in tables {
            ctx.register_table(name, table)?;
        }
        Ok((ctx, projects))
    }
}

/// The project names of each path's directory and every ancestor, as
/// `source` names them, without repeats.
fn project_names(source: &dyn Source, lines: &[BlamedLine]) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for path in distinct_paths(lines) {
        for dir in path.ancestors().skip(1) {
            let name = source.project_name(dir).map_err(external)?;
            if !names.contains(&name) {
                names.push(name);
            }
        }
    }
    Ok(names)
}

fn distinct_paths(lines: &[BlamedLine]) -> Vec<&Path> {
    let mut paths: Vec<&Path> = Vec::new();
    for line in lines {
        if !paths.contains(&line.path.as_path()) {
            paths.push(&line.path);
        }
    }
    paths
}

/// The lines to attribute, each with the window its call must fall in
/// and the path the file had at its commit. Git supplies both when
/// the file is in a repository it tracks; otherwise every line's
/// window is open-ended, the path is the file's own, and the
/// selection is applied here.
fn blamed_lines(path: &Path, selection: Option<&LineSelection>) -> Result<Vec<BlamedLine>> {
    match GitFile::open(path).and_then(|file| file.blame(selection)) {
        Ok(lines) => Ok(lines),
        // Not a repository, a file git does not track, or no git at
        // all: the windows are the only loss
        Err(e) => {
            tracing::debug!(path = %path.display(), "blame without git: {e}");
            plain_lines(path, selection)
        }
    }
}

fn plain_lines(path: &Path, selection: Option<&LineSelection>) -> Result<Vec<BlamedLine>> {
    let text = std::fs::read(path).map_err(|e| external_msg(format!("{}: {e}", path.display())))?;
    let text = String::from_utf8_lossy(&text);
    let now = now_ms();
    Ok(text
        .lines()
        .enumerate()
        .map(|(i, line)| (i as u64 + 1, line))
        .filter(|(n, _)| selection.is_none_or(|s| s.contains(*n)))
        .map(|(number, line)| BlamedLine {
            number,
            text: line.to_string(),
            path: path.to_path_buf(),
            window_start: None,
            window_end: now,
        })
        .collect())
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("the clock is after 1970")
        .as_millis() as i64
}

/// One line of the file with the window its call must fall in.
#[derive(Debug, Clone, PartialEq, Eq)]
struct BlamedLine {
    number: u64,
    text: String,
    /// Absolute path of the file at the line's commit
    path: PathBuf,
    /// Epoch milliseconds, exclusive, or `None` when nothing bounds
    /// the start
    window_start: Option<i64>,
    /// Epoch milliseconds, inclusive
    window_end: i64,
}

struct GitFile {
    root: PathBuf,
    path: PathBuf,
}

impl GitFile {
    fn open(path: &Path) -> std::result::Result<Self, String> {
        let dir = path
            .parent()
            .ok_or_else(|| format!("{} has no parent directory", path.display()))?;
        let root = git(dir, &["rev-parse", "--show-toplevel"])?;
        Ok(Self {
            root: PathBuf::from(root.trim_end()),
            path: path.to_path_buf(),
        })
    }

    /// The file's lines with their commits' windows and paths. Each
    /// range of the selection is one `-L` to git.
    fn blame(
        &self,
        selection: Option<&LineSelection>,
    ) -> std::result::Result<Vec<BlamedLine>, String> {
        let mut args = vec!["blame".to_string(), "--porcelain".to_string()];
        for &(start, end) in selection.map(LineSelection::ranges).unwrap_or_default() {
            args.push("-L".to_string());
            args.push(format!("{start},{end}"));
        }
        args.push("--".to_string());
        args.push(self.path_arg()?.to_string());
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let porcelain = git(&self.root, &args)?;
        let entries = parse_porcelain(&porcelain)?;

        let mut lines = Vec::with_capacity(entries.len());
        let mut windows: HashMap<String, (Option<i64>, i64)> = HashMap::new();
        for entry in entries {
            let window = match windows.get(&entry.sha) {
                Some(window) => *window,
                None => {
                    let window = self.window(&entry)?;
                    windows.insert(entry.sha.clone(), window);
                    window
                }
            };
            lines.push(BlamedLine {
                number: entry.line,
                text: entry.text,
                path: self.root.join(&entry.filename),
                window_start: window.0,
                window_end: window.1,
            });
        }
        Ok(lines)
    }

    fn path_arg(&self) -> std::result::Result<&str, String> {
        self.path
            .to_str()
            .ok_or_else(|| format!("{} is not UTF-8", self.path.display()))
    }

    /// The window of an entry's commit: after the previous commit
    /// touching the file, through the commit's own time. An
    /// uncommitted line's window runs from the file's last commit to
    /// now. The start is the previous commit's author time and the
    /// end this commit's committer time. A rewrite of history moves
    /// committer times later and can stamp consecutive commits with
    /// one committer time, so the author time is the earlier bound,
    /// the committer time the later one, and a rewrite only widens the
    /// window.
    fn window(&self, entry: &PorcelainEntry) -> std::result::Result<(Option<i64>, i64), String> {
        if is_uncommitted(&entry.sha) {
            let last = git(
                &self.root,
                &["log", "-1", "--format=%at", "HEAD", "--", self.path_arg()?],
            )?;
            return Ok((parse_seconds(&last), now_ms()));
        }
        let end = entry
            .committer_time
            .ok_or_else(|| format!("porcelain entry {} without committer-time", entry.sha))?;
        // A commit that added the file has no previous commit, and the
        // window is then open at the start
        let start = match &entry.previous {
            Some(previous) => {
                parse_seconds(&git(&self.root, &["log", "-1", "--format=%at", previous])?)
            }
            None => None,
        };
        Ok((start, end * 1000))
    }
}

/// Epoch milliseconds from git's `%at` output; `None` for no output.
fn parse_seconds(text: &str) -> Option<i64> {
    text.trim().parse::<i64>().ok().map(|s| s * 1000)
}

fn is_uncommitted(sha: &str) -> bool {
    sha.bytes().all(|b| b == b'0')
}

/// Run git in `dir` and return its stdout; a non-zero exit is an error
/// carrying stderr.
fn git(dir: &Path, args: &[&str]) -> std::result::Result<String, String> {
    let output = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .map_err(|e| format!("git {}: {e}", args.join(" ")))?;
    if !output.status.success() {
        return Err(format!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    String::from_utf8(output.stdout).map_err(|e| format!("git {}: {e}", args.join(" ")))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PorcelainEntry {
    sha: String,
    line: u64,
    text: String,
    committer_time: Option<i64>,
    /// Path of the file at the commit, relative to the repository root
    filename: String,
    /// The previous commit touching the file, when there is one
    previous: Option<String>,
}

/// Parse `git blame --porcelain`. Each line opens with
/// `<sha> <orig> <final> [<count>]`, carries the commit's attributes
/// the first time the commit appears, and ends with the tab-prefixed
/// content line. The attributes are carried to every later line of
/// the same commit.
fn parse_porcelain(text: &str) -> std::result::Result<Vec<PorcelainEntry>, String> {
    #[derive(Default, Clone)]
    struct Attrs {
        committer_time: Option<i64>,
        filename: Option<String>,
        previous: Option<String>,
    }
    let mut entries = Vec::new();
    let mut known: HashMap<String, Attrs> = HashMap::new();
    let mut lines = text.lines();
    while let Some(header) = lines.next() {
        let mut parts = header.split(' ');
        let sha = parts.next().unwrap_or_default().to_string();
        let (Some(_orig), Some(line)) = (parts.next(), parts.next()) else {
            return Err(format!("unexpected blame header {header:?}"));
        };
        let line: u64 = line
            .parse()
            .map_err(|e| format!("blame header {header:?}: {e}"))?;
        let mut attrs = known.get(&sha).cloned().unwrap_or_default();
        let text = loop {
            let Some(next) = lines.next() else {
                return Err("blame output ends inside an entry".to_string());
            };
            if let Some(content) = next.strip_prefix('\t') {
                break content.to_string();
            }
            if let Some(t) = next.strip_prefix("committer-time ") {
                attrs.committer_time = Some(
                    t.trim()
                        .parse::<i64>()
                        .map_err(|e| format!("committer-time {t:?}: {e}"))?,
                );
            } else if let Some(f) = next.strip_prefix("filename ") {
                attrs.filename = Some(f.to_string());
            } else if let Some(p) = next.strip_prefix("previous ") {
                attrs.previous = p.split(' ').next().map(str::to_string);
            }
        };
        let filename = attrs
            .filename
            .clone()
            .ok_or_else(|| format!("blame entry {sha} without filename"))?;
        known.insert(sha.clone(), attrs.clone());
        entries.push(PorcelainEntry {
            sha,
            line,
            text,
            committer_time: attrs.committer_time,
            filename,
            previous: attrs.previous,
        });
    }
    Ok(entries)
}

/// The evidence tying a call to a line, strongest first. Ranking
/// orders candidates by this, then by the earliest call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Tier {
    /// The call's recorded patch added the line
    Patch,
    /// The call's input carries the line and the driver judged the
    /// call wrote the file
    Input,
    /// The call's input carries the line and the driver could not
    /// judge the write (a shell command without a recorded diff)
    Command,
    /// The line itself did not match, but both neighbors are patch
    /// matches of this one call
    Neighbor,
}

impl Tier {
    fn as_str(self) -> &'static str {
        match self {
            Tier::Patch => "patch",
            Tier::Input => "input",
            Tier::Command => "command",
            Tier::Neighbor => "neighbor",
        }
    }
}

/// One call a line could be attributed to.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Candidate {
    session_id: String,
    tool_use_id: String,
    tool_name: String,
    /// Line of the call in the session
    call_line: i64,
    /// Line of the human prompt the selection starts at; the call line
    /// when no prompt precedes it
    prompt_line: i64,
    /// Epoch milliseconds of the call's result
    timestamp: i64,
    tier: Tier,
}

impl Candidate {
    /// Ranking key: strongest tier, then earliest call
    fn rank_key(&self) -> (Tier, i64, i64) {
        (self.tier, self.timestamp, self.call_line)
    }

    /// Whether `other` is the same call
    fn same_call(&self, other: &Candidate) -> bool {
        self.session_id == other.session_id && self.tool_use_id == other.tool_use_id
    }
}

/// One output row: a contiguous line range attributed to one call at
/// one rank. An unattributed line yields no row.
#[derive(Debug, Clone)]
struct OutRow {
    line: u64,
    line_end: Option<u64>,
    candidate: Candidate,
    rank: i64,
}

/// Attribute `lines` to the calls that wrote them. The scoped pass
/// reads the sessions of `projects` that the driver judged wrote the
/// file. When it attributes nothing --- a file outside the projects,
/// or one only a shell command touched --- a wide pass reads every
/// session whose summary names the file, written or merely referenced.
/// Each line keeps every matching call, ranked by evidence; a line
/// between two patch matches of one call takes that call.
async fn blame(
    ctx: &SessionContext,
    projects: &[String],
    lines: Vec<BlamedLine>,
    scope: &Scope,
) -> Result<Vec<OutRow>> {
    let paths: Vec<String> = distinct_paths(&lines)
        .into_iter()
        .map(|p| {
            p.to_str()
                .map(str::to_string)
                .ok_or_else(|| external_msg(format!("{} is not UTF-8", p.display())))
        })
        .collect::<Result<_>>()?;
    // An empty file, or a selection off the end of one, has no lines to
    // attribute and no paths to query
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    let has = |col: &str| {
        paths
            .iter()
            .map(|p| format!("array_has({col}, {})", quote(p)))
            .collect::<Vec<_>>()
            .join(" OR ")
    };
    let wrote_path = has("files_changed");

    let scoped = match scope {
        Scope::Native { .. } => writer_sessions(ctx, projects, &wrote_path).await?,
        Scope::Stored(_) => {
            let scoped = session_ids(ctx, projects).await?;
            writers_of(ctx, &scoped, &wrote_path).await?
        }
    };
    let mut found = if scoped.is_empty() {
        BTreeMap::new()
    } else {
        match_calls(ctx, &scoped, &wrote_path, &lines).await?
    };

    if found.is_empty() {
        let wide = wide_sessions(ctx, &wrote_path, &has("files_referenced")).await?;
        if !wide.is_empty() {
            found = match_calls(ctx, &wide, &wrote_path, &lines).await?;
        }
    }

    add_neighbors(&mut found, &lines);
    let prompts = prompt_lines(ctx, &found).await?;
    for cands in found.values_mut() {
        for c in cands.iter_mut() {
            if let Some(session) = prompts.get(&c.session_id)
                && let Some(prompt) = session.range(..c.call_line).next_back()
            {
                c.prompt_line = *prompt;
            }
        }
    }
    Ok(build_rows(found))
}

/// Every session whose summary names the file, in any project: a
/// judged write, or a path a call's input referred to. Empty for a
/// scope whose summaries carry no path lists, as the stored sessions
/// do not yet, so the wide pass is a native fallback.
async fn wide_sessions(
    ctx: &SessionContext,
    wrote_path: &str,
    referenced_path: &str,
) -> Result<Vec<String>> {
    let sql = format!("SELECT id FROM session WHERE ({wrote_path}) OR ({referenced_path})");
    strings(ctx, &sql, &[]).await
}

/// Give an unattributed line the call that added both its neighbors,
/// when one patch match added the line above and the line below: the
/// line between them was part of the same change. Short lines such as
/// `}` reach attribution only here.
fn add_neighbors(found: &mut BTreeMap<u64, Vec<Candidate>>, lines: &[BlamedLine]) {
    let strongest: HashMap<u64, Candidate> = found
        .iter()
        .filter_map(|(&n, cands)| {
            cands
                .iter()
                .min_by(|a, b| a.rank_key().cmp(&b.rank_key()))
                .map(|c| (n, c.clone()))
        })
        .collect();
    let mut adds: Vec<(u64, Candidate)> = Vec::new();
    for line in lines {
        let n = line.number;
        if found.contains_key(&n) || n == 0 {
            continue;
        }
        let (Some(above), Some(below)) = (strongest.get(&(n - 1)), strongest.get(&(n + 1))) else {
            continue;
        };
        if above.tier == Tier::Patch && below.tier == Tier::Patch && above.same_call(below) {
            let mut c = above.clone();
            c.tier = Tier::Neighbor;
            adds.push((n, c));
        }
    }
    for (n, c) in adds {
        found.entry(n).or_default().push(c);
    }
}

/// Rank each line's candidates, then fold consecutive lines with the
/// same call and tier at the same rank into one range row, sorted by
/// line then rank.
fn build_rows(mut found: BTreeMap<u64, Vec<Candidate>>) -> Vec<OutRow> {
    for cands in found.values_mut() {
        cands.sort_by_key(Candidate::rank_key);
    }
    let depth = found.values().map(Vec::len).max().unwrap_or(0);
    let mut out = Vec::new();
    for r in 0..depth {
        let ranked: Vec<(u64, &Candidate)> = found
            .iter()
            .filter_map(|(&n, cands)| cands.get(r).map(|c| (n, c)))
            .collect();
        let mut i = 0;
        while let Some(&(start, cand)) = ranked.get(i) {
            let mut end = start;
            let mut j = i + 1;
            while let Some(&(n, c)) = ranked.get(j) {
                if n == end + 1 && c.same_call(cand) && c.tier == cand.tier {
                    end = n;
                    j += 1;
                } else {
                    break;
                }
            }
            out.push(OutRow {
                line: start,
                line_end: (end > start).then_some(end),
                candidate: cand.clone(),
                rank: r as i64 + 1,
            });
            i = j;
        }
    }
    out.sort_by_key(|row| (row.line, row.rank));
    out
}

/// A SQL string literal.
fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Literal ids for an `IN` list. Literal ids reach the session walk
/// as path pruning; a subquery over the session table does not.
fn id_list(ids: &[String]) -> String {
    ids.iter()
        .map(|id| quote(id))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The sessions of `projects` with a write the driver attributes to
/// the file, read from the session table. A native session's summary
/// lists the paths it wrote, so the writers are found without opening
/// a session. A stored session's summary lists no paths yet, so the
/// stored scope finds its writers through `writers_of`.
async fn writer_sessions(
    ctx: &SessionContext,
    projects: &[String],
    wrote_path: &str,
) -> Result<Vec<String>> {
    if projects.is_empty() {
        return Ok(Vec::new());
    }
    let sql = format!(
        "SELECT id FROM session WHERE project IN ({}) AND ({wrote_path})",
        id_list(projects)
    );
    strings(ctx, &sql, &[]).await
}

/// The ids of the sessions in `projects`, read through the session
/// table so the walk prunes by project.
async fn session_ids(ctx: &SessionContext, projects: &[String]) -> Result<Vec<String>> {
    if projects.is_empty() {
        return Ok(Vec::new());
    }
    let sql = format!(
        "SELECT id FROM session WHERE project IN ({})",
        id_list(projects)
    );
    strings(ctx, &sql, &[]).await
}

/// The sessions among `ids` with a write the driver attributes to the
/// file. The `array_has` literals reach the walk, which skips every
/// session whose summary lists none of the paths before parsing it.
async fn writers_of(ctx: &SessionContext, ids: &[String], wrote_path: &str) -> Result<Vec<String>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let sql = format!(
        "SELECT DISTINCT session_id FROM tool_use \
         WHERE session_id IN ({ids}) AND ({wrote_path})",
        ids = id_list(ids),
    );
    strings(ctx, &sql, &[]).await
}

async fn strings(ctx: &SessionContext, sql: &str, params: &[ScalarValue]) -> Result<Vec<String>> {
    let batches = ctx
        .sql(sql)
        .await?
        .with_param_values(params.to_vec())?
        .collect()
        .await?;
    let mut out = Vec::new();
    for batch in &batches {
        let col = str_col(batch, 0);
        out.extend((0..batch.num_rows()).map(|i| col.value(i).to_string()));
    }
    Ok(out)
}

/// A trimmed, whitespace-collapsed line shorter than this is not
/// matched: `}` and `);` appear in nearly every edit
const MIN_MATCH_CHARS: usize = 4;

/// Every call of `ids`, in the union of the lines' windows, that a
/// line's text matches: its recorded patch added the line (`patch`),
/// its input carries the line and the driver judged it wrote the file
/// (`input`), or its input carries the line and the driver could not
/// judge the write (`command`). The calls read are those the driver
/// attributes to one of the file's paths, or could not judge
/// (`files_changed` null, as for a shell command whose result carries
/// no diff). Each line keeps one candidate per call, at the call's
/// strongest tier. Each call's input and patch are scanned once for
/// every line's text.
async fn match_calls(
    ctx: &SessionContext,
    ids: &[String],
    wrote_path: &str,
    lines: &[BlamedLine],
) -> Result<BTreeMap<u64, Vec<Candidate>>> {
    // Distinct line texts, each with the lines that carry it
    let mut patterns: Vec<String> = Vec::new();
    let mut by_pattern: Vec<Vec<&BlamedLine>> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    for line in lines {
        let Some(text) = match_text(&line.text) else {
            continue;
        };
        match index.get(&text) {
            Some(&p) => {
                if let Some(group) = by_pattern.get_mut(p) {
                    group.push(line);
                }
            }
            None => {
                index.insert(text.clone(), patterns.len());
                patterns.push(text);
                by_pattern.push(vec![line]);
            }
        }
    }
    let mut found: BTreeMap<u64, Vec<Candidate>> = BTreeMap::new();
    if patterns.is_empty() {
        return Ok(found);
    }
    let searcher = AhoCorasick::new(&patterns).map_err(|e| external_msg(e.to_string()))?;

    // The union of the lines' windows as literal bounds on the scan,
    // which the session walk prunes on
    let ts = |ms: i64| ScalarValue::TimestampMillisecond(Some(ms), Some("UTC".into()));
    let before = lines.iter().map(|l| l.window_end).max().unwrap_or(0);
    let after = lines
        .iter()
        .map(|l| l.window_start)
        .reduce(|a, b| Some(a?.min(b?)))
        .flatten();
    let mut params = vec![ts(before)];
    let mut span = "result_timestamp <= $1".to_string();
    if let Some(after) = after {
        params.push(ts(after));
        span.push_str(" AND result_timestamp > $2");
    }
    let sql = format!(
        "SELECT session_id, id, name, line, result_timestamp, input_raw, files_changed, result_raw \
         FROM tool_use \
         WHERE session_id IN ({ids}) AND {span} \
           AND ({wrote_path} OR files_changed IS NULL) \
         ORDER BY result_timestamp, line",
        ids = id_list(ids),
    );
    let batches = ctx
        .sql(&sql)
        .await?
        .with_param_values(params)?
        .collect()
        .await?;

    let mut in_input: BTreeSet<usize> = BTreeSet::new();
    let mut in_patch: BTreeSet<usize> = BTreeSet::new();
    for batch in &batches {
        let session_ids = str_col(batch, 0);
        let call_ids = str_col(batch, 1);
        let names = str_col(batch, 2);
        let call_lines = int_col(batch, 3);
        let timestamps = batch
            .column(4)
            .as_any()
            .downcast_ref::<TimestampMillisecondArray>()
            .ok_or_else(|| {
                external_msg("result_timestamp is not a millisecond timestamp".into())
            })?;
        let inputs = str_col(batch, 5);
        let files = batch
            .column(6)
            .as_any()
            .downcast_ref::<ListArray>()
            .ok_or_else(|| external_msg("files_changed is not a list".into()))?;
        let results = str_col(batch, 7);
        for i in 0..batch.num_rows() {
            if timestamps.is_null(i) {
                continue;
            }
            let timestamp = timestamps.value(i);
            let input = normalize_json_text(inputs.value(i));
            in_input.clear();
            in_input.extend(
                searcher
                    .find_overlapping_iter(&input)
                    .map(|m| m.pattern().as_usize()),
            );
            let added: String = patch::hunks(results.value(i))
                .iter()
                .flat_map(|h| h.lines_with('+'))
                .map(json_normalized)
                .collect::<Vec<_>>()
                .join("\n");
            in_patch.clear();
            in_patch.extend(
                searcher
                    .find_overlapping_iter(&added)
                    .map(|m| m.pattern().as_usize()),
            );
            if in_input.is_empty() && in_patch.is_empty() {
                continue;
            }
            let items;
            let changed: Option<Vec<&str>> = if files.is_null(i) {
                None
            } else {
                items = files.value(i);
                let items = items
                    .as_any()
                    .downcast_ref::<StringArray>()
                    .expect("files_changed items are strings");
                Some(
                    (0..items.len())
                        .filter(|&j| !items.is_null(j))
                        .map(|j| items.value(j))
                        .collect(),
                )
            };
            for &p in in_patch.iter().chain(in_input.iter()) {
                for &line in by_pattern.get(p).into_iter().flatten() {
                    if timestamp > line.window_end
                        || line.window_start.is_some_and(|start| timestamp <= start)
                    {
                        continue;
                    }
                    let wrote_line_path = match &changed {
                        None => true,
                        Some(changed) => line.path.to_str().is_some_and(|p| changed.contains(&p)),
                    };
                    let tier = if in_patch.contains(&p) && wrote_line_path {
                        Tier::Patch
                    } else if in_input.contains(&p) {
                        match &changed {
                            None => Tier::Command,
                            Some(_) if wrote_line_path => Tier::Input,
                            Some(_) => continue,
                        }
                    } else {
                        continue;
                    };
                    record(
                        &mut found,
                        line.number,
                        Candidate {
                            session_id: session_ids.value(i).to_string(),
                            tool_use_id: call_ids.value(i).to_string(),
                            tool_name: names.value(i).to_string(),
                            call_line: call_lines.value(i),
                            prompt_line: call_lines.value(i),
                            timestamp,
                            tier,
                        },
                    );
                }
            }
        }
    }
    Ok(found)
}

/// Add `cand` for `line`, or lower an existing candidate for the same
/// call to its stronger tier. A line matching one call through both
/// its patch and its input keeps the patch.
fn record(found: &mut BTreeMap<u64, Vec<Candidate>>, line: u64, cand: Candidate) {
    let cands = found.entry(line).or_default();
    match cands.iter_mut().find(|c| c.same_call(&cand)) {
        Some(existing) => existing.tier = existing.tier.min(cand.tier),
        None => cands.push(cand),
    }
}

/// A plain line as it reads inside a normalized JSON input: escaped as
/// a JSON string, then whitespace-collapsed, so the patterns built
/// from file lines match a patch's added lines as they match an input.
fn json_normalized(line: &str) -> String {
    let quoted = serde_json::to_string(line).unwrap_or_default();
    normalize_json_text(&quoted[1..quoted.len().saturating_sub(1).max(1)])
}

/// A line's text as it appears inside a JSON string with its
/// whitespace collapsed, for matching against a normalized input.
/// Trimmed, since an edit's indentation may differ from the file's
/// after formatting; collapsed, since a formatter may reflow the line.
fn match_text(line: &str) -> Option<String> {
    let collapsed = collapse_whitespace(line);
    if collapsed.chars().count() < MIN_MATCH_CHARS {
        return None;
    }
    let quoted = serde_json::to_string(&collapsed).ok()?;
    Some(quoted[1..quoted.len() - 1].to_string())
}

/// `text` with every run of whitespace as one space and none at the
/// ends.
fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// JSON text with every run of whitespace --- literal spaces and tabs,
/// and the escapes `\n`, `\t`, and `\r` --- as one space. Other
/// escapes are kept as written, so an escaped quote or backslash
/// still matches the line's JSON form.
fn normalize_json_text(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    let mut space = false;
    while let Some(c) = chars.next() {
        let is_space = match c {
            ' ' | '\t' | '\n' | '\r' => true,
            '\\' => match chars.peek() {
                Some('n' | 't' | 'r') => {
                    chars.next();
                    true
                }
                Some(&next) => {
                    chars.next();
                    out.push('\\');
                    out.push(next);
                    space = false;
                    continue;
                }
                None => {
                    out.push('\\');
                    space = false;
                    continue;
                }
            },
            _ => false,
        };
        if is_space {
            if !space {
                out.push(' ');
                space = true;
            }
        } else {
            out.push(c);
            space = false;
        }
    }
    out
}

/// The lines of the human prompts in the attributed sessions, by
/// session.
async fn prompt_lines(
    ctx: &SessionContext,
    found: &BTreeMap<u64, Vec<Candidate>>,
) -> Result<HashMap<String, BTreeSet<i64>>> {
    let ids: BTreeSet<&str> = found
        .values()
        .flatten()
        .map(|c| c.session_id.as_str())
        .collect();
    let mut out: HashMap<String, BTreeSet<i64>> = HashMap::new();
    if ids.is_empty() {
        return Ok(out);
    }
    let ids: Vec<String> = ids.into_iter().map(str::to_string).collect();
    let sql = format!(
        "SELECT session_id, line FROM message \
         WHERE session_id IN ({}) AND type = 'user' AND subtype = 'text'",
        id_list(&ids)
    );
    let batches = ctx.sql(&sql).await?.collect().await?;
    for batch in &batches {
        let sessions = str_col(batch, 0);
        let lines = int_col(batch, 1);
        for i in 0..batch.num_rows() {
            out.entry(sessions.value(i).to_string())
                .or_default()
                .insert(lines.value(i));
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

fn blame_batch(rows: &[OutRow]) -> Result<RecordBatch> {
    let mut line = Int64Builder::new();
    let mut line_end = Int64Builder::new();
    let mut session_id = StringBuilder::new();
    let mut session_lines = StringBuilder::new();
    let mut tool_use_id = StringBuilder::new();
    let mut tool_name = StringBuilder::new();
    let mut timestamp = TimestampMillisecondBuilder::new();
    let mut rank = Int64Builder::new();
    let mut tier = StringBuilder::new();
    for row in rows {
        let c = &row.candidate;
        line.append_value(row.line as i64);
        line_end.append_option(row.line_end.map(|n| n as i64));
        session_id.append_value(&c.session_id);
        session_lines.append_value(selection_text(c.prompt_line, c.call_line));
        tool_use_id.append_value(&c.tool_use_id);
        tool_name.append_value(&c.tool_name);
        timestamp.append_value(c.timestamp);
        rank.append_value(row.rank);
        tier.append_value(c.tier.as_str());
    }
    Ok(RecordBatch::try_new(
        blame_schema(),
        vec![
            Arc::new(line.finish()),
            Arc::new(line_end.finish()),
            Arc::new(session_id.finish()),
            Arc::new(session_lines.finish()),
            Arc::new(tool_use_id.finish()),
            Arc::new(tool_name.finish()),
            Arc::new(timestamp.finish().with_timezone("UTC")),
            Arc::new(rank.finish()),
            Arc::new(tier.finish()),
        ],
    )?)
}

/// The session selection from the prompt through the call, in the
/// syntax a session target carries.
fn selection_text(prompt_line: i64, call_line: i64) -> String {
    if prompt_line == call_line {
        call_line.to_string()
    } else {
        format!("{prompt_line}-{call_line}")
    }
}

fn external(e: gage_session::DriverError) -> DataFusionError {
    DataFusionError::External(Box::new(e))
}

fn external_msg(msg: String) -> DataFusionError {
    DataFusionError::External(msg.into())
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use datafusion::prelude::lit;
    use gage_store::SessionStore;

    use super::*;
    use crate::ContextBuilder;

    #[test]
    fn porcelain_carries_commit_attributes_to_later_lines() {
        let text = "\
aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 1 1 2
author A
author-time 1000
committer A
committer-time 1100
summary first
previous bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb old/f.rs
filename f.rs
\tfn main() {
aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 2 2
\t}
0000000000000000000000000000000000000000 3 3 1
author Not Committed Yet
committer-time 0
filename f.rs
\t// new
";
        let entries = parse_porcelain(text).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].line, 1);
        assert_eq!(entries[0].text, "fn main() {");
        assert_eq!(entries[0].committer_time, Some(1100));
        assert_eq!(entries[0].filename, "f.rs");
        assert_eq!(
            entries[0].previous.as_deref(),
            Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
        );
        assert_eq!(entries[1].committer_time, Some(1100));
        assert_eq!(entries[1].filename, "f.rs");
        assert_eq!(entries[1].text, "}");
        assert!(is_uncommitted(&entries[2].sha));
        assert_eq!(entries[2].previous, None);
    }

    #[test]
    fn match_text_is_the_collapsed_line_as_json_text() {
        assert_eq!(
            match_text("    let s = \"a\\b\";").as_deref(),
            Some(r#"let s = \"a\\b\";"#)
        );
        assert_eq!(
            match_text("fn four(a: i32,\t  b: i32)").as_deref(),
            Some("fn four(a: i32, b: i32)")
        );
        assert_eq!(match_text("  }"), None);
        assert_eq!(match_text(""), None);
    }

    #[test]
    fn normalize_json_text_collapses_whitespace_and_keeps_other_escapes() {
        assert_eq!(
            normalize_json_text(r#"{"new_string":"fn four(a: i32,\n    b: i32) {}\n"}"#),
            r#"{"new_string":"fn four(a: i32, b: i32) {} "}"#
        );
        assert_eq!(
            normalize_json_text(r#"a\"b\\n\tc"#),
            r#"a\"b\\n c"#,
            "an escaped backslash before n is not a newline"
        );
    }

    #[test]
    fn line_selection_accepts_an_integer_or_selection_text() {
        assert_eq!(line_selection(&lit(42i64)).unwrap().ranges(), [(42, 42)]);
        assert_eq!(
            line_selection(&lit("10-20,42")).unwrap().ranges(),
            [(10, 20), (42, 42)]
        );
        assert!(line_selection(&lit("20-10")).is_err());
        assert!(line_selection(&lit(1.5)).is_err());
    }

    #[test]
    fn blame_arguments_are_checked_at_plan_time() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.txt");
        std::fs::write(&file, "x\n").unwrap();
        let file = file.to_str().unwrap();
        let native = native_blame_fn(false, None);
        let err = native.call(&[lit(file)]).err().unwrap();
        assert!(
            err.to_string()
                .contains("usage native_blame(source, file [, lines])"),
            "{err}"
        );
        let err = native
            .call(&[lit(""), lit("/nonexistent/f.txt")])
            .err()
            .unwrap();
        assert!(err.to_string().contains("/nonexistent/f.txt"), "{err}");
        assert!(native.call(&[lit(""), lit(file), lit(1i64)]).is_ok());
        let err = native
            .call(&[lit(""), lit(file), lit(1i64), lit(2i64)])
            .err()
            .unwrap();
        assert!(err.to_string().contains("usage"), "{err}");
    }

    fn git(root: &Path, args: &[&str]) {
        let status = Command::new("git")
            .current_dir(root)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@x")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@x")
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    /// Commit a file at a controlled time; the returned path is the
    /// file inside the repository.
    fn commit(root: &Path, name: &str, content: &str, epoch: i64, message: &str) -> PathBuf {
        let path = root.join(name);
        std::fs::write(&path, content).unwrap();
        git(root, &["add", name]);
        commit_staged(root, epoch, message);
        path
    }

    /// Commit the index at a controlled time.
    fn commit_staged(root: &Path, epoch: i64, message: &str) {
        let date = format!("{epoch} +0000");
        let status = Command::new("git")
            .current_dir(root)
            .env("GIT_AUTHOR_DATE", &date)
            .env("GIT_COMMITTER_DATE", &date)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@x")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@x")
            .args(["commit", "-q", "-m", message])
            .status()
            .unwrap();
        assert!(status.success(), "git commit {message}");
    }

    /// An Edit call and its result, as session lines, at `ts`.
    fn edit(n: u32, id: &str, path: &Path, old: &str, new: &str, ts: &str) -> [String; 2] {
        let path = path.display().to_string();
        // A line present in `new` but not `old` is added (`+`); a line
        // in both is context, as Claude Code records in structuredPatch
        let old_lines: Vec<&str> = old.lines().collect();
        let patch: Vec<String> = new
            .lines()
            .map(|l| {
                if old_lines.contains(&l) {
                    format!(" {l}")
                } else {
                    format!("+{l}")
                }
            })
            .collect();
        [
            serde_json::json!({
                "type": "assistant", "uuid": format!("a{n}"), "timestamp": ts,
                "message": {"role": "assistant", "content": [{
                    "type": "tool_use", "id": id, "name": "Edit",
                    "input": {"file_path": path, "old_string": old, "new_string": new}
                }]}
            })
            .to_string(),
            serde_json::json!({
                "type": "user", "uuid": format!("u{n}"), "timestamp": ts,
                "message": {"role": "user", "content": [{
                    "type": "tool_result", "tool_use_id": id, "content": "ok"
                }]},
                "toolUseResult": {"filePath": path, "structuredPatch": [{"lines": patch}]}
            })
            .to_string(),
        ]
    }

    /// A Bash call and a result without a diff, as session lines.
    fn shell(n: u32, id: &str, command: &str, ts: &str) -> [String; 2] {
        [
            serde_json::json!({
                "type": "assistant", "uuid": format!("a{n}"), "timestamp": ts,
                "message": {"role": "assistant", "content": [{
                    "type": "tool_use", "id": id, "name": "Bash",
                    "input": {"command": command}
                }]}
            })
            .to_string(),
            serde_json::json!({
                "type": "user", "uuid": format!("u{n}"), "timestamp": ts,
                "message": {"role": "user", "content": [{
                    "type": "tool_result", "tool_use_id": id, "content": ""
                }]},
                "toolUseResult": {"stdout": "", "stderr": "", "interrupted": false}
            })
            .to_string(),
        ]
    }

    fn prompt(n: u32, text: &str, ts: &str) -> String {
        serde_json::json!({
            "type": "user", "uuid": format!("p{n}"), "timestamp": ts,
            "message": {"role": "user", "content": text}
        })
        .to_string()
    }

    /// Write a session of `lines` for the project at `project_root`
    /// into a Claude root, returning the source spec.
    fn write_session(
        claude_root: &Path,
        project_root: &Path,
        native_id: &str,
        lines: &[String],
    ) -> String {
        let spec = format!("claude:{}", claude_root.display());
        let source = DriverRegistry::builtin()
            .open_source(SourceOptions::new(&spec).cache_root(claude_root.join("cache")))
            .unwrap();
        let project_dir = claude_root
            .join("projects")
            .join(source.project_name(project_root).unwrap());
        std::fs::create_dir_all(&project_dir).unwrap();
        std::fs::write(
            project_dir.join(format!("{native_id}.jsonl")),
            lines.join("\n") + "\n",
        )
        .unwrap();
        spec
    }

    /// One blame output row, flattened for assertions.
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Out {
        line: i64,
        line_end: Option<i64>,
        session: Option<String>,
        session_lines: Option<String>,
        tool_use_id: Option<String>,
        rank: i64,
        tier: Option<String>,
    }

    async fn rows(ctx: &SessionContext, from: &str) -> Vec<Out> {
        let sql = format!(
            "SELECT line, line_end, session_id, session_lines, tool_use_id, \
             tool_name, timestamp, rank, match \
             FROM {from} ORDER BY line, rank"
        );
        let batches = ctx.sql(&sql).await.unwrap().collect().await.unwrap();
        let mut out = Vec::new();
        for batch in &batches {
            let opt =
                |i: usize, col: &StringArray| (!col.is_null(i)).then(|| col.value(i).to_string());
            let ends = batch
                .column(1)
                .as_any()
                .downcast_ref::<Int64Array>()
                .unwrap();
            for i in 0..batch.num_rows() {
                out.push(Out {
                    line: int_col(batch, 0).value(i),
                    line_end: (!ends.is_null(i)).then(|| ends.value(i)),
                    session: opt(i, str_col(batch, 2)),
                    session_lines: opt(i, str_col(batch, 3)),
                    tool_use_id: opt(i, str_col(batch, 4)),
                    rank: int_col(batch, 7).value(i),
                    tier: opt(i, str_col(batch, 8)),
                });
            }
        }
        out
    }

    const NATIVE_ID: &str = "11111111-2222-3333-4444-777777777777";

    /// A repository and a session that wrote one of its files across
    /// three commits. The session's calls, in order: a grep for a line
    /// not yet written, an Edit adding it, a later Edit quoting that
    /// line as context while adding another, an Edit after the second
    /// commit writing a line the committed file carries reflowed over
    /// two lines, and a shell append whose result has no diff.
    fn fixture() -> (tempfile::TempDir, tempfile::TempDir, PathBuf, String) {
        let repo = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(repo.path()).unwrap();
        git(&root, &["init", "-q"]);
        // 2025-01-01, 2025-01-02, and 2025-01-04, at 00:00:00Z
        let path = commit(&root, "a.rs", "fn one() {}\n", 1_735_689_600, "one");
        commit(
            &root,
            "a.rs",
            "fn one() {}\nfn two_added_later() {}\nfn also_later() {}\n",
            1_735_776_000,
            "two",
        );
        commit(
            &root,
            "a.rs",
            "fn one() {}\nfn two_added_later() {}\nfn also_later() {}\n\
             fn four(a: i32,\n        b: i32) {}\nfn three_by_shell() {}\n",
            1_735_948_800,
            "three",
        );

        let mut lines = vec![prompt(1, "add a second function", "2025-01-01T10:00:00Z")];
        // A read of the text before it was written: the command carries
        // the line and recorded no file change, and must not win
        lines.extend(shell(
            2,
            "t0",
            "grep -n 'fn two_added_later() {}' a.rs",
            "2025-01-01T10:00:01Z",
        ));
        lines.extend(edit(
            4,
            "t1",
            &path,
            "fn one() {}\n",
            "fn one() {}\nfn two_added_later() {}\n",
            "2025-01-01T10:00:05Z",
        ));
        lines.extend(edit(
            6,
            "t2",
            &path,
            "fn two_added_later() {}\n",
            "fn two_added_later() {}\nfn also_later() {}\n",
            "2025-01-01T11:00:00Z",
        ));
        lines.push(prompt(
            6,
            "add four, then append by shell",
            "2025-01-03T00:00:00Z",
        ));
        lines.extend(edit(
            7,
            "t3",
            &path,
            "fn also_later() {}\n",
            "fn also_later() {}\nfn four(a: i32, b: i32) {}\n",
            "2025-01-03T00:00:05Z",
        ));
        lines.extend(shell(
            11,
            "t4",
            "printf 'fn three_by_shell() {}\\n' >> a.rs",
            "2025-01-03T01:00:00Z",
        ));
        let claude_root = tempfile::tempdir().unwrap();
        let spec = write_session(claude_root.path(), &root, NATIVE_ID, &lines);
        (repo, claude_root, path, spec)
    }

    fn some(s: &str) -> Option<String> {
        Some(s.to_string())
    }

    #[tokio::test]
    async fn native_blame_attributes_lines_to_the_earliest_call_carrying_them() {
        let (_repo, claude_root, path, spec) = fixture();
        let ctx = ContextBuilder::new(None)
            .cache_root(claude_root.path().join("cache"))
            .build()
            .await;
        let from = format!("native_blame('{spec}', '{}')", path.display());
        let all = rows(&ctx, &from).await;
        assert!(
            all.iter().all(|r| r.session.as_deref() == Some(NATIVE_ID)),
            "every row names the one session: {all:?}"
        );
        assert!(
            !all.iter().any(|r| r.line == 1),
            "a line older than every session yields no row"
        );
        let shape: Vec<_> = all
            .iter()
            .map(|r| {
                (
                    r.line,
                    r.line_end,
                    r.tool_use_id.clone(),
                    r.session_lines.clone(),
                    r.rank,
                    r.tier.clone(),
                )
            })
            .collect();
        assert_eq!(
            shape,
            vec![
                // line 2: the edit that added it, the later edit quoting
                // it, then the earlier read, by descending evidence
                (2, None, some("t1"), some("1-4"), 1, some("patch")),
                (2, None, some("t2"), some("1-6"), 2, some("input")),
                (2, None, some("t0"), some("1-2"), 3, some("command")),
                (3, None, some("t2"), some("1-6"), 1, some("patch")),
                // the edit's one added line reflowed over two file lines,
                // matched by its patch and folded into one range
                (4, Some(5), some("t3"), some("8-9"), 1, some("patch")),
                // a shell append with no diff, matched by its command
                (6, None, some("t4"), some("8-11"), 1, some("command")),
            ]
        );

        let picked = rows(&ctx, &format!("{}, '1,6')", from.trim_end_matches(')'))).await;
        assert_eq!(
            picked.iter().map(|r| r.line).collect::<Vec<_>>(),
            [6],
            "line 1 has no writer, so only line 6 of the selection is a row"
        );
        let one = rows(&ctx, &format!("{}, 2)", from.trim_end_matches(')'))).await;
        assert_eq!(one.len(), 3, "line 2 keeps all three candidates");
        assert!(one.iter().all(|r| r.line == 2));
    }

    #[tokio::test]
    async fn blame_reads_stored_sessions() {
        let (_repo, claude_root, path, spec) = fixture();
        let tmp = tempfile::tempdir().unwrap();
        let store_path = tmp.path().join("store.git");
        gage_store::init(&store_path).unwrap();
        let store = Arc::new(Mutex::new(Store::open(&store_path).unwrap()));
        let registry = DriverRegistry::builtin();
        let driver = registry.driver_for(&spec).unwrap();
        let source = driver
            .open_source(SourceOptions::new(&spec).cache_root(claude_root.path().join("cache")))
            .unwrap();
        let mut native = source.open_native(NATIVE_ID).unwrap();
        {
            let store = store.lock().unwrap();
            SessionStore::from(&*store)
                .add(driver.as_ref(), native.as_mut())
                .unwrap();
        }
        let ctx = ContextBuilder::new(Some(store))
            .cache_root(claude_root.path().join("cache"))
            .build()
            .await;
        let stored_id = strings(&ctx, "SELECT id FROM session", &[]).await.unwrap();
        let rows = rows(&ctx, &format!("blame('{}', '2-3')", path.display())).await;
        let r1: Vec<_> = rows
            .iter()
            .filter(|r| r.rank == 1)
            .map(|r| {
                (
                    r.line,
                    r.session.clone(),
                    r.session_lines.clone(),
                    r.tool_use_id.clone(),
                    r.tier.clone(),
                )
            })
            .collect();
        assert_eq!(
            r1,
            [
                (
                    2,
                    Some(stored_id[0].clone()),
                    some("1-4"),
                    some("t1"),
                    some("patch")
                ),
                (
                    3,
                    Some(stored_id[0].clone()),
                    some("1-6"),
                    some("t2"),
                    some("patch")
                ),
            ],
            "stored sessions carry the recorded patch, so edits read as patch matches"
        );
    }

    /// A line committed under the file's old name is matched against
    /// the calls that wrote the old path.
    #[tokio::test]
    async fn native_blame_follows_a_rename() {
        let repo = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(repo.path()).unwrap();
        git(&root, &["init", "-q"]);
        let old = commit(
            &root,
            "old.rs",
            "fn renamed_fn() {}\n",
            1_735_689_600,
            "old",
        );
        git(&root, &["mv", "old.rs", "new.rs"]);
        commit_staged(&root, 1_735_776_000, "rename");
        let new = root.join("new.rs");

        let mut lines = vec![prompt(1, "write it", "2024-12-31T00:00:00Z")];
        lines.extend(edit(
            2,
            "t1",
            &old,
            "",
            "fn renamed_fn() {}\n",
            "2024-12-31T00:00:05Z",
        ));
        let claude_root = tempfile::tempdir().unwrap();
        let spec = write_session(claude_root.path(), &root, NATIVE_ID, &lines);

        let ctx = ContextBuilder::new(None)
            .cache_root(claude_root.path().join("cache"))
            .build()
            .await;
        let rows = rows(
            &ctx,
            &format!("native_blame('{spec}', '{}')", new.display()),
        )
        .await;
        assert_eq!(rows.len(), 1);
        assert_eq!(
            (
                rows[0].line,
                rows[0].session.clone(),
                rows[0].session_lines.clone(),
                rows[0].tier.clone(),
            ),
            (1, some(NATIVE_ID), some("1-2"), some("patch")),
            "{rows:?}"
        );
    }

    /// Outside a repository the lines come from the file, every
    /// window open at the start, and the selection applies.
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
        assert!(lines.iter().all(|l| l.path == path));
    }

    /// An empty file has no lines to attribute and returns no rows,
    /// rather than building an empty SQL predicate.
    #[tokio::test]
    async fn an_empty_file_returns_no_rows() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("empty.txt");
        std::fs::write(&file, "").unwrap();
        let claude_root = tempfile::tempdir().unwrap();
        let spec = write_session(claude_root.path(), dir.path(), NATIVE_ID, &[]);
        let ctx = ContextBuilder::new(None)
            .cache_root(claude_root.path().join("cache"))
            .build()
            .await;
        let rows = rows(
            &ctx,
            &format!("native_blame('{spec}', '{}')", file.display()),
        )
        .await;
        assert!(rows.is_empty());
    }

    /// A short line between two patch matches of one call takes that
    /// call, since the edit that added its neighbors added it too.
    #[tokio::test]
    async fn a_line_between_two_patch_matches_is_attributed_to_the_same_call() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let file = root.join("notes.md");
        let body = "alpha heading line\n}\ngamma heading line\n";
        std::fs::write(&file, body).unwrap();

        let mut lines = vec![prompt(1, "write the file", "2025-01-01T00:00:00Z")];
        lines.extend(edit(2, "t1", &file, "", body, "2025-01-01T00:00:05Z"));
        let claude_root = tempfile::tempdir().unwrap();
        let spec = write_session(claude_root.path(), &root, NATIVE_ID, &lines);

        let ctx = ContextBuilder::new(None)
            .cache_root(claude_root.path().join("cache"))
            .build()
            .await;
        let rows = rows(
            &ctx,
            &format!("native_blame('{spec}', '{}')", file.display()),
        )
        .await;
        let shape: Vec<_> = rows
            .iter()
            .map(|r| (r.line, r.tool_use_id.clone(), r.tier.clone()))
            .collect();
        assert_eq!(
            shape,
            [
                (1, some("t1"), some("patch")),
                (2, some("t1"), some("neighbor")),
                (3, some("t1"), some("patch")),
            ],
            "the short `}}` line between two patch matches is a neighbor"
        );
    }

    /// A file outside the session's project, written by a shell
    /// heredoc the driver could not judge, is found through the wide
    /// fallback on the summary's referenced paths.
    #[tokio::test]
    async fn the_wide_fallback_finds_a_file_named_only_in_a_command() {
        let home = tempfile::tempdir().unwrap();
        let home = std::fs::canonicalize(home.path()).unwrap();
        let project_root = home.join("projA");
        std::fs::create_dir_all(&project_root).unwrap();
        let file = home.join("dprint.jsonc");
        let body = "\"lineWidth\": 80,\n\"plugin\": \"markdown\"\n";
        std::fs::write(&file, body).unwrap();

        let command = format!("cat > {} <<'EOF'\n{body}EOF\n", file.display());
        let mut lines = vec![prompt(1, "set up dprint", "2025-01-01T00:00:00Z")];
        lines.extend(shell(2, "t1", &command, "2025-01-01T00:00:05Z"));
        let claude_root = tempfile::tempdir().unwrap();
        // The session belongs to projA, not to any ancestor of the file
        let spec = write_session(claude_root.path(), &project_root, NATIVE_ID, &lines);

        let ctx = ContextBuilder::new(None)
            .cache_root(claude_root.path().join("cache"))
            .build()
            .await;
        let rows = rows(
            &ctx,
            &format!("native_blame('{spec}', '{}')", file.display()),
        )
        .await;
        let shape: Vec<_> = rows
            .iter()
            .map(|r| {
                (
                    r.line,
                    r.line_end,
                    r.session.clone(),
                    r.tool_use_id.clone(),
                    r.tier.clone(),
                )
            })
            .collect();
        assert_eq!(
            shape,
            [(1, Some(2), some(NATIVE_ID), some("t1"), some("command"))],
            "both lines of the heredoc fold into one range, the command tier"
        );
    }

    /// The native writer lookup reads the session summaries: it names
    /// the project's session that wrote the file and leaves out the
    /// project's session that wrote another file and the other
    /// project's session that wrote this one. The `tool_use` lookup
    /// the stored scope uses names the same session.
    #[tokio::test]
    async fn native_writers_are_read_from_the_session_summaries() {
        let home = tempfile::tempdir().unwrap();
        let home = std::fs::canonicalize(home.path()).unwrap();
        let (proj_a, proj_b) = (home.join("projA"), home.join("projB"));
        std::fs::create_dir_all(&proj_a).unwrap();
        std::fs::create_dir_all(&proj_b).unwrap();
        let file = proj_a.join("a.rs");
        std::fs::write(&file, "fn one() {}\n").unwrap();
        let other = proj_a.join("b.rs");
        let ts = "2025-01-01T00:00:05Z";
        let writer = "aaaaaaaa-0000-0000-0000-000000000001";
        let bystander = "aaaaaaaa-0000-0000-0000-000000000002";
        let elsewhere = "aaaaaaaa-0000-0000-0000-000000000003";
        let claude_root = tempfile::tempdir().unwrap();
        let session = |id: &str, project: &Path, path: &Path| {
            let mut lines = vec![prompt(1, "write it", "2025-01-01T00:00:00Z")];
            lines.extend(edit(2, "t1", path, "", "fn one() {}\n", ts));
            write_session(claude_root.path(), project, id, &lines)
        };
        session(bystander, &proj_a, &other);
        session(elsewhere, &proj_b, &file);
        let spec = session(writer, &proj_a, &file);

        let table = BlameTable {
            scope: Scope::Native { show_agents: false },
            source: Some(spec),
            cache_root: Some(claude_root.path().join("cache")),
            path: file.clone(),
            selection: None,
        };
        let lines = blamed_lines(&file, None).unwrap();
        let (ctx, projects) = table.open(&lines).await.unwrap();
        let wrote_path = format!(
            "array_has(files_changed, {})",
            quote(file.to_str().unwrap())
        );

        let from_summaries = writer_sessions(&ctx, &projects, &wrote_path).await.unwrap();
        assert_eq!(from_summaries, [writer.to_string()]);

        let scoped = session_ids(&ctx, &projects).await.unwrap();
        let from_calls = writers_of(&ctx, &scoped, &wrote_path).await.unwrap();
        assert_eq!(
            from_calls, from_summaries,
            "both lookups name the same writer"
        );
    }
}
