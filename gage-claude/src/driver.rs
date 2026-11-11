//! `ClaudeDriver` implements `gage_session::Driver` for Claude Code
//! sessions stored under a projects directory as `<slug>/<uuid>.jsonl`.
//!
//! Source grammar, with or without the `claude:` scheme:
//! - `` (empty) -- default location: `$CLAUDE_PROJECTS_DIR`, else
//!   `projects/` under `$CLAUDE_CONFIG_DIR` or `$HOME/.claude`
//! - `<path>` -- explicit filesystem root; sessions live under
//!   `<path>/projects/**/*.jsonl`
//!
//! A session's own URL is `claude:<absolute root>/<uuid>`, always with
//! the resolved root, so the same file read through any spelling of
//! its source records the same `native_source`.

use std::cell::OnceCell;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime};

use gage_core::config::{default_cache_root, plugin_marketplace_dir};
use gage_session::progress;
use gage_session::{
    AgentSession, AgentSpec, AuthInfo, ContentSchema, ContentSink, ContentSource, DecodedEntry,
    Derivation, Driver, DriverError, DriverStatus, DriverTables, Entry, EntryBody, EntryCodec,
    EntryValue, Field, HarnessInfo, HarnessSession, InstallInfo, InstallReport, InstallUi,
    MaintenanceOutcome, MaintenanceStatus, MessageTextFilter, MessageTextHit, NativeLookupError,
    NativeSession, SessionAttrs, SessionLiveError, SkillSession, Source, SourceOptions,
    StoredSession, SummaryAttrs, ToolUse, WrittenContent, split_scheme,
};
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::body;
use crate::codec::codec;
use crate::content_schema;
use crate::home::ClaudeHome;
use crate::index::{
    ClaudeSessionAttrs, Fingerprint, IndexStore, LockMode, SUMMARY_VERSION, ToolUseCollector,
    derive_entry, derive_session, derive_session_from, project_of,
};
use crate::model::resolve_model;
use crate::plugin;
use crate::preflight;
use crate::proc::find_claude;
use crate::session::{
    SESSION_RE, SessionInfo, delete_session, encode_project_dir, is_agent_tmp_slug, move_session,
    projects_dir,
};
use crate::session_reader::SessionReader;
use crate::skill::skill_command;
use crate::tables::{EntryTable, MessageTable, SessionTable, ToolUseFileTable, ToolUseTable};

/// MCP server name under which Gage registers in `.mcp.json`.
pub(crate) const MCP_SERVER_NAME: &str = "gage";
/// The plugin's name, the first half of [`PLUGIN_ID`]. Claude Code
/// names a plugin's MCP tools `mcp__plugin_<plugin>_<server>__<tool>`.
pub(crate) const PLUGIN_NAME: &str = "gage";
/// Plugin identifier Claude Code uses: `<plugin>@<marketplace>`.
const PLUGIN_ID: &str = "gage@gage";

const NAME: &str = "claude";
/// The harness's display name
const HARNESS_NAME: &str = "Claude Code";
const SCHEMES: &[&str] = &["claude"];
const VERSION: &str = env!("CARGO_PKG_VERSION");
pub(crate) const SESSION_TYPE: &str = "claude";
/// `content_format` value returned by `write_native` and expected by
/// `read_stored`. The name identifies the byte layout under
/// `files.d/`; the trailing digit is its version.
pub(crate) const CONTENT_FORMAT: &str = "claude-jsonl 1";

pub struct ClaudeDriver;

impl ClaudeDriver {
    pub fn new() -> Self {
        Self
    }
}

impl Default for ClaudeDriver {
    fn default() -> Self {
        Self::new()
    }
}

impl Driver for ClaudeDriver {
    fn name(&self) -> &'static str {
        NAME
    }

    fn version(&self) -> &'static str {
        VERSION
    }

    fn schemes(&self) -> &'static [&'static str] {
        SCHEMES
    }

    fn open_source(&self, options: &SourceOptions) -> Result<Box<dyn Source>, DriverError> {
        Ok(Box::new(ClaudeSource::open(options)?))
    }

    /// A session URL is `claude:<root>/<id>`; its source is
    /// `claude:<root>`. A bare id is a session in the default
    /// location, `claude:`.
    fn split_session_url(&self, session_url: &str) -> Result<(String, String), DriverError> {
        let body = match split_scheme(session_url) {
            Some((scheme, _)) if scheme != NAME => {
                return Err(DriverError::Other(format!(
                    "{session_url}: not a {NAME} session URL"
                )));
            }
            Some((_, body)) => body,
            None => session_url,
        };
        let (root, id) = body.rsplit_once('/').unwrap_or(("", body));
        if id.is_empty() {
            return Err(DriverError::Other(format!(
                "{session_url}: session URL has no session id"
            )));
        }
        Ok((format!("{NAME}:{root}"), id.to_string()))
    }

    /// Drops the `claude-` vendor prefix every Claude model name carries
    fn content_schemas(&self) -> Vec<ContentSchema> {
        content_schema::content_schemas()
    }

    fn title_entry(&self, content_format: &str, title: &str) -> Option<EntryValue> {
        (content_format == CONTENT_FORMAT).then(|| {
            let text = format!(
                r#"{{"type":"custom-title","customTitle":{}}}"#,
                serde_json::Value::from(title)
            );
            codec()
                .decode(text.as_bytes())
                .expect("a title entry is one JSON object")
        })
    }

    fn codec(&self, content_format: &str) -> Result<Arc<dyn EntryCodec>, DriverError> {
        if content_format != CONTENT_FORMAT {
            return Err(DriverError::Other(format!(
                "content format {content_format:?} has no codec in this build; \
                 this build reads {CONTENT_FORMAT:?} (upgrade gage)"
            )));
        }
        Ok(codec())
    }

    fn entry_body(&self, content_format: &str, entry: &EntryValue) -> EntryBody {
        if content_format != CONTENT_FORMAT {
            return EntryBody::default();
        }
        body::entry_body(entry)
    }

    fn format_model(&self, model: &str) -> String {
        model.strip_prefix("claude-").unwrap_or(model).to_string()
    }

    fn format_project(&self, name: &str, max_chars: usize) -> String {
        shorten_project_name(strip_home_prefix(name), max_chars)
    }

    fn write_native(
        &self,
        session: &mut dyn NativeSession,
        sink: &mut dyn ContentSink,
    ) -> Result<WrittenContent, DriverError> {
        use std::io::Write;
        let claude = session
            .as_any()
            .downcast_ref::<ClaudeNativeSession>()
            .ok_or_else(|| {
                DriverError::Other("write_native: session is not a ClaudeNativeSession".into())
            })?;
        // Only the session transcript is stored. Subagent and sidecar
        // content is intentionally left out until a consumer exists for
        // it; a session is re-added over time, so that content can join
        // later at no duplication cost (git shares identical blobs).
        //
        // The session added is the transcript as this read finds it.
        // A change to the file after the read is not included. One
        // read serves the write and the derive, so the derived attrs
        // and the size describe the bytes stored.
        //
        // The mtime is the one read when the session was opened. A
        // transcript that grows between the open and this read is
        // stored with its new content and that earlier mtime. The
        // interval is one store lookup for a session opened by id, and
        // the cost is one early timestamp, so the race is accepted.
        let path = claude.session_path().to_path_buf();
        let bytes = fs::read(&path).map_err(DriverError::Io)?;
        sink.create("session.jsonl")
            .and_then(|mut w| w.write_all(&bytes))
            .map_err(DriverError::Io)?;
        let fingerprint = Fingerprint {
            mtime_ms: millis(claude.mtime),
            size: bytes.len() as u64,
        };
        let native_id = claude.native_id.clone();
        let project = claude.project.clone();
        let store = claude.store.clone();
        Ok(WrittenContent::with_derive(
            CONTENT_FORMAT,
            SUMMARY_VERSION,
            move || {
                let derived = derive_session_from(
                    &native_id,
                    SessionReader::new(bytes.as_slice()),
                    fingerprint,
                    project,
                )
                .map_err(|e| DriverError::Other(format!("{}: {e}", path.display())))?;
                cache_attrs(&store, &native_id, &derived.attrs);
                Ok(Box::new(derived.attrs) as Box<dyn SessionAttrs>)
            },
        ))
    }

    /// A transcript grows by lines: more lines is a later state,
    /// fewer is an older copy, and the same count with other bytes is
    /// a modification in place. A stored session without a line
    /// count is from before the count was recorded, so the native one
    /// supersedes it.
    fn native_order(&self, native: &SummaryAttrs, stored: &SummaryAttrs) -> Ordering {
        match (native.line_count, stored.line_count) {
            (Some(native), Some(stored)) => native.cmp(&stored),
            _ => Ordering::Greater,
        }
    }

    /// A stored session is read under the format this build writes;
    /// any other value names bytes this build cannot interpret.
    fn read_stored(
        &self,
        native_id: String,
        content_format: &str,
        source: Box<dyn ContentSource>,
    ) -> Result<Box<dyn StoredSession>, DriverError> {
        if content_format != CONTENT_FORMAT {
            return Err(DriverError::Other(format!(
                "stored session {native_id} has content format {content_format:?}; \
                 this build reads {CONTENT_FORMAT:?} (upgrade gage)"
            )));
        }
        Ok(Box::new(ClaudeStoredSession {
            native_id,
            content_format: content_format.to_string(),
            source,
        }))
    }

    /// Spawn a `claude -p` child; see [`crate::agent`]. Requires a
    /// tokio runtime.
    fn start_agent(&self, spec: AgentSpec) -> Result<Box<dyn AgentSession>, DriverError> {
        Ok(Box::new(crate::agent::start(spec)?))
    }

    /// An agent starts when `claude` is on PATH and logged in.
    fn scan_ready(&self) -> Result<(), DriverError> {
        preflight::check_auth().map_err(|e| DriverError::Other(e.to_string()))
    }

    /// Start the skill session. The plugin must be installed at this
    /// build's version and the user logged in. A given model has its
    /// size alias resolved.
    fn start_skill_session(
        &self,
        session: &SkillSession<'_>,
    ) -> Result<Box<dyn HarnessSession>, DriverError> {
        preflight::check().map_err(|e| DriverError::Other(e.to_string()))?;
        let model = session.model.map(resolve_model);
        let session = SkillSession { model, ..*session };
        let child = skill_command(&session)?.spawn().map_err(DriverError::Io)?;
        Ok(Box::new(ClaudeHarnessSession(child)))
    }

    fn status(&self) -> Result<DriverStatus, DriverError> {
        Ok(DriverStatus {
            harness: harness_status(),
            auth: auth_status(),
            install: install_status(),
        })
    }

    fn system_init(
        &self,
        mcp_cmd: &[&str],
        ui: &mut dyn InstallUi,
    ) -> Result<InstallReport, DriverError> {
        if mcp_cmd.is_empty() {
            return Err(DriverError::Other(
                "system_init: mcp_cmd must name an executable".into(),
            ));
        }
        let claude_bin = find_claude().map_err(DriverError::Io)?;
        let marketplace = plugin_marketplace_dir();
        let mut notes = Vec::new();

        ui.step("Writing plugin files");
        plugin::write_plugin_files_to(&marketplace).map_err(DriverError::Io)?;
        plugin::write_marketplace_manifest_to(&marketplace).map_err(DriverError::Io)?;
        plugin::write_mcp_json(&marketplace, mcp_cmd).map_err(DriverError::Io)?;
        notes.push(format!("plugin materialized at {}", marketplace.display()));
        ui.step_done();

        ui.step("Registering plugin marketplace");
        run_claude_strict(
            &claude_bin,
            &[
                "plugin",
                "marketplace",
                "add",
                &marketplace.to_string_lossy(),
            ],
        )?;
        notes.push("marketplace 'gage' registered".into());
        ui.step_done();

        ui.step("Installing plugin");
        // Silent uninstall first: `plugin install` on an already-
        // installed plugin is a no-op and leaves the cached plugin.json
        // frozen at the prior installed version. Uninstall-then-install
        // forces Claude to re-copy from source.
        silent_claude(&claude_bin, &["plugin", "uninstall", PLUGIN_ID]);
        run_claude_strict(&claude_bin, &["plugin", "install", PLUGIN_ID])?;
        notes.push(format!("plugin {PLUGIN_ID} installed"));
        ui.step_done();

        Ok(InstallReport {
            registered_as: MCP_SERVER_NAME.to_string(),
            notes,
        })
    }

    fn system_remove(&self, ui: &mut dyn InstallUi) -> Result<(), DriverError> {
        let claude_bin = find_claude().map_err(DriverError::Io)?;
        ui.step("Uninstalling plugin");
        run_claude_best_effort(&claude_bin, &["plugin", "uninstall", PLUGIN_ID], ui);
        ui.step_done();
        ui.step("Removing marketplace");
        run_claude_best_effort(
            &claude_bin,
            &["plugin", "marketplace", "remove", "gage"],
            ui,
        );
        ui.step_done();
        Ok(())
    }
}

