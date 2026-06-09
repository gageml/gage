//! Build a markdown summary of an eval run and page it.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;

use crate::eval::Test;
use crate::score;
use crate::storage::{self, RunSummary};
use crate::tokens::{Tokens, entry_tokens, fmt_tokens};
use gage_claude::entry::entry_subtype;
use gage_claude::session_reader::SessionReader;
use serde_json::Value;

/// Resolve a run-id prefix to exactly one `RunSummary`. Errors with a
/// table of matches when ambiguous, with `NotFound` when nothing
/// matches.
pub fn resolve(prefix: &str) -> io::Result<RunSummary> {
    let runs = storage::list_runs()?;
    let matches: Vec<RunSummary> = runs
        .into_iter()
        .filter(|r| r.run_id.starts_with(prefix))
        .collect();
    match matches.len() {
        0 => Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("no run matches prefix `{prefix}`"),
        )),
        1 => Ok(matches.into_iter().next().expect("len == 1")),
        _ => Err(io::Error::other(AmbiguousError { matches })),
    }
}

/// Carries the matched runs so the CLI can render them in the same
/// table style as `gage-eval list`.
pub struct AmbiguousError {
    pub matches: Vec<RunSummary>,
}

impl std::fmt::Debug for AmbiguousError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AmbiguousError")
            .field("matches", &self.matches.len())
            .finish()
    }
}

impl std::fmt::Display for AmbiguousError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} runs match prefix", self.matches.len())
    }
}

impl std::error::Error for AmbiguousError {}

/// Build (if missing) and return the path of the markdown report.
pub fn ensure_report(run: &RunSummary, refresh: bool) -> io::Result<PathBuf> {
    let path = storage::run_dir(&run.run_id).join("report.md");
    if refresh || !path.exists() {
        let body = render(run)?;
        fs::write(&path, body)?;
    }
    Ok(path)
}

#[derive(Deserialize)]
struct Manifest {
    run_id: String,
    started_at: String,
    finished_at: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    effort: Option<String>,
    #[serde(default)]
    test_names: Vec<String>,
    #[serde(default)]
    note: Option<String>,
}

fn render(run: &RunSummary) -> io::Result<String> {
    let manifest_bytes = fs::read(storage::manifest_path(&run.run_id))?;
    let manifest: Manifest = serde_json::from_slice(&manifest_bytes).map_err(io::Error::other)?;

    let mut body = String::new();
    let mut run_tokens = Tokens::default();
    let mut passed: u32 = 0;
    let mut failed: u32 = 0;
    for name in &manifest.test_names {
        let score = score::read_score(&run.run_id, name)?;
        let glyph = match &score {
            Some(s) if s.passed => "✓ ",
            Some(_) => "✗ ",
            None => "",
        };
        body.push_str(&format!("## {glyph}{name}\n\n"));
        match read_test_json(&run.run_id, name) {
            Ok(test) => run_tokens += render_test(&mut body, &run.run_id, name, &test),
            Err(e) => body.push_str(&format!("_Failed to read test.json: {e}_\n\n")),
        }
        if let Some(s) = score {
            if s.passed {
                passed += 1;
            } else {
                failed += 1;
            }
        }
    }

    let mut out = String::new();
    out.push_str(&format!("# Eval run `{}`\n\n", manifest.run_id));
    out.push_str(&format!("- Started: `{}`\n", manifest.started_at));
    if let Some(f) = &manifest.finished_at
        && let Some(d) = duration_between(&manifest.started_at, f)
    {
        out.push_str(&format!("- Duration: {d}\n"));
    }
    if let Some(m) = &manifest.model {
        out.push_str(&format!("- Model: `{m}`\n"));
    }
    if let Some(e) = &manifest.effort {
        out.push_str(&format!("- Effort: `{e}`\n"));
    }
    out.push_str(&format!("- Tests: {}\n", manifest.test_names.len()));
    let scored = passed + failed;
    if scored > 0 {
        out.push_str(&format!("- Passed: {passed}/{scored}\n"));
    }
    if let Some(line) = fmt_tokens(&run_tokens) {
        out.push_str(&format!("- Tokens: {line}\n"));
    }
    if let Some(n) = manifest
        .note
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        out.push_str(&format!("- Note: {n}\n"));
    }
    out.push('\n');
    out.push_str(&body);
    Ok(out)
}

