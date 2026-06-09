//! Discover evals from `<crate>/evals/*.toml`. One file = one eval.
//! Each file holds a `[[test]]` array.

use std::fs;
use std::path::{Path, PathBuf};

use regex::Regex;
use serde::{Deserialize, Serialize};

/// Crate-relative path baked at compile time. Internal-tool simplicity.
const EVALS_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/evals");
const FIXTURES_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures");

/// Absolute path to the `projects/` dir for the named fixture. The
/// caller is responsible for first validating the fixture exists via
/// [`validate`].
pub fn fixture_projects_dir(name: &str) -> PathBuf {
    Path::new(FIXTURES_DIR).join(name).join("projects")
}

/// One eval file: a named group of related test prompts. The name
/// lives on each `Test.eval` rather than here.
#[derive(Debug, Clone)]
pub struct EvalFile {
    pub tests: Vec<Test>,
}

/// One prompt within an eval file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Test {
    pub eval: String,
    pub index: usize,
    pub name: Option<String>,
    pub prompt: String,
    pub expect: Option<Expect>,
    pub disabled: bool,
    /// Settings JSON passed verbatim to `claude --settings`. The TOML
    /// table mirrors the on-disk settings.json schema 1:1; we never
    /// interpret it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claude: Option<toml::Value>,
    /// Name of a subdir under `gage-eval/fixtures/` whose `projects/`
    /// dir is exposed as `GAGE_PROJECTS_DIR`. `None` → an empty
    /// per-test projects dir.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fixture: Option<String>,
    /// Per-test cap on assistant turns. Overrides the default of 3.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<u32>,
    /// SQL executed against this test's fresh `gage.db` before the
    /// prompt runs. The schema is whatever the current `gage-db`
    /// migration produces — insert directly into `note`, `issue`,
    /// `scan_scanner`, etc.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub db_init: Option<String>,
}

/// Pass criteria for a test. Each pattern is a regex run against the
/// test's `output.txt`; all listed patterns must match to pass.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Expect {
    #[serde(default, rename = "match")]
    pub pattern: Option<String>,
    #[serde(default, rename = "match_all")]
    pub match_all: Vec<String>,
    /// Fails the test if the assistant used more than this many turns.
    /// Distinct from `Test.max_turns`, which is the hard cap passed to
    /// `claude --max-turns` (and serves as a runaway brake). This
    /// asserts an efficiency standard without truncating the session.
    #[serde(default)]
    pub max_turns: Option<u32>,
    /// SQL run against the test's `gage.db` after the prompt completes.
    /// Each query passes when it returns at least one row. Accepts a
    /// single string or an array of strings. Use to assert the agent's
    /// actions had the intended db effect.
    #[serde(default, deserialize_with = "string_or_seq")]
    pub db_rows: Vec<String>,
}

/// Deserialize a field written as either a single string or an array of
/// strings into a `Vec<String>`.
fn string_or_seq<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
    }
    Ok(match OneOrMany::deserialize(deserializer)? {
        OneOrMany::One(s) => vec![s],
        OneOrMany::Many(v) => v,
    })
}

impl Expect {
    /// Flatten `match` + `match_all` into a single ordered pattern list.
    pub fn patterns(&self) -> Vec<String> {
        let mut out = Vec::with_capacity(1 + self.match_all.len());
        if let Some(p) = &self.pattern {
            out.push(p.clone());
        }
        out.extend(self.match_all.iter().cloned());
        out
    }
}

impl Test {
    /// `<eval>/<name-or-1-based-index>`. Used as the path-safe
    /// identifier for results.
    pub fn id(&self) -> String {
        format!("{}/{}", self.eval, self.test_id())
    }

    /// The portion after the eval prefix: the `name` field if set,
    /// else the 1-based index.
    pub fn test_id(&self) -> String {
        match &self.name {
            Some(n) => n.clone(),
            None => self.index.to_string(),
        }
    }
}

#[derive(Deserialize)]
struct File {
    #[serde(default)]
    test: Vec<TestEntry>,
}

#[derive(Deserialize)]
struct TestEntry {
    #[serde(default)]
    name: Option<String>,
    prompt: String,
    #[serde(default)]
    expect: Option<Expect>,
    #[serde(default)]
    disabled: bool,
    #[serde(default)]
    claude: Option<toml::Value>,
    #[serde(default)]
    fixture: Option<String>,
    #[serde(default)]
    max_turns: Option<u32>,
    #[serde(default)]
    db_init: Option<String>,
}

/// Discover and load every eval file under `<crate>/evals/`.
pub fn load_all() -> std::io::Result<Vec<EvalFile>> {
    let dir = Path::new(EVALS_DIR);
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut files: Vec<PathBuf> = fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("toml"))
        .collect();
    files.sort();

    let mut out = Vec::new();
    for path in files {
        out.push(load_file(&path)?);
    }
    Ok(out)
}

