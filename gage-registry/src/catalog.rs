//! The scanner catalog and the installed scanners.
//!
//! Scanners are published in one Git repository, the source, as
//! `<namespace>/<name>/scanner.rn`. Gage keeps a copy of the source's
//! current tree, without history, under the cache root. Installing a
//! scanner copies its directory from that copy to
//! `<gage_home>/lib/scanners/<namespace>/<name>`. The installed
//! directory is the whole record of an install: the namespace is in
//! its path and the version is in its `SCANNER` declaration.
//!
//! The functions here decide and act; they print nothing. A command
//! reads a plan, shows it, and applies its steps.

use std::collections::HashSet;
use std::fmt;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use gage_core::config::default_cache_root;
use gage_core::fs::remove_checked_dir_all;
use semver::Version;

use crate::scanner::{
    ApiMismatch, ScannerDef, check_api, installed_scanner_files, parse_scanner_file, scanners_dir,
    split_qualified,
};

/// The namespace of the scanners Gage itself publishes.
pub const GAGE_NAMESPACE: &str = "gage";

/// Where the copy of the source is kept.
pub fn source_cache_dir() -> PathBuf {
    default_cache_root().join("scanners")
}

#[derive(Debug)]
pub enum CatalogError {
    Io(PathBuf, io::Error),
    /// `git` could not be started
    GitMissing(io::Error),
    /// A `git` command failed; `message` is its stderr
    Git {
        command: String,
        message: String,
    },
    /// No scanner of the set has this name
    Unknown(String),
    /// A bare name that more than one namespace holds
    Ambiguous {
        name: String,
        candidates: Vec<String>,
    },
    /// A scanner of another namespace with the same bare name is
    /// installed, or is installed by the same plan
    NameTaken {
        scanner: String,
        holder: String,
    },
    Api {
        scanner: String,
        mismatch: ApiMismatch,
    },
    /// A scanner names a requirement the source does not hold
    MissingRequirement {
        scanner: String,
        requirement: String,
    },
    /// Installed scanners require the one to uninstall
    RequiredBy {
        scanner: String,
        dependents: Vec<String>,
    },
}

impl fmt::Display for CatalogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CatalogError::Io(path, e) => write!(f, "{}: {e}", path.display()),
            CatalogError::GitMissing(e) => write!(f, "git could not be run: {e}"),
            CatalogError::Git { command, message } => write!(f, "git {command}: {message}"),
            CatalogError::Unknown(name) => write!(f, "Unknown scanner: {name}"),
            CatalogError::Ambiguous { name, candidates } => write!(
                f,
                "Scanner name '{name}' is ambiguous; use one of {}",
                candidates.join(", ")
            ),
            CatalogError::NameTaken { scanner, holder } => write!(
                f,
                "Cannot install {scanner}: {holder} has the same scanner name"
            ),
            CatalogError::Api { scanner, mismatch } => {
                write!(f, "Cannot install {scanner}: {mismatch}")
            }
            CatalogError::MissingRequirement {
                scanner,
                requirement,
            } => write!(
                f,
                "Cannot install {scanner}: it requires {requirement}, which is not available"
            ),
            CatalogError::RequiredBy {
                scanner,
                dependents,
            } => write!(
                f,
                "Cannot uninstall {scanner}: required by {}",
                dependents.join(", ")
            ),
        }
    }
}

impl std::error::Error for CatalogError {}

/// The lock every command that reads the source or changes the
/// installed scanners holds while it works. It is released on drop.
pub struct Lock {
    _file: File,
}

/// Take the lock beside the installed root, waiting for another
/// process that holds it.
pub fn lock() -> Result<Lock, CatalogError> {
    lock_in(&scanners_dir())
}

/// [`lock`] for the installed root `root`.
pub fn lock_in(root: &Path) -> Result<Lock, CatalogError> {
    let lib = root.parent().expect("installed root should have a parent");
    fs::create_dir_all(lib).map_err(|e| CatalogError::Io(lib.to_path_buf(), e))?;
    let path = lib.join("scanners.lock");
    let file = File::create(&path).map_err(|e| CatalogError::Io(path.clone(), e))?;
    fs4::FileExt::lock(&file).map_err(|e| CatalogError::Io(path, e))?;
    Ok(Lock { _file: file })
}

