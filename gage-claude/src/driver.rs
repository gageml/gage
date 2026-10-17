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

use std::collections::HashMap;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime};

use gage_core::config::plugin_marketplace_dir;
use gage_session::{
    AgentSession, AgentSpec, AuthInfo, ContentSchema, ContentSink, ContentSource, Driver,
    DriverError, DriverStatus, DriverTables, Entry, Field, HarnessInfo, InstallInfo, InstallReport,
    InstallUi, MaintenanceOutcome, MaintenanceStatus, NativeLookupError, NativeSession,
    ResolveSession, SessionAttrs, SessionLiveError, Source, SourceOptions, StoredSession, ToolUse,
    split_scheme,
};
use serde::Deserialize;

use crate::content_schema;
use crate::home::ClaudeHome;
use crate::index::{
    ClaudeSessionAttrs, IndexStore, LockMode, ReconcileEvent, ToolUseCollector, cache_dir_for,
    derive_entry, derive_session,
};
use crate::model::{configured_model, resolve_model, resolved_model};
use crate::plugin;
use crate::preflight;
use crate::proc::find_claude;
use crate::resolve::resolve_command;
use crate::session::{
    SESSION_RE, SessionInfo, delete_session, encode_project_dir, is_agent_tmp_slug, projects_dir,
};
use crate::session_reader::SessionReader;
use crate::tables::{EntryTable, MessageTable, SessionTable, ToolUseTable};

/// MCP server name under which Gage registers in `.mcp.json`.
const MCP_SERVER_NAME: &str = "gage";
/// Plugin identifier Claude Code uses: `<plugin>@<marketplace>`.
const PLUGIN_ID: &str = "gage@gage";