/// The skill session's `claude` process.
struct ClaudeHarnessSession(Child);

impl HarnessSession for ClaudeHarnessSession {
    fn wait(&mut self) -> io::Result<i32> {
        Ok(self.0.wait()?.code().unwrap_or(-1))
    }

    fn kill(&mut self) -> io::Result<()> {
        self.0.kill()
    }
}

/// The `claude` binary on PATH and its version from `claude --version`,
/// whose first token is the version.
fn harness_status() -> Field<HarnessInfo> {
    let path = match find_claude() {
        Ok(path) => path,
        Err(_) => {
            return Field::Problem {
                info: None,
                message: format!("{HARNESS_NAME} not found on PATH"),
                remedy: format!("Install {HARNESS_NAME}"),
            };
        }
    };
    let version = std::process::Command::new(&path)
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| {
            String::from_utf8_lossy(&o.stdout)
                .split_whitespace()
                .next()
                .map(String::from)
        });
    Field::Ok(HarnessInfo {
        name: HARNESS_NAME.to_string(),
        version,
        path,
    })
}

fn auth_status() -> Field<AuthInfo> {
    match preflight::auth_status() {
        Ok(s) => {
            let info = AuthInfo {
                logged_in: s.logged_in,
                account: s.email,
                plan: s.subscription_type,
                org: s.org_name,
                provider: s.api_provider,
                method: s.auth_method,
            };
            if info.logged_in {
                Field::Ok(info)
            } else {
                Field::Problem {
                    info: Some(info),
                    message: "not logged in".to_string(),
                    remedy: "run `claude` and complete /login".to_string(),
                }
            }
        }
        Err(e) => Field::Problem {
            info: None,
            message: format!("login status unavailable: {e}"),
            remedy: String::new(),
        },
    }
}

fn install_status() -> Field<InstallInfo> {
    let expected_version = preflight::EXPECTED_VERSION.to_string();
    match preflight::installed_plugin() {
        Ok(Some(p)) => {
            let info = InstallInfo {
                installed: true,
                version: Some(p.version.clone()),
                expected_version: expected_version.clone(),
                enabled: p.enabled,
                path: p.install_path.map(PathBuf::from),
            };
            if p.version == expected_version {
                Field::Ok(info)
            } else {
                Field::Problem {
                    info: Some(info),
                    message: format!(
                        "Gage plugin v{} installed, this gage expects v{expected_version}",
                        p.version
                    ),
                    remedy: "run `gage init`".to_string(),
                }
            }
        }
        Ok(None) => Field::Problem {
            info: Some(InstallInfo {
                installed: false,
                version: None,
                expected_version,
                enabled: false,
                path: None,
            }),
            message: "Gage plugin not installed".to_string(),
            remedy: "run `gage init`".to_string(),
        },
        Err(e) => Field::Problem {
            info: None,
            message: format!("plugin status unavailable: {e}"),
            remedy: String::new(),
        },
    }
}

/// Run `claude <args>`, inheriting stderr. Nonzero exit and spawn
/// failures are errors.
fn run_claude_strict(claude_bin: &Path, args: &[&str]) -> Result<(), DriverError> {
    let output = std::process::Command::new(claude_bin)
        .args(args)
        .stderr(std::process::Stdio::inherit())
        .output()
        .map_err(DriverError::Io)?;
    if !output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        return Err(DriverError::Other(format!(
            "claude {} failed: {stdout}",
            args.join(" ")
        )));
    }
    Ok(())
}

/// Run `claude <args>` and report a non-fatal failure through `ui`.
/// Both nonzero exit and spawn failure are warnings.
fn run_claude_best_effort(claude_bin: &Path, args: &[&str], ui: &mut dyn InstallUi) {
    match std::process::Command::new(claude_bin)
        .args(args)
        .stderr(std::process::Stdio::inherit())
        .output()
    {
        Ok(output) if !output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            ui.warn(&format!("claude {} failed: {stdout}", args.join(" ")));
        }
        Ok(_) => {}
        Err(e) => {
            ui.warn(&format!("failed to run claude {}: {e}", args.join(" ")));
        }
    }
}

/// Run `claude <args>` discarding stdout/stderr and ignoring the exit
/// code. Used before an install to force Claude to re-copy the plugin
/// from source; a "not installed" error on first-time init is expected
/// and silent.
fn silent_claude(claude_bin: &Path, args: &[&str]) {
    match std::process::Command::new(claude_bin)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
    {
        Ok(_) | Err(_) => {}
    }
}

/// Handle over one Claude projects directory. Holds the index store
/// for the directory and the project registry parsed from
/// `.claude.json` on first use.
pub struct ClaudeSource {
    source: String,
    projects_dir: PathBuf,
    /// The Claude root the projects dir sits under
    root: PathBuf,
    store: Arc<IndexStore>,
    /// Project slug to recorded cwd, loaded once. The error string is
    /// kept so every later call reports the same failure.
    projects: OnceLock<Result<HashMap<String, PathBuf>, String>>,
    /// Native id to transcript path, walked once. A lookup per
    /// selected session over 1,600 files walked the tree 3,000 times
    /// for one `session add --all`; one walk serves every lookup.
    files: OnceLock<Result<HashMap<String, PathBuf>, String>>,
}

impl ClaudeSource {
    /// The index store behind this source: the summary cache the
    /// agent runner reads session attributes through.
    pub(crate) fn index_store(&self) -> Arc<IndexStore> {
        Arc::clone(&self.store)
    }

