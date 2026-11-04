//! `Patterns`: a compiled set of regexes for known shapes of sensitive
//! text, for a filter's `pattern_redact`. A set is built from a Rune
//! object of name to regex, or from a gitleaks rule file with
//! `Patterns::from_gitleaks(text)`. A gitleaks rule carries a secret
//! group, an entropy floor, keywords that prefilter the text, and
//! allowlists; a plain regex carries none.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;

use aho_corasick::{AhoCorasick, AhoCorasickBuilder};
use regex::Regex;
use rune::runtime::{Ref, VmError};
use rune::{Any, ContextError, Module};
use serde::Deserialize;

pub(crate) fn types_module() -> Result<Module, ContextError> {
    let mut m = Module::new();
    m.ty::<Patterns>()?;
    m.function_meta(Patterns::rune_from_gitleaks)?;
    Ok(m)
}

/// A set of patterns, shared by handle.
#[derive(Any, Clone)]
#[rune(item = ::gage)]
pub struct Patterns {
    #[rune(skip)]
    inner: Arc<PatternSet>,
}

struct PatternSet {
    patterns: Vec<Pattern>,
    /// The keyword prefilter: a pattern with keywords runs only on text
    /// containing one of them
    keywords: Option<Keywords>,
}

impl Patterns {
    pub fn new(patterns: Vec<Pattern>) -> Self {
        let keywords = Keywords::new(&patterns);
        Self {
            inner: Arc::new(PatternSet { patterns, keywords }),
        }
    }

    /// The patterns of a gitleaks rule file. A rule without a regex,
    /// or with a path condition, does not apply to session text and is
    /// left out. The global allowlist is appended to every rule's.
    pub fn from_gitleaks(text: &str) -> Result<Self, RuleError> {
        Ok(Self::new(gitleaks_patterns(text)?))
    }

    /// A rule file this build cannot use is a defect in the filter's
    /// source and raises a VM fault naming the rule.
    #[rune::function(path = Self::from_gitleaks)]
    fn rune_from_gitleaks(text: Ref<str>) -> Result<Self, VmError> {
        Self::from_gitleaks(&text).map_err(|e| VmError::panic(e.to_string()))
    }

    pub fn len(&self) -> usize {
        self.inner.patterns.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.patterns.is_empty()
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.inner.patterns.iter().map(|p| p.name.as_str())
    }

    /// Every `(pattern name, literal)` the patterns find in `text`.
    pub fn matches<'t>(&self, text: &'t str) -> Vec<(&str, &'t str)> {
        let set = &*self.inner;
        let candidates: Vec<&Pattern> = match &set.keywords {
            Some(keywords) => keywords
                .candidates(text)
                .into_iter()
                .map(|i| {
                    set.patterns
                        .get(i)
                        .expect("keyword index came from patterns.len() at build time")
                })
                .collect(),
            None => set.patterns.iter().collect(),
        };
        candidates
            .into_iter()
            .flat_map(|p| p.find(text).into_iter().map(move |m| (p.name.as_str(), m)))
            .collect()
    }
}

impl fmt::Debug for Patterns {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.names()).finish()
    }
}

/// A compiled matcher for a known shape of sensitive text. The secret
/// is the configured capture group, else the first non-empty capture
/// group, else the whole match; a secret at or below the entropy floor
/// or accepted by an allowlist is not reported.
pub struct Pattern {
    pub name: String,
    regex: Regex,
    secret_group: Option<usize>,
    entropy: Option<f64>,
    allowlists: Vec<Allowlist>,
    keywords: Vec<String>,
}

impl Pattern {
    pub fn new(name: &str, regex: &str) -> Result<Self, regex::Error> {
        Ok(Self {
            name: name.to_string(),
            regex: Regex::new(regex)?,
            secret_group: None,
            entropy: None,
            allowlists: Vec::new(),
            keywords: Vec::new(),
        })
    }

