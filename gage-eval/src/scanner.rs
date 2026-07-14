//! Scanner tests: run `gage scan` over a fixture's sessions in a fresh
//! sandbox, sample by sample, and judge the written notes and issues against
//! the test's expectations. Aggregated per-item match rates land in the
//! test's `score.json`; per-sample artifacts are kept for post-mortem:
//!
//! ```text
//! results/{eval}/{test}/
//! ├── score.json
//! └── sample{n}/
//!     ├── gage-home/          # sandbox, kept for post-mortem
//!     ├── scan-output.txt
//!     ├── scan-error.txt
//!     ├── dump.json           # notes + issues written by the scan
//!     ├── judge-prompt.md
//!     ├── judge-output.txt
//!     └── verdict.json
//! ```

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::eval::{Expect, ExpectEntry, Root, Test};
use crate::score::{MatchResult, Score};
use crate::storage;

/// Run one scanner test: `test.samples` sandboxed scans (up to `jobs`
/// concurrent), each judged, aggregated into a written `score.json`.
pub fn run_test(
    run: &Path,
    test: &Test,
    root: &Root,
    gage_bin: &Path,
    jobs: usize,
    judge_model: &str,
) -> io::Result<Score> {
    let fixture = test.fixture.as_deref().expect("validated scanner test");
    let projects = root.fixture_projects_dir(fixture);
    let sessions = session_ids(&projects)?;
    if sessions.is_empty() {
        return Err(io::Error::other(format!(
            "fixture `{fixture}` has no session files"
        )));
    }

    let ctx = Arc::new(SampleContext {
        run: run.to_path_buf(),
        test: test.clone(),
        projects,
        sessions,
        gage_bin: gage_bin.to_path_buf(),
        judge_model: judge_model.to_string(),
    });
    let outcomes = run_samples(&ctx, jobs);

    let score = aggregate(test, &outcomes);
    let bytes = serde_json::to_vec_pretty(&score).map_err(io::Error::other)?;
    fs::write(storage::score_path(run, &test.id()), bytes)?;
    Ok(score)
}

struct SampleContext {
    run: PathBuf,
    test: Test,
    projects: PathBuf,
    sessions: Vec<String>,
    gage_bin: PathBuf,
    judge_model: String,
}

fn run_samples(ctx: &Arc<SampleContext>, jobs: usize) -> Vec<SampleOutcome> {
    let queue: VecDeque<u32> = (1..=ctx.test.samples).collect();
    let queue = Arc::new(Mutex::new(queue));
    let results: Arc<Mutex<Vec<SampleOutcome>>> = Arc::new(Mutex::new(Vec::new()));

    let workers = jobs.clamp(1, ctx.test.samples as usize);
    let mut handles = Vec::with_capacity(workers);
    for _ in 0..workers {
        let queue = Arc::clone(&queue);
        let results = Arc::clone(&results);
        let ctx = Arc::clone(ctx);
        handles.push(std::thread::spawn(move || {
            loop {
                let sample = queue.lock().unwrap().pop_front();
                let Some(sample) = sample else { break };
                let outcome = match try_sample(&ctx, sample) {
                    Ok(o) => o,
                    Err(e) => SampleOutcome::Error(e.to_string()),
                };
                if let SampleOutcome::Error(message) = &outcome {
                    let dir = storage::sample_dir(&ctx.run, &ctx.test.id(), sample);
                    let recorded = fs::create_dir_all(&dir)
                        .and_then(|()| fs::write(dir.join("ERROR"), message));
                    if let Err(e) = recorded {
                        eprintln!("sample{sample} failed ({message}); ERROR file unwritable: {e}");
                    }
                }
                results.lock().unwrap().push(outcome);
            }
        }));
    }
    for h in handles {
        h.join().expect("sample workers don't panic");
    }
    Arc::try_unwrap(results)
        .ok()
        .expect("workers joined; no other refs")
        .into_inner()
        .unwrap()
}

enum SampleOutcome {
    /// The sample didn't produce a verdict: scan exit, unusable judge
    /// output, or harness failure. The message is written to the sample
    /// dir's `ERROR` file.
    Error(String),
    Judged {
        verdict: Verdict,
        /// Per-`db_rows` query: did it return at least one row.
        db_rows: Vec<bool>,
    },
}