    fn open(options: &SourceOptions) -> Result<Self, DriverError> {
        let source = options.url();
        let projects_dir = resolve_projects_dir(source)?;
        let root = root_of(&projects_dir);
        let cache_root = options
            .cache_root_path()
            .map(Path::to_path_buf)
            .unwrap_or_else(default_cache_root);
        let store = Arc::new(
            IndexStore::new(projects_dir.clone(), cache_root).show_agents(options.shows_agents()),
        );
        store.ensure_summary_dir()?;
        Ok(Self {
            source: source.to_string(),
            projects_dir,
            root,
            store,
            projects: OnceLock::new(),
            files: OnceLock::new(),
        })
    }

    /// The Claude root the projects directory sits under
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn projects(&self) -> Result<&HashMap<String, PathBuf>, DriverError> {
        self.projects
            .get_or_init(|| load_projects(&self.root).map_err(|e| e.to_string()))
            .as_ref()
            .map_err(|e| DriverError::Other(format!("reading project registry: {e}")))
    }

    fn files(&self) -> Result<&HashMap<String, PathBuf>, DriverError> {
        self.files
            .get_or_init(|| {
                walk_session_files(&self.projects_dir, self.store.shows_agents())
                    .map(|hit| hit.map(|(id, path, _meta)| (id, path)))
                    .collect::<io::Result<HashMap<_, _>>>()
                    .map_err(|e| e.to_string())
            })
            .as_ref()
            .map_err(|e| DriverError::Other(format!("reading sessions: {e}")))
    }
}

impl Source for ClaudeSource {
    fn source(&self) -> &str {
        &self.source
    }

    fn session_url(&self, session_id: &str) -> String {
        session_url(&self.root, session_id)
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn tables(&self) -> Result<DriverTables, DriverError> {
        Ok(DriverTables {
            session: Arc::new(SessionTable::new(Arc::clone(&self.store))),
            message: Arc::new(MessageTable::new(Arc::clone(&self.store))),
            entry: Arc::new(EntryTable::new(Arc::clone(&self.store))),
            tool_use: Arc::new(ToolUseTable::new(Arc::clone(&self.store))),
            tool_use_file: Arc::new(ToolUseFileTable::new(Arc::clone(&self.store))),
        })
    }

    fn warm_summaries(&self, limit: usize) -> Result<(), DriverError> {
        let newest = self.store.session_list().limit(limit).build();
        let uncached: Vec<&SessionInfo> = newest
            .iter()
            .filter(|s| self.store.session_attrs(&s.id, s.mtime).is_none())
            .collect();
        let span = progress::read_sessions(uncached.len());
        for (done, s) in uncached.iter().enumerate() {
            // The first read of the attrs derives the summary and
            // writes it to the cache. A session that cannot be read
            // caches its read error and shows with default attributes.
            ClaudeNativeSession::open(&s.id, &s.src, s.mtime, s.size, &self.root, &self.store)
                .claude_attrs();
            progress::advance(&span, done + 1);
        }
        Ok(())
    }

    /// The reconcile pass over the summary cache: every session the
    /// cache does not hold at its current fingerprint is derived, and
    /// the cache of every session gone from disk is removed.
    fn reconcile(&self, rebuild: bool) -> Result<MaintenanceOutcome, DriverError> {
        let outcome = if rebuild {
            self.store.rebuild()
        } else {
            self.store.reconcile(LockMode::Wait)
        }
        .map_err(|e| DriverError::Other(e.to_string()))?;
        Ok(MaintenanceOutcome {
            discovered: outcome.discovered,
            prepared: outcome.indexed,
            removed: outcome.removed,
            elapsed_ms: outcome.elapsed_ms,
        })
    }

    fn maintenance_status(&self) -> Result<MaintenanceStatus, DriverError> {
        let status = self.store.status();
        Ok(MaintenanceStatus {
            discovered: status.discovered,
            prepared: status.indexed,
            stale: status.dirty,
            state_dir: Some(status.summary_dir),
            state_bytes: status.cache_bytes,
            text_dir: Some(status.text_dir),
            text_bytes: status.text_bytes,
            last_reconcile_ms: status.last_reconcile_ms,
        })
    }

    /// Search the text index, after a reconcile that yields to a pass
    /// already running: on contention the committed index is searched
    /// as it stands, at most one pass behind the corpus.
    fn message_text(
        &self,
        query: &str,
        filter: &MessageTextFilter,
        snippet_chars: usize,
        limit: usize,
    ) -> Result<Vec<MessageTextHit>, DriverError> {
        self.store
            .reconcile(LockMode::Try)
            .map_err(|e| DriverError::Other(e.to_string()))?;
        self.store
            .search(query, filter, limit, snippet_chars)
            .map_err(|e| DriverError::Other(e.to_string()))
    }

    fn find_native(&self, prefix: &str) -> Result<String, NativeLookupError> {
        let files = self.files().map_err(NativeLookupError::Driver)?;
        let mut matches: Vec<String> = files
            .keys()
            .filter(|id| id.starts_with(prefix))
            .cloned()
            .collect();
        match matches.len() {
            0 => Err(NativeLookupError::NoMatch(prefix.to_string())),
            1 => Ok(matches.pop().unwrap()),
            _ => {
                matches.sort();
                Err(NativeLookupError::TooManyMatches {
                    prefix: prefix.to_string(),
                    candidates: matches,
                })
            }
        }
    }

    fn open_native(&self, native_id: &str) -> Result<Box<dyn NativeSession>, DriverError> {
        let (path, meta) = self.session_file(native_id)?;
        let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        Ok(Box::new(ClaudeNativeSession::open(
            native_id,
            &path,
            mtime,
            meta.len(),
            &self.root,
            &self.store,
        )))
    }

    fn delete_native(&self, native_id: &str) -> Result<(), DriverError> {
        let (path, _meta) = self.session_file(native_id)?;
        Ok(delete_session(&path)?)
    }

    fn move_native(&self, native_id: &str, project: &Path) -> Result<(), DriverError> {
        let (path, _meta) = self.session_file(native_id)?;
        let project = fs::canonicalize(project)
            .map_err(|e| DriverError::Other(format!("{}: {e}", project.display())))?;
        let dest_dir = self.projects_dir.join(encode_project_dir(&project));
        if path.parent() == Some(dest_dir.as_path()) {
            return Err(DriverError::Other(format!(
                "session {native_id} is already in {}",
                project.display()
            )));
        }
        let dest = dest_dir.join(format!("{native_id}.jsonl"));
        if dest.exists() {
            return Err(DriverError::Other(format!(
                "destination already holds a session with this id: {}",
                dest.display()
            )));
        }
        Ok(move_session(&path, &dest_dir, &project)?)
    }

    fn project_name(&self, path: &Path) -> Result<String, DriverError> {
        let canonical = match fs::canonicalize(path) {
            Ok(c) => c,
            Err(e) if e.kind() == io::ErrorKind::NotFound => path.to_path_buf(),
            Err(e) => return Err(DriverError::Io(e)),
        };
        Ok(encode_project_dir(&canonical))
    }

    fn project_path(&self, name: &str) -> Result<Option<PathBuf>, DriverError> {
        Ok(self.projects()?.get(name).cloned())
    }

    fn is_session_live(&self, session_id: &str) -> Result<bool, SessionLiveError> {
        session_live(&self.root, session_id)
    }

    fn close(self: Box<Self>) -> Result<(), DriverError> {
        Ok(())
    }
}

/// Whether Claude Code records `session_id` as a live session now.
/// Claude Code writes one `sessions/<pid>.json` file per running
/// session under the state root, each carrying the session id, its
/// process id, and the pidDomain that id belongs to. A session is
/// live when a matching entry's process is running on this host.
/// Absent entry is `Ok(false)`: the session is not live. An entry in
/// another pidDomain cannot be probed from here, so it is
/// [`SessionLiveError::Unknown`].
fn session_live(root: &Path, session_id: &str) -> Result<bool, SessionLiveError> {
    let dir = root.join("sessions");
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(DriverError::Io(e).into()),
    };
    let mut newest: Option<SessionRegistryEntry> = None;
    for entry in entries {
        let path = entry.map_err(DriverError::Io)?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            // The registry file was removed between the listing and the
            // read: a session that exited mid-scan is simply not here.
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(DriverError::Io(e).into()),
        };
        let parsed = match serde_json::from_slice::<SessionRegistryEntry>(&bytes) {
            Ok(parsed) => parsed,
            Err(e) => {
                tracing::error!(path = %path.display(), "unreadable session registry entry: {e}");
                continue;
            }
        };
        if parsed.session_id != session_id {
            continue;
        }
        let newer = newest
            .as_ref()
            .is_none_or(|held| parsed.updated_at.unwrap_or(0) >= held.updated_at.unwrap_or(0));
        if newer {
            newest = Some(parsed);
        }
    }
    let Some(entry) = newest else {
        return Ok(false);
    };
    match local_pid_domain() {
        Some(local) if entry.pid_domain.as_deref() == Some(local.as_str()) => {
            Ok(pid_alive(entry.pid))
        }
        _ => Err(SessionLiveError::Unknown(format!(
            "session {session_id} is recorded in pidDomain {:?}, which this host cannot probe",
            entry.pid_domain
        ))),
    }
}

