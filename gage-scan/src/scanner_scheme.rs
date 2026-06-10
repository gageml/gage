use std::path::PathBuf;

use gage_core::text_resolve::{TextResolveError, TextResolverScheme};

use crate::scanner::{ScannerRegistry, scanner_home_paths};

/// Resolver scheme for `scanner:` URIs.
///
/// - `scanner:rel/path` reads from the directory passed to `new`. If
///   no directory is set (e.g. `absolute_only`), relative URIs error.
/// - `scanner:/abs/path` searches the configured scanner home paths
///   in order.
pub struct ScannerScheme {
    scanner_dir: Option<PathBuf>,
}

impl ScannerScheme {
    pub fn new(scanner_dir: PathBuf) -> Self {
        Self {
            scanner_dir: Some(scanner_dir),
        }
    }

    pub fn absolute_only() -> Self {
        Self { scanner_dir: None }
    }

    /// Build a scheme rooted at the directory of the scanner named
    /// `scanner_name`. Used to resolve `scanner:` URIs for a note whose
    /// `author` is `scanner:{name}`.
    pub fn for_scanner_name(
        registry: &ScannerRegistry,
        scanner_name: &str,
    ) -> Result<Self, TextResolveError> {
        if let Some(def) = registry.get_def(scanner_name) {
            return Ok(Self::new(def.module_dir()));
        }
        // Composite names from `-f` file scanners (`{name}[{path-spec}]`)
        // are not in the registry at display time. Derive the search
        // root from the composite name.
        if let Some(dir) = dir_from_composite_name(scanner_name) {
            return Ok(Self::new(dir));
        }
        Err(TextResolveError::SchemeResolve(format!(
            "unknown scanner '{scanner_name}'"
        )))
    }
}

/// Scheme that always returns the same error on resolve. Useful as a
/// placeholder when scheme construction failed, so the deferred error
/// surfaces at the actual call site rather than at setup time.
pub struct ErrorScheme {
    message: String,
}

impl ErrorScheme {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl TextResolverScheme for ErrorScheme {
    fn resolve(&self, _input: &str) -> Result<String, TextResolveError> {
        Err(TextResolveError::SchemeResolve(self.message.clone()))
    }
}

impl TextResolverScheme for ScannerScheme {
    fn resolve(&self, input: &str) -> Result<String, TextResolveError> {
        let rest = input.strip_prefix("scanner:").ok_or_else(|| {
            TextResolveError::SchemeResolve(format!(
                "scanner scheme received non-scanner input '{input}'"
            ))
        })?;
        if let Some(abs) = rest.strip_prefix('/') {
            deref_absolute(input, abs)
        } else {
            let dir = self.scanner_dir.as_ref().ok_or_else(|| {
                TextResolveError::SchemeResolve(format!(
                    "{input}: relative scanner URI has no anchoring scanner directory"
                ))
            })?;
            let path = dir.join(rest);
            std::fs::read_to_string(&path).map_err(|e| {
                TextResolveError::SchemeResolve(format!("{input}: read {}: {e}", path.display()))
            })
        }
    }
}

/// If `name` matches `{bare}[{path-spec}]`, recover the directory
/// containing the scanner file by inverting the display-path elision
/// (see "Scanner file refs" in `.local.design/run-scanner-file.md`):
/// a path spec ending in `.rn` is the literal file, so the root is its
/// parent; otherwise the file is `{path-spec}/{bare}/scanner.rn`, so
/// the root is `{path-spec}/{bare}`.
fn dir_from_composite_name(name: &str) -> Option<PathBuf> {
    let open = name.find('[')?;
    if !name.ends_with(']') {
        return None;
    }
    let bare = &name[..open];
    let path_spec = &name[open + 1..name.len() - 1];
    if bare.is_empty() || path_spec.is_empty() {
        return None;
    }

    let expanded = if let Some(rest) = path_spec.strip_prefix("~/") {
        let home = std::env::var_os("HOME")?;
        PathBuf::from(home).join(rest)
    } else if path_spec == "~" {
        PathBuf::from(std::env::var_os("HOME")?)
    } else {
        PathBuf::from(path_spec)
    };

    if path_spec.ends_with(".rn") {
        Some(
            expanded
                .parent()
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/")),
        )
    } else {
        Some(expanded.join(bare))
    }
}

fn deref_absolute(uri: &str, abs_path: &str) -> Result<String, TextResolveError> {
    let mut searched = Vec::new();
    for home in scanner_home_paths() {
        let candidate = home.join(abs_path);
        if candidate.exists() {
            return std::fs::read_to_string(&candidate).map_err(|e| {
                TextResolveError::SchemeResolve(format!("{uri}: read {}: {e}", candidate.display()))
            });
        }
        searched.push(candidate);
    }
    let dirs: Vec<String> = searched.iter().map(|p| p.display().to_string()).collect();
    Err(TextResolveError::SchemeResolve(format!(
        "{uri}: not found in {}",
        dirs.join(", ")
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gage_core::text_resolve::TextResolver;

    #[test]
    fn relative_uri_requires_scanner_dir() {
        let r = TextResolver::new().with_scheme("scanner", ScannerScheme::absolute_only());
        let err = r.resolve("scanner:fix.md".into()).unwrap_err();
        assert!(
            matches!(err, TextResolveError::SchemeResolve(s) if s.contains("relative scanner URI"))
        );
    }

    #[test]
    fn relative_uri_read_failure_reports_path() {
        let r = TextResolver::new()
            .with_scheme("scanner", ScannerScheme::new(PathBuf::from("/nonexistent")));
        let err = r.resolve("scanner:fix.md".into()).unwrap_err();
        assert!(matches!(err, TextResolveError::SchemeResolve(s) if s.contains("scanner:fix.md")));
    }

    #[test]
    fn composite_name_with_directory_path_spec() {
        assert_eq!(
            dir_from_composite_name("foo[/scanners]"),
            Some(PathBuf::from("/scanners/foo"))
        );
    }

    #[test]
    fn composite_name_with_literal_file_path_spec() {
        assert_eq!(
            dir_from_composite_name("foo[/scanners/foo.rn]"),
            Some(PathBuf::from("/scanners"))
        );
        assert_eq!(
            dir_from_composite_name("foo[/scanners/bar/scanner.rn]"),
            Some(PathBuf::from("/scanners/bar"))
        );
    }

    #[test]
    fn non_composite_names_are_rejected() {
        assert_eq!(dir_from_composite_name("foo"), None);
        assert_eq!(dir_from_composite_name("foo[]"), None);
        assert_eq!(dir_from_composite_name("[/scanners]"), None);
        assert_eq!(dir_from_composite_name("foo[/scanners"), None);
    }
}
