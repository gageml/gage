//! The Pi corpus: the session files under a sessions directory, as
//! the shared derived data layer reads them.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use gage_corpus::{Corpus, DerivedSession, Result, SessionInfo, project_slug};

use crate::derive::{SUMMARY_VERSION, derive_session};
use crate::driver::{CONTENT_FORMAT, NAME, SESSION_TYPE};
use crate::home::Location;
use crate::session_file::{Header, read_header, session_files};

/// The sessions of one Pi location.
#[derive(Debug)]
pub(crate) struct PiCorpus {
    location: Location,
}

impl PiCorpus {
    pub(crate) fn new(location: Location) -> Self {
        Self { location }
    }
}

/// The project name of a session: the slug of the working directory
/// its header names. The directory a session file sits in is named
/// for the working directory too, but two working directories can
/// share that name, so it is not read. A header that names no working
/// directory has no project.
pub(crate) fn project_of(header: &Header) -> String {
    header.cwd.as_deref().map(project_slug).unwrap_or_default()
}

impl Corpus for PiCorpus {
    fn driver(&self) -> &'static str {
        NAME
    }

    fn session_type(&self) -> &'static str {
        SESSION_TYPE
    }

    fn content_format(&self) -> &'static str {
        CONTENT_FORMAT
    }

    fn summary_version(&self) -> u32 {
        SUMMARY_VERSION
    }

    fn root(&self) -> &Path {
        &self.location.root
    }

    fn sessions_dir(&self) -> &Path {
        &self.location.sessions_dir
    }

    /// The session files with a header, newest first. The first line
    /// of each file is read for the header; nothing else of a file is.
    /// A file named as a session with no header is not a Pi session
    /// and is not listed.
    ///
    /// The id is the one the file name carries. Pi keeps an id unique
    /// within one working directory only, so a user who names sessions
    /// can give two the same id. The newest file has the id and the
    /// others are not listed.
    fn sessions(&self, _show_agents: bool) -> io::Result<Vec<SessionInfo>> {
        let mut by_id: HashMap<String, SessionInfo> = HashMap::new();
        for file in session_files(&self.location.sessions_dir)? {
            let header = match read_header(&file.path) {
                Ok(Some(header)) => header,
                Ok(None) => continue,
                // Removed since the walk listed it
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e),
            };
            let session = SessionInfo {
                id: file.id,
                src: file.path,
                mtime: file.mtime,
                size: file.size,
                project: project_of(&header),
            };
            match by_id.get(&session.id) {
                Some(listed) => {
                    let (kept, left_out) = match listed.mtime >= session.mtime {
                        true => (listed.src.clone(), session.src.clone()),
                        false => (session.src.clone(), listed.src.clone()),
                    };
                    tracing::warn!(
                        session_id = %session.id,
                        listed = %kept.display(),
                        not_listed = %left_out.display(),
                        "two session files carry one id"
                    );
                    if kept == session.src {
                        by_id.insert(session.id.clone(), session);
                    }
                }
                None => {
                    by_id.insert(session.id.clone(), session);
                }
            }
        }
        let mut sessions: Vec<SessionInfo> = by_id.into_values().collect();
        sessions.sort_by(|a, b| b.mtime.cmp(&a.mtime).then_with(|| a.id.cmp(&b.id)));
        Ok(sessions)
    }

    fn derive(&self, id: &str, path: &Path) -> Result<DerivedSession> {
        let project = read_header(path)?
            .map(|header| project_of(&header))
            .unwrap_or_default();
        derive_session(id, path, project)
    }
}

/// The working directory a session file's header names.
pub(crate) fn working_dir(path: &Path) -> io::Result<Option<PathBuf>> {
    Ok(read_header(path)?.and_then(|header| header.cwd))
}
