//! The source files of a scanner.
//!
//! The runtime is closed: a scanner cannot read a file except through
//! a facility this runtime provides, and every such facility takes a
//! string literal, so the complete set of files a scanner is built
//! from is computable from its token stream. [`source_files`] is that
//! computation, and a scan stores exactly the files it returns as the
//! scanner's source. A facility added to this runtime that reads a
//! file at compile time extends [`source_files`] in the same change;
//! the guard is `enumerated_files_are_all_the_compiler_reads`, which
//! compiles every builtin scanner in a directory holding only the
//! enumerated files.
//!
//! Today the facilities are `include_str!` and `include_json!`, from
//! `gage-runtime`. Each takes one string literal and reads it relative
//! to the directory of the file containing the call, and neither
//! result can contain a further include, so the set is the scanner
//! file plus its include arguments. The scan is a token scan rather
//! than a parse: the sequence `include_str ! ( "…" )` is matched on
//! the lexer's tokens, which excludes comments and string contents.
//!
//! Each file is stored under one flat name: the path as written in
//! the scanner, percent-encoded so that `/` and every byte a
//! filesystem or a Git tree entry cannot carry are escaped. A path
//! that reaches a parent or peer directory keeps its `..` components
//! inside the name (`..%2Fshared%2Fdoc.md`), so the stored name says
//! where the file came from and decodes back to the path exactly.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use percent_encoding::{AsciiSet, CONTROLS, percent_decode, percent_encode};
use rune::SourceId;
use rune::ast::{Delimiter, Kind, StrSource, Token};
use rune::parse::Parser;

/// Bytes escaped in a stored name: the controls, `/` and `\`, `%`
/// itself, and the characters a Windows filesystem reserves. Every
/// non-ASCII byte is escaped as well.
const ESCAPED: &AsciiSet = &CONTROLS
    .add(b'/')
    .add(b'\\')
    .add(b'%')
    .add(b':')
    .add(b'*')
    .add(b'?')
    .add(b'"')
    .add(b'<')
    .add(b'>')
    .add(b'|');

/// The longest name the staging directory can hold: `NAME_MAX` on the
/// filesystems Gage home lives on.
const MAX_NAME_BYTES: usize = 255;

/// The macros that read a file at compile time
const INCLUDE_MACROS: [&str; 2] = ["include_str", "include_json"];

/// One file a scanner is built from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceFile {
    /// The path as the scanner names it: the scanner file's own file
    /// name, or an include argument as written
    pub literal: String,
    /// The stored name, [`encode_name`] of `literal`
    pub name: String,
    /// Where the file is on disk
    pub path: PathBuf,
}

#[derive(Debug)]
pub enum SourceError {
    /// The scanner path has no file name
    NoFileName(PathBuf),
    /// The scanner file could not be read
    Read(PathBuf, io::Error),
    /// The scanner could not be tokenized
    Lex(String),
    /// An include macro's argument is not a string literal, at the
    /// given 1-based line
    NonLiteralInclude { macro_name: String, line: usize },
    /// The literal cannot be stored under any name: empty, `.`, `..`,
    /// or longer than a filesystem allows once encoded
    BadName { literal: String, reason: String },
}

impl fmt::Display for SourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SourceError::NoFileName(path) => {
                write!(f, "scanner path has no file name: {}", path.display())
            }
            SourceError::Read(path, e) => write!(f, "{}: {e}", path.display()),
            SourceError::Lex(msg) => write!(f, "tokenizing scanner: {msg}"),
            SourceError::NonLiteralInclude { macro_name, line } => write!(
                f,
                "{macro_name}! at line {line} takes a string literal path"
            ),
            SourceError::BadName { literal, reason } => {
                write!(f, "source path {literal:?} cannot be stored: {reason}")
            }
        }
    }
}

impl std::error::Error for SourceError {}

/// The files `scanner` is built from: the scanner file first, then
/// each distinct include argument in order of first appearance.
pub fn source_files(scanner: &Path) -> Result<Vec<SourceFile>, SourceError> {
    let file_name = scanner
        .file_name()
        .ok_or_else(|| SourceError::NoFileName(scanner.to_path_buf()))?
        .to_string_lossy()
        .into_owned();
    let source =
        fs::read_to_string(scanner).map_err(|e| SourceError::Read(scanner.to_path_buf(), e))?;
    let dir = scanner.parent().unwrap_or_else(|| Path::new(""));
    let mut files = vec![source_file(&file_name, scanner.to_path_buf())?];
    for literal in include_literals(&source)? {
        if files.iter().any(|f| f.literal == literal) {
            continue;
        }
        files.push(source_file(&literal, dir.join(&literal))?);
    }
    Ok(files)
}

