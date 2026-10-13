//! Remotes: the Git remotes a store pushes to, fetches from, and backs
//! up to, reached through [`RemoteStore`].
//!
//! A remote is a stanza in the store's Git config. `add` writes the
//! fetch refspec `+refs/gage/1/*:refs/remotes/<name>/gage/1/*`, so a
//! `git fetch <name>` tracks the remote's Gage refs and nothing else,
//! and turns tag auto-following off, since Gage tags are not under
//! `refs/tags/`. A backup target carries `gage-backup = true`, which
//! the store's `pre-push` hook reads and which `backup` requires.
//! `delete` removes the stanza and the remote-tracking refs with it.
//!
//! A fetch does not need a remote. A bare Git URL or path is fetched
//! with the refspec given on the command line, as git itself allows,
//! into a transient namespace under `refs/gage-fetch/` that the
//! command deletes before it exits. [`Repository`] names either kind
//! for push, fetch, and backup alike; [`FetchSource`] adds the prefix
//! a fetch's refs land under.

use std::collections::BTreeMap;
use std::process::{ExitStatus, Stdio};

use gage_core::uuid::new_uuid;

use crate::git::{git_in, git_transport_in, run, run_with_stdin};
use crate::refs::{GENERATION, ROOT};
use crate::{Store, StoreError};

/// Typed view over a [`Store`] for remotes.
pub struct RemoteStore<'a> {
    store: &'a Store,
}

impl<'a> From<&'a Store> for RemoteStore<'a> {
    fn from(store: &'a Store) -> Self {
        RemoteStore { store }
    }
}

/// One remote as the store's config records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remote {
    pub name: String,
    pub url: String,
    /// True when the remote may receive the whole store (`gage-backup`)
    pub backup: bool,
}

/// The remote-tracking prefix of `name`: `refs/remotes/<name>/gage/1/`.
pub fn tracking_prefix(name: &str) -> String {
    format!("refs/remotes/{name}/gage/1/")
}

/// A repository the store talks to: a configured remote by name, or
/// a bare Git URL or path with no configuration. Push, fetch, and
/// backup take either.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repository {
    spec: String,
    name: String,
    remote: bool,
}

impl Repository {
    /// The configured remote `name`.
    pub fn from_remote(name: &str) -> Repository {
        Repository {
            spec: name.to_string(),
            name: name.to_string(),
            remote: true,
        }
    }

    /// A Git URL or path with no configuration.
    pub fn from_url(url: &str) -> Repository {
        Repository {
            spec: url.to_string(),
            name: redact_password(url),
            remote: false,
        }
    }

    /// What git is given: the remote name or the URL.
    pub fn spec(&self) -> &str {
        &self.spec
    }

    /// The repository as a report and a merge entry name it: the
    /// remote name, or the URL with any password removed. A merge
    /// entry is object content and travels to every other store.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// True for a configured remote.
    pub fn is_remote(&self) -> bool {
        self.remote
    }
}

/// `scheme://user:password@host/...` with the password removed. Other
/// forms, including scp-style `user@host:path`, carry no password and
/// pass through.
fn redact_password(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    let (authority, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
    let Some((userinfo, host)) = authority.rsplit_once('@') else {
        return url.to_string();
    };
    let user = userinfo.split_once(':').map_or(userinfo, |(user, _)| user);
    format!("{scheme}://{user}@{host}{path}")
}

/// The root of the transient namespaces a URL fetch writes under:
/// `refs/gage-fetch/<run>/1/`. Nothing under it outlives the command
/// that wrote it.
pub const TRANSIENT_ROOT: &str = "refs/gage-fetch/";

/// A repository to fetch from and the prefix its refs are written
/// under: the remote-tracking prefix of a configured remote, or a
/// transient namespace for a URL that the command deletes when it
/// ends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchSource {
    repository: Repository,
    tracking: String,
}

impl From<Repository> for FetchSource {
    /// A URL names a new transient namespace on each conversion.
    fn from(repository: Repository) -> FetchSource {
        let tracking = if repository.is_remote() {
            tracking_prefix(repository.name())
        } else {
            format!("{TRANSIENT_ROOT}{}/{GENERATION}/", new_uuid())
        };
        FetchSource {
            repository,
            tracking,
        }
    }
}

impl FetchSource {
    pub fn repository(&self) -> &Repository {
        &self.repository
    }

    /// The prefix fetched refs are written under, ending in `/`.
    pub fn tracking(&self) -> &str {
        &self.tracking
    }