const NAME: &str = "claude";
/// The harness's display name
const HARNESS_NAME: &str = "Claude Code";
const SCHEMES: &[&str] = &["claude"];
const VERSION: &str = env!("CARGO_PKG_VERSION");
const SESSION_TYPE: &str = "claude";
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

    /// A native session URL is `claude:<root>/<native id>`; its
    /// container is `claude:<root>`. A bare id is a session in the
    /// default location, `claude:`.
    fn open_native_source(&self, native_source: &str) -> Result<Box<dyn Source>, DriverError> {
        let body = match split_scheme(native_source) {
            Some((_, body)) => body,
            None => native_source,
        };
        let root = body.rsplit_once('/').map_or("", |(root, _id)| root);
        self.open_source(&SourceOptions::new(&format!("{NAME}:{root}")))
    }

    /// Drops the `claude-` vendor prefix every Claude model name carries
    fn content_schemas(&self) -> Vec<ContentSchema> {
        content_schema::content_schemas()
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
    ) -> Result<String, DriverError> {
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
        let session_path = claude.session_path().to_path_buf();
        copy_file_into_sink(&session_path, "session.jsonl", sink)?;
        Ok(CONTENT_FORMAT.to_string())
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

    /// Start the interactive `/gage:resolve` session. The plugin must
    /// be installed at this build's version and the user logged in.
    /// `None` for the model is the model Claude Code is configured
    /// with from the current directory, else the driver's default;
    /// a given model has its size alias resolved.
    fn start_resolve_session(
        &self,
        issue_ids: &[String],
        model: Option<&str>,
        extra_args: &[String],
    ) -> Result<Box<dyn ResolveSession>, DriverError> {
        preflight::check().map_err(|e| DriverError::Other(e.to_string()))?;
        let model = match model {
            Some(m) => resolve_model(m).to_string(),
            None => std::env::current_dir()
                .ok()
                .and_then(|cwd| configured_model(&cwd))
                .unwrap_or_else(|| resolved_model(None)),
        };
        let child = resolve_command(issue_ids, &model, extra_args)?
            .spawn()
            .map_err(DriverError::Io)?;
        Ok(Box::new(ClaudeResolveSession(child)))
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

/// The resolve session's `claude` process.
struct ClaudeResolveSession(Child);

impl ResolveSession for ClaudeResolveSession {
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
        let cache_dir = cache_dir_for(&projects_dir);
        let store = Arc::new(
            IndexStore::new(projects_dir.clone(), cache_dir).show_agents(options.shows_agents()),
        );
        store.ensure_cache_dir()?;
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

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn tables(&self) -> Result<DriverTables, DriverError> {
        Ok(DriverTables {
            session: Arc::new(SessionTable::new(Arc::clone(&self.store))),
            message: Arc::new(MessageTable::new(Arc::clone(&self.store))),
            entry: Arc::new(EntryTable::new(Arc::clone(&self.store))),
            tool_use: Arc::new(ToolUseTable::new(Arc::clone(&self.store))),
        })
    }

    fn warm_summaries(
        &self,
        limit: usize,
        on_progress: &mut dyn FnMut(usize, usize),
    ) -> Result<(), DriverError> {
        let newest = self.store.session_list().limit(limit).build();
        let uncached: Vec<&SessionInfo> = newest
            .iter()
            .filter(|s| self.store.session_attrs(&s.id, s.mtime).is_none())
            .collect();
        let total = uncached.len();
        on_progress(0, total);
        for (done, s) in uncached.iter().enumerate() {
            // Opening derives the summary and writes it to the cache.
            // A session that cannot be read is skipped here and
            // reported by the table scan, which shows it with default
            // attributes.
            match ClaudeNativeSession::open(&s.id, &s.src, s.mtime, &self.root, &self.store) {
                Ok(_) => {}
                Err(e) => tracing::warn!(session_id = %s.id, "session summary unavailable: {e}"),
            }
            on_progress(done + 1, total);
        }
        Ok(())
    }

    /// The reconcile pass over the summary cache: every session the
    /// cache does not hold at its current fingerprint is derived, and
    /// the cache of every session gone from disk is removed.
    fn reconcile(
        &self,
        rebuild: bool,
        on_progress: &mut dyn FnMut(usize, usize),
    ) -> Result<MaintenanceOutcome, DriverError> {
        let mut total = 0;
        let mut done = 0;
        let mut on_event = |event: ReconcileEvent| match event {
            ReconcileEvent::Start { total: n } => {
                total = n as usize;
                on_progress(0, total);
            }
            ReconcileEvent::Advance => {
                done += 1;
                on_progress(done, total);
            }
        };
        let outcome = if rebuild {
            self.store.rebuild_with_progress(&mut on_event)
        } else {
            self.store
                .reconcile_with_progress(LockMode::Wait, &mut on_event)
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
            state_dir: Some(status.cache_dir),
            state_bytes: status.cache_bytes,
            last_reconcile_ms: status.last_reconcile_ms,
        })
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
            &self.root,
            &self.store,
        )?))
    }

    fn delete_native(&self, native_id: &str) -> Result<(), DriverError> {
        let (path, _meta) = self.session_file(native_id)?;
        Ok(delete_session(&path)?)
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
        // A half-written or foreign-shaped entry carries no usable
        // session id, so it cannot confirm this session; skip it.
        let Ok(parsed) = serde_json::from_slice::<SessionRegistryEntry>(&bytes) else {
            continue;
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

fn copy_file_into_sink(
    src: &Path,
    dest_key: &str,
    sink: &mut dyn ContentSink,
) -> Result<(), DriverError> {
    let mut writer = sink.create(dest_key).map_err(DriverError::Io)?;
    let mut file = File::open(src).map_err(DriverError::Io)?;
    io::copy(&mut file, &mut writer).map_err(DriverError::Io)?;
    Ok(())
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
    attrs: ClaudeSessionAttrs,
}

impl ClaudeNativeSession {
    /// Open the session at `path`. `mtime` and `size` are the file's
    /// stat values the caller already holds from its directory walk;
    /// `root` is the absolute Claude root the file sits under.
    pub fn open(
        native_id: &str,
        path: &Path,
        mtime: SystemTime,
        root: &Path,
        store: &IndexStore,
    ) -> Result<Self, DriverError> {
        let attrs = match store.session_attrs(native_id, mtime) {
            Some(cached) => cached,
            None => {
                let derived = derive_session(native_id, path)
                    .map_err(|e| DriverError::Other(format!("{}: {e}", path.display())))?;
                store.put_session_attrs(native_id, &derived.attrs)?;
                derived.attrs
            }
        };
        Ok(Self {
            native_id: native_id.to_string(),
            source: format!("{NAME}:{}/{native_id}", root.display()),
            session_path: path.to_path_buf(),
            attrs,
        })
    }

    pub fn session_path(&self) -> &Path {
        &self.session_path
    }

    /// Record `project` as the session's project in place of the slug
    /// of the directory the transcript sits in.
    pub fn set_project(&mut self, project: String) {
        self.attrs.project = project;
    }

    /// The full derived attributes, including the token counts the
    /// generic [`SessionAttrs`] view does not carry.
    pub fn claude_attrs(&self) -> &ClaudeSessionAttrs {
        &self.attrs
    }
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
        &self.attrs
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

impl SessionAttrs for ClaudeSessionAttrs {
    fn native_mtime(&self) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_millis(self.native_mtime_ms.max(0) as u64)
    }

    fn native_size(&self) -> u64 {
        self.native_size
    }

    fn is_empty(&self) -> bool {
        self.is_empty
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
        Some(self.message_count.max(0) as u64)
    }

    fn line_count(&self) -> Option<u64> {
        Some(self.line_count)
    }

    fn input_tokens(&self) -> Option<u64> {
        let total =
            self.input_tokens + self.cache_read_input_tokens + self.cache_creation_input_tokens;
        Some(total.max(0) as u64)
    }

    fn output_tokens(&self) -> Option<u64> {
        Some(self.output_tokens.max(0) as u64)
    }

    fn cached_tokens(&self) -> Option<u64> {
        Some(self.cache_read_input_tokens.max(0) as u64)
    }

    fn cost_usd(&self) -> Option<f64> {
        self.cost_usd
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
    /// the same derivation the native tables use. Blank lines,
    /// unparseable lines, and lines without a type consume a line
    /// number and yield no row, as they do for a native session.
    fn entries(&mut self) -> Box<dyn Iterator<Item = Result<Entry, DriverError>> + '_> {
        let reader = match self.source.open("session.jsonl") {
            Ok(r) => SessionReader::new(r),
            Err(e) => return Box::new(std::iter::once(Err(DriverError::Io(e)))),
        };
        let native_id = self.native_id.clone();
        // Read failures pass through; only a typeless line is dropped
        Box::new(reader.filter_map(move |item| match item {
            Ok((line_num, value)) => derive_entry(&native_id, line_num, &value).map(Ok),
            Err(e) => Some(Err(DriverError::Io(e))),
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
                Ok((line_num, value)) => collector.push(line_num, &value),
                Err(e) => failures.push(Err(DriverError::Io(e))),
            }
        }
        Box::new(collector.finish().into_iter().map(Ok).chain(failures))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn delete_native_removes_session_and_sidecar() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let id = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let path = seed_session(root, "/Users/alice/Code/gage", id, "");
        let sidecar = path.with_extension("");
        fs::create_dir_all(sidecar.join("subagents")).unwrap();

        let source = ClaudeDriver::new()
            .open_source(&SourceOptions::new(&source_for(root)))
            .unwrap();
        source.delete_native(id).unwrap();
        assert!(!path.exists());
        assert!(!sidecar.exists());
        assert!(source.open_native(id).is_err());
    }

    #[test]
    fn delete_native_unknown_id_errors() {
        let tmp = TempDir::new().unwrap();
        let source = ClaudeDriver::new()
            .open_source(&SourceOptions::new(&source_for(tmp.path())))
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
        let source = ClaudeDriver::new()
            .open_source(&SourceOptions::new(&source_for(root)))
            .unwrap();

        let mut reported = Vec::new();
        source
            .warm_summaries(10, &mut |done, total| reported.push((done, total)))
            .unwrap();
        assert_eq!(reported, [(0, 2), (1, 2), (2, 2)]);

        let mut reported = Vec::new();
        source
            .warm_summaries(10, &mut |done, total| reported.push((done, total)))
            .unwrap();
        assert_eq!(reported, [(0, 0)]);
    }

    fn seed_session(root: &Path, cwd: &str, id: &str, contents: &str) -> PathBuf {
        gage_core::config::use_test_cache_root();
        let slug = encode_project_dir(Path::new(cwd));
        let dir = root.join("projects").join(&slug);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{id}.jsonl"));
        fs::write(&path, contents).unwrap();
        path
    }

    fn source_for(root: &Path) -> String {
        gage_core::config::use_test_cache_root();
        format!("{NAME}:{}", root.to_string_lossy())
    }

    #[test]
    fn find_native_disambiguates_prefix() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let a = "aaaaaaaa-1111-1111-1111-111111111111";
        let b = "bbbbbbbb-2222-2222-2222-222222222222";
        seed_session(root, "/Users/alice/x", a, "");
        seed_session(root, "/Users/alice/y", b, "");

        let source = ClaudeDriver::new()
            .open_source(&SourceOptions::new(&source_for(root)))
            .unwrap();
        assert_eq!(source.find_native("aaaa").unwrap(), a);
        let bare = ClaudeDriver::new()
            .open_source(&SourceOptions::new(&root.to_string_lossy()))
            .unwrap();
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

        let hidden = ClaudeDriver::new()
            .open_source(&SourceOptions::new(&source_for(root)))
            .unwrap();
        let err = hidden.find_native("aaaa").unwrap_err();
        assert!(matches!(err, NativeLookupError::NoMatch(_)));

        let mut options = SourceOptions::new(&source_for(root));
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
            .open_source(&SourceOptions::new(&source_for(&root)))
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

        let source = ClaudeDriver::new()
            .open_source(&SourceOptions::new(&source_for(root)))
            .unwrap();
        let name = source.project_name(Path::new(cwd)).unwrap();
        assert_eq!(name, "-Users-alice-Code-gage");
    }

    #[test]
    fn open_native_source_opens_the_container_of_a_session_url() {
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
            .open_source(&SourceOptions::new(&source_for(root)))
            .unwrap()
            .open_native(id)
            .unwrap();
        let source = driver.open_native_source(native.source()).unwrap();
        assert_eq!(source.source(), source_for(root));
        assert_eq!(
            source.project_path(&encode_project_dir(&cwd)).unwrap(),
            Some(cwd)
        );
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

        let source = ClaudeDriver::new()
            .open_source(&SourceOptions::new(&source_for(root)))
            .unwrap();
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