/// One scanner of the source or of the installed root.
pub struct Entry {
    pub namespace: String,
    pub def: ScannerDef,
    /// The scanner's directory
    pub dir: PathBuf,
}

impl Entry {
    /// `<namespace>/<name>`
    pub fn qualified(&self) -> String {
        format!("{}/{}", self.namespace, self.def.name)
    }

    /// Whether the scanner appears in a listing.
    pub fn visible(&self) -> bool {
        !self.def.hidden
    }
}

/// The scanners under one root: the copy of the source, or the
/// installed root.
pub struct ScannerSet {
    entries: Vec<Entry>,
    /// The scanner files left out, each with the reason
    pub problems: Vec<(PathBuf, String)>,
}

impl ScannerSet {
    /// Read the installed scanners.
    pub fn installed() -> Self {
        Self::read_installed(&scanners_dir())
    }

    /// Read the installed scanners under `root`. A file that does not
    /// parse is left out.
    pub fn read_installed(root: &Path) -> Self {
        Self::read(root, false)
    }

    /// Read the scanners of a copy of the source at `root`. A scanner
    /// is left out unless it declares a semantic version and an API
    /// level, has no defect in its declaration, and sits in a
    /// directory of its own name.
    pub fn read_source(root: &Path) -> Self {
        Self::read(root, true)
    }

    fn read(root: &Path, published: bool) -> Self {
        let mut entries = Vec::new();
        let mut problems = Vec::new();
        for file in installed_scanner_files(root) {
            let dir = file
                .parent()
                .expect("a scanner file has a directory")
                .to_path_buf();
            let namespace = dir
                .parent()
                .and_then(Path::file_name)
                .expect("a scanner directory is under a namespace")
                .to_string_lossy()
                .to_string();
            let def = match parse_scanner_file(&file) {
                Ok(def) => def,
                Err(e) => {
                    problems.push((file, e.to_string()));
                    continue;
                }
            };
            if published && let Err(reason) = check_published(&def, &dir) {
                problems.push((file, reason));
                continue;
            }
            entries.push(Entry {
                namespace,
                def,
                dir,
            });
        }
        ScannerSet { entries, problems }
    }

