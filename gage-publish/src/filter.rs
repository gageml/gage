//! The named filters: the instruction prose the agent follows and the
//! deterministic patterns the engine applies. The `secret` filter's
//! patterns are the gitleaks rule set, vendored as `filters/gitleaks.toml`
//! under the MIT license beside it. A filter's typed-sensitive paths are
//! content-format knowledge and come from the driver's text descriptor,
//! not from here; the attributes it leaves out are store knowledge and
//! are named here.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::str::FromStr;
use std::sync::LazyLock;

use aho_corasick::{AhoCorasick, AhoCorasickBuilder};
use gage_core::front_matter;
use regex::Regex;
use serde::Deserialize;

use crate::PublishError;

/// A named filter.
pub struct Filter {
    pub name: &'static str,
    /// The reason code its redactions carry
    pub reason: &'static str,
    /// One line on what the filter removes, for a listing
    pub description: &'static str,
    /// The instruction prose for the agent, after the front matter. A
    /// filter that requires a prompt has prose on how the prompt is
    /// read
    pub instructions: &'static str,
    /// The audiences the filter is selected for. The values are
    /// independent: a listed value selects the filter, and no value
    /// implies another.
    pub audience: Vec<Audience>,
    /// The uses the filter is selected for, under the same rule.
    pub uses: Vec<Use>,
    /// The attributes of `attrs.json` the filter leaves out of the
    /// published session
    pub attrs: &'static [&'static str],
    patterns: Vec<Pattern>,
    /// The keyword prefilter: a pattern with keywords runs only on text
    /// containing one of them.
    keywords: Option<Keywords>,
}

/// Who will read the published session. The values are independent
/// labels; a filter lists each one it is selected for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Audience {
    /// Private use only; no filter lists it
    #[serde(rename = "self")]
    Private,
    /// An audience the publisher knows and trusts
    Trusted,
    /// Anyone
    Public,
}

impl Audience {
    /// The values as the CLI and the front matter spell them, in
    /// listing order.
    pub const NAMES: &[&str] = &["self", "trusted", "public"];

    pub fn name(self) -> &'static str {
        match self {
            Audience::Private => "self",
            Audience::Trusted => "trusted",
            Audience::Public => "public",
        }
    }
}

impl FromStr for Audience {
    type Err = PublishError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "self" => Ok(Audience::Private),
            "trusted" => Ok(Audience::Trusted),
            "public" => Ok(Audience::Public),
            other => Err(PublishError::Input(format!(
                "unknown audience {other:?}; audiences are {}",
                Audience::NAMES.join(", ")
            ))),
        }
    }
}

impl fmt::Display for Audience {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// What the published session is for. The values are independent
/// labels; a filter lists each one it is selected for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Use {
    /// Reading for narrative content
    Reading,
    /// A record of work; every entry stays, and no filter lists it
    Provenance,
    /// Another use; no filter lists it
    Other,
}

impl Use {
    /// The values as the CLI and the front matter spell them, in
    /// listing order.
    pub const NAMES: &[&str] = &["reading", "provenance", "other"];

    pub fn name(self) -> &'static str {
        match self {
            Use::Reading => "reading",
            Use::Provenance => "provenance",
            Use::Other => "other",
        }
    }
}

impl FromStr for Use {
    type Err = PublishError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "reading" => Ok(Use::Reading),
            "provenance" => Ok(Use::Provenance),
            "other" => Ok(Use::Other),
            other => Err(PublishError::Input(format!(
                "unknown use {other:?}; uses are {}",
                Use::NAMES.join(", ")
            ))),
        }
    }
}

impl fmt::Display for Use {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// The front matter of a filter's prose file: the audiences and uses
/// the filter is selected for. A file without front matter lists
/// neither.
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FrontMatter {
    #[serde(default)]
    audience: Vec<Audience>,
    #[serde(default, rename = "use")]
    uses: Vec<Use>,
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
    fn new(name: &str, regex: &str) -> Self {
        Self {
            name: name.to_string(),
            regex: Regex::new(regex).expect("filter patterns are fixed and valid"),
            secret_group: None,
            entropy: None,
            allowlists: Vec::new(),
            keywords: Vec::new(),
        }
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

/// The keyword prefilter over a filter's patterns: one automaton over
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
            let admitted = self
                .admits
                .get(hit.pattern().as_usize())
                .expect("automaton pattern ids index admits");
            set.extend(admitted);
        }
        set
    }
}

/// The gitleaks configuration file, as far as the filter reads it.
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

/// A rule in the vendored file that this build cannot use.
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

/// The patterns of the vendored gitleaks rule set. A rule without a
/// regex, or with a path condition, does not apply to session text and
/// is left out. The global allowlist is appended to every rule's.
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