fn try_sample(ctx: &SampleContext, sample: u32) -> io::Result<SampleOutcome> {
    let dir = storage::sample_dir(&ctx.run, &ctx.test.id(), sample);
    let sandbox = dir.join("gage-home");
    fs::create_dir_all(&sandbox)?;

    let stdout = fs::File::create(dir.join("scan-output.txt"))?;
    let stderr = fs::File::create(dir.join("scan-error.txt"))?;
    let mut cmd = Command::new(&ctx.gage_bin);
    cmd.arg("scan").args(&ctx.sessions);
    for scanner in &ctx.test.scanners {
        cmd.args(["--scanner", scanner]);
    }
    let status = cmd
        .args(["--yes", "--no-progress"])
        .env("GAGE_HOME", &sandbox)
        .env("CLAUDE_PROJECTS_DIR", &ctx.projects)
        .env("CLAUDE_CODE_DISABLE_TERMINAL_TITLE", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .status()?;
    if !status.success() {
        return Ok(SampleOutcome::Error(format!(
            "gage scan exited {}",
            status.code().unwrap_or(-1)
        )));
    }

    let db_path = sandbox.join("data").join("gage.db");
    let dump = dump_outputs(&db_path)?;
    write_json(&dir.join("dump.json"), &dump)?;

    let expect = ctx.test.expect.as_ref().expect("validated scanner test");
    let prompt = judge_prompt(expect, &dump);
    fs::write(dir.join("judge-prompt.md"), &prompt)?;
    let output = run_judge(&prompt, &ctx.judge_model)?;
    fs::write(dir.join("judge-output.txt"), &output)?;
    let verdict = match parse_verdict(&output) {
        Ok(v) => v,
        Err(e) => {
            return Ok(SampleOutcome::Error(format!("judge output unusable: {e}")));
        }
    };
    if let Err(e) = check_verdict_coverage(&verdict, expect_entries(expect).count()) {
        return Ok(SampleOutcome::Error(format!("judge output unusable: {e}")));
    }
    write_json(&dir.join("verdict.json"), &verdict)?;

    let db_rows = check_db_rows(&db_path, &expect.db_rows)?;
    Ok(SampleOutcome::Judged { verdict, db_rows })
}

/// Notes and issues the scan wrote, read straight from the sandbox db.
#[derive(Debug, Serialize, Deserialize)]
pub struct Dump {
    pub notes: Vec<NoteRow>,
    pub issues: Vec<IssueRow>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct NoteRow {
    pub name: String,
    pub author: String,
    pub value: String,
    pub metadata: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct IssueRow {
    pub name: String,
    pub title: String,
    pub description: String,
}

fn dump_outputs(db_path: &Path) -> io::Result<Dump> {
    let conn = gage_db::db::open_db_at(db_path).map_err(io::Error::other)?;

    let mut notes = Vec::new();
    let mut stmt = conn
        .prepare("SELECT name, author, value, metadata FROM note ORDER BY created")
        .map_err(io::Error::other)?;
    let rows = stmt
        .query_map([], |row| {
            Ok(NoteRow {
                name: row.get(0)?,
                author: row.get(1)?,
                value: row.get(2)?,
                metadata: row.get(3)?,
            })
        })
        .map_err(io::Error::other)?;
    for row in rows {
        notes.push(row.map_err(io::Error::other)?);
    }

    let mut issues = Vec::new();
    let mut stmt = conn
        .prepare("SELECT name, title, description FROM issue ORDER BY created")
        .map_err(io::Error::other)?;
    let rows = stmt
        .query_map([], |row| {
            Ok(IssueRow {
                name: row.get(0)?,
                title: row.get(1)?,
                description: row.get(2)?,
            })
        })
        .map_err(io::Error::other)?;
    for row in rows {
        issues.push(row.map_err(io::Error::other)?);
    }

    Ok(Dump { notes, issues })
}

/// Run each `db_rows` query against the sample's sandbox db; an entry is
/// true when the query returned at least one row.
fn check_db_rows(db_path: &Path, queries: &[String]) -> io::Result<Vec<bool>> {
    if queries.is_empty() {
        return Ok(Vec::new());
    }
    let conn = gage_db::db::open_db_at(db_path).map_err(io::Error::other)?;
    let mut out = Vec::with_capacity(queries.len());
    for sql in queries {
        let mut stmt = conn.prepare(sql).map_err(io::Error::other)?;
        let mut rows = stmt.query([]).map_err(io::Error::other)?;
        out.push(rows.next().map_err(io::Error::other)?.is_some());
    }
    Ok(out)
}

fn judge_prompt(expect: &Expect, dump: &Dump) -> String {
    let mut p = String::new();
    p.push_str(
        "You are scoring the output of a session-scanning pipeline against a \
         test's expectations.\n\n## Expectations\n\n",
    );
    if expect.empty {
        p.push_str("The test expects the scan to write no substantive output.\n");
    } else {
        p.push_str(
            "The test expects the scan to have written the following items. \
             Each has an index, a kind, the item's db name, and a prose \
             description of the expected content.\n\n",
        );
        for (i, (kind, entry)) in expect_entries(expect).enumerate() {
            writeln!(p, "{i}. [{kind} {}] {}", entry.name, entry.expect).unwrap();
        }
    }

    p.push_str("\n## Scan output\n\n### Notes\n\n");
    if dump.notes.is_empty() {
        p.push_str("(none)\n");
    }
    for (i, n) in dump.notes.iter().enumerate() {
        writeln!(p, "note {i}: name={} author={}", n.name, n.author).unwrap();
        writeln!(p, "value: {}", n.value).unwrap();
        if let Some(m) = &n.metadata {
            writeln!(p, "metadata: {m}").unwrap();
        }
        p.push('\n');
    }
    p.push_str("### Issues\n\n");
    if dump.issues.is_empty() {
        p.push_str("(none)\n");
    }
    for (i, iss) in dump.issues.iter().enumerate() {
        writeln!(p, "issue {i}: name={}", iss.name).unwrap();
        writeln!(p, "title: {}", iss.title).unwrap();
        writeln!(p, "description: {}", iss.description).unwrap();
        p.push('\n');
    }

    p.push_str(
        "\n## Instructions\n\n\
         - For each expectation, decide whether some written item matches it. \
         Match strictly: the item must state the specific content described, \
         not merely touch the same theme.\n\
         - Then look for written items whose substantive claims are NOT \
         sanctioned by any expectation, and list those as unexpected. \
         Scanners also write summary and bookkeeping items describing their \
         own work (e.g. a summary of the findings pass); these are context. \
         Omit them from the output entirely unless they assert a substantive \
         result that no expectation sanctions. An empty \"unexpected\" array \
         is the normal result when the scan wrote only what was expected.\n\
         - Reply with only a JSON object, no code fence, in this shape:\n\n\
         {\"expected\": [{\"index\": 0, \"matched\": true, \
         \"evidence\": \"<short quote from the matching item>\", \
         \"reason\": \"<one sentence>\"}],\n \
         \"unexpected\": [{\"kind\": \"note\", \"name\": \"finding.general\", \
         \"excerpt\": \"<short quote>\", \"reason\": \"<one sentence>\"}]}\n\n\
         Include one entry in \"expected\" for every expectation index. If \
         there are no expectations, \"expected\" is an empty array.\n",
    );
    p
}

/// Expectation entries in judge index order: notes then issues.
fn expect_entries(expect: &Expect) -> impl Iterator<Item = (&'static str, &ExpectEntry)> {
    expect
        .notes
        .iter()
        .map(|e| ("note", e))
        .chain(expect.issues.iter().map(|e| ("issue", e)))
}

fn run_judge(prompt: &str, model: &str) -> io::Result<String> {
    let out = Command::new("claude")
        .args(["-p", prompt, "--tools", "", "--model", model])
        .env("CLAUDE_CODE_DISABLE_TERMINAL_TITLE", "1")
        .stdin(Stdio::null())
        .output()?;
    if !out.status.success() {
        return Err(io::Error::other(format!(
            "judge claude exited {}: {}",
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Verdict {
    pub expected: Vec<ExpectedVerdict>,
    pub unexpected: Vec<UnexpectedItem>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ExpectedVerdict {
    pub index: usize,
    pub matched: bool,
    #[serde(default)]
    pub evidence: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct UnexpectedItem {
    pub kind: String,
    pub name: String,
    #[serde(default)]
    pub excerpt: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
}

/// Extract the outermost JSON object from the judge's reply.
fn parse_verdict(output: &str) -> Result<Verdict, String> {
    let start = output.find('{').ok_or("no JSON object in judge output")?;
    let end = output.rfind('}').ok_or("no JSON object in judge output")?;
    if end < start {
        return Err("no JSON object in judge output".to_string());
    }
    serde_json::from_str(output.get(start..=end).expect("indices from find"))
        .map_err(|e| e.to_string())
}

/// Require exactly one verdict entry per expectation index. A reply
/// that skips an expectation would otherwise score it as "not met",
/// misattributing a judge failure to the scanner under test.
fn check_verdict_coverage(verdict: &Verdict, expectations: usize) -> Result<(), String> {
    let mut seen = vec![false; expectations];
    for e in &verdict.expected {
        match seen.get_mut(e.index) {
            Some(s) if !*s => *s = true,
            Some(_) => return Err(format!("duplicate verdict for expectation {}", e.index)),
            None => {
                return Err(format!(
                    "verdict index {} out of range ({expectations} expectations)",
                    e.index
                ));
            }
        }
    }
    match seen.iter().position(|s| !s) {
        Some(i) => Err(format!("no verdict for expectation {i}")),
        None => Ok(()),
    }
}

/// Fold sample outcomes into the standard `Score` shape: one match row per
/// expectation (and `db_rows` query) carrying its sample rate, plus rows
/// asserting no unexpected items and full sample completion.
fn aggregate(test: &Test, outcomes: &[SampleOutcome]) -> Score {
    let expect = test.expect.as_ref().expect("validated scanner test");
    let entries: Vec<(&'static str, &ExpectEntry)> = expect_entries(expect).collect();
    let k = test.samples;

    let mut item_matched = vec![0u32; entries.len()];
    let mut db_matched = vec![0u32; expect.db_rows.len()];
    let mut unexpected_samples = 0u32;
    let mut judged_samples = 0u32;
    for outcome in outcomes {
        let SampleOutcome::Judged { verdict, db_rows } = outcome else {
            continue;
        };
        judged_samples += 1;
        for e in &verdict.expected {
            if e.matched
                && let Some(m) = item_matched.get_mut(e.index)
            {
                *m += 1;
            }
        }
        if !verdict.unexpected.is_empty() {
            unexpected_samples += 1;
        }
        for (i, ok) in db_rows.iter().enumerate() {
            if *ok && let Some(m) = db_matched.get_mut(i) {
                *m += 1;
            }
        }
    }

    let mut matches = Vec::new();
    for ((kind, entry), m) in entries.iter().zip(&item_matched) {
        matches.push(MatchResult {
            pattern: format!("{kind} {} ({m}/{k} samples) — {}", entry.name, entry.expect),
            matched: *m == k,
        });
    }
    for (sql, m) in expect.db_rows.iter().zip(&db_matched) {
        matches.push(MatchResult {
            pattern: format!("db rows ({m}/{k} samples): {sql}"),
            matched: *m == k,
        });
    }
    matches.push(MatchResult {
        pattern: format!("no unexpected items (extras in {unexpected_samples}/{k} samples)"),
        matched: unexpected_samples == 0,
    });
    matches.push(MatchResult {
        pattern: format!("all samples judged ({judged_samples}/{k})"),
        matched: judged_samples == k,
    });

    Score {
        passed: matches.iter().all(|m| m.matched),
        matches,
        turns: None,
    }
}

/// Session IDs under a fixture's `projects/` dir, from JSONL stems.
fn session_ids(projects: &Path) -> io::Result<Vec<String>> {
    let mut out = Vec::new();
    for entry in fs::read_dir(projects)? {
        let subdir = entry?.path();
        if !subdir.is_dir() {
            continue;
        }
        for file in fs::read_dir(&subdir)? {
            let path = file?.path();
            if path.extension().and_then(|x| x.to_str()) != Some("jsonl") {
                continue;
            }
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                out.push(stem.to_string());
            }
        }
    }
    out.sort();
    Ok(out)
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    let bytes = serde_json::to_vec_pretty(value).map_err(io::Error::other)?;
    fs::write(path, bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, expect: &str) -> ExpectEntry {
        toml::from_str(&format!("name = \"{name}\"\nexpect = \"{expect}\"")).unwrap()
    }

    fn scanner_test(expect: Expect, samples: u32) -> Test {
        Test {
            eval: "e".to_string(),
            index: 1,
            name: Some("t".to_string()),
            prompt: None,
            scanners: vec!["general-issues".to_string()],
            samples,
            expect: Some(expect),
            disabled: false,
            claude: None,
            fixture: Some("fx".to_string()),
            max_turns: None,
            db_init: None,
        }
    }

    fn expect_with(notes: Vec<ExpectEntry>, empty: bool) -> Expect {
        let mut e: Expect = toml::from_str("").unwrap();
        e.notes = notes;
        e.empty = empty;
        e
    }

    #[test]
    fn parse_verdict_extracts_json() {
        let out =
            "Sure:\n{\"expected\": [{\"index\": 0, \"matched\": true}], \"unexpected\": []}\n";
        let v = parse_verdict(out).unwrap();
        assert_eq!(v.expected.len(), 1);
        assert!(v.expected.first().unwrap().matched);
        assert!(v.unexpected.is_empty());
        assert!(parse_verdict("no json here").is_err());
    }

    #[test]
    fn verdict_coverage_requires_every_expectation() {
        let v = |indices: &[usize]| Verdict {
            expected: indices
                .iter()
                .map(|&index| ExpectedVerdict {
                    index,
                    matched: true,
                    evidence: None,
                    reason: None,
                })
                .collect(),
            unexpected: vec![],
        };
        assert!(check_verdict_coverage(&v(&[0, 1]), 2).is_ok());
        assert!(check_verdict_coverage(&v(&[]), 0).is_ok());
        assert!(
            check_verdict_coverage(&v(&[]), 1)
                .is_err_and(|e| e.contains("no verdict for expectation 0"))
        );
        assert!(check_verdict_coverage(&v(&[0, 0]), 2).is_err_and(|e| e.contains("duplicate")));
        assert!(check_verdict_coverage(&v(&[2]), 2).is_err_and(|e| e.contains("out of range")));
    }

    #[test]
    fn aggregate_rates_and_pass() {
        let test = scanner_test(
            expect_with(vec![entry("finding.general", "the thing")], false),
            2,
        );
        let judged = |matched: bool, extras: bool| SampleOutcome::Judged {
            verdict: Verdict {
                expected: vec![ExpectedVerdict {
                    index: 0,
                    matched,
                    evidence: None,
                    reason: None,
                }],
                unexpected: if extras {
                    vec![UnexpectedItem {
                        kind: "issue".to_string(),
                        name: "general".to_string(),
                        excerpt: None,
                        reason: None,
                    }]
                } else {
                    Vec::new()
                },
            },
            db_rows: Vec::new(),
        };

        let score = aggregate(&test, &[judged(true, false), judged(true, false)]);
        assert!(score.passed);

        let score = aggregate(&test, &[judged(true, false), judged(false, true)]);
        assert!(!score.passed);
        assert!(
            score
                .matches
                .iter()
                .any(|m| m.pattern.contains("(1/2 samples)"))
        );
        assert!(
            score
                .matches
                .iter()
                .any(|m| m.pattern.contains("extras in 1/2"))
        );

        let score = aggregate(
            &test,
            &[
                judged(true, false),
                SampleOutcome::Error("scan exited 1".to_string()),
            ],
        );
        assert!(!score.passed);
        assert!(
            score
                .matches
                .iter()
                .any(|m| m.pattern.contains("all samples judged (1/2)"))
        );
    }

    #[test]
    fn judge_prompt_covers_expectations_and_dump() {
        let expect = expect_with(vec![entry("finding.general", "iterator misuse")], false);
        let dump = Dump {
            notes: vec![NoteRow {
                name: "finding.general".to_string(),
                author: "agent:x".to_string(),
                value: "v".to_string(),
                metadata: None,
            }],
            issues: vec![],
        };
        let p = judge_prompt(&expect, &dump);
        assert!(p.contains("0. [note finding.general] iterator misuse"));
        assert!(p.contains("note 0: name=finding.general"));
        assert!(p.contains("### Issues"));

        let p = judge_prompt(&expect_with(vec![], true), &dump);
        assert!(p.contains("no substantive output"));
    }
}