/// The fields of a `sessions/<pid>.json` entry this probe reads. Other
/// fields of the entry are ignored.
#[derive(Deserialize)]
struct SessionRegistryEntry {
    #[serde(rename = "sessionId")]
    session_id: String,
    pid: i32,
    #[serde(default, rename = "pidDomain")]
    pid_domain: Option<String>,
    #[serde(default, rename = "updatedAt")]
    updated_at: Option<i64>,
}

/// This host's Claude Code pidDomain, `linux:<machine-id>:pid:[<pid
/// namespace inode>]`, or `None` when either part cannot be read. A
/// `None` result makes every registered session unprobeable, which is
/// reported as unknown rather than guessed.
fn local_pid_domain() -> Option<String> {
    let machine_id = fs::read_to_string("/etc/machine-id").ok()?;
    let machine_id = machine_id.trim();
    if machine_id.is_empty() {
        return None;
    }
    let ns = fs::read_link("/proc/self/ns/pid").ok()?;
    let ns = ns.to_str()?;
    Some(format!("linux:{machine_id}:{ns}"))
}

/// Whether `pid` names a process that exists on this host. A signal of
/// 0 sends nothing; it only reports whether the process is there and
/// whether this process could signal it.
fn pid_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    // SAFETY: kill with signal 0 performs the permission and existence
    // check without delivering a signal.
    let ret = unsafe { libc::kill(pid, 0) };
    if ret == 0 {
        return true;
    }
    io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Slug to cwd for every project the root's registry records. A
/// missing registry is an empty map.
/// The project name without a leading `-home-<user>-` or
/// `-Users-<user>-`, the encoded home directory Claude Code puts in
/// front of nearly every project. The match is on the name's shape,
/// so names written on another machine shorten the same way.
fn strip_home_prefix(name: &str) -> &str {
    ["-home-", "-Users-"]
        .iter()
        .find_map(|prefix| {
            let rest = name.strip_prefix(prefix)?;
            let (user, project) = rest.split_once('-')?;
            (!user.is_empty() && !project.is_empty()).then_some(project)
        })
        .unwrap_or(name)
}

/// The name cut to `max_chars` characters. A long name keeps a head
/// of one quarter of the budget, then `…`, then as many of its final
/// characters as fill the rest. The tail carries the project's own
/// directory name, so it gets the larger share.
fn shorten_project_name(name: &str, max_chars: usize) -> String {
    const ELLIPSIS: char = '…';
    let count = name.chars().count();
    if count <= max_chars {
        return name.to_string();
    }
    if max_chars == 0 {
        return String::new();
    }
    let head_len = max_chars / 4;
    let tail_len = max_chars - head_len - 1;
    let head: String = name.chars().take(head_len).collect();
    let tail: String = name.chars().skip(count - tail_len).collect();
    format!("{head}{ELLIPSIS}{tail}")
}

impl ClaudeSource {
    /// The session file for `native_id` and its metadata
    /// The transcript path and a fresh stat of it. The stat is taken
    /// per call so a file removed after the walk reports not found and
    /// the mtime reflects the file as it is now.
    fn session_file(&self, native_id: &str) -> Result<(PathBuf, fs::Metadata), DriverError> {
        let not_found =
            || DriverError::Other(format!("native session not found: {NAME}:{native_id}"));
        let path = self.files()?.get(native_id).ok_or_else(not_found)?;
        match path.metadata() {
            Ok(meta) => Ok((path.clone(), meta)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Err(not_found()),
            Err(e) => Err(DriverError::Io(e)),
        }
    }
}

fn load_projects(root: &Path) -> io::Result<HashMap<String, PathBuf>> {
    let home = claude_home_for_root(root)?;
    let projects = match home.projects() {
        Ok(list) => list,
        Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e),
    };
    Ok(projects
        .into_iter()
        .map(|p| (encode_project_dir(&p.path), p.path))
        .collect())
}

/// Resolve the projects directory from a source value. A leading
/// `claude:` scheme is stripped; any other scheme is an error. An
/// empty body is the default location (`CLAUDE_PROJECTS_DIR`, else
/// `projects/` under the Claude home). A non-empty body is a Claude
/// root path (`~` expanded) whose sessions live under `projects/`.
fn resolve_projects_dir(source: &str) -> Result<PathBuf, DriverError> {
    let body = match split_scheme(source) {
        Some((scheme, body)) if SCHEMES.contains(&scheme) => body,
        Some((scheme, _)) => {
            return Err(DriverError::Other(format!(
                "unsupported source scheme {scheme:?}: {source}"
            )));
        }
        None => source,
    };
    let projects_dir = if body.is_empty() {
        projects_dir().ok_or_else(|| {
            DriverError::Other("CLAUDE_PROJECTS_DIR, CLAUDE_CONFIG_DIR, or HOME must be set".into())
        })?
    } else {
        expand_tilde(body).join("projects")
    };
    // Absolute so a session URL spelled from the root is location
    // independent
    std::path::absolute(&projects_dir).map_err(DriverError::Io)
}

/// The Claude root a projects directory sits under
fn root_of(projects_dir: &Path) -> PathBuf {
    projects_dir
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| projects_dir.to_path_buf())
}

fn expand_tilde(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    if s == "~"
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home);
    }
    PathBuf::from(s)
}

fn walk_session_files(
    projects_dir: &Path,
    show_agents: bool,
) -> Box<dyn Iterator<Item = io::Result<(String, PathBuf, fs::Metadata)>> + '_> {
    let entries = match fs::read_dir(projects_dir) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Box::new(std::iter::empty());
        }
        Err(e) => return Box::new(std::iter::once(Err(e))),
    };
    let it = entries.flat_map(move |project_entry| {
        let project_path = match project_entry {
            Ok(e) => e.path(),
            Err(e) => return Box::new(std::iter::once(Err(e))) as _,
        };
        if !project_path.is_dir() {
            return Box::new(std::iter::empty()) as _;
        }
        let slug = project_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if !show_agents && is_agent_tmp_slug(&slug) {
            return Box::new(std::iter::empty()) as _;
        }
        let dir_entries = match fs::read_dir(&project_path) {
            Ok(e) => e,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Box::new(std::iter::empty()) as _;
            }
            Err(e) => return Box::new(std::iter::once(Err(e))) as _,
        };
        Box::new(dir_entries.filter_map(|f| {
            let path = match f {
                Ok(e) => e.path(),
                Err(e) => return Some(Err(e)),
            };
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned())?;
            if !SESSION_RE.is_match(&name) {
                return None;
            }
            let id = name[..36].to_string();
            let meta = match path.metadata() {
                Ok(m) => m,
                Err(e) if e.kind() == io::ErrorKind::NotFound => return None,
                Err(e) => return Some(Err(e)),
            };
            Some(Ok((id, path, meta)))
        })) as Box<dyn Iterator<Item = io::Result<(String, PathBuf, fs::Metadata)>>>
    });
    Box::new(it)
}

/// `claude:<absolute root>/<id>`, the URL of one session under `root`
fn session_url(root: &Path, session_id: &str) -> String {
    format!("{NAME}:{}/{session_id}", root.display())
}

/// A native Claude session with its attributes read at open. The
/// summary (title, model, message count, emptiness, token usage) comes
/// from the source's summary cache when the cache entry is current for
/// the file's mtime, else from one parse of the transcript, which then
/// refreshes the cache.
pub struct ClaudeNativeSession {
    native_id: String,
    /// `claude:<absolute root>/<uuid>`
    source: String,
    session_path: PathBuf,
    /// The file's mtime and size as the caller last read them from
    /// the filesystem, before the session was opened.
    mtime: SystemTime,
    size: u64,
    /// Also held in the derived attrs. The derive needs it before
    /// those exist, and the store reads it from them afterwards.
    project: String,
    store: IndexStore,
    /// The derived attributes, read on first use: from the summary
    /// cache when it is current, otherwise from the transcript
    derived: OnceCell<ClaudeSessionAttrs>,
}

impl ClaudeNativeSession {
    /// Open the session at `path` without reading it. `mtime` and
    /// `size` are the file's stat values as the caller last read them:
    /// from its directory walk when it lists sessions, or from a stat
    /// of this one file when it opens a session by id. `root` is the
    /// absolute Claude root the file sits under; `store` holds the
    /// summary cache.
    pub fn open(
        native_id: &str,
        path: &Path,
        mtime: SystemTime,
        size: u64,
        root: &Path,
        store: &IndexStore,
    ) -> Self {
        Self {
            native_id: native_id.to_string(),
            source: session_url(root, native_id),
            session_path: path.to_path_buf(),
            mtime,
            size,
            project: project_of(path),
            store: store.clone(),
            derived: OnceCell::new(),
        }
    }

    pub fn session_path(&self) -> &Path {
        &self.session_path
    }

    /// Record `project` as the session's project in place of the slug
    /// of the directory the transcript sits in.
    pub fn set_project(&mut self, project: String) {
        if let Some(derived) = self.derived.get_mut() {
            derived.project = project.clone();
        }
        self.project = project;
    }

