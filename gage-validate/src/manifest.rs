//! The list of sources a run checks: upstream repositories it
//! downloads, and directories of this repository.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use regex::Regex;
use serde::Deserialize;

use crate::Error;

/// The sources bundled with the suite
pub const BUNDLED: &str = include_str!("../sources.toml");

/// One source of native session files: an upstream repository, named
/// by `git`, or a directory of this repository, named by `path`. A
/// source names exactly one of the two.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SourceSpec {
    pub name: String,
    /// The harness whose sessions the source holds, by driver name
    pub harness: String,
    /// The URL of the Git repository holding the files
    #[serde(default)]
    pub git: Option<String>,
    /// The directory of this repository holding the files, relative
    /// to the repository's root: sessions the repository keeps
    /// itself, which a run reads in place
    #[serde(default)]
    pub path: Option<PathBuf>,
    /// The licence the source states for itself
    pub license: String,
    /// Globs over the paths under the repository or the directory:
    /// `*` and `?` match within a path segment, `**` matches across
    /// segments
    pub include: Vec<String>,
}

/// Where a source's files are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin<'a> {
    /// The Git repository at this URL
    Git(&'a str),
    /// This directory of the repository the suite is built from
    Repository(&'a Path),
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
        if spec.git.is_some() == spec.path.is_some() {
            return Err(Error::Other(format!(
                "sources: {:?} names a git repository or a path, one of the two",
                spec.name
            )));
        }
    }
    Ok(manifest.source)
}

impl SourceSpec {
    /// Where the source's files are. A source [`parse`] returned
    /// names one origin; one built without either is a directory at
    /// the repository's root.
    pub fn origin(&self) -> Origin<'_> {
        match (&self.git, &self.path) {
            (Some(url), _) => Origin::Git(url),
            (None, Some(path)) => Origin::Repository(path),
            (None, None) => Origin::Repository(Path::new("")),
        }
    }

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

/// The root of the repository the suite is built from, which a
/// source's `path` is relative to.
pub fn repository_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the crate should sit in the workspace root")
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
    fn a_source_is_a_git_repository_or_a_path_of_this_one() {
        let source = |origin: &str| {
            let text = format!(
                "[[source]]\nname = \"a\"\nharness = \"pi\"\n{origin}\n\
                 license = \"MIT\"\ninclude = [\"*.jsonl\"]\n"
            );
            parse(&text).map(|mut specs| specs.remove(0))
        };
        let upstream = source(r#"git = "https://example.com/a""#).unwrap();
        assert_eq!(upstream.origin(), Origin::Git("https://example.com/a"));
        let kept = source(r#"path = "gage-pi/fixtures""#).unwrap();
        assert_eq!(
            kept.origin(),
            Origin::Repository(Path::new("gage-pi/fixtures"))
        );
        for origin in ["", "git = \"https://example.com/a\"\npath = \"b\""] {
            let err = source(origin).unwrap_err().to_string();
            assert!(err.contains("one of the two"), "{err}");
        }
    }

    /// A source of this repository is read in place, so its directory
    /// has to be there with files its globs match.
    #[test]
    fn every_bundled_source_of_this_repository_holds_files() {
        for spec in parse(BUNDLED).unwrap() {
            if let Origin::Repository(path) = spec.origin() {
                let files = spec.files(&repository_root().join(path)).unwrap();
                assert!(!files.is_empty(), "{}", spec.name);
            }
        }
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
                git: None,
                path: None,
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
