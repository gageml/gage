//! Scan eval engine: deterministic measurements over a completed scan,
//! written as `scan.json` in a new eval run. `scan.json` is also the
//! sentinel marking the run as a scan eval. Measurements are stored per
//! agent session; aggregation happens at render time.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use gage_db::scan as db_scan;

use crate::run::{Manifest, RunResult, now_iso};
use crate::storage;

/// Structure version; bump when fields change shape.
const VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
pub struct ScanEval {
    pub version: u32,
    pub scan_id: String,
    /// Wall time across the scan's tasks, min(started)..max(stopped)
    pub wall_time_ms: Option<u64>,
    /// Σ agent durations / wall time
    pub parallelism: Option<f64>,
    /// Scan cost as the db reports it (summed `total_cost_usd`)
    pub reported_cost: f64,
    /// Σ per-session cost − reported cost. A gap means cost is
    /// incurred somewhere the session records don't cover.
    pub cost_gap: f64,
    /// Distinct tool-error message shapes across all agent sessions
    pub error_clusters: Vec<ErrorCluster>,
    pub sessions: Vec<AgentSession>,
}

/// One agent session's measurements (one `task_agent` row).
#[derive(Serialize, Deserialize)]
pub struct AgentSession {
    pub session_id: String,
    pub scanner: String,
    pub task: String,
    /// Transcript JSONL; None when no file was found on disk
    pub path: Option<PathBuf>,
    pub cost: Option<f64>,
    pub duration_ms: Option<u64>,
    pub turns: Option<u32>,
    /// Assistant turns spent between an error tool result and the next
    /// successful one
    pub recovery_turns: u32,
    pub tokens: Tokens,
    /// cache_read / (cache_read + input); None when both are zero
    pub cache_ratio: Option<f64>,
    /// Calls and errors per tool, keyed by short tool name
    pub tool_use: BTreeMap<String, ToolUse>,
    /// Repeat calls: same tool, identical input, shortly after an error
    pub retries: u32,
    pub notes_written: u32,
    pub issues_written: u32,
    pub exit_code: Option<i64>,
    pub stderr_bytes: u64,
    pub stop_reason: Option<String>,
}

#[derive(Serialize, Deserialize, Default)]
pub struct Tokens {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_creation: u64,
}

#[derive(Serialize, Deserialize, Default, Clone, Copy)]
pub struct ToolUse {
    pub calls: u32,
    pub errors: u32,
}

#[derive(Serialize, Deserialize)]
pub struct ErrorCluster {
    /// Normalized first line of the error message
    pub shape: String,
    pub count: u32,
    /// Sessions the shape occurred in
    pub sessions: Vec<String>,
}

/// Evaluate `scan_id_prefix` and store the result as a new eval run.
pub fn run_scan_eval(
    scan_id_prefix: &str,
    note: Option<&str>,
) -> io::Result<(RunResult, ScanEval)> {
    let conn = gage_db::db::open_db().map_err(io::Error::other)?;
    let scan = db_scan::get_scan(&conn, scan_id_prefix).map_err(io::Error::other)?;
    let eval = evaluate(&conn, &scan.id)?;

    let run_id = gage_core::uuid::new_uuid();
    let run_dir = storage::run_dir(&run_id);
    fs::create_dir_all(&run_dir)?;
    let manifest = Manifest {
        run_id: run_id.clone(),
        started_at: now_iso(),
        finished_at: Some(now_iso()),
        model: String::new(),
        effort: String::new(),
        test_names: Vec::new(),
        note: note.map(str::to_string),
        evals_dir: None,
        judge_model: None,
    };
    let bytes = serde_json::to_vec_pretty(&manifest).map_err(io::Error::other)?;
    fs::write(storage::manifest_path(&run_dir), bytes)?;
    let bytes = serde_json::to_vec_pretty(&eval).map_err(io::Error::other)?;
    fs::write(storage::scan_json_path(&run_dir), bytes)?;
    Ok((RunResult { run_id }, eval))
}