    /// The full derived attributes, including the token counts the
    /// generic [`SessionAttrs`] view does not carry. The first call
    /// reads the transcript unless the summary cache holds it.
    pub fn claude_attrs(&self) -> &ClaudeSessionAttrs {
        self.derived.get_or_init(|| {
            // A transcript that does not derive is still a session: the
            // walk listed it. Its attributes carry the error in place
            // of the derived values and are cached like any other, so
            // the file is read again only when it changes.
            let mut attrs = match self.store.session_attrs(&self.native_id, self.mtime) {
                Some(cached) => cached,
                None => {
                    let attrs = match derive_session(&self.native_id, &self.session_path) {
                        Ok(derived) => derived.attrs,
                        Err(e) => {
                            let fingerprint = Fingerprint {
                                mtime_ms: millis(self.mtime),
                                size: self.size,
                            };
                            ClaudeSessionAttrs::with_read_error(&self.session_path, fingerprint, e)
                        }
                    };
                    cache_attrs(&self.store, &self.native_id, &attrs);
                    attrs
                }
            };
            attrs.project = self.project.clone();
            attrs
        })
    }
}

/// Write `attrs` to the summary cache. The cache is rebuilt from the
/// transcript on any read that misses it, so a failed write costs a
/// later read and is logged rather than raised.
fn cache_attrs(store: &IndexStore, native_id: &str, attrs: &ClaudeSessionAttrs) {
    if let Err(e) = store.put_session_attrs(native_id, attrs) {
        tracing::warn!(session_id = %native_id, "summary cache write failed: {e}");
    }
}

fn millis(time: SystemTime) -> i64 {
    time.duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

impl NativeSession for ClaudeNativeSession {
    fn id(&self) -> &str {
        &self.native_id
    }

    fn session_type(&self) -> &str {
        SESSION_TYPE
    }

    fn source(&self) -> &str {
        &self.source
    }

    fn attrs(&self) -> &dyn SessionAttrs {
        self.claude_attrs()
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// A session with a read error has no derived values: every derived
/// accessor is `None`, and `extra_attrs` carries the error.
impl SessionAttrs for ClaudeSessionAttrs {
    fn native_mtime(&self) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_millis(self.native_mtime_ms.max(0) as u64)
    }

    fn native_size(&self) -> u64 {
        self.native_size
    }

    fn is_empty(&self) -> Option<bool> {
        self.derived(|a| a.is_empty)
    }

    fn project_name(&self) -> Option<&str> {
        Some(&self.project)
    }

    fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    fn message_count(&self) -> Option<u64> {
        self.derived(|a| a.message_count.max(0) as u64)
    }

    fn line_count(&self) -> Option<u64> {
        self.derived(|a| a.line_count)
    }

    fn input_tokens(&self) -> Option<u64> {
        self.derived(|a| {
            let total = a.input_tokens + a.cache_read_input_tokens + a.cache_creation_input_tokens;
            total.max(0) as u64
        })
    }

    fn output_tokens(&self) -> Option<u64> {
        self.derived(|a| a.output_tokens.max(0) as u64)
    }

    fn cached_tokens(&self) -> Option<u64> {
        self.derived(|a| a.cache_read_input_tokens.max(0) as u64)
    }

    fn cost_usd(&self) -> Option<f64> {
        self.cost_usd
    }

    /// The token split the normalized totals fold together, the span
    /// of entry timestamps, and the read error when there is one.
    fn extra_attrs(&self) -> Map<String, Value> {
        let mut extra = Map::new();
        if let Some(error) = &self.read_error {
            extra.insert("read_error".to_string(), Value::from(error.as_str()));
            return extra;
        }
        extra.insert(
            "cache_read_input_tokens".to_string(),
            Value::from(self.cache_read_input_tokens),
        );
        extra.insert(
            "cache_creation_input_tokens".to_string(),
            Value::from(self.cache_creation_input_tokens),
        );
        if let Some(ms) = self.first_timestamp_ms {
            extra.insert("first_timestamp_ms".to_string(), Value::from(ms));
        }
        if let Some(ms) = self.last_timestamp_ms {
            extra.insert("last_timestamp_ms".to_string(), Value::from(ms));
        }
        extra
    }
}

impl ClaudeSessionAttrs {
    /// A derived value, or `None` when the session has a read error
    fn derived<T>(&self, value: impl FnOnce(&Self) -> T) -> Option<T> {
        match self.read_error {
            Some(_) => None,
            None => Some(value(self)),
        }
    }
}

/// The `ClaudeHome` for a resolved root. The env-resolved home carries
/// the registry location Claude Code uses for it (`$HOME/.claude.json`
/// as a sibling of `$HOME/.claude`); any other root keeps
/// `.claude.json` inside itself.
fn claude_home_for_root(root: &Path) -> Result<ClaudeHome, io::Error> {
    match crate::home::claude_home() {
        Some(default) if default == root => ClaudeHome::from_env(),
        _ => Ok(ClaudeHome::new(root.to_path_buf())),
    }
}

struct ClaudeStoredSession {
    native_id: String,
    content_format: String,
    source: Box<dyn ContentSource>,
}

impl StoredSession for ClaudeStoredSession {
    fn native_id(&self) -> &str {
        &self.native_id
    }

    fn content_format(&self) -> &str {
        &self.content_format
    }

    /// One normalized row per parsed line of `session.jsonl`, through
    /// the same derivation the native tables use. Blank lines and
    /// lines without a type consume a line number and yield no row,
    /// as they do for a native session; a line that does not read or
    /// decode is an error naming the line.
    fn entries(&mut self) -> Box<dyn Iterator<Item = Result<Entry, DriverError>> + '_> {
        let reader = match self.source.open("session.jsonl") {
            Ok(r) => SessionReader::new(r),
            Err(e) => return Box::new(std::iter::once(Err(DriverError::Io(e)))),
        };
        let native_id = self.native_id.clone();
        Box::new(reader.filter_map(move |item| match item {
            Ok((line_num, raw, decoded)) => {
                let value = serde_json::Value::from(decoded);
                derive_entry(&native_id, line_num, &value, raw).map(Ok)
            }
            Err(e) => Some(Err(DriverError::from(e))),
        }))
    }

    /// The calls of `session.jsonl` paired with their results, through
    /// the same collector the native table uses. Pairing needs the
    /// whole file, so the rows are read before the first is yielded;
    /// read failures follow the rows.
    fn tool_uses(&mut self) -> Box<dyn Iterator<Item = Result<ToolUse, DriverError>> + '_> {
        let reader = match self.source.open("session.jsonl") {
            Ok(r) => SessionReader::new(r),
            Err(e) => return Box::new(std::iter::once(Err(DriverError::Io(e)))),
        };
        let mut collector = ToolUseCollector::new();
        let mut failures = Vec::new();
        for item in reader {
            match item {
                Ok((line_num, _, decoded)) => {
                    collector.push(line_num, &serde_json::Value::from(decoded));
                }
                Err(e) => failures.push(Err(DriverError::from(e))),
            }
        }
        Box::new(collector.finish().into_iter().map(Ok).chain(failures))
    }

    /// Each entry of `session.jsonl` decoded, numbered as `entries`
    /// numbers it. A blank line and a line without a type consume a
    /// line number and yield nothing, as they do for `entries`; a line
    /// that does not decode is an error naming the line.
    fn entry_values(&mut self) -> Box<dyn Iterator<Item = Result<DecodedEntry, DriverError>> + '_> {
        use std::io::BufRead;
        let reader = match self.source.open("session.jsonl") {
            Ok(r) => io::BufReader::new(r),
            Err(e) => return Box::new(std::iter::once(Err(DriverError::Io(e)))),
        };
        let codec = codec();
        Box::new(reader.lines().enumerate().filter_map(move |(i, item)| {
            let line = i as u32 + 1;
            match item {
                Ok(text) if text.trim().is_empty() => None,
                Ok(text) => match codec.decode(text.as_bytes()) {
                    Ok(value) => is_entry(&value).then_some(Ok(DecodedEntry { line, value })),
                    Err(e) => Some(Err(DriverError::Other(format!("line {line}: {e}")))),
                },
                Err(e) => Some(Err(DriverError::Io(e))),
            }
        }))
    }

    fn write_derived(
        &mut self,
        derivation: &Derivation,
        sink: &mut dyn ContentSink,
    ) -> Result<Vec<u32>, DriverError> {
        self.write_derived_content(derivation, sink)
    }

    /// The transcript's attributes through the same derivation a
    /// native session gets. The project is not in the content; the
    /// store keeps the original's.
    fn attrs(&mut self) -> Result<Box<dyn SessionAttrs>, DriverError> {
        let mut reader = self.source.open("session.jsonl")?;
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes)?;
        let fingerprint = Fingerprint {
            mtime_ms: 0,
            size: bytes.len() as u64,
        };
        let derived = derive_session_from(
            &self.native_id,
            SessionReader::new(bytes.as_slice()),
            fingerprint,
            String::new(),
        )
        .map_err(|e| DriverError::Other(format!("stored session {}: {e}", self.native_id)))?;
        Ok(Box::new(derived.attrs))
    }

    fn summary_version(&self) -> u32 {
        SUMMARY_VERSION
    }
}