    pub fn iter(&self) -> impl Iterator<Item = &Entry> {
        self.entries.iter()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The scanner with the qualified name `<namespace>/<name>`.
    pub fn get(&self, qualified: &str) -> Option<&Entry> {
        let (namespace, name) = split_qualified(qualified)?;
        self.entries
            .iter()
            .find(|e| e.namespace == namespace && e.def.name == name)
    }

    /// The scanner a user names: a qualified name, or a bare name
    /// that one namespace holds.
    pub fn resolve(&self, name: &str) -> Result<&Entry, CatalogError> {
        if name.contains('/') {
            return self
                .get(name)
                .ok_or_else(|| CatalogError::Unknown(name.to_string()));
        }
        let found: Vec<&Entry> = self.entries.iter().filter(|e| e.def.name == name).collect();
        match found.as_slice() {
            [] => Err(CatalogError::Unknown(name.to_string())),
            [entry] => Ok(entry),
            many => Err(CatalogError::Ambiguous {
                name: name.to_string(),
                candidates: many.iter().map(|e| e.qualified()).collect(),
            }),
        }
    }
}

/// Why a scanner of the source cannot be offered, when it cannot.
fn check_published(def: &ScannerDef, dir: &Path) -> Result<(), String> {
    if !def.problems.is_empty() {
        return Err(def.render_problems());
    }
    if dir.file_name().is_none_or(|name| name != def.name.as_str()) {
        return Err(format!(
            "scanner '{}' is not in a directory of its own name",
            def.name
        ));
    }
    if Version::parse(&def.version).is_err() {
        return Err(format!(
            "scanner '{}' declares no semantic version",
            def.name
        ));
    }
    if def.api.is_none() {
        return Err(format!("scanner '{}' declares no api", def.name));
    }
    Ok(())
}

/// Bring the copy of `source` up to date and read its scanners.
/// `source` is a Git URL or the path of a local repository.
pub fn fetch_catalog(source: &str) -> Result<ScannerSet, CatalogError> {
    fetch_catalog_in(source, &source_cache_dir())
}

/// [`fetch_catalog`] with the copy kept at `cache`.
pub fn fetch_catalog_in(source: &str, cache: &Path) -> Result<ScannerSet, CatalogError> {
    refresh(source, cache)?;
    Ok(ScannerSet::read_source(cache))
}

/// Make `cache` hold the tree of the source's default branch and
/// nothing else. The fetch names the source each time, so a changed
/// source replaces the tree without a second code path.
fn refresh(source: &str, cache: &Path) -> Result<(), CatalogError> {
    fs::create_dir_all(cache).map_err(|e| CatalogError::Io(cache.to_path_buf(), e))?;
    if !cache.join(".git").exists() {
        git(cache, &["init", "--quiet"])?;
    }
    let source = local_source(source);
    git(
        cache,
        &[
            "fetch",
            "--quiet",
            "--depth",
            "1",
            "--no-tags",
            &source,
            "HEAD",
        ],
    )?;
    git(
        cache,
        &["checkout", "--quiet", "--force", "--detach", "FETCH_HEAD"],
    )?;
    git(cache, &["clean", "--quiet", "-fdx"])?;
    Ok(())
}

/// `source` as `git fetch` run in the cache directory reads it: a
/// local path is made absolute, with a leading `~/` as the home
/// directory, and a URL is returned as given.
fn local_source(source: &str) -> String {
    let path = match (source.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => PathBuf::from(source),
    };
    match path.canonicalize() {
        Ok(abs) => abs.to_string_lossy().to_string(),
        // Not a path that exists, so a URL
        Err(_) => source.to_string(),
    }
}

/// Run git in `dir`. The variables that would point git at another
/// repository are cleared, and git never prompts: a source that asks
/// for credentials fails with git's message.
fn git(dir: &Path, args: &[&str]) -> Result<(), CatalogError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(CatalogError::GitMissing)?;
    if !output.status.success() {
        return Err(CatalogError::Git {
            command: args.first().copied().unwrap_or_default().to_string(),
            message: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    Ok(())
}

/// One scanner a plan installs.
pub struct Step<'a> {
    pub entry: &'a Entry,
    /// The installed version the step replaces
    pub replaces: Option<String>,
    /// The scanner whose `requires` brought this one into the plan
    pub required_by: Option<String>,
}

/// What an install does.
pub struct InstallPlan<'a> {
    /// The scanners to install, each after the scanners it requires
    pub steps: Vec<Step<'a>>,
    /// The named scanners already installed at the source's version
    pub current: Vec<String>,
}

/// Plan the install of the scanners `names` from `catalog`, with every
/// scanner they require that is not installed. A named scanner
/// installed at another version is replaced.
pub fn plan_install<'a>(
    catalog: &'a ScannerSet,
    installed: &ScannerSet,
    names: &[String],
) -> Result<InstallPlan<'a>, CatalogError> {
    let mut planner = Planner::new(catalog, installed);
    let mut current = Vec::new();
    for name in names {
        let entry = catalog.resolve(name)?;
        let same_version = installed
            .get(&entry.qualified())
            .is_some_and(|i| i.def.version == entry.def.version);
        if same_version {
            current.push(entry.qualified());
            continue;
        }
        planner.add(entry, None)?;
    }
    Ok(InstallPlan {
        steps: planner.steps,
        current,
    })
}

/// What an upgrade does.
pub struct UpgradePlan<'a> {
    pub steps: Vec<Step<'a>>,
    /// The scanners with a newer version that this build cannot run
    pub held: Vec<(String, ApiMismatch)>,
}