fn evaluate(conn: &gage_db::rusqlite::Connection, scan_id: &str) -> io::Result<ScanEval> {
    let tasks = db_scan::tasks_for_scan(conn, scan_id).map_err(io::Error::other)?;
    let agents = db_scan::agents_for_scan(conn, scan_id).map_err(io::Error::other)?;
    let reported_cost = db_scan::cost_for_scan(conn, scan_id)
        .map_err(io::Error::other)?
        .total_usd;

    let start = tasks.iter().filter_map(|t| t.started).min();
    let stop = tasks.iter().filter_map(|t| t.stopped).max();
    let wall_time_ms = match (start, stop) {
        (Some(a), Some(b)) if b > a => Some((b - a) as u64),
        _ => None,
    };

    let mut clusters: BTreeMap<String, ErrorCluster> = BTreeMap::new();
    let mut sessions = Vec::with_capacity(agents.len());
    for agent in &agents {
        sessions.push(eval_agent(agent, &mut clusters));
    }

    let summed_duration: u64 = sessions.iter().filter_map(|s| s.duration_ms).sum();
    let parallelism = wall_time_ms
        .filter(|w| *w > 0)
        .map(|w| summed_duration as f64 / w as f64);
    let cost_gap = sessions.iter().filter_map(|s| s.cost).sum::<f64>() - reported_cost;

    let mut error_clusters: Vec<ErrorCluster> = clusters.into_values().collect();
    error_clusters.sort_by_key(|c| std::cmp::Reverse(c.count));

    Ok(ScanEval {
        version: VERSION,
        scan_id: scan_id.to_string(),
        wall_time_ms,
        parallelism,
        reported_cost,
        cost_gap,
        error_clusters,
        sessions,
    })
}