fn load_file(path: &Path) -> std::io::Result<EvalFile> {
    let body = fs::read_to_string(path)?;
    let file: File = toml::from_str(&body).map_err(std::io::Error::other)?;
    let name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_string();
    let tests: Vec<Test> = file
        .test
        .into_iter()
        .enumerate()
        .map(|(i, t)| Test {
            eval: name.clone(),
            index: i + 1,
            name: t.name,
            prompt: t.prompt,
            expect: t.expect,
            disabled: t.disabled,
            claude: t.claude,
            fixture: t.fixture,
            max_turns: t.max_turns,
            db_init: t.db_init,
        })
        .collect();
    check_unique_names(path, &tests)?;
    Ok(EvalFile { tests })
}

fn check_unique_names(path: &Path, tests: &[Test]) -> std::io::Result<()> {
    let mut seen: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for t in tests {
        if let Some(n) = &t.name
            && let Some(prev) = seen.insert(n.as_str(), t.index)
        {
            return Err(std::io::Error::other(format!(
                "duplicate test name `{n}` in {} (tests #{prev} and #{})",
                path.display(),
                t.index,
            )));
        }
    }
    Ok(())
}

/// Select tests across all evals according to a list of SPEC patterns.
///
/// A SPEC is a glob with `*` matching any run of characters that does
/// not cross `/`. SPECs containing `/` match against `eval/test-id`;
/// SPECs without `/` match against either the `test-id` (the part
/// after the eval prefix) or the eval name, so a bare `query` selects
/// every test in the `query` eval and a bare `session-count` selects
/// that test wherever it lives. A leading `!` makes it an exclude.
///
/// With no SPECs, or only excludes, the base set is every test.
/// Otherwise the base set is the union of tests matching any include.
/// Excludes are then removed from the base set. Disabled tests are
/// dropped last and SPECs cannot override that.
pub fn select<'a>(evals: &'a [EvalFile], specs: &[String]) -> Result<Vec<&'a Test>, String> {
    let mut includes: Vec<CompiledSpec> = Vec::new();
    let mut excludes: Vec<CompiledSpec> = Vec::new();
    for spec in specs {
        let (negate, body) = match spec.strip_prefix('!') {
            Some(rest) => (true, rest),
            None => (false, spec.as_str()),
        };
        if body.is_empty() {
            return Err(format!("empty spec `{spec}`"));
        }
        let compiled = CompiledSpec::compile(body)?;
        if negate { &mut excludes } else { &mut includes }.push(compiled);
    }

    let all_tests: Vec<&Test> = evals.iter().flat_map(|e| e.tests.iter()).collect();
    let base: Vec<&Test> = if includes.is_empty() {
        all_tests
    } else {
        all_tests
            .into_iter()
            .filter(|t| includes.iter().any(|c| c.matches(t)))
            .collect()
    };
    let mut kept: Vec<&Test> = base
        .into_iter()
        .filter(|t| !excludes.iter().any(|c| c.matches(t)))
        .filter(|t| !t.disabled)
        .collect();
    kept.sort_by_key(|t| t.id());
    Ok(kept)
}

/// Validate that every named fixture exists as a subdir of
/// `gage-eval/fixtures/` with a `projects/` child. Returns the list of
/// missing `(test-id, fixture-name)` pairs.
pub fn validate(tests: &[&Test]) -> Result<(), Vec<(String, String)>> {
    let mut missing: Vec<(String, String)> = Vec::new();
    for t in tests {
        let Some(name) = &t.fixture else {
            continue;
        };
        let projects = fixture_projects_dir(name);
        if !projects.is_dir() {
            missing.push((t.id(), name.clone()));
        }
    }
    if missing.is_empty() {
        Ok(())
    } else {
        Err(missing)
    }
}

/// A SPEC compiled into a regex plus the bookkeeping needed to know
/// whether the original source contained `/` (which decides whether
/// the regex matches against `eval/test-id` or just `test-id`).
struct CompiledSpec {
    re: Regex,
    has_slash: bool,
}

impl CompiledSpec {
    /// Compile a SPEC body (no leading `!`). `*` becomes `[^/]*`;
    /// every other character is escaped.
    fn compile(body: &str) -> Result<Self, String> {
        let mut pattern = String::with_capacity(body.len() + 4);
        pattern.push('^');
        for ch in body.chars() {
            if ch == '*' {
                pattern.push_str("[^/]*");
            } else {
                pattern.push_str(&regex::escape(&ch.to_string()));
            }
        }
        pattern.push('$');
        let re = Regex::new(&pattern).map_err(|e| format!("invalid spec `{body}`: {e}"))?;
        Ok(Self {
            re,
            has_slash: body.contains('/'),
        })
    }