impl Filter {
    /// The named filter, when `name` is one.
    pub fn named(name: &str) -> Option<&'static Filter> {
        FILTERS.iter().find(|f| f.name == name)
    }

    /// Every named filter, in listing order.
    pub fn all() -> &'static [Filter] {
        &FILTERS
    }

    /// The named filters that list `audience` or list `uses`, in
    /// listing order. A value not given selects nothing.
    pub fn selected(audience: Option<Audience>, uses: Option<Use>) -> Vec<&'static Filter> {
        FILTERS
            .iter()
            .filter(|f| {
                audience.is_some_and(|a| f.audience.contains(&a))
                    || uses.is_some_and(|u| f.uses.contains(&u))
            })
            .collect()
    }

    /// The filter with its front matter read out of `instructions`:
    /// the audiences and uses it lists, and the prose after them.
    fn read_front_matter(mut self) -> Self {
        let (toml, body) = front_matter::split(self.instructions)
            .unwrap_or_else(|e| panic!("filter {}: {e}", self.name));
        let front: FrontMatter = toml
            .map(|t| toml::from_str(t).unwrap_or_else(|e| panic!("filter {}: {e}", self.name)))
            .unwrap_or_default();
        self.audience = front.audience;
        self.uses = front.uses;
        self.instructions = body.trim_start();
        self
    }

    /// The instructions with `prompt` in place of every `{{ prompt }}`
    /// placeholder. Without a placeholder the prompt has no place in
    /// the instructions and is left out.
    pub fn instructions_with_prompt(&self, prompt: Option<&str>) -> String {
        static PLACEHOLDER: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"\{\{\s*prompt\s*\}\}").unwrap());
        PLACEHOLDER
            .replace_all(self.instructions, prompt.unwrap_or_default())
            .trim_end()
            .to_string()
    }

    pub fn patterns(&self) -> &[Pattern] {
        &self.patterns
    }

    /// Every `(pattern name, literal)` the filter's patterns find in
    /// `text`.
    pub fn matches<'t>(&self, text: &'t str) -> Vec<(&str, &'t str)> {
        let candidates: Vec<&Pattern> = match &self.keywords {
            Some(keywords) => keywords
                .candidates(text)
                .into_iter()
                .map(|i| self.patterns.get(i).expect("candidates index patterns"))
                .collect(),
            None => self.patterns.iter().collect(),
        };
        candidates
            .into_iter()
            .flat_map(|p| p.find(text).into_iter().map(move |m| (p.name.as_str(), m)))
            .collect()
    }
}

/// The vendored gitleaks rule file.
const GITLEAKS: &str = include_str!("../filters/gitleaks.toml");

static FILTERS: LazyLock<Vec<Filter>> = LazyLock::new(|| {
    let secret = gitleaks_patterns(GITLEAKS).expect("vendored gitleaks rules compile");
    let filters = vec![
        Filter {
            name: "pii",
            reason: "pii",
            description: "Remove personally identifiable information",
            instructions: include_str!("../filters/pii.md"),
            audience: Vec::new(),
            uses: Vec::new(),
            attrs: &[],
            patterns: vec![
                Pattern::new("email", r"[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}"),
                Pattern::new(
                    "phone",
                    r"(?:^|[^0-9A-Za-z])((?:\+?[0-9]{1,3}[ .-])?\(?[0-9]{3}\)?[ .-][0-9]{3}[ .-][0-9]{4})(?:[^0-9A-Za-z]|$)",
                ),
                Pattern::new(
                    "ssn",
                    r"(?:^|[^0-9-])([0-9]{3}-[0-9]{2}-[0-9]{4})(?:[^0-9-]|$)",
                ),
                // The name the harness's repository status reminder
                // quotes from the Git identity
                Pattern::new("git-user", r"(?m)^Git user: (.+?)[ \t]*$"),
            ],
            keywords: None,
        },
        Filter {
            name: "secret",
            reason: "secret",
            description: "Remove passwords, API keys, and other secrets",
            instructions: include_str!("../filters/secret.md"),
            audience: Vec::new(),
            uses: Vec::new(),
            attrs: &[],
            keywords: Keywords::new(&secret),
            patterns: secret,
        },
        Filter {
            name: "language",
            reason: "language",
            description: "Remove profanity and other language unfit for a shared record",
            instructions: include_str!("../filters/language.md"),
            audience: Vec::new(),
            uses: Vec::new(),
            attrs: &[],
            patterns: Vec::new(),
            keywords: None,
        },
        Filter {
            name: "system",
            reason: "system",
            description: "Remove paths, hosts, and OS versions",
            instructions: include_str!("../filters/system.md"),
            audience: Vec::new(),
            uses: Vec::new(),
            attrs: &["project"],
            patterns: Vec::new(),
            keywords: None,
        },
        Filter {
            name: "harness-context",
            reason: "harness-context",
            description: "Leave out what the harness placed in the model's context",
            instructions: include_str!("../filters/harness-context.md"),
            audience: Vec::new(),
            uses: Vec::new(),
            attrs: &[],
            patterns: Vec::new(),
            keywords: None,
        },
        Filter {
            name: "harness-records",
            reason: "harness-records",
            description: "Leave out records the harness kept about the run",
            instructions: include_str!("../filters/harness-records.md"),
            audience: Vec::new(),
            uses: Vec::new(),
            attrs: &[],
            patterns: Vec::new(),
            keywords: None,
        },
        Filter {
            name: "custom",
            reason: "custom",
            description: "Remove what a prompt of your own describes",
            instructions: include_str!("../filters/custom.md"),
            audience: Vec::new(),
            uses: Vec::new(),
            attrs: &[],
            patterns: Vec::new(),
            keywords: None,
        },
    ];
    filters.into_iter().map(Filter::read_front_matter).collect()
});