fn eval_agent(
    agent: &db_scan::TaskAgent,
    clusters: &mut BTreeMap<String, ErrorCluster>,
) -> AgentSession {
    let result: Option<Value> = agent
        .result
        .as_deref()
        .and_then(|r| serde_json::from_str(r).ok());
    let cost = result
        .as_ref()
        .and_then(|r| r.get("total_cost_usd"))
        .and_then(Value::as_f64);
    let duration_ms = result
        .as_ref()
        .and_then(|r| r.get("duration_ms"))
        .and_then(Value::as_u64);
    let turns = result
        .as_ref()
        .and_then(|r| r.get("num_turns"))
        .and_then(Value::as_u64)
        .map(|n| n as u32);
    let stop_reason = result
        .as_ref()
        .and_then(|r| r.get("stop_reason"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let tokens = result
        .as_ref()
        .and_then(|r| r.get("modelUsage"))
        .map(sum_model_usage)
        .unwrap_or_default();
    let cache_ratio = match tokens.cache_read + tokens.input {
        0 => None,
        denom => Some(tokens.cache_read as f64 / denom as f64),
    };

    let path = find_transcript(&agent.session_id);
    let mut transcript = TranscriptStats::default();
    if let Some(p) = &path {
        transcript = read_transcript(p, &agent.session_id, clusters);
    }

    AgentSession {
        session_id: agent.session_id.clone(),
        scanner: agent.scanner_name.clone(),
        task: agent.task_name.clone(),
        path,
        cost,
        duration_ms,
        turns: turns.or(transcript.turns),
        recovery_turns: transcript.recovery_turns,
        tokens,
        cache_ratio,
        tool_use: transcript.tool_use,
        retries: transcript.retries,
        notes_written: transcript.notes_written,
        issues_written: transcript.issues_written,
        exit_code: agent.exit_code,
        stderr_bytes: agent.stderr.as_deref().map(str::len).unwrap_or(0) as u64,
        stop_reason,
    }
}

fn sum_model_usage(usage: &Value) -> Tokens {
    let mut t = Tokens::default();
    let Some(models) = usage.as_object() else {
        return t;
    };
    for m in models.values() {
        let field = |k: &str| m.get(k).and_then(Value::as_u64).unwrap_or(0);
        t.input += field("inputTokens");
        t.output += field("outputTokens");
        t.cache_read += field("cacheReadInputTokens");
        t.cache_creation += field("cacheCreationInputTokens");
    }
    t
}

/// Locate the agent session transcript under `~/.gage/claude/<agent>/`.
fn find_transcript(session_id: &str) -> Option<PathBuf> {
    let root = gage_core::config::gage_home().join("claude");
    let file = format!("{session_id}.jsonl");
    for entry in fs::read_dir(&root).ok()?.flatten() {
        let candidate = entry.path().join(&file);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

#[derive(Default)]
struct TranscriptStats {
    turns: Option<u32>,
    recovery_turns: u32,
    tool_use: BTreeMap<String, ToolUse>,
    retries: u32,
    notes_written: u32,
    issues_written: u32,
}

/// Retry window: a same-tool, same-input call within this many entries
/// after that tool errored counts as a retry.
const RETRY_WINDOW: usize = 6;

/// Tools whose calls constitute scan output. Matches the output
/// counting used by the scan analysis scripts.
const NOTE_TOOLS: &[&str] = &["NoteWrite", "Finding", "ProjectSummary"];
const ISSUE_TOOLS: &[&str] = &["IssueWrite"];

fn read_transcript(
    path: &Path,
    session_id: &str,
    clusters: &mut BTreeMap<String, ErrorCluster>,
) -> TranscriptStats {
    let mut stats = TranscriptStats::default();
    let Ok(reader) = gage_claude::session_reader::SessionReader::open(path) else {
        // The path was found moments ago; a read failure here leaves
        // the transcript-derived stats at their defaults, which the
        // view renders as absent.
        return stats;
    };

    let mut counter = gage_claude::stats::TurnCounter::new();
    let mut turns: u32 = 0;
    // tool_use id -> (short name, serialized input)
    let mut calls: std::collections::HashMap<String, (String, String)> =
        std::collections::HashMap::new();
    // (tool, input) of recent errors, with the entry index they occurred at
    let mut recent_errors: Vec<(String, String, usize)> = Vec::new();
    let mut in_recovery = false;
    let mut entry_index: usize = 0;

    for item in reader {
        let Ok((_line, value)) = item else { break };
        entry_index += 1;
        let is_assistant = value.get("type").and_then(Value::as_str) == Some("assistant");
        if counter.observe(&value).is_some() && is_assistant {
            turns += 1;
            if in_recovery {
                stats.recovery_turns += 1;
            }
        }
        let Some(content) = value.pointer("/message/content").and_then(Value::as_array) else {
            continue;
        };
        for block in content {
            match block.get("type").and_then(Value::as_str) {
                Some("tool_use") => {
                    let name =
                        short_tool_name(block.get("name").and_then(Value::as_str).unwrap_or("?"));
                    let input = block
                        .get("input")
                        .map(|v| v.to_string())
                        .unwrap_or_default();
                    if let Some(id) = block.get("id").and_then(Value::as_str) {
                        calls.insert(id.to_string(), (name.clone(), input.clone()));
                    }
                    stats.tool_use.entry(name.clone()).or_default().calls += 1;
                    if NOTE_TOOLS.contains(&name.as_str()) {
                        stats.notes_written += 1;
                    }
                    if ISSUE_TOOLS.contains(&name.as_str()) {
                        stats.issues_written += 1;
                    }
                    if recent_errors.iter().any(|(t, i, at)| {
                        *t == name && *i == input && entry_index.saturating_sub(*at) <= RETRY_WINDOW
                    }) {
                        stats.retries += 1;
                    }
                }
                Some("tool_result") => {
                    let id = block
                        .get("tool_use_id")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let (name, input) = calls
                        .get(id)
                        .cloned()
                        .unwrap_or_else(|| ("?".to_string(), String::new()));
                    let is_error = block
                        .get("is_error")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    if is_error {
                        stats.tool_use.entry(name.clone()).or_default().errors += 1;
                        recent_errors.push((name, input, entry_index));
                        in_recovery = true;
                        let shape = error_shape(&result_text(block));
                        let cluster = clusters.entry(shape.clone()).or_insert(ErrorCluster {
                            shape,
                            count: 0,
                            sessions: Vec::new(),
                        });
                        cluster.count += 1;
                        if cluster.sessions.last().map(String::as_str) != Some(session_id) {
                            cluster.sessions.push(session_id.to_string());
                        }
                    } else {
                        in_recovery = false;
                    }
                }
                _ => {}
            }
        }
    }
    stats.turns = Some(turns);
    stats
}

/// Strip an MCP prefix (`mcp__server__Tool` -> `Tool`).
fn short_tool_name(name: &str) -> String {
    name.rsplit("__").next().unwrap_or(name).to_string()
}

/// First text block of a tool result, for error clustering.
fn result_text(block: &Value) -> String {
    match block.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .find_map(|i| i.get("text").and_then(Value::as_str))
            .unwrap_or("")
            .to_string(),
        _ => String::new(),
    }
}

/// Normalize an error message into a cluster shape: first line with
/// digit runs collapsed to `#`, truncated.
fn error_shape(message: &str) -> String {
    let first = message.lines().next().unwrap_or("");
    let mut out = String::with_capacity(first.len().min(160));
    let mut in_digits = false;
    for c in first.chars() {
        if c.is_ascii_digit() {
            if !in_digits {
                out.push('#');
                in_digits = true;
            }
            continue;
        }
        in_digits = false;
        out.push(c);
        if out.chars().count() >= 160 {
            break;
        }
    }
    out
}