fn render_test(out: &mut String, run_id: &str, test_name: &str, test: &Test) -> Tokens {
    let session = storage::session_path(run_id, test_name);
    let (session_body, test_tokens) = match &session {
        Some(p) => match render_session(p) {
            Ok((body, tokens)) => (Some(body), tokens),
            Err(e) => (
                Some(format!("_Failed to read session: {e}_\n\n")),
                Tokens::default(),
            ),
        },
        None => (None, Tokens::default()),
    };

    if let Some(p) = &session
        && let Some((first, last)) = session_timespan(p)
    {
        out.push_str(&format!("- Started: `{first}`\n"));
        if let Some(d) = duration_between(&first, &last) {
            out.push_str(&format!("- Duration: {d}\n"));
        }
    }
    if let Some(line) = fmt_tokens(&test_tokens) {
        out.push_str(&format!("- Tokens: {line}\n"));
    }
    if let Some(p) = &session {
        match score::count_turns(p) {
            Ok(turns) => out.push_str(&format!("- Turns: {turns}\n")),
            Err(e) => out.push_str(&format!("- Turns: _count failed: {e}_\n")),
        }
    }
    let exit_code_path = storage::error_exit_code_path(run_id, test_name);
    if let Ok(code) = fs::read_to_string(&exit_code_path) {
        out.push_str(&format!("- Exit code: `{}`\n", code.trim()));
    }
    match score::read_score(run_id, test_name) {
        Ok(Some(s)) => {
            if s.passed {
                out.push_str("- Result: ✓ pass\n");
            } else {
                let missed: Vec<&str> = s
                    .matches
                    .iter()
                    .filter(|m| !m.matched)
                    .map(|m| m.pattern.as_str())
                    .collect();
                out.push_str(&format!("- Result: ✗ fail (missed: {missed:?})\n"));
            }
        }
        Ok(None) => {}
        Err(e) => out.push_str(&format!("- Result: _score read failed: {e}_\n")),
    }
    out.push('\n');

    out.push_str("### Summary\n\n");
    out.push_str("**Input**\n\n");
    out.push_str(test.prompt.trim());
    out.push_str("\n\n");

    let stdout = fs::read_to_string(storage::stdout_path(run_id, test_name)).unwrap_or_default();
    let stdout_trimmed = stdout.trim();
    if !stdout_trimmed.is_empty() {
        out.push_str("**Output**\n\n");
        out.push_str(stdout_trimmed);
        out.push_str("\n\n");
    }

    out.push_str("**Score**\n\n");
    match score::read_score(run_id, test_name) {
        Ok(Some(s)) => {
            let verdict = if s.passed { "✓ pass" } else { "✗ fail" };
            out.push_str(&format!("{verdict}\n\n"));
            for m in &s.matches {
                let g = if m.matched { "✓" } else { "✗" };
                out.push_str(&format!("- {g} `{}`\n", m.pattern));
            }
            out.push('\n');
        }
        Ok(None) => {
            out.push_str("_No score.json — test had no `expect` or scoring did not run._\n\n")
        }
        Err(e) => out.push_str(&format!("_Failed to read score.json: {e}_\n\n")),
    }

    let stderr = fs::read_to_string(storage::stderr_path(run_id, test_name)).unwrap_or_default();
    let stderr_trimmed = stderr.trim();
    if !stderr_trimmed.is_empty() {
        out.push_str("**Error**\n\n");
        out.push_str(stderr_trimmed);
        out.push_str("\n\n");
    }

    if let Some(body) = session_body {
        out.push_str(&body);
    }

    test_tokens
}

fn format_duration_ms(ms: i64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        format!("{}m{}s", ms / 60_000, (ms % 60_000) / 1000)
    }
}