impl ClaudeStoredSession {
    /// `session.jsonl` with `derivation` applied, line by line, the
    /// appended entries after its last line, and every other path
    /// copied as stored.
    fn write_derived_content(
        &mut self,
        derivation: &Derivation,
        sink: &mut dyn ContentSink,
    ) -> Result<Vec<u32>, DriverError> {
        use std::io::{BufRead, Write};
        let mut appended = Vec::new();
        for path in self.source.paths()? {
            let mut reader = self.source.open(&path)?;
            let mut writer = sink.create(&path)?;
            if path != "session.jsonl" {
                io::copy(&mut reader, &mut writer)?;
                continue;
            }
            let codec = codec();
            let mut written: u32 = 0;
            for (i, line) in io::BufReader::new(reader).lines().enumerate() {
                let line = line?;
                let number = i as u32 + 1;
                if derivation.dropped.contains(&number) {
                    continue;
                }
                match derivation.replaced.get(&number) {
                    Some(value) => writer.write_all(&codec.encode(value)?)?,
                    None => writer.write_all(line.as_bytes())?,
                }
                writer.write_all(b"\n")?;
                written += 1;
            }
            for value in &derivation.appended {
                writer.write_all(&codec.encode(value)?)?;
                writer.write_all(b"\n")?;
                written += 1;
                appended.push(written);
            }
        }
        Ok(appended)
    }
}

/// Whether a decoded line is one `entries` yields a row for: an entry
/// with a `type`.
fn is_entry(value: &EntryValue) -> bool {
    value.get("type").and_then(EntryValue::as_str).is_some()
}

#[cfg(test)]
mod tests {
    use gage_session::progress::recorder;
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn delete_native_removes_session_and_sidecar() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let id = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let path = seed_session(root, "/Users/alice/Code/gage", id, "");
        let sidecar = path.with_extension("");
        fs::create_dir_all(sidecar.join("subagents")).unwrap();

