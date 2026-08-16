use serde::{Deserialize, Serialize};

/// What a note is attached to. Always exactly one variant when set;
/// a note may also have no target (`Note.target = None`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NoteTarget {
    Session(SessionTarget),
    Scan(ScanTarget),
    Project(ProjectTarget),
}

/// A note attached to a session, optionally narrowed to a line.
/// A note attached to a session, optionally narrowed to a line or an
/// inclusive line range (`line..=line_end`). `line_end` is only set
/// when `line` is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionTarget {
    pub session_id: String,
    pub line: Option<u32>,
    pub line_end: Option<u32>,
}

impl SessionTarget {
    pub fn new(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            line: None,
            line_end: None,
        }
    }

    pub fn with_line(mut self, line: u32) -> Self {
        self.line = Some(line);
        self
    }

    pub fn with_line_range(mut self, line: u32, line_end: u32) -> Self {
        self.line = Some(line);
        self.line_end = Some(line_end);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanTarget {
    pub scan_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectTarget {
    pub project_path: String,
}

impl ProjectTarget {
    /// The project path with a leading `$HOME` replaced by `~`, for
    /// display. Falls back to the full path when `HOME` is unset or
    /// is not a prefix.
    pub fn to_shortened_path(&self) -> String {
        match std::env::var_os("HOME") {
            Some(home) => shorten_home(&self.project_path, &home.to_string_lossy()),
            None => self.project_path.clone(),
        }
    }
}

fn shorten_home(path: &str, home: &str) -> String {
    if path == home {
        "~".to_string()
    } else if let Some(rest) = path.strip_prefix(home).and_then(|r| r.strip_prefix('/')) {
        format!("~/{rest}")
    } else {
        path.to_string()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub input: String,
    pub message: String,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid target '{}': {}", self.input, self.message)
    }
}

impl std::error::Error for ParseError {}

impl NoteTarget {
    pub fn kind(&self) -> &'static str {
        match self {
            NoteTarget::Session(_) => "session",
            NoteTarget::Scan(_) => "scan",
            NoteTarget::Project(_) => "project",
        }
    }

    pub fn to_uri(&self) -> String {
        match self {
            NoteTarget::Session(t) => format!("session:{}", t.to_uri()),
            NoteTarget::Scan(t) => format!("scan:{}", t.scan_id),
            NoteTarget::Project(t) => format!("project:{}", t.project_path),
        }
    }

    /// Parse a target URI produced by [`NoteTarget::to_uri`]. The scheme
    /// (up to the first `:`) selects the variant; the remainder is the
    /// variant body. Inverse of [`NoteTarget::to_uri`].
    pub fn from_uri(input: &str) -> Result<Self, ParseError> {
        let err = |msg: &str| ParseError {
            input: input.to_string(),
            message: msg.to_string(),
        };
        let Some((scheme, body)) = input.split_once(':') else {
            return Err(err("missing scheme"));
        };
        if body.is_empty() {
            return Err(err("empty target body"));
        }
        match scheme {
            "session" => Ok(NoteTarget::Session(SessionTarget::parse(body)?)),
            "scan" => Ok(NoteTarget::Scan(ScanTarget {
                scan_id: body.to_string(),
            })),
            "project" => Ok(NoteTarget::Project(ProjectTarget {
                project_path: body.to_string(),
            })),
            other => Err(err(&format!("unknown target scheme '{other}'"))),
        }
    }
}

impl SessionTarget {
    /// Parse `session-id`, `session-id:N`, or `session-id:N-M`.
    pub fn parse(input: &str) -> Result<Self, ParseError> {
        let err = |msg: &str| ParseError {
            input: input.to_string(),
            message: msg.to_string(),
        };
        if input.is_empty() {
            return Err(err("empty input"));
        }
        let Some((session_id, rest)) = input.split_once(':') else {
            return Ok(SessionTarget::new(input));
        };
        if session_id.is_empty() {
            return Err(err("empty session_id"));
        }
        if rest.is_empty() {
            return Err(err("missing line number after ':'"));
        }
        let parse_line = |s: &str| -> Result<u32, ParseError> {
            let n: u32 = s
                .parse()
                .map_err(|e| err(&format!("line must be a positive integer: {e}")))?;
            if n == 0 {
                return Err(err("line must be >= 1"));
            }
            Ok(n)
        };
        let (line, line_end) = match rest.split_once('-') {
            Some((start, end)) => {
                let start = parse_line(start)?;
                let end = parse_line(end)?;
                if end < start {
                    return Err(err("line range end must be >= start"));
                }
                (start, Some(end))
            }
            None => (parse_line(rest)?, None),
        };
        Ok(SessionTarget {
            session_id: session_id.to_string(),
            line: Some(line),
            line_end,
        })
    }

    /// Encode as the path portion of a target URI (no `session:`
    /// scheme prefix). Round-trips with [`SessionTarget::parse`].
    pub fn to_uri(&self) -> String {
        let mut out = self.session_id.clone();
        if let Some(l) = self.line {
            out.push_str(&format!(":{l}"));
            if let Some(e) = self.line_end {
                out.push_str(&format!("-{e}"));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SESSION: &str = "550e8400-e29b-41d4-a716-446655440000";

    #[test]
    fn parse_session_only() {
        let t = SessionTarget::parse(SESSION).unwrap();
        assert_eq!(t.session_id, SESSION);
        assert_eq!(t.line, None);
    }

    #[test]
    fn parse_line() {
        let t = SessionTarget::parse(&format!("{SESSION}:42")).unwrap();
        assert_eq!(t.session_id, SESSION);
        assert_eq!(t.line, Some(42));
        assert_eq!(t.line_end, None);
    }

    #[test]
    fn parse_line_range() {
        let t = SessionTarget::parse(&format!("{SESSION}:42-50")).unwrap();
        assert_eq!(t.line, Some(42));
        assert_eq!(t.line_end, Some(50));
    }

    #[test]
    fn parse_inverted_range_errors() {
        assert!(SessionTarget::parse(&format!("{SESSION}:50-42")).is_err());
    }

    #[test]
    fn roundtrip_simple() {
        for input in [
            SESSION.to_string(),
            format!("{SESSION}:42"),
            format!("{SESSION}:42-50"),
        ] {
            let parsed = SessionTarget::parse(&input).unwrap();
            assert_eq!(parsed.to_uri(), input);
        }
    }

    #[test]
    fn empty_input_errors() {
        assert!(SessionTarget::parse("").is_err());
    }

    #[test]
    fn zero_line_errors() {
        assert!(SessionTarget::parse(&format!("{SESSION}:0")).is_err());
    }

    #[test]
    fn uri_roundtrips_all_variants() {
        let cases = [
            NoteTarget::Session(SessionTarget::new(SESSION)),
            NoteTarget::Session(SessionTarget::new(SESSION).with_line(42)),
            NoteTarget::Session(SessionTarget::new(SESSION).with_line_range(42, 50)),
            NoteTarget::Scan(ScanTarget {
                scan_id: "scan-1".to_string(),
            }),
            NoteTarget::Project(ProjectTarget {
                project_path: "/home/me/proj".to_string(),
            }),
        ];
        for t in cases {
            assert_eq!(NoteTarget::from_uri(&t.to_uri()).unwrap(), t);
        }
    }

    #[test]
    fn shorten_home_substitutes_tilde() {
        assert_eq!(shorten_home("/home/me/proj", "/home/me"), "~/proj");
        assert_eq!(shorten_home("/home/me", "/home/me"), "~");
        assert_eq!(shorten_home("/opt/proj", "/home/me"), "/opt/proj");
        assert_eq!(
            shorten_home("/home/melon/proj", "/home/me"),
            "/home/melon/proj"
        );
    }

    #[test]
    fn from_uri_keeps_colons_in_project_path() {
        let t = NoteTarget::from_uri("project:/a:b/c").unwrap();
        match t {
            NoteTarget::Project(p) => assert_eq!(p.project_path, "/a:b/c"),
            other => panic!("expected project target, got {other:?}"),
        }
    }

    #[test]
    fn from_uri_rejects_unknown_scheme() {
        assert!(NoteTarget::from_uri("bogus:x").is_err());
        assert!(NoteTarget::from_uri("noscheme").is_err());
        assert!(NoteTarget::from_uri("session:").is_err());
    }
}
