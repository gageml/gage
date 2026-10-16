//! View options parsed from the `-v/--options` CLI flag.

use std::fmt;

/// View state for the session outline and content pane. The membership
/// flags (`assistant`, `user`, `tools`, `all_entries`) choose which
/// entries populate the left tree; `details` and `turns` are independent
/// of membership and change only how an entry renders.
#[derive(Debug, Clone)]
pub struct ViewOptions {
    /// Assistant messages and thinking
    pub assistant: bool,
    /// User messages
    pub user: bool,
    /// Tool calls, meaning a tool use paired with its result
    pub tools: bool,
    /// Every entry, including bookkeeping, system, and summary entries
    pub all_entries: bool,
    /// Raw entry detail in the content pane
    pub details: bool,
    /// Turn markers in the outline
    pub turns: bool,
}

impl Default for ViewOptions {
    fn default() -> Self {
        Self {
            assistant: true,
            user: true,
            tools: false,
            all_entries: false,
            details: false,
            turns: false,
        }
    }
}

impl ViewOptions {
    /// Apply a comma-separated option list over the defaults. A term is a
    /// short letter or long name; a leading `!` turns the option off
    /// instead of on.
    pub fn parse(spec: &str) -> Result<Self, UnknownOption> {
        let mut opts = Self::default();
        for term in spec.split(',') {
            let t = term.trim();
            if t.is_empty() {
                continue;
            }
            let (on, name) = match t.strip_prefix('!') {
                Some(rest) => (false, rest.trim()),
                None => (true, t),
            };
            match name {
                "a" | "assistant" => opts.assistant = on,
                "u" | "user" => opts.user = on,
                "t" | "tools" => opts.tools = on,
                "e" | "entries" => opts.all_entries = on,
                "d" | "details" => opts.details = on,
                "n" | "turns" => opts.turns = on,
                other => return Err(UnknownOption(other.to_string())),
            }
        }
        Ok(opts)
    }
}

#[derive(Debug)]
pub struct UnknownOption(pub String);

impl fmt::Display for UnknownOption {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown view option: {}", self.0)
    }
}

impl std::error::Error for UnknownOption {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_shows_user_and_assistant_only() {
        let o = ViewOptions::default();
        assert!(o.assistant);
        assert!(o.user);
        assert!(!o.tools);
        assert!(!o.all_entries);
        assert!(!o.details);
        assert!(!o.turns);
    }

    #[test]
    fn empty_spec_keeps_the_defaults() {
        let o = ViewOptions::parse("").unwrap();
        assert!(o.assistant && o.user && !o.tools);
    }

    #[test]
    fn negation_disables_a_default_on_option() {
        let o = ViewOptions::parse("!user").unwrap();
        assert!(!o.user);
        assert!(o.assistant);
    }

    #[test]
    fn comma_list_enables_default_off_options() {
        let o = ViewOptions::parse("tools,details,turns,entries").unwrap();
        assert!(o.tools);
        assert!(o.details);
        assert!(o.turns);
        assert!(o.all_entries);
    }

    #[test]
    fn short_and_long_names_are_equivalent() {
        let short = ViewOptions::parse("t,!u").unwrap();
        let long = ViewOptions::parse("tools,!user").unwrap();
        assert_eq!(short.tools, long.tools);
        assert_eq!(short.user, long.user);
    }

    #[test]
    fn unknown_term_is_rejected() {
        assert!(ViewOptions::parse("bogus").is_err());
    }
}
