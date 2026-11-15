//! The recorded patch of a tool call: the hunks a harness records in a
//! call's result when the call changed files.

use std::path::PathBuf;

/// One hunk of a recorded patch
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchHunk {
    /// The file the hunk changed, when the record names it
    pub path: Option<PathBuf>,
    /// Each line with its marker: `+`, `-`, or a space
    pub lines: Vec<(char, String)>,
}

impl PatchHunk {
    /// The lines with marker `op`
    pub fn lines_with(&self, op: char) -> impl Iterator<Item = &str> {
        self.lines
            .iter()
            .filter(move |(m, _)| *m == op)
            .map(|(_, l)| l.as_str())
    }
}