/// The string literal argument of every `include_*!` call in
/// `source`, in order. Matched on tokens: `include_str ! ( "…" )`.
fn include_literals(source: &str) -> Result<Vec<String>, SourceError> {
    let mut parser = Parser::new(source, SourceId::empty(), false);
    let lex = |e: rune::compile::Error| SourceError::Lex(e.to_string());
    let text = |token: &Token| &source[token.span.range()];
    let mut literals = Vec::new();
    let mut window: Vec<Token> = Vec::new();
    while !parser.is_eof().map_err(lex)? {
        let token = parser.parse::<Token>().map_err(lex)?;
        window.push(token);
        if window.len() > 3 {
            window.remove(0);
        }
        let [name, bang, open] = window.as_slice() else {
            continue;
        };
        let is_include = matches!(name.kind, Kind::Ident(_))
            && INCLUDE_MACROS.contains(&text(name))
            && bang.kind == Kind::Bang
            && open.kind == Kind::Open(Delimiter::Parenthesis);
        if !is_include {
            continue;
        }
        let macro_name = text(name).to_string();
        let line = source[..name.span.start.into_usize()]
            .lines()
            .count()
            .max(1);
        if parser.is_eof().map_err(lex)? {
            return Err(SourceError::NonLiteralInclude { macro_name, line });
        }
        let arg = parser.parse::<Token>().map_err(lex)?;
        let Kind::Str(StrSource::Text(str_text)) = arg.kind else {
            return Err(SourceError::NonLiteralInclude { macro_name, line });
        };
        let raw = text(&arg);
        let inner = if str_text.wrapped {
            &raw[1..raw.len() - 1]
        } else {
            raw
        };
        literals.push(if str_text.escaped {
            unescape(inner)
        } else {
            inner.to_string()
        });
        window.clear();
    }
    Ok(literals)
}

