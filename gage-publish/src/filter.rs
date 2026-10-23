//! The named filters: the instruction prose the agent follows and the
//! deterministic patterns the engine applies. A filter's typed-
//! sensitive paths are content-format knowledge and come from the
//! driver's text descriptor, not from here.

use std::sync::LazyLock;

use regex::Regex;

/// A named filter.
pub struct Filter {
    pub name: &'static str,
    /// The reason code its redactions carry
    pub reason: &'static str,
    /// One line on what the filter removes, for a listing
    pub description: &'static str,
    /// The instruction prose for the agent
    pub instructions: &'static str,
    patterns: Vec<Pattern>,
}

/// A compiled matcher for a known shape of sensitive text. When the
/// regex has a capture group, the group is the sensitive literal;
/// otherwise the whole match is.
pub struct Pattern {
    pub name: &'static str,
    regex: Regex,
}

impl Pattern {
    fn new(name: &'static str, regex: &str) -> Self {
        Self {
            name,
            regex: Regex::new(regex).expect("filter patterns are fixed and valid"),
        }
    }

    /// The sensitive literals this pattern finds in `text`.
    pub fn find<'t>(&self, text: &'t str) -> Vec<&'t str> {
        self.regex
            .captures_iter(text)
            .filter_map(|c| c.get(1).or_else(|| c.get(0)))
            .map(|m| m.as_str())
            .collect()
    }
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

    pub fn patterns(&self) -> &[Pattern] {
        &self.patterns
    }

    /// Every `(pattern name, literal)` the filter's patterns find in
    /// `text`.
    pub fn matches<'t>(&self, text: &'t str) -> Vec<(&'static str, &'t str)> {
        self.patterns
            .iter()
            .flat_map(|p| p.find(text).into_iter().map(move |m| (p.name, m)))
            .collect()
    }
}

static FILTERS: LazyLock<Vec<Filter>> = LazyLock::new(|| {
    vec![
        Filter {
            name: "pii",
            reason: "pii",
            description: "Remove personally identifiable information",
            instructions: include_str!("../filters/pii.md"),
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
            ],
        },
        Filter {
            name: "secret",
            reason: "secret",
            description: "Remove passwords, API keys, and other secrets",
            instructions: include_str!("../filters/secret.md"),
            patterns: vec![
                Pattern::new("aws-access-key", r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b"),
                Pattern::new("github-token", r"\bgh[pousr]_[A-Za-z0-9]{36,}\b"),
                Pattern::new("anthropic-key", r"\bsk-ant-[A-Za-z0-9_-]{20,}"),
                Pattern::new("openai-key", r"\bsk-(?:proj-)?[A-Za-z0-9_-]{32,}"),
                Pattern::new("slack-token", r"\bxox[abpr]-[A-Za-z0-9-]{10,}"),
                Pattern::new(
                    "private-key",
                    r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----",
                ),
                Pattern::new("bearer-token", r"(?i)\bbearer\s+([A-Za-z0-9._~+/=-]{20,})"),
                Pattern::new(
                    "assignment",
                    r#"(?i)\b(?:password|passwd|secret|api[_-]?key|access[_-]?token)\s*[:=]\s*["']?([^\s"']{8,})"#,
                ),
            ],
        },
        Filter {
            name: "nsfw",
            reason: "privacy",
            description: "Remove profanity and other language unfit for a public record",
            instructions: include_str!("../filters/nsfw.md"),
            patterns: Vec::new(),
        },
    ]
});

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn secret_patterns_find_keys_and_the_value_of_an_assignment() {
        let text = "export ANTHROPIC_API_KEY=sk-ant-api03-abcdefghijklmnopqrstuvwxyz0123 and AKIAABCDEFGHIJKLMNOP\nAuthorization: Bearer eyJhbGciOiJIUzI1NiJ9.e30.abc\npassword = \"hunter2hunter2\"";
        let found = Filter::named("secret").unwrap().matches(text);
        let literals: Vec<&str> = found.iter().map(|(_, m)| *m).collect();
        assert!(
            literals.contains(&"sk-ant-api03-abcdefghijklmnopqrstuvwxyz0123"),
            "{literals:?}"
        );
        assert!(literals.contains(&"AKIAABCDEFGHIJKLMNOP"), "{literals:?}");
        assert!(
            literals.contains(&"eyJhbGciOiJIUzI1NiJ9.e30.abc"),
            "{literals:?}"
        );
        assert!(literals.contains(&"hunter2hunter2"), "{literals:?}");
    }

    #[test]
    fn pii_patterns_find_emails_phones_and_ssns() {
        let text =
            "mail alice@example.com or call 555-123-4567; ssn 123-45-6789; ticket 2026-10-23";
        let found = Filter::named("pii").unwrap().matches(text);
        let literals: Vec<&str> = found.iter().map(|(_, m)| *m).collect();
        assert_eq!(
            literals,
            ["alice@example.com", "555-123-4567", "123-45-6789"]
        );
    }
}
