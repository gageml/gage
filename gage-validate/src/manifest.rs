//! The list of upstream sources a run downloads.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use regex::Regex;
use serde::Deserialize;

use crate::Error;

/// The sources bundled with the suite
pub const BUNDLED: &str = include_str!("../sources.toml");

/// One upstream source of native session files.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SourceSpec {
    pub name: String,
    /// The harness whose sessions the source holds, by driver name
    pub harness: String,
    /// The URL of the Git repository holding the files
    pub git: String,
    /// The licence the source states for itself
    pub license: String,
    /// Globs over the repository's paths: `*` and `?` match within a
    /// path segment, `**` matches across segments
    pub include: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    source: Vec<SourceSpec>,
}

/// The sources `text` lists, in order. Two sources of one name are an
/// error: a report names a source by it.
pub fn parse(text: &str) -> Result<Vec<SourceSpec>, Error> {
    let manifest: Manifest =
        toml::from_str(text).map_err(|e| Error::Other(format!("sources: {e}")))?;
    let mut names = BTreeSet::new();
    for spec in &manifest.source {
        if !names.insert(spec.name.as_str()) {
            return Err(Error::Other(format!(
                "sources: {:?} is listed twice",
                spec.name
            )));
        }
    }
    Ok(manifest.source)
}

impl SourceSpec {
    /// The files of `checkout` the source includes, as paths relative
    /// to it, in path order.
    pub fn files(&self, checkout: &Path) -> Result<Vec<PathBuf>, Error> {
        let globs = self
            .include
            .iter()
            .map(|glob| glob_regex(glob))
            .collect::<Result<Vec<_>, _>>()?;
        let mut all = Vec::new();
        walk(checkout, Path::new(""), &mut all)?;
        let mut files: Vec<PathBuf> = all
            .into_iter()
            .filter(|path| {
                let text = path.to_string_lossy().replace('\\', "/");
                globs.iter().any(|glob| glob.is_match(&text))
            })
            .collect();
        files.sort();
        Ok(files)
    }
}

fn walk(root: &Path, rel: &Path, out: &mut Vec<PathBuf>) -> Result<(), Error> {
    for entry in fs::read_dir(root.join(rel))? {
        let entry = entry?;
        let name = entry.file_name();
        if name == ".git" {
            continue;
        }
        let path = rel.join(&name);
        if entry.file_type()?.is_dir() {
            walk(root, &path, out)?;
        } else {
            out.push(path);
        }
    }
    Ok(())
}

fn glob_regex(glob: &str) -> Result<Regex, Error> {
    let mut pattern = String::from("^");
    let mut rest = glob;
    while let Some(c) = rest.chars().next() {
        if let Some(after) = rest.strip_prefix("**/") {
            pattern.push_str("(?:.*/)?");
            rest = after;
        } else if let Some(after) = rest.strip_prefix("**") {
            pattern.push_str(".*");
            rest = after;
        } else {
            match c {
                '*' => pattern.push_str("[^/]*"),
                '?' => pattern.push_str("[^/]"),
                _ => pattern.push_str(&regex::escape(c.encode_utf8(&mut [0; 4]))),
            }
            rest = rest.get(c.len_utf8()..).unwrap_or_default();
        }
    }
    pattern.push('$');
    Regex::new(&pattern).map_err(|e| Error::Other(format!("glob {glob:?}: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::harness;

    #[test]
    fn every_bundled_source_names_a_harness_the_suite_stages() {
        let specs = parse(BUNDLED).unwrap();
        assert!(!specs.is_empty());
        for spec in &specs {
            assert!(harness(&spec.harness).is_some(), "{}", spec.name);
        }
    }

    #[test]
    fn a_source_listed_twice_is_refused() {
        let one = r#"
            [[source]]
            name = "a"
            harness = "claude"
            git = "https://example.com/a"
            license = "MIT"
            include = ["*.jsonl"]
        "#;
        let err = parse(&format!("{one}{one}")).unwrap_err();
        assert_eq!(err.to_string(), "sources: \"a\" is listed twice");
    }

    #[test]
    fn a_star_stays_in_its_segment_and_two_stars_cross_segments() {
        let dir = tempfile::tempdir().unwrap();
        for file in ["a/x.jsonl", "a/b/y.jsonl", "a/b/z.json", "c.jsonl"] {
            let path = dir.path().join(file);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "").unwrap();
        }
        let files_of = |include: &str| {
            let spec = SourceSpec {
                name: "s".into(),
                harness: "claude".into(),
                git: String::new(),
                license: String::new(),
                include: vec![include.into()],
            };
            spec.files(dir.path()).unwrap()
        };
        assert_eq!(files_of("a/*.jsonl"), [PathBuf::from("a/x.jsonl")]);
        assert_eq!(
            files_of("a/**/*.jsonl"),
            [PathBuf::from("a/b/y.jsonl"), PathBuf::from("a/x.jsonl")]
        );
        assert_eq!(files_of("?.jsonl"), [PathBuf::from("c.jsonl")]);
    }
}