        let source = ClaudeDriver::new().open_source(&options_for(root)).unwrap();
        source.delete_native(id).unwrap();
        assert!(!path.exists());
        assert!(!sidecar.exists());
        assert!(source.open_native(id).is_err());
    }

    #[test]
    fn move_native_relocates_session_under_destination_slug() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let id = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let project = root.join("work").join("newproj");
        fs::create_dir_all(&project).unwrap();
        let project = fs::canonicalize(&project).unwrap();
        let path = seed_session(
            root,
            "/Users/alice/Code/old",
            id,
            "{\"type\":\"user\",\"cwd\":\"/Users/alice/Code/old\"}\n",
        );
        let sidecar = path.with_extension("");
        fs::create_dir_all(sidecar.join("subagents")).unwrap();

        let source = ClaudeDriver::new().open_source(&options_for(root)).unwrap();
        source.move_native(id, &project).unwrap();

        let dest_dir = root.join("projects").join(encode_project_dir(&project));
        let dest = dest_dir.join(format!("{id}.jsonl"));
        assert!(!path.exists());
        assert!(!sidecar.exists());
        assert!(dest_dir.join(id).join("subagents").is_dir());
        let moved = fs::read_to_string(&dest).unwrap();
        let entry: serde_json::Value = serde_json::from_str(moved.trim()).unwrap();
        assert_eq!(entry["cwd"], project.to_string_lossy().as_ref());
    }

    #[test]
    fn move_native_refuses_same_project_and_occupied_destination() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let id = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let here = root.join("here");
        let there = root.join("there");
        fs::create_dir_all(&here).unwrap();
        fs::create_dir_all(&there).unwrap();
        let here = fs::canonicalize(&here).unwrap();
        let there = fs::canonicalize(&there).unwrap();
        let path = seed_session(root, &here.to_string_lossy(), id, "");

        let source = ClaudeDriver::new().open_source(&options_for(root)).unwrap();
        let err = source.move_native("nope", &there).unwrap_err();
        assert!(err.to_string().contains("not found"), "{err}");
        let err = source.move_native(id, &here).unwrap_err();
        assert!(err.to_string().contains("already in"), "{err}");
        // Written after the source walked its files, so the id still
        // resolves to the copy under `here`.
        seed_session(root, &there.to_string_lossy(), id, "");
        let err = source.move_native(id, &there).unwrap_err();
        assert!(err.to_string().contains("already holds"), "{err}");
        let err = source.move_native(id, &root.join("missing")).unwrap_err();
        assert!(err.to_string().contains("missing"), "{err}");
        assert!(path.exists());
    }

    #[test]
    fn delete_native_unknown_id_errors() {
        let tmp = TempDir::new().unwrap();
        let source = ClaudeDriver::new()
            .open_source(&options_for(tmp.path()))
            .unwrap();
        let err = source.delete_native("nope").unwrap_err();
        assert!(err.to_string().contains("native session not found"));
    }

    #[test]
    fn format_project_strips_home_prefix() {
        let driver = ClaudeDriver::new();
        assert_eq!(
            driver.format_project("-home-garrett-Code-gage", 25),
            "Code-gage"
        );
        assert_eq!(
            driver.format_project("-Users-alice-Code-gage", 25),
            "Code-gage"
        );
        assert_eq!(driver.format_project("-tmp-scratch", 25), "-tmp-scratch");
        assert_eq!(driver.format_project("-home-garrett", 25), "-home-garrett");
        assert_eq!(
            driver.format_project("-home-garrett-", 25),
            "-home-garrett-"
        );
    }

    #[test]
    fn format_project_splits_head_and_tail_by_quarter() {
        let driver = ClaudeDriver::new();
        let short = "-home-garrett-Code-gage";
        let long = "-home-garrett-Code-gage-crates-gage-cli-src";
        let deep = "-home-garrett-Projects-abcdef-ghijk-7654-90876-hajhasd-1232132";
        assert_eq!(driver.format_project(short, 7), "C…-gage");
        assert_eq!(driver.format_project(long, 7), "C…i-src");
        assert_eq!(driver.format_project(deep, 7), "P…32132");
        assert_eq!(driver.format_project(short, 12), "Code-gage");
        assert_eq!(driver.format_project(long, 12), "Cod…-cli-src");
        assert_eq!(driver.format_project(deep, 12), "Pro…-1232132");
        assert_eq!(driver.format_project(long, 18), "Code…-gage-cli-src");
        assert_eq!(driver.format_project(deep, 18), "Proj…jhasd-1232132");
    }

    #[test]
    fn format_project_tiny_budgets() {
        let driver = ClaudeDriver::new();
        let name = "averylongprojectname";
        assert_eq!(driver.format_project(name, 3), "…me");
        assert_eq!(driver.format_project(name, 2), "…e");
        assert_eq!(driver.format_project(name, 1), "…");
        assert_eq!(driver.format_project(name, 0), "");
    }

    #[test]
    fn format_model_strips_vendor_prefix() {
        let driver = ClaudeDriver::new();
        assert_eq!(driver.format_model("claude-fable-5-1"), "fable-5-1");
        assert_eq!(driver.format_model("gpt-x"), "gpt-x");
        assert_eq!(driver.format_model(""), "");
    }

    #[test]
    fn warm_summaries_caches_the_newest_sessions_once() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let line = r#"{"type":"user","message":{"role":"user","content":"hi"}}"#;
        seed_session(
            root,
            "/Users/alice/x",
            "aaaaaaaa-1111-1111-1111-111111111111",
            line,
        );
        seed_session(
            root,
            "/Users/alice/y",
            "bbbbbbbb-2222-2222-2222-222222222222",
            line,
        );
        let source = ClaudeDriver::new().open_source(&options_for(root)).unwrap();

        let reported = recorder::record(|| source.warm_summaries(10).unwrap());
        let work = progress::READ_SESSIONS;
        assert_eq!(reported, [(work, 0, 2), (work, 1, 2), (work, 2, 2)]);

        let reported = recorder::record(|| source.warm_summaries(10).unwrap());
        assert_eq!(reported, []);
    }

    #[test]
    fn open_source_keeps_the_cache_under_the_given_root() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        seed_session(
            root,
            "/Users/alice/x",
            "aaaaaaaa-1111-1111-1111-111111111111",
            "",
        );
        let cache_root = root.join("elsewhere");
        let mut options = SourceOptions::new(&source_for(root));
        options.cache_root(&cache_root);
        let source = ClaudeDriver::new().open_source(&options).unwrap();
        let status = source.maintenance_status().unwrap();
        let state_dir = status.state_dir.unwrap();
        assert!(
            state_dir.starts_with(cache_root.join("driver").join("claude")),
            "{}",
            state_dir.display()
        );
        assert!(state_dir.ends_with("summary"), "{}", state_dir.display());
        assert!(state_dir.join("manifest.json").is_file());
        let corpus = gage_core::cache::read_corpus(state_dir.parent().unwrap()).unwrap();
        assert_eq!(corpus.source, source_for(root));
    }

    #[test]
    fn message_text_searches_the_source_and_names_sessions_by_url() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let id = "aaaaaaaa-1111-1111-1111-111111111111";
        seed_session(
            root,
            "/Users/alice/x",
            id,
            r#"{"type":"user","uuid":"u1","message":{"role":"user","content":"the quokka smiled"}}"#,
        );
        let source = ClaudeDriver::new().open_source(&options_for(root)).unwrap();
        let hits = source
            .message_text("quokka", &MessageTextFilter::new(), 200, 10)
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session_id, id);
        assert_eq!(hits[0].url, source.session_url(id));
        assert_eq!(hits[0].line, 1);
        assert_eq!(hits[0].type_.as_deref(), Some("user"));
        assert!(hits[0].snippet.contains("«quokka»"));
        assert!(
            source
                .message_text("wombat", &MessageTextFilter::new(), 200, 10)
                .unwrap()
                .is_empty()
        );
        let err = source
            .message_text("\"open", &MessageTextFilter::new(), 200, 10)
            .unwrap_err();
        assert!(
            err.to_string().contains("invalid text search query"),
            "{err}"
        );
    }

    fn seed_session(root: &Path, cwd: &str, id: &str, contents: &str) -> PathBuf {
        let slug = encode_project_dir(Path::new(cwd));
        let dir = root.join("projects").join(&slug);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{id}.jsonl"));
        fs::write(&path, contents).unwrap();
        path
    }

    fn source_for(root: &Path) -> String {
        format!("{NAME}:{}", root.to_string_lossy())
    }

    /// Options opening the source at `root` with its cache kept inside
    /// the same temp directory.
    fn options_for(root: &Path) -> SourceOptions {
        let mut options = SourceOptions::new(&source_for(root));
        options.cache_root(root.join("cache"));
        options
    }

    #[test]
    fn find_native_disambiguates_prefix() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let a = "aaaaaaaa-1111-1111-1111-111111111111";
        let b = "bbbbbbbb-2222-2222-2222-222222222222";
        seed_session(root, "/Users/alice/x", a, "");
        seed_session(root, "/Users/alice/y", b, "");

        let source = ClaudeDriver::new().open_source(&options_for(root)).unwrap();
        assert_eq!(source.find_native("aaaa").unwrap(), a);
        let mut bare_options = SourceOptions::new(&root.to_string_lossy());
        bare_options.cache_root(root.join("cache"));
        let bare = ClaudeDriver::new().open_source(&bare_options).unwrap();
        assert_eq!(bare.find_native("bbbb").unwrap(), b);
        match ClaudeDriver::new().open_source(&SourceOptions::new("zzz:/x")) {
            Err(DriverError::Other(msg)) => assert!(msg.contains("zzz"), "{msg}"),
            Err(e) => panic!("unexpected error: {e}"),
            Ok(_) => panic!("unknown scheme was accepted"),
        }
        let err = source.find_native("no-such").unwrap_err();
        assert!(matches!(err, NativeLookupError::NoMatch(_)));
    }

    #[test]
    fn show_agents_admits_agent_tmp_sessions() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let id = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        seed_session(root, "/Users/alice/.gage/tmp/run-1/cwd", id, "");

        let hidden = ClaudeDriver::new().open_source(&options_for(root)).unwrap();
        let err = hidden.find_native("aaaa").unwrap_err();
        assert!(matches!(err, NativeLookupError::NoMatch(_)));

        let mut options = options_for(root);
        options.show_agents(true);
        let shown = ClaudeDriver::new().open_source(&options).unwrap();
        assert_eq!(shown.find_native("aaaa").unwrap(), id);
    }

    #[test]
    fn native_session_source_is_root_and_id() {
        let tmp = TempDir::new().unwrap();
        let root = fs::canonicalize(tmp.path()).unwrap();
        let id = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        seed_session(&root, "/Users/alice/x", id, "");
        let source = ClaudeDriver::new()
            .open_source(&options_for(&root))
            .unwrap();
        let session = source.open_native(id).unwrap();
        assert_eq!(session.source(), format!("claude:{}/{id}", root.display()));
    }

    #[test]
    fn project_name_encodes_slug() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let cwd = "/Users/alice/Code/gage";
        seed_session(root, cwd, "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee", "");

        let source = ClaudeDriver::new().open_source(&options_for(root)).unwrap();
        let name = source.project_name(Path::new(cwd)).unwrap();
        assert_eq!(name, "-Users-alice-Code-gage");
    }

    #[test]
    fn a_session_url_splits_to_the_source_that_holds_it() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("work")).unwrap();
        let cwd = fs::canonicalize(root.join("work")).unwrap();
        let id = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        seed_session(root, &cwd.to_string_lossy(), id, "");
        fs::write(
            root.join(".claude.json"),
            format!(r#"{{"projects": {{"{}": {{}}}}}}"#, cwd.display()),
        )
        .unwrap();
        let driver = ClaudeDriver::new();
        let native = driver
            .open_source(&options_for(root))
            .unwrap()
            .open_native(id)
            .unwrap();
        let (source_url, _) = driver.split_session_url(native.source()).unwrap();
        assert_eq!(source_url, source_for(root));
        let mut options = SourceOptions::new(&source_url);
        options.cache_root(root.join("cache"));
        let source = driver.open_source(&options).unwrap();
        assert_eq!(source.source(), source_for(root));
        assert_eq!(
            source.project_path(&encode_project_dir(&cwd)).unwrap(),
            Some(cwd)
        );
    }

    #[test]
    fn split_session_url_reads_back_what_session_url_spells() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let id = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        seed_session(root, "/Users/alice/Code/gage", id, "");
        let driver = ClaudeDriver::new();
        let source = driver.open_source(&options_for(root)).unwrap();
        let url = source.session_url(id);
        assert_eq!(url, format!("{}/{id}", source_for(root)));
        assert_eq!(url, source.open_native(id).unwrap().source());
        assert_eq!(
            driver.split_session_url(&url).unwrap(),
            (source_for(root), id.to_string())
        );
        assert_eq!(
            driver.split_session_url(id).unwrap(),
            (format!("{NAME}:"), id.to_string())
        );
        assert!(driver.split_session_url("other:/x/y").is_err());
        assert!(driver.split_session_url("claude:/x/").is_err());
        assert!(driver.split_session_url("").is_err());
    }

    #[test]
    fn project_path_resolves_through_registry() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let cwd = tmp.path().join("work");
        fs::create_dir_all(&cwd).unwrap();
        let cwd = fs::canonicalize(&cwd).unwrap();
        seed_session(
            root,
            &cwd.to_string_lossy(),
            "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
            "",
        );
        fs::write(
            root.join(".claude.json"),
            format!(r#"{{"projects": {{"{}": {{}}}}}}"#, cwd.display()),
        )
        .unwrap();

        let source = ClaudeDriver::new().open_source(&options_for(root)).unwrap();
        let slug = encode_project_dir(&cwd);
        assert_eq!(source.project_path(&slug).unwrap(), Some(cwd));
        assert_eq!(source.project_path("-no-such").unwrap(), None);
    }

    struct NoContent;

    impl ContentSource for NoContent {
        fn paths(&self) -> io::Result<Vec<String>> {
            Ok(Vec::new())
        }
        fn open(&self, _path: &str) -> io::Result<Box<dyn io::Read + Send>> {
            Err(io::Error::other("no content"))
        }
    }

    /// A transcript of three lines as stored content.
    struct Transcript(&'static str);

    impl ContentSource for Transcript {
        fn paths(&self) -> io::Result<Vec<String>> {
            Ok(vec!["session.jsonl".to_string(), "other.txt".to_string()])
        }
        fn open(&self, path: &str) -> io::Result<Box<dyn io::Read + Send>> {
            let bytes = match path {
                "session.jsonl" => self.0.as_bytes(),
                _ => b"as stored",
            };
            Ok(Box::new(io::Cursor::new(bytes.to_vec())))
        }
    }

    #[derive(Default)]
    struct MemorySink(std::collections::BTreeMap<String, Vec<u8>>);

    impl ContentSink for MemorySink {
        fn create<'a>(&'a mut self, path: &str) -> io::Result<Box<dyn io::Write + 'a>> {
            Ok(Box::new(self.0.entry(path.to_string()).or_default()))
        }
    }

    /// The derived write leaves out a dropped line, writes a replaced
    /// line as given, appends the appended entries after the last
    /// line and reports their lines, and copies every other path.
    #[test]
    fn write_derived_applies_the_derivation_and_appends_entries() {
        let driver = ClaudeDriver::new();
        let mut stored = driver
            .read_stored(
                "s1".into(),
                CONTENT_FORMAT,
                Box::new(Transcript(
                    "{\"type\":\"a\"}\n{\"type\":\"b\"}\n{\"type\":\"c\"}\n",
                )),
            )
            .unwrap();
        let title = driver
            .title_entry(CONTENT_FORMAT, "Ducks \"quoted\"")
            .unwrap();
        assert_eq!(
            title.to_string(),
            r#"{"type":"custom-title","customTitle":"Ducks \"quoted\""}"#
        );
        assert_eq!(driver.title_entry("other 1", "x"), None);
        let c2 = codec().decode(b"{\"type\":\"c2\"}").unwrap();
        let derivation = Derivation {
            dropped: [2].into_iter().collect(),
            replaced: [(3, c2)].into_iter().collect(),
            appended: vec![title.clone()],
        };
        let mut sink = MemorySink::default();
        let appended = stored.write_derived(&derivation, &mut sink).unwrap();
        assert_eq!(appended, [3]);
        assert_eq!(
            String::from_utf8(sink.0["session.jsonl"].clone()).unwrap(),
            format!("{{\"type\":\"a\"}}\n{{\"type\":\"c2\"}}\n{title}\n")
        );
        assert_eq!(sink.0["other.txt"], b"as stored");
    }

    /// A torn line in a stored session is copied byte for byte by a
    /// derived write that leaves it alone, and `entry_values` yields
    /// it as an invalid entry at its line.
    #[test]
    fn write_derived_copies_a_torn_line_byte_for_byte() {
        let driver = ClaudeDriver::new();
        let text =
            "{\"type\":\"a\"}\n{\"type\":\"b\",\"x\":\"cu{\"type\":\"c\"}\n{\"type\":\"d\"}\n";
        let mut stored = driver
            .read_stored("s1".into(), CONTENT_FORMAT, Box::new(Transcript(text)))
            .unwrap();
        let types: Vec<(u32, String)> = stored
            .entry_values()
            .map(|r| r.unwrap())
            .map(|e| {
                (
                    e.line,
                    e.value.get("type").unwrap().as_str().unwrap().to_string(),
                )
            })
            .collect();
        assert_eq!(
            types,
            [
                (1, "a".into()),
                (2, "$gage-invalid-entry".into()),
                (3, "d".into())
            ]
        );
        let mut sink = MemorySink::default();
        stored
            .write_derived(&Derivation::default(), &mut sink)
            .unwrap();
        assert_eq!(
            String::from_utf8(sink.0["session.jsonl"].clone()).unwrap(),
            text
        );
    }

    const USER_LINE: &str =
        "{\"type\":\"user\",\"uuid\":\"u1\",\"message\":{\"role\":\"user\",\"content\":\"hi\"}}\n";

    /// The attrs reported with a write describe the bytes written: a
    /// transcript that grew after the session was opened is stored as
    /// it is at the write, and the line count and size say so.
    #[test]
    fn write_native_derives_attrs_from_the_bytes_it_writes() {
        use std::io::Write;
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let id = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let path = seed_session(root, "/Users/alice/Code/gage", id, USER_LINE);
        let source = ClaudeDriver::new().open_source(&options_for(root)).unwrap();
        let mut session = source.open_native(id).unwrap();
        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(USER_LINE.as_bytes())
            .unwrap();

        let mut sink = MemorySink::default();
        let written = ClaudeDriver::new()
            .write_native(session.as_mut(), &mut sink)
            .unwrap();
        assert_eq!(written.content_format, CONTENT_FORMAT);
        assert_eq!(written.summary_version, SUMMARY_VERSION);
        assert_eq!(sink.0["session.jsonl"], fs::read(&path).unwrap());
        let attrs = (written.attrs)().unwrap();
        assert_eq!(attrs.line_count(), Some(2));
        assert_eq!(attrs.native_size(), (USER_LINE.len() * 2) as u64);
        assert_eq!(attrs.project_name(), Some("-Users-alice-Code-gage"));
    }

    /// A line the codec does not recognize fails the derive, naming
    /// the line, so the store writes no such session; a torn line is
    /// admitted as the codec admits it.
    #[test]
    fn write_native_refuses_a_line_the_codec_does_not_recognize() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let torn = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeee1";
        seed_session(
            root,
            "/Users/alice/Code/gage",
            torn,
            "{\"type\":\"a\"}\n{\"type\":\"b\",\"x\":\"cu{\"type\":\"c\"}\n",
        );
        let prose = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeee2";
        seed_session(
            root,
            "/Users/alice/Code/gage",
            prose,
            "{\"type\":\"a\"}\n\nhello\n",
        );
        let source = ClaudeDriver::new().open_source(&options_for(root)).unwrap();

        let mut session = source.open_native(torn).unwrap();
        let mut sink = MemorySink::default();
        let written = ClaudeDriver::new()
            .write_native(session.as_mut(), &mut sink)
            .unwrap();
        assert_eq!((written.attrs)().unwrap().line_count(), Some(2));

        let mut session = source.open_native(prose).unwrap();
        let mut sink = MemorySink::default();
        let written = ClaudeDriver::new()
            .write_native(session.as_mut(), &mut sink)
            .unwrap();
        let err = match (written.attrs)() {
            Err(e) => e.to_string(),
            Ok(attrs) => panic!(
                "a line the codec does not recognize derived: {:?}",
                attrs.line_count()
            ),
        };
        assert!(err.contains(&format!("{prose}.jsonl: line 3: ")), "{err}");
    }

    /// Opening a native session reads nothing: the stat facts answer
    /// from the walk, and a transcript that cannot be read still opens
    /// and reports the read error in place of its derived values.
    #[cfg(unix)]
    #[test]
    fn opening_a_native_session_does_not_read_the_transcript() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let id = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let path = seed_session(root, "/Users/alice/Code/gage", id, USER_LINE);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
        let source = ClaudeDriver::new().open_source(&options_for(root)).unwrap();
        let session = source.open_native(id).unwrap();
        assert_eq!(session.attrs().native_size(), USER_LINE.len() as u64);
        assert_eq!(session.attrs().title(), None);
        assert!(
            session.attrs().extra_attrs().contains_key("read_error"),
            "{:?}",
            session.attrs().extra_attrs()
        );
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    }

    #[test]
    fn native_order_compares_line_counts() {
        let lines = |line_count| SummaryAttrs {
            line_count,
            ..Default::default()
        };
        let order =
            |native, stored| ClaudeDriver::new().native_order(&lines(native), &lines(stored));
        assert_eq!(order(Some(3), Some(2)), Ordering::Greater);
        assert_eq!(order(Some(2), Some(3)), Ordering::Less);
        assert_eq!(order(Some(2), Some(2)), Ordering::Equal);
        assert_eq!(order(Some(1), None), Ordering::Greater);
        assert_eq!(order(None, Some(1)), Ordering::Greater);
    }

    #[test]
    fn read_stored_refuses_a_content_format_this_build_does_not_write() {
        let driver = ClaudeDriver::new();
        let text = match driver.read_stored("s1".into(), "claude-jsonl 2", Box::new(NoContent)) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("a format this build does not write was accepted"),
        };
        assert!(text.contains("claude-jsonl 2"), "{text}");
        assert!(text.contains("upgrade gage"), "{text}");
        let accepted = driver.read_stored("s1".into(), CONTENT_FORMAT, Box::new(NoContent));
        assert_eq!(
            accepted.map(|s| s.content_format().to_string()).unwrap(),
            CONTENT_FORMAT
        );
    }

    fn write_registry_entry(root: &Path, file: &str, json: &str) {
        let dir = root.join("sessions");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(file), json).unwrap();
    }

    #[test]
    fn session_live_is_false_when_no_registry_dir() {
        let tmp = TempDir::new().unwrap();
        assert!(!session_live(tmp.path(), "whatever").unwrap());
    }

    #[test]
    fn session_live_is_false_when_the_session_is_absent() {
        let tmp = TempDir::new().unwrap();
        write_registry_entry(
            tmp.path(),
            "100.json",
            r#"{"sessionId":"other","pid":100,"pidDomain":"linux:m:pid:[1]"}"#,
        );
        assert!(!session_live(tmp.path(), "target").unwrap());
    }

    #[test]
    fn session_live_is_unknown_for_a_foreign_pid_domain() {
        let tmp = TempDir::new().unwrap();
        write_registry_entry(
            tmp.path(),
            "100.json",
            r#"{"sessionId":"target","pid":100,"pidDomain":"linux:another-machine:pid:[999]"}"#,
        );
        let err = session_live(tmp.path(), "target").unwrap_err();
        assert!(matches!(err, SessionLiveError::Unknown(_)), "{err}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn session_live_is_true_for_a_running_process_in_this_domain() {
        let tmp = TempDir::new().unwrap();
        let domain = local_pid_domain().expect("a Linux host has a pidDomain");
        let pid = std::process::id();
        let json =
            format!(r#"{{"sessionId":"target","pid":{pid},"pidDomain":"{domain}","updatedAt":1}}"#);
        write_registry_entry(tmp.path(), &format!("{pid}.json"), &json);
        assert!(session_live(tmp.path(), "target").unwrap());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn session_live_skips_an_unreadable_entry_beside_the_target() {
        let tmp = TempDir::new().unwrap();
        let domain = local_pid_domain().expect("a Linux host has a pidDomain");
        let pid = std::process::id();
        write_registry_entry(tmp.path(), "99.json", "{not json");
        let json =
            format!(r#"{{"sessionId":"target","pid":{pid},"pidDomain":"{domain}","updatedAt":1}}"#);
        write_registry_entry(tmp.path(), &format!("{pid}.json"), &json);
        assert!(session_live(tmp.path(), "target").unwrap());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn session_live_is_false_for_a_dead_process_in_this_domain() {
        let tmp = TempDir::new().unwrap();
        let domain = local_pid_domain().expect("a Linux host has a pidDomain");
        // Spawn and reap a child, so its pid names a process that has
        // exited by the time the probe runs.
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        let json =
            format!(r#"{{"sessionId":"target","pid":{pid},"pidDomain":"{domain}","updatedAt":1}}"#);
        write_registry_entry(tmp.path(), &format!("{pid}.json"), &json);
        assert!(!session_live(tmp.path(), "target").unwrap());
    }
}