/// Plan the upgrade of the installed scanners `names`, or of every
/// installed scanner when `names` is empty. A scanner is upgraded when
/// the source holds a newer version of it.
pub fn plan_upgrade<'a>(
    catalog: &'a ScannerSet,
    installed: &ScannerSet,
    names: &[String],
) -> Result<UpgradePlan<'a>, CatalogError> {
    let targets: Vec<&Entry> = if names.is_empty() {
        installed.iter().collect()
    } else {
        names
            .iter()
            .map(|name| installed.resolve(name))
            .collect::<Result<_, _>>()?
    };
    let mut planner = Planner::new(catalog, installed);
    let mut held = Vec::new();
    for target in targets {
        let Some(entry) = catalog.get(&target.qualified()) else {
            continue;
        };
        if !is_newer(&entry.def.version, &target.def.version) {
            continue;
        }
        if let Some(Err(mismatch)) = entry.def.api.map(check_api) {
            held.push((entry.qualified(), mismatch));
            continue;
        }
        planner.add(entry, None)?;
    }
    Ok(UpgradePlan {
        steps: planner.steps,
        held,
    })
}

/// Whether the version `candidate` replaces the installed version
/// `installed`: greater when both are semantic versions, and different
/// otherwise.
pub fn is_newer(candidate: &str, installed: &str) -> bool {
    match (Version::parse(candidate), Version::parse(installed)) {
        (Ok(candidate), Ok(installed)) => candidate > installed,
        _ => candidate != installed,
    }
}

struct Planner<'a, 'i> {
    catalog: &'a ScannerSet,
    installed: &'i ScannerSet,
    steps: Vec<Step<'a>>,
    seen: HashSet<String>,
}

impl<'a, 'i> Planner<'a, 'i> {
    fn new(catalog: &'a ScannerSet, installed: &'i ScannerSet) -> Self {
        Planner {
            catalog,
            installed,
            steps: Vec::new(),
            seen: HashSet::new(),
        }
    }

    /// Add `entry` after the scanners it requires. A requirement that
    /// is installed is left as it is.
    fn add(&mut self, entry: &'a Entry, required_by: Option<String>) -> Result<(), CatalogError> {
        let qualified = entry.qualified();
        if !self.seen.insert(qualified.clone()) {
            return Ok(());
        }
        if let Some(Err(mismatch)) = entry.def.api.map(check_api) {
            return Err(CatalogError::Api {
                scanner: qualified,
                mismatch,
            });
        }
        self.check_name_free(entry)?;
        for requirement in &entry.def.requires {
            if self.installed.get(requirement).is_some() {
                continue;
            }
            let required =
                self.catalog
                    .get(requirement)
                    .ok_or_else(|| CatalogError::MissingRequirement {
                        scanner: qualified.clone(),
                        requirement: requirement.clone(),
                    })?;
            self.add(required, Some(qualified.clone()))?;
        }
        self.steps.push(Step {
            entry,
            replaces: self
                .installed
                .get(&qualified)
                .map(|i| i.def.version.clone()),
            required_by,
        });
        Ok(())
    }

    /// A scan names a scanner by its bare name, so two namespaces
    /// cannot both install one.
    fn check_name_free(&self, entry: &Entry) -> Result<(), CatalogError> {
        let other_namespace =
            |e: &&Entry| e.def.name == entry.def.name && e.namespace != entry.namespace;
        let holder = self
            .installed
            .iter()
            .find(other_namespace)
            .or_else(|| self.steps.iter().map(|s| s.entry).find(other_namespace));
        match holder {
            Some(holder) => Err(CatalogError::NameTaken {
                scanner: entry.qualified(),
                holder: holder.qualified(),
            }),
            None => Ok(()),
        }
    }
}

/// Plan the uninstall of the installed scanners `names`. A scanner
/// that an installed scanner outside `names` requires is an error.
pub fn plan_uninstall<'a>(
    installed: &'a ScannerSet,
    names: &[String],
) -> Result<Vec<&'a Entry>, CatalogError> {
    let mut targets: Vec<&Entry> = Vec::new();
    for name in names {
        let entry = installed.resolve(name)?;
        if !targets.iter().any(|t| t.dir == entry.dir) {
            targets.push(entry);
        }
    }
    let removed: HashSet<String> = targets.iter().map(|t| t.qualified()).collect();
    for target in &targets {
        let qualified = target.qualified();
        let dependents: Vec<String> = installed
            .iter()
            .filter(|e| !removed.contains(&e.qualified()))
            .filter(|e| e.def.requires.contains(&qualified))
            .map(Entry::qualified)
            .collect();
        if !dependents.is_empty() {
            return Err(CatalogError::RequiredBy {
                scanner: qualified,
                dependents,
            });
        }
    }
    Ok(targets)
}