/// Return (earliest, latest) `timestamp` strings across all session
/// entries, or `None` if the file is unreadable or has no timestamps.
fn session_timespan(path: &Path) -> Option<(String, String)> {
    let reader = SessionReader::open(path).ok()?;
    let mut first: Option<String> = None;
    let mut last: Option<String> = None;
    for item in reader {
        let (_line, value) = item.ok()?;
        if let Some(ts) = value.get("timestamp").and_then(Value::as_str) {
            if first.as_deref().is_none_or(|f| ts < f) {
                first = Some(ts.to_string());
            }
            if last.as_deref().is_none_or(|l| ts > l) {
                last = Some(ts.to_string());
            }
        }
    }
    Some((first?, last?))
}

/// Render the session JSONL as a series of `### Line N` sections, each
/// followed by a fenced yaml block with that entry serialized. Returns
/// the rendered body and the summed token counts across all entries.
fn render_session(path: &Path) -> io::Result<(String, Tokens)> {
    let entries: Vec<Value> = SessionReader::open(path)?
        .map(|item| item.map(|(_, v)| v))
        .collect::<io::Result<Vec<_>>>()?;
    let mut out = String::new();
    let mut n = 0usize;
    let mut prev: Option<chrono::DateTime<chrono::FixedOffset>> = None;
    let mut totals = Tokens::default();
    let mut turn_index: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut current_turn: usize = 0;
    for value in entries {
        n += 1;
        let mut turn_suffix = String::new();
        // A round is one Anthropic API response, keyed on `message.id`. All
        // blocks from one inference (thinking, text, tool_use) share a round;
        // tool_result is not numbered (it has no message.id). Intentional.
        if value.get("type").and_then(Value::as_str) == Some("assistant")
            && let Some(id) = value.pointer("/message/id").and_then(Value::as_str)
        {
            let t = match turn_index.get(id) {
                Some(&t) => t,
                None => {
                    current_turn += 1;
                    turn_index.insert(id.to_string(), current_turn);
                    current_turn
                }
            };
            turn_suffix = format!(" {}", circled_digit(t));
        }
        let ty = value.get("type").and_then(|v| v.as_str()).unwrap_or("?");
        let subtype = entry_subtype(&value);
        let label = match subtype {
            Some("text") | None => ty,
            Some(sub) => sub,
        };
        out.push_str(&format!("### {n}. {label}{turn_suffix}\n\n"));

        let ts = value.get("timestamp").and_then(Value::as_str);
        if let Some(ts) = ts {
            out.push_str(&format!("- Timestamp: `{ts}`\n"));
            if let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(ts) {
                let dur = match prev {
                    Some(p) => format_duration_ms((parsed - p).num_milliseconds()),
                    None => "-".to_string(),
                };
                out.push_str(&format!("- Duration: {dur}\n"));
                prev = Some(parsed);
            }
        }
        let entry_tokens = entry_tokens(&value);
        if let Some(line) = fmt_tokens(&entry_tokens) {
            out.push_str(&format!("- Tokens: {line}\n"));
        }
        totals += entry_tokens;
        out.push('\n');

        let body = match subtype {
            Some("text") | Some("thinking") | None => entry_text(&value),
            Some("tool_use") => render_tool_uses(&value),
            Some("tool_result") => render_tool_results(&value),
            _ => None,
        };
        if let Some(body) = body {
            let trimmed = body.trim();
            if !trimmed.is_empty() {
                out.push_str(trimmed);
                out.push_str("\n\n");
            }
        }
        let yaml = serde_yaml::to_string(&value).map_err(io::Error::other)?;
        out.push_str(&format!("```yaml\n{yaml}```\n\n"));
    }
    Ok((out, totals))
}

fn circled_digit(n: usize) -> String {
    if (1..=20).contains(&n) {
        char::from_u32(0x2460 + (n as u32) - 1)
            .expect("U+2460..=U+2473 are valid scalars")
            .to_string()
    } else {
        format!("({n})")
    }
}