    /// True when the tracking prefix is a transient namespace.
    pub fn is_transient(&self) -> bool {
        self.tracking.starts_with(TRANSIENT_ROOT)
    }
}

impl RemoteStore<'_> {
    /// Add the remote `name` at `url`, as a backup target when `backup`.
    /// A name git's remote rules reject is [`StoreError::RemoteName`];
    /// a name in use is [`StoreError::RemoteExists`].
    pub fn add(&self, name: &str, url: &str, backup: bool) -> Result<Remote, StoreError> {
        self.validate_name(name)?;
        if self.list()?.iter().any(|r| r.name == name) {
            return Err(StoreError::RemoteExists(name.to_string()));
        }
        let path = self.store.path();
        run(git_in(path, ["remote", "add", name, url]))?;
        let fetch = format!("+{ROOT}*:{}*", tracking_prefix(name));
        run(git_in(
            path,
            [
                "config",
                "--replace-all",
                &format!("remote.{name}.fetch"),
                &fetch,
            ],
        ))?;
        run(git_in(
            path,
            ["config", &format!("remote.{name}.tagOpt"), "--no-tags"],
        ))?;
        if backup {
            run(git_in(
                path,
                ["config", &format!("remote.{name}.gage-backup"), "true"],
            ))?;
        }
        Ok(Remote {
            name: name.to_string(),
            url: url.to_string(),
            backup,
        })
    }

    /// Every remote in name order.
    pub fn list(&self) -> Result<Vec<Remote>, StoreError> {
        let output = match run(git_in(
            self.store.path(),
            ["config", "--get-regexp", r"^remote\..+\.(url|gage-backup)$"],
        )) {
            Ok(output) => output,
            // Exit 1 is "no match": a store with no remotes
            Err(StoreError::Git { status, .. }) if status.code() == Some(1) => String::new(),
            Err(e) => return Err(e),
        };
        Ok(parse_remote_config(&output))
    }

    /// `arg` as the configured remote of that name when one exists,
    /// else as a URL or path: git's own rule for a repository argument.
    pub fn resolve(&self, arg: &str) -> Result<Repository, StoreError> {
        if self.list()?.iter().any(|r| r.name == arg) {
            Ok(Repository::from_remote(arg))
        } else {
            Ok(Repository::from_url(arg))
        }
    }

    /// Remove the remote `name`, its config, and its remote-tracking
    /// refs. A name no remote has is [`StoreError::RemoteNotFound`].
    pub fn delete(&self, name: &str) -> Result<Remote, StoreError> {
        let remote = self
            .list()?
            .into_iter()
            .find(|r| r.name == name)
            .ok_or_else(|| StoreError::RemoteNotFound(name.to_string()))?;
        run(git_in(self.store.path(), ["remote", "remove", name]))?;
        Ok(remote)
    }

    /// The Gage refs `repository` advertises, as ref name to commit
    /// SHA. One `git ls-remote` over `refs/gage/1/*`.
    pub fn ls_remote(
        &self,
        repository: &Repository,
    ) -> Result<BTreeMap<String, String>, StoreError> {
        let pattern = format!("{ROOT}*");
        let out = run(git_transport_in(
            self.store.path(),
            ["ls-remote", "--refs", repository.spec(), &pattern],
        ))?;
        let mut refs = BTreeMap::new();
        for line in out.lines() {
            let (sha, ref_name) = line
                .split_once('\t')
                .ok_or_else(|| StoreError::Parse(format!("ls-remote line: {line}")))?;
            refs.insert(ref_name.to_string(), sha.to_string());
        }
        Ok(refs)
    }

    /// The digest the store's `pre-push` hook accepts for a push of
    /// `ref_names`: the names sorted, one per line, hashed with
    /// `git hash-object --stdin`.
    pub fn approval_digest(&self, ref_names: &[String]) -> Result<String, StoreError> {
        let mut names: Vec<&str> = ref_names.iter().map(String::as_str).collect();
        names.sort_unstable();
        let input: String = names.iter().map(|n| format!("{n}\n")).collect();
        let sha = run_with_stdin(
            git_in(self.store.path(), ["hash-object", "--stdin"]),
            input.as_bytes(),
        )?;
        Ok(sha.trim().to_string())
    }

    /// Push `ref_names` to `repository`, one refspec per ref and no
    /// force, with `approval` set for the store's `pre-push` hook.
    /// Git's output goes to the terminal as it runs. Returns git's
    /// exit status; a rejection by either hook is a failing status
    /// whose reason git has already printed.
    pub fn push(
        &self,
        repository: &Repository,
        ref_names: &[String],
        approval: &str,
    ) -> Result<ExitStatus, StoreError> {
        let mut cmd = git_transport_in(self.store.path(), ["push", repository.spec()]);
        for ref_name in ref_names {
            cmd.arg(format!("{ref_name}:{ref_name}"));
        }
        cmd.env("GAGE_PUSH_APPROVAL", approval)
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        cmd.status().map_err(StoreError::Spawn)
    }

    /// Every Gage ref of this store, objects, tags, and keys, as ref
    /// name to commit SHA: the set `backup` sends.
    pub fn gage_refs(&self) -> Result<BTreeMap<String, String>, StoreError> {
        let out = run(git_in(
            self.store.path(),
            ["for-each-ref", "--format=%(refname) %(objectname)", ROOT],
        ))?;
        let mut refs = BTreeMap::new();
        for line in out.lines() {
            let (ref_name, sha) = line
                .split_once(' ')
                .ok_or_else(|| StoreError::Parse(format!("for-each-ref line: {line}")))?;
            refs.insert(ref_name.to_string(), sha.to_string());
        }
        Ok(refs)
    }

    /// Fetch every Gage ref `source` advertises into its tracking
    /// prefix, pruning the ones it no longer has. The refspec is given
    /// on the command line, so a URL with no configuration fetches the
    /// same way a remote does. Git's output goes to the terminal as it
    /// runs.
    pub fn fetch_all(&self, source: &FetchSource) -> Result<ExitStatus, StoreError> {
        let refspec = format!("+{ROOT}*:{}*", source.tracking());
        let mut cmd = git_transport_in(
            self.store.path(),
            ["fetch", "--prune", source.repository().spec(), &refspec],
        );
        cmd.stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        cmd.status().map_err(StoreError::Spawn)
    }

    /// Fetch the named Gage refs of `source` into its tracking prefix,
    /// forced.
    pub fn fetch_refs(
        &self,
        source: &FetchSource,
        ref_names: &[String],
    ) -> Result<ExitStatus, StoreError> {
        let mut cmd = git_transport_in(self.store.path(), ["fetch", source.repository().spec()]);
        for ref_name in ref_names {
            let tracking = match ref_name.strip_prefix(ROOT) {
                Some(rest) => format!("{}{rest}", source.tracking()),
                None => return Err(StoreError::Parse(format!("not a Gage ref: {ref_name}"))),
            };
            cmd.arg(format!("+{ref_name}:{tracking}"));
        }
        cmd.stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        cmd.status().map_err(StoreError::Spawn)
    }

    /// Delete every ref under every transient namespace: what a URL
    /// fetch that did not reach its end left behind. Runs before a URL
    /// fetch starts, so two URL fetches in one store at the same time
    /// are not supported. Returns how many refs were deleted.
    pub fn clear_transient(&self) -> Result<usize, StoreError> {
        self.delete_refs_under(TRANSIENT_ROOT)
    }

    /// Delete the transient namespace of `source`. A configured
    /// remote's tracking refs are left alone. Returns how many refs
    /// were deleted.
    pub fn discard(&self, source: &FetchSource) -> Result<usize, StoreError> {
        if !source.is_transient() {
            return Ok(0);
        }
        self.delete_refs_under(source.tracking())
    }

    /// Delete every ref under `prefix` in one `update-ref` transaction.
    fn delete_refs_under(&self, prefix: &str) -> Result<usize, StoreError> {
        let out = run(git_in(
            self.store.path(),
            ["for-each-ref", "--format=%(refname)", prefix],
        ))?;
        let names: Vec<&str> = out.lines().collect();
        if names.is_empty() {
            return Ok(0);
        }
        let input: String = names.iter().map(|n| format!("delete {n}\n")).collect();
        run_with_stdin(
            git_in(self.store.path(), ["update-ref", "--stdin"]),
            input.as_bytes(),
        )?;
        Ok(names.len())
    }

    /// Git's rule for a remote name is the ref name rule with one
    /// level allowed. A leading `-` passes it but reads as an option.
    fn validate_name(&self, name: &str) -> Result<(), StoreError> {
        if name.is_empty() || name.starts_with('-') {
            return Err(StoreError::RemoteName(name.to_string()));
        }
        match run(git_in(
            self.store.path(),
            ["check-ref-format", "--allow-onelevel", name],
        )) {
            Ok(_) => Ok(()),
            Err(StoreError::Git { status, .. }) if status.code() == Some(1) => {
                Err(StoreError::RemoteName(name.to_string()))
            }
            Err(e) => Err(e),
        }
    }
}