    /// The sensitive literals this pattern finds in `text`.
    pub fn find<'t>(&self, text: &'t str) -> Vec<&'t str> {
        let mut found = Vec::new();
        for captures in self.regex.captures_iter(text) {
            let whole = captures.get(0).expect("group 0 is the match");
            let secret = match self.secret_group {
                Some(group) => captures.get(group).map(|m| m.as_str()),
                None => captures
                    .iter()
                    .skip(1)
                    .flatten()
                    .map(|m| m.as_str())
                    .find(|s| !s.is_empty())
                    .or_else(|| Some(whole.as_str().trim_matches('\n'))),
            };
            let Some(secret) = secret else {
                continue;
            };
            if secret.is_empty() {
                continue;
            }
            if let Some(floor) = self.entropy
                && shannon_entropy(secret) <= floor
            {
                continue;
            }
            let line = line_around(text, whole.start(), whole.end());
            if self
                .allowlists
                .iter()
                .any(|a| a.accepts(secret, whole.as_str(), line))
            {
                continue;
            }
            found.push(secret);
        }
        found
    }
}

/// The Shannon entropy of `text` in bits per character.
fn shannon_entropy(text: &str) -> f64 {
    let mut counts: BTreeMap<char, usize> = BTreeMap::new();
    let mut total = 0usize;
    for c in text.chars() {
        *counts.entry(c).or_insert(0) += 1;
        total += 1;
    }
    if total == 0 {
        return 0.0;
    }
    let total = total as f64;
    counts
        .values()
        .map(|&n| {
            let p = n as f64 / total;
            -p * p.log2()
        })
        .sum()
}

/// The line of `text` containing the span `start..end`.
fn line_around(text: &str, start: usize, end: usize) -> &str {
    let from = text[..start].rfind('\n').map_or(0, |i| i + 1);
    let to = text[end..].find('\n').map_or(text.len(), |i| end + i);
    &text[from..to]
}

/// A gitleaks allowlist: a finding it accepts is not a secret. Under
/// `OR`, any listed regex or stopword hit accepts; under `AND`, every
/// listed criterion must hit, and a path criterion never hits because
/// session text has no path.
struct Allowlist {
    target: Target,
    regexes: Vec<Regex>,
    stopwords: Vec<String>,
    all_of: bool,
    has_paths: bool,
}

/// What an allowlist regex is matched against.
#[derive(Clone, Copy)]
enum Target {
    Secret,
    Match,
    Line,
}

impl Allowlist {
    fn accepts(&self, secret: &str, matched: &str, line: &str) -> bool {
        let target = match self.target {
            Target::Secret => secret,
            Target::Match => matched,
            Target::Line => line,
        };
        let regex_hit = self.regexes.iter().any(|r| r.is_match(target));
        let lower = secret.to_lowercase();
        let stopword_hit = self.stopwords.iter().any(|w| lower.contains(w.as_str()));
        if self.all_of {
            !self.has_paths
                && (self.regexes.is_empty() || regex_hit)
                && (self.stopwords.is_empty() || stopword_hit)
                && (!self.regexes.is_empty() || !self.stopwords.is_empty())
        } else {
            regex_hit || stopword_hit
        }
    }
}

/// The keyword prefilter over a set's patterns: one automaton over
/// every keyword, and the patterns each keyword admits.
struct Keywords {
    automaton: AhoCorasick,
    /// Indexed by automaton pattern id: the patterns that list the keyword
    admits: Vec<Vec<usize>>,
    /// The patterns with no keywords, which always run
    always: Vec<usize>,
}

impl Keywords {
    fn new(patterns: &[Pattern]) -> Option<Self> {
        if patterns.iter().all(|p| p.keywords.is_empty()) {
            return None;
        }
        let mut by_keyword: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        let mut always = Vec::new();
        for (i, pattern) in patterns.iter().enumerate() {
            if pattern.keywords.is_empty() {
                always.push(i);
            }
            for keyword in &pattern.keywords {
                by_keyword
                    .entry(keyword.to_lowercase())
                    .or_default()
                    .push(i);
            }
        }
        let (keywords, admits): (Vec<String>, Vec<Vec<usize>>) = by_keyword.into_iter().unzip();
        let automaton = AhoCorasickBuilder::new()
            .ascii_case_insensitive(true)
            .build(&keywords)
            .expect("gitleaks keywords are short literals");
        Some(Self {
            automaton,
            admits,
            always,
        })
    }