/// Install `entry` under the installed root.
pub fn install(entry: &Entry) -> Result<(), CatalogError> {
    install_in(entry, &scanners_dir())
}

/// Copy `entry` to `<root>/<namespace>/<name>`, replacing what is
/// there. The copy is made beside the target and renamed over it, so a
/// failed copy leaves the installed scanner as it was.
pub fn install_in(entry: &Entry, root: &Path) -> Result<(), CatalogError> {
    let namespace_dir = root.join(&entry.namespace);
    let target = namespace_dir.join(&entry.def.name);
    // The registry skips a directory whose name starts with a dot
    let staging = namespace_dir.join(format!(".{}.installing", entry.def.name));
    remove_checked_dir_all(&staging).map_err(|e| CatalogError::Io(staging.clone(), e))?;
    copy_dir(&entry.dir, &staging)?;
    remove_checked_dir_all(&target).map_err(|e| CatalogError::Io(target.clone(), e))?;
    fs::rename(&staging, &target).map_err(|e| CatalogError::Io(target, e))
}

/// Copy the files and directories of `from` to the new directory
/// `to`. A symbolic link is not copied: a link in a fetched tree could
/// name a file outside it.
fn copy_dir(from: &Path, to: &Path) -> Result<(), CatalogError> {
    let io_error = |path: &Path| {
        let path = path.to_path_buf();
        move |e| CatalogError::Io(path, e)
    };
    fs::create_dir_all(to).map_err(io_error(to))?;
    for entry in fs::read_dir(from).map_err(io_error(from))? {
        let entry = entry.map_err(io_error(from))?;
        let source = entry.path();
        let dest = to.join(entry.file_name());
        let kind = entry.file_type().map_err(io_error(&source))?;
        if kind.is_dir() {
            copy_dir(&source, &dest)?;
        } else if kind.is_file() {
            fs::copy(&source, &dest).map_err(io_error(&source))?;
        }
    }
    Ok(())
}

/// Install every scanner of `namespace` that `catalog` holds and this
/// build can run, with the scanners they require. Returns the
/// qualified names installed, and the scanners left out with the
/// reason.
pub fn install_namespace(
    catalog: &ScannerSet,
    namespace: &str,
) -> Result<NamespaceInstall, CatalogError> {
    install_namespace_in(catalog, namespace, &scanners_dir())
}

/// What [`install_namespace`] did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct NamespaceInstall {
    pub installed: Vec<String>,
    /// The scanners this build cannot run
    pub held: Vec<(String, ApiMismatch)>,
}

/// [`install_namespace`] for the installed root `root`.
pub fn install_namespace_in(
    catalog: &ScannerSet,
    namespace: &str,
    root: &Path,
) -> Result<NamespaceInstall, CatalogError> {
    let installed = ScannerSet::read_installed(root);
    let mut done = NamespaceInstall::default();
    let mut names = Vec::new();
    for entry in catalog.iter().filter(|e| e.namespace == namespace) {
        match entry.def.api.map(check_api) {
            Some(Err(mismatch)) => done.held.push((entry.qualified(), mismatch)),
            _ => names.push(entry.qualified()),
        }
    }
    let plan = plan_install(catalog, &installed, &names)?;
    for step in &plan.steps {
        install_in(step.entry, root)?;
        done.installed.push(step.entry.qualified());
    }
    Ok(done)
}