#[cfg(test)]
mod tests {
    use super::*;

    /// Every code a filter's prose quotes as a reason is a listed
    /// reason, and a filter's own code is one.
    #[test]
    fn reason_codes_in_the_prose_are_listed() {
        use crate::definition::Reason;
        for filter in FILTERS.iter() {
            assert!(Reason::find(filter.reason).is_some(), "{}", filter.name);
            for quoted in filter
                .instructions
                .split("reason \"")
                .skip(1)
                .filter_map(|rest| rest.split('"').next())
            {
                assert!(Reason::find(quoted).is_some(), "{}: {quoted}", filter.name);
            }
        }
    }

    /// The prompt lands at every placeholder, whatever the spacing
    /// inside the braces, and nowhere when the prose has none.
    #[test]
    fn prompt_replaces_each_placeholder_or_is_left_out() {
        let custom = Filter::named("custom").unwrap();
        let with = custom.instructions_with_prompt(Some("Remove every duck"));
        assert!(with.ends_with("\n\nRemove every duck"), "{with}");
        assert!(!with.contains("{{"), "{with}");
        let without = custom.instructions_with_prompt(None);
        assert_eq!(
            without,
            custom
                .instructions
                .trim_end()
                .trim_end_matches("{{ prompt }}")
                .trim_end()
        );
        let filter = Filter {
            name: "t",
            reason: "custom",
            description: "",
            instructions: "A {{prompt}} B {{  prompt  }} C",
            audience: Vec::new(),
            uses: Vec::new(),
            attrs: &[],
            patterns: Vec::new(),
            keywords: None,
        };
        assert_eq!(filter.instructions_with_prompt(Some("x")), "A x B x C");
        let pii = Filter::named("pii").unwrap();
        let with = pii.instructions_with_prompt(Some("Remove every duck"));
        assert!(with.ends_with("\n\nRemove every duck"), "{with}");
        let language = Filter::named("language").unwrap();
        assert_eq!(
            language.instructions_with_prompt(Some("ignored")),
            language.instructions.trim_end()
        );
    }

    #[test]
    fn every_named_filter_has_prose_and_a_description() {
        for name in crate::NAMED_FILTERS {
            let filter = Filter::named(name).unwrap();
            assert!(!filter.instructions.trim().is_empty(), "{name}");
            assert!(!filter.description.is_empty(), "{name}");
        }
        assert_eq!(Filter::all().len(), crate::NAMED_FILTERS.len());
        assert!(Filter::named("ducks").is_none());
    }

    /// The front matter never reaches the model: every filter's
    /// instructions open with prose.
    #[test]
    fn instructions_begin_with_prose_and_carry_no_front_matter() {
        for filter in Filter::all() {
            let rendered = filter.instructions_with_prompt(None);
            assert!(!rendered.contains("+++"), "{}: {rendered}", filter.name);
            assert!(
                rendered.starts_with(|c: char| c.is_ascii_uppercase()),
                "{}: {rendered}",
                filter.name
            );
        }
    }

