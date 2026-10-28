//! The candidate text: every distinct text value a pending publish
//! discloses, once, as the redaction agent reads it. The text is one
//! value per line as a JSON string, so a newline inside a value is an
//! escape and a value is never cut, and it is divided into splits
//! bounded by size, each a whole number of values.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::derive::Candidate;

/// Each distinct text once, in order of first occurrence.
pub fn unique_texts(candidates: &[Candidate]) -> Vec<String> {
    let mut seen = HashSet::new();
    candidates
        .iter()
        .filter(|c| seen.insert(c.text.as_str()))
        .map(|c| c.text.clone())
        .collect()
}

/// A text as a line of candidate text: a JSON string and a newline.
pub fn encode_line(text: &str) -> String {
    let mut line = serde_json::to_string(text).expect("a str is a JSON string");
    line.push('\n');
    line
}

/// `texts` divided into splits of at most `max_bytes` of encoded
/// lines. A split closes when its next line would carry it past the
/// bound, and a line larger than the bound is a split by itself. No
/// value is cut.
pub fn splits(texts: &[String], max_bytes: usize) -> Vec<Vec<&str>> {
    let mut out: Vec<Vec<&str>> = Vec::new();
    let mut size = 0;
    for text in texts {
        let len = encode_line(text).len();
        match out.last_mut() {
            Some(last) if size + len <= max_bytes => {
                last.push(text);
                size += len;
            }
            _ => {
                out.push(vec![text]);
                size = len;
            }
        }
    }
    out
}

/// The splits as one text, with a line holding `null` between splits.
pub fn render(splits: &[Vec<&str>]) -> String {
    let mut out = String::new();
    for (i, split) in splits.iter().enumerate() {
        if i > 0 {
            out.push_str("null\n");
        }
        for text in split {
            out.push_str(&encode_line(text));
        }
    }
    out
}

/// Write each split to `dir` as `000001.jsonl` onward and return the
/// paths. Split files already in `dir` are removed first, so a longer
/// earlier run leaves nothing behind.
pub fn write_files(dir: &Path, splits: &[Vec<&str>]) -> io::Result<Vec<PathBuf>> {
    fs::create_dir_all(dir)?;
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if is_split_file(&path) {
            fs::remove_file(&path)?;
        }
    }
    let mut paths = Vec::with_capacity(splits.len());
    for (i, split) in splits.iter().enumerate() {
        let path = dir.join(format!("{:06}.jsonl", i + 1));
        let text: String = split.iter().map(|t| encode_line(t)).collect();
        fs::write(&path, text)?;
        paths.push(path);
    }
    Ok(paths)
}

fn is_split_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_suffix(".jsonl"))
        .is_some_and(|stem| stem.len() == 6 && stem.bytes().all(|b| b.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(text: &str) -> Candidate {
        Candidate {
            line: 1,
            pointer: "/p".into(),
            text: text.into(),
            spans: Vec::new(),
        }
    }

    #[test]
    fn unique_texts_keep_first_occurrence_order_and_exact_case() {
        let candidates = [
            candidate("b"),
            candidate("a"),
            candidate("b"),
            candidate("A"),
        ];
        assert_eq!(unique_texts(&candidates), ["b", "a", "A"]);
    }

    /// Each line is a JSON string, so a newline inside a value is an
    /// escape. A split closes before the line that would overflow it,
    /// an oversize line stands alone, and the bound counts encoded
    /// bytes.
    #[test]
    fn splits_hold_whole_lines_within_the_bound() {
        let texts: Vec<String> = ["ab", "c\nd", "efghijklmnop", "q", "r"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(encode_line(&texts[1]), "\"c\\nd\"\n");
        // Lines are 5, 7, 15, 4, 4 bytes
        assert_eq!(
            splits(&texts, 12),
            [vec!["ab", "c\nd"], vec!["efghijklmnop"], vec!["q", "r"]]
        );
        assert_eq!(splits(&texts, 0).len(), 5);
        assert!(splits(&[], 12).is_empty());
    }

    #[test]
    fn render_separates_splits_with_a_null_line() {
        let texts: Vec<String> = ["a", "b", "c"].iter().map(|s| s.to_string()).collect();
        assert_eq!(render(&splits(&texts, 8)), "\"a\"\n\"b\"\nnull\n\"c\"\n");
        assert_eq!(render(&splits(&texts, 100)), "\"a\"\n\"b\"\n\"c\"\n");
    }

    #[test]
    fn write_files_numbers_the_splits_and_removes_stale_ones() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("000009.jsonl"), "stale").unwrap();
        fs::write(dir.path().join("notes.txt"), "kept").unwrap();
        let texts: Vec<String> = ["a", "b", "c"].iter().map(|s| s.to_string()).collect();
        let paths = write_files(dir.path(), &splits(&texts, 8)).unwrap();
        assert_eq!(
            paths,
            [
                dir.path().join("000001.jsonl"),
                dir.path().join("000002.jsonl")
            ]
        );
        assert_eq!(fs::read_to_string(&paths[0]).unwrap(), "\"a\"\n\"b\"\n");
        assert_eq!(fs::read_to_string(&paths[1]).unwrap(), "\"c\"\n");
        assert!(!dir.path().join("000009.jsonl").exists());
        assert!(dir.path().join("notes.txt").exists());
    }
}