/// Parse `git config --get-regexp` lines of the form
/// `remote.<name>.url <url>` and `remote.<name>.gage-backup <bool>`
/// into remotes, in name order. A remote name may hold dots, so the
/// key is split at its last dot.
fn parse_remote_config(output: &str) -> Vec<Remote> {
    let mut remotes: BTreeMap<String, Remote> = BTreeMap::new();
    for line in output.lines() {
        let Some((key, value)) = line.split_once(' ') else {
            continue;
        };
        let Some(rest) = key.strip_prefix("remote.") else {
            continue;
        };
        let Some((name, field)) = rest.rsplit_once('.') else {
            continue;
        };
        let remote = remotes.entry(name.to_string()).or_insert_with(|| Remote {
            name: name.to_string(),
            url: String::new(),
            backup: false,
        });
        match field {
            "url" => remote.url = value.to_string(),
            "gage-backup" => remote.backup = value == "true",
            _ => {}
        }
    }
    remotes.into_values().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::open_store;

    #[test]
    fn add_writes_the_tracking_refspec_and_list_reads_it_back() {
        let dir = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(dir.path());
        let remotes = RemoteStore::from(&store);
        assert!(remotes.list().unwrap().is_empty());

        remotes.add("share", "/srv/share.git", false).unwrap();
        remotes.add("offsite", "git@host:x.git", true).unwrap();
        assert_eq!(
            remotes.list().unwrap(),
            [
                Remote {
                    name: "offsite".into(),
                    url: "git@host:x.git".into(),
                    backup: true
                },
                Remote {
                    name: "share".into(),
                    url: "/srv/share.git".into(),
                    backup: false
                },
            ]
        );
        let config = run(git_in(store.path(), ["config", "--list", "--local"])).unwrap();
        assert!(
            config.contains("remote.share.fetch=+refs/gage/1/*:refs/remotes/share/gage/1/*"),
            "{config}"
        );
        assert!(!config.contains("refs/heads/*"), "{config}");
        assert!(config.contains("remote.share.tagopt=--no-tags"), "{config}");
        assert!(
            config.contains("remote.offsite.gage-backup=true"),
            "{config}"
        );
        assert!(!config.contains("remote.share.gage-backup"), "{config}");
    }

    #[test]
    fn add_rejects_a_used_or_invalid_name() {
        let dir = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(dir.path());
        let remotes = RemoteStore::from(&store);
        remotes.add("share", "/srv/share.git", false).unwrap();
        assert!(matches!(
            remotes.add("share", "/elsewhere.git", false),
            Err(StoreError::RemoteExists(n)) if n == "share"
        ));
        for bad in ["", "-x", "a b", "a..b", "x/"] {
            assert!(
                matches!(remotes.add(bad, "/x.git", false), Err(StoreError::RemoteName(n)) if n == bad),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn delete_removes_the_config_and_tracking_refs() {
        let dir = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(dir.path());
        let remotes = RemoteStore::from(&store);
        remotes.add("share", "/srv/share.git", true).unwrap();
        // A tracking ref the remote left behind
        let tree = run(git_in(store.path(), ["mktree"])).unwrap();
        let commit = run(git_in(
            store.path(),
            ["commit-tree", tree.trim(), "-m", "x"],
        ))
        .unwrap();
        let tracking = format!("{}object/x", tracking_prefix("share"));
        run(git_in(
            store.path(),
            ["update-ref", &tracking, commit.trim()],
        ))
        .unwrap();

        let deleted = remotes.delete("share").unwrap();
        assert_eq!(deleted.name, "share");
        assert!(deleted.backup);
        assert!(remotes.list().unwrap().is_empty());
        assert!(store.rev_parse(&tracking).unwrap().is_none());
        assert!(matches!(
            remotes.delete("share"),
            Err(StoreError::RemoteNotFound(n)) if n == "share"
        ));
    }

    #[test]
    fn push_sends_approved_refs_and_ls_remote_reads_them_back() {
        use crate::note::{NoteInput, NoteStore, NoteValue};
        use crate::refs::OBJECT_REFS;
        let dir = tempfile::tempdir().unwrap();
        let (source, _fsck) = open_store(&dir.path().join("source"));
        let (target, _fsck2) = open_store(&dir.path().join("target"));
        let remotes = RemoteStore::from(&source);
        remotes
            .add("share", target.path().to_str().unwrap(), false)
            .unwrap();
        let id = NoteStore::from(&source)
            .create(NoteInput {
                name: "comment",
                value: NoteValue::Text("v".into()),
                author: "user:test",
                target: None,
                metadata: None,
                work_id: None,
            })
            .unwrap();
        let ref_name = format!("{OBJECT_REFS}{id}");
        let tip = source.rev_parse(&ref_name).unwrap().unwrap();
        let share = remotes.resolve("share").unwrap();
        assert!(share.is_remote());
        assert!(remotes.ls_remote(&share).unwrap().is_empty());

        // The hook rejects a digest for another set
        let wrong = remotes
            .approval_digest(&[ref_name.clone(), "refs/gage/1/tag/x".into()])
            .unwrap();
        let status = remotes.push(&share, &[ref_name.clone()], &wrong).unwrap();
        assert!(!status.success());

        let digest = remotes.approval_digest(&[ref_name.clone()]).unwrap();
        let status = remotes.push(&share, &[ref_name.clone()], &digest).unwrap();
        assert!(status.success());
        assert_eq!(
            remotes.ls_remote(&share).unwrap(),
            BTreeMap::from([(ref_name.clone(), tip.clone())])
        );
        assert_eq!(target.rev_parse(&ref_name).unwrap().unwrap(), tip);

        // The same push to the target's path, with no remote configured
        let url = remotes.resolve(target.path().to_str().unwrap()).unwrap();
        assert!(!url.is_remote());
        let second = NoteStore::from(&source)
            .create(NoteInput {
                name: "comment",
                value: NoteValue::Text("w".into()),
                author: "user:test",
                target: None,
                metadata: None,
                work_id: None,
            })
            .unwrap();
        let second_ref = format!("{OBJECT_REFS}{second}");
        let status = remotes.push(&url, &[second_ref.clone()], &wrong).unwrap();
        assert!(!status.success(), "a URL push still needs the digest");
        let digest = remotes.approval_digest(&[second_ref.clone()]).unwrap();
        let status = remotes.push(&url, &[second_ref.clone()], &digest).unwrap();
        assert!(status.success());
        assert_eq!(remotes.ls_remote(&url).unwrap().len(), 2);
        assert_eq!(remotes.list().unwrap().len(), 1, "no remote is added");
    }

    #[test]
    fn url_repository_redacts_the_password_and_names_a_valid_transient_prefix() {
        let url = Repository::from_url("https://me:s3cret@host/x.git");
        assert_eq!(url.spec(), "https://me:s3cret@host/x.git");
        assert_eq!(url.name(), "https://me@host/x.git");
        assert!(!url.is_remote());
        let source = FetchSource::from(url);
        assert!(source.is_transient());
        for plain in [
            "git@host:x.git",
            "/srv/x.git",
            "https://host/x.git",
            "https://me@host/x.git",
            "ssh://git@host:22/x.git",
            "https://host",
        ] {
            assert_eq!(Repository::from_url(plain).name(), plain);
        }
        assert_ne!(
            FetchSource::from(Repository::from_url("/x")).tracking(),
            FetchSource::from(Repository::from_url("/x")).tracking()
        );
        let dir = tempfile::tempdir().unwrap();
        let ref_name = format!("{}object/x", source.tracking());
        run(git_in(dir.path(), ["check-ref-format", &ref_name])).unwrap();

        let remote = FetchSource::from(Repository::from_remote("share"));
        assert_eq!(remote.repository().spec(), "share");
        assert_eq!(remote.repository().name(), "share");
        assert!(remote.repository().is_remote());
        assert_eq!(remote.tracking(), "refs/remotes/share/gage/1/");
        assert!(!remote.is_transient());
    }

    #[test]
    fn parses_remote_config_with_dotted_names() {
        let output = "remote.a.b.url /x.git\nremote.a.b.gage-backup true\nremote.c.url /y.git\n";
        assert_eq!(
            parse_remote_config(output),
            [
                Remote {
                    name: "a.b".into(),
                    url: "/x.git".into(),
                    backup: true
                },
                Remote {
                    name: "c".into(),
                    url: "/y.git".into(),
                    backup: false
                },
            ]
        );
    }
}