    /// The indexes of the patterns that may match `text`.
    fn candidates(&self, text: &str) -> BTreeSet<usize> {
        let mut set: BTreeSet<usize> = self.always.iter().copied().collect();
        for hit in self.automaton.find_overlapping_iter(text) {
            set.extend(
                self.admits
                    .get(hit.pattern().as_usize())
                    .expect("automaton pattern ids index admits one to one"),
            );
        }
        set
    }
}

/// The gitleaks configuration file, as far as the set reads it.
#[derive(Deserialize)]
struct GitleaksConfig {
    #[serde(default)]
    allowlist: Option<GitleaksAllowlist>,
    rules: Vec<GitleaksRule>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GitleaksRule {
    id: String,
    regex: Option<String>,
    secret_group: Option<usize>,
    entropy: Option<f64>,
    #[serde(default)]
    keywords: Vec<String>,
    path: Option<String>,
    #[serde(default)]
    allowlists: Vec<GitleaksAllowlist>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GitleaksAllowlist {
    regex_target: Option<String>,
    #[serde(default)]
    regexes: Vec<String>,
    #[serde(default)]
    stopwords: Vec<String>,
    #[serde(default)]
    paths: Vec<String>,
    condition: Option<String>,
}

/// A rule of a gitleaks file that this build cannot use.
#[derive(Debug)]
pub struct RuleError {
    pub rule: String,
    pub message: String,
}

impl fmt::Display for RuleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "gitleaks rule {}: {}", self.rule, self.message)
    }
}

impl std::error::Error for RuleError {}

fn gitleaks_patterns(text: &str) -> Result<Vec<Pattern>, RuleError> {
    let config: GitleaksConfig = toml::from_str(text).map_err(|e| RuleError {
        rule: "<file>".to_string(),
        message: e.to_string(),
    })?;
    let global = config
        .allowlist
        .map(|a| compile_allowlist("<global>", &a))
        .transpose()?;
    let mut patterns = Vec::new();
    for rule in config.rules {
        let Some(regex) = rule.regex else {
            continue;
        };
        if rule.path.is_some() {
            continue;
        }
        let fail = |message: String| RuleError {
            rule: rule.id.clone(),
            message,
        };
        let regex = Regex::new(&port_go_regex(&regex)).map_err(|e| fail(e.to_string()))?;
        if let Some(group) = rule.secret_group
            && group >= regex.captures_len()
        {
            return Err(fail(format!("secretGroup {group} is not a capture group")));
        }
        let mut allowlists = rule
            .allowlists
            .iter()
            .map(|a| compile_allowlist(&rule.id, a))
            .collect::<Result<Vec<_>, _>>()?;
        if let Some(global) = &global {
            allowlists.push(Allowlist {
                target: global.target,
                regexes: global.regexes.clone(),
                stopwords: global.stopwords.clone(),
                all_of: global.all_of,
                has_paths: global.has_paths,
            });
        }
        patterns.push(Pattern {
            name: rule.id,
            regex,
            secret_group: rule.secret_group,
            entropy: rule.entropy,
            allowlists,
            keywords: rule.keywords,
        });
    }
    Ok(patterns)
}

fn compile_allowlist(rule: &str, a: &GitleaksAllowlist) -> Result<Allowlist, RuleError> {
    let fail = |message: String| RuleError {
        rule: rule.to_string(),
        message,
    };
    let target = match a.regex_target.as_deref() {
        None | Some("secret") => Target::Secret,
        Some("match") => Target::Match,
        Some("line") => Target::Line,
        Some(other) => return Err(fail(format!("unknown regexTarget {other:?}"))),
    };
    let all_of = match a.condition.as_deref() {
        None | Some("OR") => false,
        Some("AND") => true,
        Some(other) => return Err(fail(format!("unknown condition {other:?}"))),
    };
    let regexes = a
        .regexes
        .iter()
        .map(|r| Regex::new(&port_go_regex(r)).map_err(|e| fail(format!("allowlist: {e}"))))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Allowlist {
        target,
        regexes,
        stopwords: a.stopwords.iter().map(|w| w.to_lowercase()).collect(),
        all_of,
        has_paths: !a.paths.is_empty(),
    })
}