/// Rune string escapes as they occur in a path: `\\`, `\"`, `\'`,
/// `\n`, `\r`, `\t`, `\0`. Any other escape is kept as written.
fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('\\') => out.push('\\'),
            Some('"') => out.push('"'),
            Some('\'') => out.push('\''),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('0') => out.push('\0'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

fn source_file(literal: &str, path: PathBuf) -> Result<SourceFile, SourceError> {
    let name = encode_name(literal.as_bytes());
    let reason = if literal.is_empty() {
        Some("empty path")
    } else if literal == "." || literal == ".." {
        Some("path is `.` or `..`")
    } else if name.len() > MAX_NAME_BYTES {
        Some("encoded name exceeds 255 bytes")
    } else {
        None
    };
    if let Some(reason) = reason {
        return Err(SourceError::BadName {
            literal: literal.to_string(),
            reason: reason.to_string(),
        });
    }
    Ok(SourceFile {
        literal: literal.to_string(),
        name,
        path,
    })
}

/// The stored name of a source path.
pub fn encode_name(path: &[u8]) -> String {
    percent_encode(path, ESCAPED).to_string()
}

/// The source path a stored name was made from.
pub fn decode_name(name: &str) -> Vec<u8> {
    percent_decode(name.as_bytes()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_scanner(dir: &Path, name: &str, source: &str) -> PathBuf {
        let path = dir.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, source).unwrap();
        path
    }

    fn literals(files: &[SourceFile]) -> Vec<&str> {
        files.iter().map(|f| f.literal.as_str()).collect()
    }

    #[test]
    fn encoding_round_trips_and_is_readable() {
        for (literal, expected) in [
            ("findings-prompt.md.j2", "findings-prompt.md.j2"),
            ("../shared/doc.md", "..%2Fshared%2Fdoc.md"),
            ("../../lib/x.md", "..%2F..%2Flib%2Fx.md"),
            ("/abs/path/x.md", "%2Fabs%2Fpath%2Fx.md"),
            ("a 100% file.md", "a 100%25 file.md"),
            ("win\\style:name?.md", "win%5Cstyle%3Aname%3F.md"),
            ("tab\tnul\0.md", "tab%09nul%00.md"),
        ] {
            let name = encode_name(literal.as_bytes());
            assert_eq!(name, expected, "{literal}");
            assert_eq!(decode_name(&name), literal.as_bytes(), "{literal}");
            assert!(!name.contains('/'));
        }
    }

    #[test]
    fn non_utf8_bytes_survive_the_round_trip() {
        let raw = b"caf\xe9/\xff.md";
        let name = encode_name(raw);
        assert_eq!(name, "caf%E9%2F%FF.md");
        assert_eq!(decode_name(&name), raw);
    }

    #[test]
    fn a_scanner_without_includes_is_its_one_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_scanner(tmp.path(), "general/scanner.rn", "pub fn main() {}\n");
        let files = source_files(&path).unwrap();
        assert_eq!(
            files,
            [SourceFile {
                literal: "scanner.rn".into(),
                name: "scanner.rn".into(),
                path: path.clone(),
            }]
        );
    }

    /// Includes are found across line breaks and duplicates, and are
    /// not found in comments or inside string literals.
    #[test]
    fn includes_are_enumerated_from_tokens() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_scanner(
            tmp.path(),
            "s/scanner.rn",
            r#"
            // include_str!("commented.md")
            const A = include_str!("a.md");
            const B = include_json!(
                "sub/b.jsonc"
            );
            const C = include_str!("../shared/c.md");
            const D = include_str!("a.md");
            const E = "include_str!(\"in-string.md\")";
            const F = include_str!("esc\"aped\\name.md");
            "#,
        );
        let files = source_files(&path).unwrap();
        assert_eq!(
            literals(&files),
            [
                "scanner.rn",
                "a.md",
                "sub/b.jsonc",
                "../shared/c.md",
                "esc\"aped\\name.md",
            ]
        );
        assert_eq!(files[3].name, "..%2Fshared%2Fc.md");
        assert_eq!(files[3].path, tmp.path().join("s/../shared/c.md"));
        assert_eq!(files[4].name, "esc%22aped%5Cname.md");
    }

    #[test]
    fn a_non_literal_include_argument_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_scanner(
            tmp.path(),
            "s/scanner.rn",
            "const P = \"x.md\";\n\nconst A = include_str!(P);\n",
        );
        let err = source_files(&path).unwrap_err();
        assert!(
            matches!(&err, SourceError::NonLiteralInclude { macro_name, line: 3 } if macro_name == "include_str"),
            "{err}"
        );
    }

    #[test]
    fn unstorable_names_are_rejected() {
        let long = "x".repeat(300);
        for literal in ["", ".", "..", long.as_str()] {
            let err = source_file(literal, PathBuf::from("p")).unwrap_err();
            assert!(
                matches!(err, SourceError::BadName { .. }),
                "{literal:?}: {err}"
            );
        }
        assert!(matches!(
            source_files(Path::new("/")).unwrap_err(),
            SourceError::NoFileName(_)
        ));
    }

    /// The guard on the closed-runtime contract: every builtin scanner
    /// compiles in a directory holding only its enumerated files, with
    /// the full first-generation context. A file the compiler reads
    /// that the enumerator missed fails this compile.
    #[test]
    fn enumerated_files_are_all_the_compiler_reads() {
        let scanners_dir: PathBuf = [env!("CARGO_MANIFEST_DIR"), "..", "scanners"]
            .iter()
            .collect();
        let mut checked = 0;
        for entry in fs::read_dir(&scanners_dir).unwrap() {
            let scanner = entry.unwrap().path().join("scanner.rn");
            if !scanner.is_file() {
                continue;
            }
            let files = source_files(&scanner).unwrap();
            // A scanner with no includes has nothing to guard
            if files.len() == 1 {
                continue;
            }

            let tmp = tempfile::tempdir().unwrap();
            let sandbox = tmp.path().join("sandbox");
            for file in &files {
                let dest = sandbox.join(&file.literal);
                fs::create_dir_all(dest.parent().unwrap()).unwrap();
                fs::copy(&file.path, &dest).unwrap();
            }
            let copied = sandbox.join(&files[0].literal);
            let source = fs::read_to_string(&copied).unwrap();

            let context = gage_runtime::lsp_context().unwrap();
            let mut sources = rune::Sources::new();
            sources
                .insert(rune::Source::with_path("scanner", source, &copied).unwrap())
                .unwrap();
            let mut diagnostics = rune::Diagnostics::new();
            let result = rune::prepare(&mut sources)
                .with_context(&context)
                .with_diagnostics(&mut diagnostics)
                .build();
            let mut buf = rune::termcolor::Buffer::no_color();
            diagnostics.emit(&mut buf, &sources).unwrap();
            assert!(
                result.is_ok(),
                "{} in a sandbox of its enumerated files:\n{}",
                scanner.display(),
                String::from_utf8(buf.into_inner()).unwrap()
            );
            checked += 1;
        }
        assert!(checked >= 5, "checked {checked} builtin scanners");
    }
}