/// Concatenate `text` and `thinking` strings from `message.content`.
/// Used for entries whose subtype is `text` or `thinking`.
fn entry_text(value: &Value) -> Option<String> {
    let content = value.get("message")?.get("content")?;
    if let Some(s) = content.as_str() {
        return Some(s.to_string());
    }
    let items = content.as_array()?;
    let parts: Vec<String> = items
        .iter()
        .filter_map(|item| {
            item.get("text")
                .or_else(|| item.get("thinking"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect();
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n\n"))
    }
}

/// Render each `tool_use` block in `message.content` as a bold name
/// followed by a list of input key/value pairs.
fn render_tool_uses(value: &Value) -> Option<String> {
    let items = value.get("message")?.get("content")?.as_array()?;
    let parts: Vec<String> = items
        .iter()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("tool_use"))
        .map(|item| {
            let name = item.get("name").and_then(Value::as_str).unwrap_or("?");
            let mut s = format!("**`{name}`**\n");
            if let Some(obj) = item.get("input").and_then(Value::as_object) {
                for (k, v) in obj {
                    let val = match v {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    s.push_str(&format!("\n- `{k}`: `{val}`"));
                }
            }
            s
        })
        .collect();
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n\n"))
    }
}

/// Render `tool_result` block content as plain text. Each block's
/// `content` is either a string or an array of blocks; `text` blocks
/// render their text and `tool_reference` blocks (e.g. a tool-search
/// result) render their `tool_name` as a bulleted list.
fn render_tool_results(value: &Value) -> Option<String> {
    let items = value.get("message")?.get("content")?.as_array()?;
    let mut parts: Vec<String> = Vec::new();
    for item in items {
        if item.get("type").and_then(Value::as_str) != Some("tool_result") {
            continue;
        }
        match item.get("content") {
            Some(Value::String(s)) => parts.push(s.clone()),
            Some(Value::Array(blocks)) => {
                let refs: Vec<String> = blocks
                    .iter()
                    .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_reference"))
                    .filter_map(|b| b.get("tool_name").and_then(Value::as_str))
                    .map(|n| format!("- `tool_reference: {n}`"))
                    .collect();
                if !refs.is_empty() {
                    parts.push(format!("Tool result:\n\n{}", refs.join("\n")));
                }
                for b in blocks {
                    if let Some(t) = b.get("text").and_then(Value::as_str) {
                        parts.push(t.to_string());
                    }
                }
            }
            _ => {}
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n\n"))
    }
}

fn read_test_json(run_id: &str, test_name: &str) -> io::Result<Test> {
    let bytes = fs::read(storage::test_json_path(run_id, test_name))?;
    serde_json::from_slice(&bytes).map_err(io::Error::other)
}

fn duration_between(start: &str, end: &str) -> Option<String> {
    let s = chrono::DateTime::parse_from_rfc3339(start).ok()?;
    let e = chrono::DateTime::parse_from_rfc3339(end).ok()?;
    let ms = (e - s).num_milliseconds();
    if ms < 1000 {
        Some(format!("{ms}ms"))
    } else if ms < 60_000 {
        Some(format!("{:.1}s", ms as f64 / 1000.0))
    } else {
        let m = ms / 60_000;
        let s = (ms % 60_000) / 1000;
        Some(format!("{m}m{s}s"))
    }
}

/// View the markdown file. Tries `treemd` first (interactive TUI
/// markdown viewer); falls back to `$PAGER` or `less` if treemd isn't
/// available or fails to launch.
pub fn page(path: &Path) -> io::Result<()> {
    if let Some(status) = Command::new("treemd")
        .args(["--collapse", "2", "--no-outline-hash"])
        .arg(path)
        .status()
        .ok()
        && status.success()
    {
        return Ok(());
    }
    let pager = std::env::var("PAGER").unwrap_or_else(|_| "less".to_string());
    let status = Command::new(&pager).arg(path).status()?;
    if !status.success() {
        return Err(io::Error::other(format!(
            "{pager} exited with status {status}"
        )));
    }
    Ok(())
}