/// A gitleaks regex ported from Go's RE2 syntax to the `regex` crate's.
/// Go takes a brace that opens no repetition and a bracket inside a
/// class as literals; the crate rejects the first and nests the second.
/// Go's `\w`, `\d`, `\s`, and `\b` are ASCII; the crate's are Unicode,
/// and a Unicode class under a wide repetition exceeds the compiled
/// size limit.
fn port_go_regex(pattern: &str) -> String {
    let chars: Vec<char> = pattern.chars().collect();
    let mut out = String::with_capacity(pattern.len() + 16);
    let mut rest: &[char] = &chars;
    let mut in_class = false;
    while let Some((&c, tail)) = rest.split_first() {
        if c == '\\'
            && let Some((&next, after)) = tail.split_first()
        {
            let ported = match (in_class, next) {
                (false, 'w') => Some("(?-u:\\w)"),
                (false, 'd') => Some("(?-u:\\d)"),
                (false, 's') => Some("(?-u:\\s)"),
                (false, 'b') => Some("(?-u:\\b)"),
                (false, 'B') => Some("(?-u:\\B)"),
                (true, 'w') => Some("0-9A-Za-z_"),
                (true, 'd') => Some("0-9"),
                (true, 's') => Some("\\t\\n\\x0B\\x0C\\r "),
                _ => None,
            };
            match ported {
                Some(text) => out.push_str(text),
                None => {
                    out.push(c);
                    out.push(next);
                }
            }
            rest = after;
            continue;
        }
        if in_class {
            match c {
                ']' => {
                    in_class = false;
                    out.push(c);
                }
                '[' if tail.first() == Some(&':') => {
                    // A POSIX class, copied through its closing ":]"
                    let close = rest
                        .windows(2)
                        .position(|w| w == [':', ']'])
                        .expect("a POSIX class closes");
                    let (class, after) = rest.split_at(close + 2);
                    out.extend(class);
                    rest = after;
                    continue;
                }
                '[' => out.push_str("\\["),
                '&' | '-' | '~' if tail.first() == Some(&c) && out.ends_with(c) => {
                    out.push('\\');
                    out.push(c);
                }
                _ => out.push(c),
            }
            rest = tail;
            continue;
        }
        match c {
            '[' => {
                in_class = true;
                out.push(c);
                // A leading "^" and then a leading "]" are part of the class
                let mut tail = tail;
                if let Some((&'^', after)) = tail.split_first() {
                    out.push('^');
                    tail = after;
                }
                if let Some((&']', after)) = tail.split_first() {
                    out.push_str("\\]");
                    tail = after;
                }
                rest = tail;
                continue;
            }
            '{' => match repetition_len(rest) {
                Some(len) => {
                    let (repetition, after) = rest.split_at(len);
                    out.extend(repetition);
                    rest = after;
                    continue;
                }
                None => out.push_str("\\{"),
            },
            '}' => out.push_str("\\}"),
            _ => out.push(c),
        }
        rest = tail;
    }
    out
}