/// Remove the installed scanner `entry`, and its namespace directory
/// when that leaves it empty.
pub fn uninstall(entry: &Entry) -> Result<(), CatalogError> {
    remove_checked_dir_all(&entry.dir).map_err(|e| CatalogError::Io(entry.dir.clone(), e))?;
    let namespace_dir = entry
        .dir
        .parent()
        .expect("an installed scanner is under a namespace");
    let empty = fs::read_dir(namespace_dir)
        .map_err(|e| CatalogError::Io(namespace_dir.to_path_buf(), e))?
        .next()
        .is_none();
    if empty {
        fs::remove_dir(namespace_dir)
            .map_err(|e| CatalogError::Io(namespace_dir.to_path_buf(), e))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The scanners every test source holds, copied from the test
    /// data: `test/parent`, which requires `test/child`.
    fn testdata() -> PathBuf {
        [env!("CARGO_MANIFEST_DIR"), "testdata", "scanners"]
            .iter()
            .collect()
    }

    fn write_scanner(root: &Path, qualified: &str, fields: &str) {
        let (_, name) = split_qualified(qualified).unwrap();
        let dir = root.join(qualified);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("scanner.rn"),
            format!(
                "pub const SCANNER = #{{ name: {name:?}, description: \"d\", {fields} tasks: #{{}} }};"
            ),
        )
        .unwrap();
    }

    fn run_git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    /// Commit everything in `dir`, a repository or a directory that
    /// becomes one.
    fn commit(dir: &Path) {
        if !dir.join(".git").exists() {
            run_git(dir, &["init", "--quiet"]);
        }
        run_git(dir, &["add", "--all"]);
        run_git(dir, &["commit", "--quiet", "--message", "scanners"]);
    }

    /// A source repository holding the test data scanners.
    fn source_repo(tmp: &Path) -> PathBuf {
        let repo = tmp.join("source");
        copy_dir(&testdata(), &repo).unwrap();
        commit(&repo);
        repo
    }

    fn names<'a>(entries: impl Iterator<Item = &'a Entry>) -> Vec<String> {
        let mut names: Vec<String> = entries.map(Entry::qualified).collect();
        names.sort();
        names
    }

    #[test]
    fn a_fetch_reads_the_source_and_a_later_fetch_follows_it() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = source_repo(tmp.path());
        let cache = tmp.path().join("cache");
        let source = repo.to_string_lossy().to_string();

        let catalog = fetch_catalog_in(&source, &cache).unwrap();
        assert_eq!(names(catalog.iter()), ["test/child", "test/parent"]);
        assert_eq!(catalog.get("test/parent").unwrap().def.version, "1.0.0");
        assert!(catalog.problems.is_empty(), "{:?}", catalog.problems);

        // The source gains a scanner and loses one
        write_scanner(&repo, "extra/added", "version: \"0.1.0\", api: 1,");
        fs::remove_dir_all(repo.join("test/parent")).unwrap();
        commit(&repo);
        let catalog = fetch_catalog_in(&source, &cache).unwrap();
        assert_eq!(names(catalog.iter()), ["extra/added", "test/child"]);
    }

    #[test]
    fn a_source_scanner_without_a_version_or_an_api_is_left_out() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write_scanner(root, "ns/good", "version: \"1.0.0\", api: 1,");
        write_scanner(root, "ns/no-version", "api: 1,");
        write_scanner(root, "ns/bad-version", "version: \"one\", api: 1,");
        write_scanner(root, "ns/no-api", "version: \"1.0.0\",");
        fs::rename(root.join("ns/good"), root.join("ns/moved")).unwrap();
        write_scanner(root, "ns/good", "version: \"1.0.0\", api: 1,");

        let catalog = ScannerSet::read_source(root);
        assert_eq!(names(catalog.iter()), ["ns/good"]);
        let reasons: String = catalog
            .problems
            .iter()
            .map(|(_, reason)| format!("{reason}\n"))
            .collect();
        assert!(
            reasons.contains("'no-version' declares no semantic version"),
            "{reasons}"
        );
        assert!(
            reasons.contains("'bad-version' declares no semantic version"),
            "{reasons}"
        );
        assert!(reasons.contains("'no-api' declares no api"), "{reasons}");
        assert!(
            reasons.contains("'good' is not in a directory of its own name"),
            "{reasons}"
        );

        // The same files are all read as installed scanners
        assert_eq!(ScannerSet::read_installed(root).iter().count(), 5);
    }

    #[test]
    fn a_bare_name_resolves_when_one_namespace_holds_it() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write_scanner(root, "a/one", "");
        write_scanner(root, "a/both", "");
        write_scanner(root, "b/both", "");
        let set = ScannerSet::read_installed(root);

        assert_eq!(set.resolve("one").unwrap().qualified(), "a/one");
        assert_eq!(set.resolve("b/both").unwrap().qualified(), "b/both");
        assert!(matches!(
            set.resolve("both"),
            Err(CatalogError::Ambiguous { candidates, .. }) if candidates == ["a/both", "b/both"]
        ));
        assert!(matches!(set.resolve("none"), Err(CatalogError::Unknown(_))));
        assert!(matches!(
            set.resolve("a/none"),
            Err(CatalogError::Unknown(_))
        ));
    }

    #[test]
    fn installing_a_scanner_installs_what_it_requires_first() {
        let tmp = tempfile::tempdir().unwrap();
        let catalog = ScannerSet::read_source(&testdata());
        let root = tmp.path().join("lib/scanners");
        let installed = ScannerSet::read_installed(&root);

        let plan = plan_install(&catalog, &installed, &["parent".to_string()]).unwrap();
        let order: Vec<String> = plan.steps.iter().map(|s| s.entry.qualified()).collect();
        assert_eq!(order, ["test/child", "test/parent"]);
        assert_eq!(plan.steps[0].required_by.as_deref(), Some("test/parent"));
        assert_eq!(plan.steps[1].required_by, None);
        for step in &plan.steps {
            install_in(step.entry, &root).unwrap();
        }

        let installed = ScannerSet::read_installed(&root);
        assert_eq!(names(installed.iter()), ["test/child", "test/parent"]);
        assert!(root.join("test/parent/scanner.rn").is_file());

        // Nothing is left to do, and the named scanner is reported
        let plan = plan_install(&catalog, &installed, &["test/parent".to_string()]).unwrap();
        assert!(plan.steps.is_empty());
        assert_eq!(plan.current, ["test/parent"]);
    }

    #[test]
    fn a_required_scanner_cannot_be_uninstalled_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let catalog = ScannerSet::read_source(&testdata());
        let root = tmp.path().join("lib/scanners");
        for entry in catalog.iter() {
            install_in(entry, &root).unwrap();
        }
        let installed = ScannerSet::read_installed(&root);

        assert!(matches!(
            plan_uninstall(&installed, &["child".to_string()]),
            Err(CatalogError::RequiredBy { scanner, dependents })
                if scanner == "test/child" && dependents == ["test/parent"]
        ));

        // Together they go, and the empty namespace goes with them
        let targets =
            plan_uninstall(&installed, &["child".to_string(), "parent".to_string()]).unwrap();
        for target in targets {
            uninstall(target).unwrap();
        }
        assert!(ScannerSet::read_installed(&root).is_empty());
        assert!(!root.join("test").exists());
    }

    #[test]
    fn an_upgrade_takes_newer_versions_and_new_requirements() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("lib/scanners");
        write_scanner(&root, "ns/old", "version: \"1.0.0\", api: 1,");
        write_scanner(&root, "ns/same", "version: \"1.0.0\", api: 1,");
        write_scanner(&root, "ns/gone", "version: \"1.0.0\", api: 1,");
        let source = tmp.path().join("source");
        write_scanner(
            &source,
            "ns/old",
            "version: \"1.1.0\", api: 1, requires: [\"ns/dep\"],",
        );
        write_scanner(&source, "ns/same", "version: \"1.0.0\", api: 1,");
        write_scanner(&source, "ns/dep", "version: \"0.3.0\", api: 1,");
        let catalog = ScannerSet::read_source(&source);
        let installed = ScannerSet::read_installed(&root);

        let plan = plan_upgrade(&catalog, &installed, &[]).unwrap();
        let steps: Vec<(String, Option<String>)> = plan
            .steps
            .iter()
            .map(|s| (s.entry.qualified(), s.replaces.clone()))
            .collect();
        assert_eq!(
            steps,
            [
                ("ns/dep".to_string(), None),
                ("ns/old".to_string(), Some("1.0.0".to_string())),
            ]
        );
        assert!(plan.held.is_empty());

        let plan = plan_upgrade(&catalog, &installed, &["same".to_string()]).unwrap();
        assert!(plan.steps.is_empty());
        assert!(matches!(
            plan_upgrade(&catalog, &installed, &["absent".to_string()]),
            Err(CatalogError::Unknown(_))
        ));
    }

    #[test]
    fn a_scanner_for_a_newer_api_is_refused_and_held() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("lib/scanners");
        write_scanner(&root, "ns/ahead", "version: \"1.0.0\", api: 1,");
        let source = tmp.path().join("source");
        let newer = crate::scanner::API_LEVEL + 1;
        write_scanner(
            &source,
            "ns/ahead",
            &format!("version: \"2.0.0\", api: {newer},"),
        );
        let catalog = ScannerSet::read_source(&source);

        let none = ScannerSet::read_installed(&tmp.path().join("none"));
        assert!(matches!(
            plan_install(&catalog, &none, &["ahead".to_string()]),
            Err(CatalogError::Api {
                mismatch: ApiMismatch::NeedsNewerGage { level },
                ..
            }) if level == newer
        ));

        let installed = ScannerSet::read_installed(&root);
        let plan = plan_upgrade(&catalog, &installed, &[]).unwrap();
        assert!(plan.steps.is_empty());
        assert_eq!(plan.held.len(), 1);
        assert_eq!(plan.held[0].0, "ns/ahead");
    }

    #[test]
    fn one_scanner_name_installs_from_one_namespace() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("lib/scanners");
        write_scanner(&root, "a/same", "version: \"1.0.0\", api: 1,");
        let source = tmp.path().join("source");
        write_scanner(&source, "b/same", "version: \"1.0.0\", api: 1,");
        let catalog = ScannerSet::read_source(&source);
        let installed = ScannerSet::read_installed(&root);

        assert!(matches!(
            plan_install(&catalog, &installed, &["b/same".to_string()]),
            Err(CatalogError::NameTaken { scanner, holder })
                if scanner == "b/same" && holder == "a/same"
        ));
    }

    #[test]
    fn a_namespace_installs_whole_with_what_it_requires() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        let newer = crate::scanner::API_LEVEL + 1;
        write_scanner(&source, "gage/one", "version: \"1.0.0\", api: 1,");
        write_scanner(
            &source,
            "gage/two",
            "version: \"1.0.0\", api: 1, hidden: true, requires: [\"lib/dep\"],",
        );
        write_scanner(
            &source,
            "gage/ahead",
            &format!("version: \"1.0.0\", api: {newer},"),
        );
        write_scanner(&source, "lib/dep", "version: \"1.0.0\", api: 1,");
        write_scanner(&source, "lib/other", "version: \"1.0.0\", api: 1,");
        let catalog = ScannerSet::read_source(&source);
        let root = tmp.path().join("lib/scanners");

        let done = install_namespace_in(&catalog, "gage", &root).unwrap();
        assert_eq!(done.installed, ["gage/one", "lib/dep", "gage/two"]);
        assert_eq!(
            done.held,
            [(
                "gage/ahead".to_string(),
                ApiMismatch::NeedsNewerGage { level: newer }
            )]
        );
        assert_eq!(
            names(ScannerSet::read_installed(&root).iter()),
            ["gage/one", "gage/two", "lib/dep"]
        );

        // A second run finds nothing to install
        let done = install_namespace_in(&catalog, "gage", &root).unwrap();
        assert!(done.installed.is_empty());
    }

    #[test]
    fn a_version_is_newer_by_semantic_order() {
        assert!(is_newer("1.10.0", "1.9.0"));
        assert!(!is_newer("1.0.0", "1.0.0"));
        assert!(!is_newer("1.0.0", "1.1.0"));
        // An installed scanner without a semantic version is replaced
        assert!(is_newer("1.0.0", ""));
    }
}
