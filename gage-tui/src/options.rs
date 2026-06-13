//! View options parsed from the `-o/--options` CLI flag.

use std::fmt;

#[derive(Default, Debug, Clone)]
pub struct ViewOptions {
    pub show_turns: bool,
}

impl ViewOptions {
    pub fn parse(terms: &[String]) -> Result<Self, UnknownOption> {
        let mut opts = Self::default();
        for term in terms {
            let t = term.trim();
            if t.is_empty() {
                continue;
            }
            match t {
                "turns" => opts.show_turns = true,
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