    fn matches(&self, test: &Test) -> bool {
        if self.has_slash {
            self.re.is_match(&test.id())
        } else {
            self.re.is_match(&test.test_id()) || self.re.is_match(&test.eval)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_discovers_files() {
        let evals = load_all().unwrap();
        assert!(
            !evals.is_empty(),
            "expected at least one eval file in evals/"
        );
        for e in &evals {
            for t in &e.tests {
                assert!(!t.prompt.is_empty(), "prompt non-empty");
                assert!(!t.eval.is_empty(), "eval name non-empty");
            }
        }
    }

    fn fixture() -> Vec<EvalFile> {
        let mk = |eval: &str, name: Option<&str>, index: usize, disabled: bool| Test {
            eval: eval.to_string(),
            index,
            name: name.map(str::to_string),
            prompt: "p".to_string(),
            expect: None,
            disabled,
            claude: None,
            fixture: None,
            max_turns: None,
            db_init: None,
        };
        vec![
            EvalFile {
                tests: vec![
                    mk("query", Some("session-count"), 1, false),
                    mk("query", Some("list-tools"), 2, false),
                    mk("query", None, 3, false),
                    mk("query", Some("off"), 4, true),
                ],
            },
            EvalFile {
                tests: vec![
                    mk("notes", Some("session-count"), 1, false),
                    mk("notes", Some("write"), 2, false),
                ],
            },
        ]
    }

    fn ids(tests: &[&Test]) -> Vec<String> {
        let mut v: Vec<String> = tests.iter().map(|t| t.id()).collect();
        v.sort();
        v
    }

    #[test]
    fn no_specs_returns_all_active_tests() {
        let evals = fixture();
        let got = select(&evals, &[]).unwrap();
        assert_eq!(
            ids(&got),
            vec![
                "notes/session-count",
                "notes/write",
                "query/3",
                "query/list-tools",
                "query/session-count",
            ]
        );
    }

    #[test]
    fn star_matches_everything_active() {
        let evals = fixture();
        let got = select(&evals, &["*".to_string()]).unwrap();
        assert_eq!(got.len(), 5);
    }

    #[test]
    fn bare_name_matches_test_id_across_evals() {
        let evals = fixture();
        let got = select(&evals, &["session-count".to_string()]).unwrap();
        assert_eq!(
            ids(&got),
            vec!["notes/session-count", "query/session-count"]
        );
    }

    #[test]
    fn bare_name_matches_eval_name() {
        let evals = fixture();
        let got = select(&evals, &["query".to_string()]).unwrap();
        assert_eq!(
            ids(&got),
            vec!["query/3", "query/list-tools", "query/session-count"]
        );
    }

    #[test]
    fn bare_name_unions_eval_and_test_id() {
        let evals = fixture();
        let got = select(&evals, &["write".to_string()]).unwrap();
        assert_eq!(ids(&got), vec!["notes/write"]);
    }

    #[test]
    fn eval_slash_test_matches_exactly_one() {
        let evals = fixture();
        let got = select(&evals, &["query/list-tools".to_string()]).unwrap();
        assert_eq!(ids(&got), vec!["query/list-tools"]);
    }

    #[test]
    fn eval_slash_index_reaches_unnamed_test() {
        let evals = fixture();
        let got = select(&evals, &["query/3".to_string()]).unwrap();
        assert_eq!(ids(&got), vec!["query/3"]);
    }

    #[test]
    fn glob_in_eval_segment() {
        let evals = fixture();
        let got = select(&evals, &["q*/session-count".to_string()]).unwrap();
        assert_eq!(ids(&got), vec!["query/session-count"]);
    }

    #[test]
    fn glob_does_not_cross_slash() {
        let evals = fixture();
        let got = select(&evals, &["*".to_string()]).unwrap();
        assert!(!got.is_empty());
        let got = select(&evals, &["*-count".to_string()]).unwrap();
        assert_eq!(
            ids(&got),
            vec!["notes/session-count", "query/session-count"]
        );
    }

    #[test]
    fn exclude_only_starts_from_all() {
        let evals = fixture();
        let got = select(&evals, &["!session-count".to_string()]).unwrap();
        assert_eq!(
            ids(&got),
            vec!["notes/write", "query/3", "query/list-tools"]
        );
    }

    #[test]
    fn include_then_exclude() {
        let evals = fixture();
        let got = select(
            &evals,
            &["query/*".to_string(), "!query/list-tools".to_string()],
        )
        .unwrap();
        assert_eq!(ids(&got), vec!["query/3", "query/session-count"]);
    }

    #[test]
    fn disabled_tests_are_always_dropped() {
        let evals = fixture();
        let got = select(&evals, &["query/off".to_string()]).unwrap();
        assert!(got.is_empty());
    }

    #[test]
    fn empty_spec_errors() {
        let evals = fixture();
        assert!(select(&evals, &["".to_string()]).is_err());
        assert!(select(&evals, &["!".to_string()]).is_err());
    }
}