/// The length of the repetition `{n}`, `{n,}`, or `{n,m}` that opens
/// `chars`, when one does.
fn repetition_len(chars: &[char]) -> Option<usize> {
    let mut i = 1;
    let digits = |i: &mut usize| {
        let start = *i;
        while chars.get(*i).is_some_and(char::is_ascii_digit) {
            *i += 1;
        }
        *i > start
    };
    if !digits(&mut i) {
        return None;
    }
    if chars.get(i) == Some(&',') {
        i += 1;
        digits(&mut i);
    }
    (chars.get(i) == Some(&'}')).then_some(i + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rule set the secret filter ships.
    const GITLEAKS: &str = include_str!("../../filters/secret/gitleaks.toml");

    fn secret() -> Patterns {
        Patterns::from_gitleaks(GITLEAKS).unwrap()
    }

    #[test]
    fn every_gitleaks_rule_with_a_regex_compiles() {
        let patterns = secret();
        assert!(patterns.len() > 200, "{}", patterns.len());
        let names: Vec<&str> = patterns.names().collect();
        assert!(names.contains(&"anthropic-api-key"));
        assert!(names.contains(&"generic-api-key"));
        assert!(
            !names.contains(&"pkcs12-file"),
            "a path-only rule does not apply to session text"
        );
        let err = Patterns::from_gitleaks("rules = 3").unwrap_err();
        assert_eq!(err.rule, "<file>");
    }

    #[test]
    fn secret_patterns_find_keys_and_the_value_of_an_assignment() {
        let key = "sk-ant-api03-PtYgjmUhBel31iEl2hpChYgCfrL1spNxnyVmihA-2O76UMFxFkM-R5Kjp1vRt_1fjORS-6ilI8ihN5KXSc7Tvo-hBKqFYAA";
        let text = format!(
            "export ANTHROPIC_API_KEY={key} and AKIAQ7XJ2M5N3P4R6S7T\n\
             password = \"xK9mP2vL5nQ8wRt3\"\n\
             password = \"hunter2hunter2\"\n\
             api_key = \"abcdefghijklmnopqrstuvwxyz\"\n\
             token = \"${{GITHUB_TOKEN}}\""
        );
        let secret = secret();
        let found = secret.matches(&text);
        let literals: Vec<&str> = found.iter().map(|(_, m)| *m).collect();
        let rules: Vec<&str> = found.iter().map(|(r, _)| *r).collect();
        assert!(literals.contains(&key), "{found:?}");
        assert!(rules.contains(&"anthropic-api-key"), "{found:?}");
        assert!(literals.contains(&"AKIAQ7XJ2M5N3P4R6S7T"), "{found:?}");
        assert!(literals.contains(&"xK9mP2vL5nQ8wRt3"), "{found:?}");
        assert!(
            !literals.contains(&"hunter2hunter2"),
            "a low-entropy value is not a secret: {found:?}"
        );
        assert!(
            !literals.contains(&"abcdefghijklmnopqrstuvwxyz"),
            "a global stopword is not a secret: {found:?}"
        );
        assert!(
            !literals.iter().any(|l| l.contains("GITHUB_TOKEN")),
            "a variable reference is not a secret: {found:?}"
        );
    }

    #[test]
    fn the_secret_is_the_configured_group_else_the_first_group_else_the_match() {
        let whole = Pattern::new("whole", r"key-[a-z]+").unwrap();
        assert_eq!(whole.find("use key-abc now"), ["key-abc"]);
        let first = Pattern::new("first", r"(?:(x)|(y))=([a-z]+)").unwrap();
        assert_eq!(first.find("y=abc"), ["y"]);
        let mut second = Pattern::new("second", r"(?:(x)|(y))=([a-z]+)").unwrap();
        second.secret_group = Some(3);
        assert_eq!(second.find("y=abc"), ["abc"]);
        assert!(Pattern::new("bad", "(").is_err());
    }

    #[test]
    fn entropy_and_allowlists_gate_a_match() {
        let mut p = Pattern::new("p", r"secret=(\w+)").unwrap();
        p.entropy = Some(3.0);
        assert_eq!(p.find("secret=aaaaaaaa"), Vec::<&str>::new());
        assert_eq!(p.find("secret=q7Zp2mK9vL"), ["q7Zp2mK9vL"]);

        p.entropy = None;
        p.allowlists.push(Allowlist {
            target: Target::Line,
            regexes: vec![Regex::new("^# ").unwrap()],
            stopwords: vec!["example".to_string()],
            all_of: false,
            has_paths: false,
        });
        assert_eq!(p.find("# secret=q7Zp2mK9vL"), Vec::<&str>::new());
        assert_eq!(p.find("secret=exampleKey"), Vec::<&str>::new());
        assert_eq!(p.find("secret=q7Zp2mK9vL"), ["q7Zp2mK9vL"]);

        p.allowlists[0].all_of = true;
        assert_eq!(
            p.find("# secret=q7Zp2mK9vL"),
            ["q7Zp2mK9vL"],
            "AND needs the stopword too"
        );
        assert_eq!(p.find("# secret=exampleKey"), Vec::<&str>::new());
        p.allowlists[0].has_paths = true;
        assert_eq!(
            p.find("# secret=exampleKey"),
            ["exampleKey"],
            "AND with a path criterion never applies"
        );
    }

    #[test]
    fn shannon_entropy_counts_bits_per_character() {
        assert_eq!(shannon_entropy(""), 0.0);
        assert_eq!(shannon_entropy("aaaa"), 0.0);
        assert!((shannon_entropy("abcd") - 2.0).abs() < 1e-9);
    }

    #[test]
    fn keywords_admit_only_the_patterns_that_list_them() {
        let secret = secret();
        let set = &*secret.inner;
        let keywords = set.keywords.as_ref().unwrap();
        let none = keywords.candidates("nothing of interest here");
        assert!(none.is_empty(), "{none:?}");
        let some = keywords.candidates("my SK-ANT-API03 thing");
        let names: Vec<&str> = some
            .iter()
            .map(|&i| set.patterns[i].name.as_str())
            .collect();
        assert!(names.contains(&"anthropic-api-key"), "{names:?}");
        assert!(!names.contains(&"aws-access-token"), "{names:?}");
    }

    #[test]
    fn port_go_regex_escapes_literal_braces_and_brackets_and_narrows_classes() {
        assert_eq!(
            port_go_regex(r"^\$(?:\d+|{\d+})$"),
            r"^\$(?:(?-u:\d)+|\{(?-u:\d)+\})$"
        );
        assert_eq!(port_go_regex(r"a{2,3}b{4}c{5,}"), r"a{2,3}b{4}c{5,}");
        assert_eq!(port_go_regex(r"[\w.-]{10,150}"), r"[0-9A-Za-z_.-]{10,150}");
        assert_eq!(port_go_regex(r"[^}]+"), r"[^}]+");
        assert_eq!(port_go_regex(r"[[:alnum:]]{14}"), r"[[:alnum:]]{14}");
        assert_eq!(port_go_regex(r"[a[b]"), r"[a\[b]");
        assert_eq!(port_go_regex(r"\bx\s\Sy"), r"(?-u:\b)x(?-u:\s)\Sy");
        assert_eq!(port_go_regex(r"[\s\S-]"), r"[\t\n\x0B\x0C\r \S-]");
        for ported in [
            port_go_regex(r"^\${(?:[A-Z_]+|[a-z_]+)}$"),
            port_go_regex(r#"['"]?\$?{{[^}]+}}['"]?"#),
        ] {
            Regex::new(&ported).unwrap_or_else(|e| panic!("{ported}: {e}"));
        }
    }

    /// The pii filter's patterns, as its `PATTERNS` object declares
    /// them, find emails, phones, SSNs, and the git user line's name
    /// alone.
    #[test]
    fn plain_patterns_find_each_match_and_the_first_group() {
        let patterns = Patterns::new(vec![
            Pattern::new("email", "[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\\.[A-Za-z]{2,}").unwrap(),
            Pattern::new(
                "phone",
                "(?:^|[^0-9A-Za-z])((?:\\+?[0-9]{1,3}[ .-])?\\(?[0-9]{3}\\)?[ .-][0-9]{3}[ .-][0-9]{4})(?:[^0-9A-Za-z]|$)",
            )
            .unwrap(),
            Pattern::new("ssn", "(?:^|[^0-9-])([0-9]{3}-[0-9]{2}-[0-9]{4})(?:[^0-9-]|$)").unwrap(),
            Pattern::new("git_user", "(?m)^Git user: (.+?)[ \t]*$").unwrap(),
        ]);
        let text =
            "mail alice@example.com or call 555-123-4567; ssn 123-45-6789; ticket 2026-10-23";
        let found = patterns.matches(text);
        let literals: Vec<&str> = found.iter().map(|(_, m)| *m).collect();
        assert_eq!(
            literals,
            ["alice@example.com", "555-123-4567", "123-45-6789"]
        );
        let status = "Main branch: main\n\nGit user: Alice B. Carol  \n\nStatus:\n(clean)\nsee Git user: docs";
        assert_eq!(patterns.matches(status), [("git_user", "Alice B. Carol")]);
    }
}
