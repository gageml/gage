//! `--scanner` values, shared by the commands that take them: a
//! registry name, else a path.
//!
//! A value is `<scanner>` or `<scanner>#{...}`, the suffix being a
//! params override. The scanner part resolves as a registry name
//! first, so a registry scanner shadows a file of the same name the
//! way the registry's own roots shadow one another. Otherwise it is a
//! path relative to the current directory: a directory holds its
//! `scanner.rn`, a file is parsed as given.

use std::path::{Path, PathBuf};

use gage_registry::scanner::{Scanner, ScannerDef, ScannerRegistry, parse_scanner_file};

/// Where a resolved `--scanner` value's definition lives.
pub(crate) enum ScannerSource<'a> {
    Registry(&'a ScannerDef),
    File(Box<ScannerDef>),
}

impl ScannerSource<'_> {
    pub(crate) fn def(&self) -> &ScannerDef {
        match self {
            ScannerSource::Registry(def) => def,
            ScannerSource::File(def) => def,
        }
    }
}

/// One resolved `--scanner` value.
pub(crate) struct ResolvedSpec<'a> {
    pub source: ScannerSource<'a>,
    /// The `#{...}` params override, when given
    pub params: Option<&'a str>,
    /// The value as the user gave it, for diagnostics
    pub spec: &'a str,
}

impl ResolvedSpec<'_> {
    /// The scanner with its params override applied.
    pub(crate) fn scanner(&self) -> Result<Scanner<'_>, String> {
        Scanner::from_spec(self.source.def(), self.params, self.spec).map_err(|e| e.to_string())
    }
}

/// Resolve every `--scanner` value, in order. Every failure is
/// reported in one error so the user sees them all at once. A scanner
/// named twice, by any two values, is an error.
pub(crate) fn resolve<'a>(
    registry: &'a ScannerRegistry,
    specs: &'a [String],
) -> Result<Vec<ResolvedSpec<'a>>, String> {
    let mut out: Vec<ResolvedSpec<'a>> = Vec::with_capacity(specs.len());
    let mut errors: Vec<String> = Vec::new();
    for spec in specs {
        let (name, params) = gage_registry::scanner::split_scanner_spec(spec);
        let source = match registry.get_def(name) {
            Some(def) => ScannerSource::Registry(def),
            None => match parse_path(Path::new(name)) {
                Ok(def) => ScannerSource::File(Box::new(def)),
                Err(e) => {
                    errors.push(e);
                    continue;
                }
            },
        };
        let resolved_name = &source.def().name;
        if out.iter().any(|r| &r.source.def().name == resolved_name) {
            errors.push(format!(
                "Scanner '{resolved_name}' specified more than once"
            ));
            continue;
        }
        out.push(ResolvedSpec {
            source,
            params,
            spec,
        });
    }
    if !errors.is_empty() {
        return Err(errors.join("\n"));
    }
    Ok(out)
}

/// Parse the scanner at `path`: a directory's `scanner.rn`, or the
/// file itself.
fn parse_path(path: &Path) -> Result<ScannerDef, String> {
    if !path.exists() {
        return Err(format!("Unknown scanner: {}", path.display()));
    }
    let file: PathBuf = if path.is_dir() {
        path.join("scanner.rn")
    } else {
        path.to_path_buf()
    };
    parse_scanner_file(&file).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn write_scanner(dir: &Path, name: &str) {
        fs::create_dir_all(dir).unwrap();
        fs::write(
            dir.join("scanner.rn"),
            format!("pub const SCANNER = #{{ name: {name:?}, description: \"d\", tasks: #{{}} }};"),
        )
        .unwrap();
    }

    #[test]
    fn registry_name_wins_then_a_directory_or_file_path() {
        let tmp = tempfile::tempdir().unwrap();
        let builtin = tmp.path().join("builtin");
        write_scanner(&builtin.join("reg"), "reg");
        // A file scanner that shares the registry name is shadowed
        let local = tmp.path().join("local");
        write_scanner(&local.join("reg"), "reg-from-file");
        write_scanner(&local.join("bundle"), "bundled");
        fs::write(
            local.join("single.rn"),
            "pub const SCANNER = #{ name: \"single\", description: \"d\", tasks: #{} };",
        )
        .unwrap();
        let registry = ScannerRegistry::load_from_roots(&builtin, vec![builtin.clone()]);

        let specs = vec![
            "reg".to_string(),
            format!("{}#{{ a: 1 }}", local.join("bundle").display()),
            local.join("single.rn").display().to_string(),
        ];
        let resolved = resolve(&registry, &specs).unwrap();
        let names: Vec<&str> = resolved
            .iter()
            .map(|r| r.source.def().name.as_str())
            .collect();
        assert_eq!(names, ["reg", "bundled", "single"]);
        assert!(matches!(resolved[0].source, ScannerSource::Registry(_)));
        assert!(matches!(resolved[1].source, ScannerSource::File(_)));
        assert_eq!(resolved[1].params, Some("#{ a: 1 }"));
        assert_eq!(resolved[2].params, None);
    }

    #[test]
    fn missing_paths_and_duplicates_are_reported_together() {
        let tmp = tempfile::tempdir().unwrap();
        let builtin = tmp.path().join("builtin");
        write_scanner(&builtin.join("reg"), "reg");
        let registry = ScannerRegistry::load_from_roots(&builtin, vec![builtin.clone()]);

        let specs = vec![
            "reg".to_string(),
            "nope".to_string(),
            builtin.join("reg").display().to_string(),
        ];
        let Err(err) = resolve(&registry, &specs) else {
            panic!("a missing path and a duplicate resolved");
        };
        assert!(err.contains("Unknown scanner: nope"), "{err}");
        assert!(
            err.contains("Scanner 'reg' specified more than once"),
            "{err}"
        );
    }
}