    /// The audiences and uses each filter lists, as its front matter
    /// states them. A change here is a change to what an audience or
    /// a use selects.
    #[test]
    fn front_matter_lists_each_filters_audiences_and_uses() {
        use Audience::{Public, Trusted};
        use Use::Reading;
        let listed: Vec<(&str, &[Audience], &[Use])> = Filter::all()
            .iter()
            .map(|f| (f.name, f.audience.as_slice(), f.uses.as_slice()))
            .collect();
        assert_eq!(
            listed,
            [
                ("pii", &[Public][..], &[][..]),
                ("secret", &[Public, Trusted][..], &[][..]),
                ("language", &[Public, Trusted][..], &[][..]),
                ("system", &[Public][..], &[][..]),
                ("harness-context", &[][..], &[Reading][..]),
                ("harness-records", &[][..], &[Reading][..]),
                ("custom", &[][..], &[][..]),
            ]
        );
    }

    /// A selection is the filters that list either value, in listing
    /// order. A value not given, and the values no filter lists,
    /// select nothing, and the values imply nothing about one another.
    #[test]
    fn selection_is_containment_in_either_list() {
        let names = |audience, uses| -> Vec<&str> {
            Filter::selected(audience, uses)
                .into_iter()
                .map(|f| f.name)
                .collect()
        };
        assert!(names(None, None).is_empty());
        assert!(names(Some(Audience::Private), Some(Use::Provenance)).is_empty());
        assert!(names(Some(Audience::Private), Some(Use::Other)).is_empty());
        assert_eq!(
            names(Some(Audience::Public), Some(Use::Reading)),
            [
                "pii",
                "secret",
                "language",
                "system",
                "harness-context",
                "harness-records"
            ]
        );
        assert_eq!(names(Some(Audience::Trusted), None), ["secret", "language"]);
        assert_eq!(
            names(None, Some(Use::Reading)),
            ["harness-context", "harness-records"]
        );
    }

    #[test]
    fn audience_and_use_parse_their_names_and_reject_others() {
        for name in Audience::NAMES {
            assert_eq!(name.parse::<Audience>().unwrap().name(), *name);
        }
        for name in Use::NAMES {
            assert_eq!(name.parse::<Use>().unwrap().name(), *name);
        }
        let err = "ducks".parse::<Audience>().unwrap_err().to_string();
        assert!(err.contains("self, trusted, public"), "{err}");
        let err = "ducks".parse::<Use>().unwrap_err().to_string();
        assert!(err.contains("reading, provenance, other"), "{err}");
    }

    #[test]
    fn every_gitleaks_rule_with_a_regex_compiles() {
        let patterns = gitleaks_patterns(GITLEAKS).unwrap();
        assert!(patterns.len() > 200, "{}", patterns.len());
        assert!(patterns.iter().any(|p| p.name == "anthropic-api-key"));
        assert!(patterns.iter().any(|p| p.name == "generic-api-key"));
        assert!(
            !patterns.iter().any(|p| p.name == "pkcs12-file"),
            "a path-only rule does not apply to session text"
        );
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
        let found = Filter::named("secret").unwrap().matches(&text);
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
        let whole = Pattern::new("whole", r"key-[a-z]+");
        assert_eq!(whole.find("use key-abc now"), ["key-abc"]);
        let first = Pattern::new("first", r"(?:(x)|(y))=([a-z]+)");
        assert_eq!(first.find("y=abc"), ["y"]);
        let mut second = Pattern::new("second", r"(?:(x)|(y))=([a-z]+)");
        second.secret_group = Some(3);
        assert_eq!(second.find("y=abc"), ["abc"]);
    }

    #[test]
    fn entropy_and_allowlists_gate_a_match() {
        let mut p = Pattern::new("p", r"secret=(\w+)");
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
        let secret = Filter::named("secret").unwrap();
        let keywords = secret.keywords.as_ref().unwrap();
        let none = keywords.candidates("nothing of interest here");
        assert!(none.is_empty(), "{none:?}");
        let some = keywords.candidates("my SK-ANT-API03 thing");
        let names: Vec<&str> = some
            .iter()
            .map(|&i| secret.patterns[i].name.as_str())
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

    #[test]
    fn pii_patterns_find_emails_phones_ssns_and_the_git_user() {
        let text =
            "mail alice@example.com or call 555-123-4567; ssn 123-45-6789; ticket 2026-10-23";
        let found = Filter::named("pii").unwrap().matches(text);
        let literals: Vec<&str> = found.iter().map(|(_, m)| *m).collect();
        assert_eq!(
            literals,
            ["alice@example.com", "555-123-4567", "123-45-6789"]
        );
        // The git user line yields the name alone, without the label or
        // trailing whitespace; the label mid-line is not the reminder
        let status = "Main branch: main\n\nGit user: Alice B. Carol  \n\nStatus:\n(clean)\nsee Git user: docs";
        let found = Filter::named("pii").unwrap().matches(status);
        assert_eq!(found, [("git-user", "Alice B. Carol")]);
    }
}
