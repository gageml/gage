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

use std::collections::BTreeMap;
use std::process::{ExitStatus, Stdio};

use crate::git::{git_in, run, run_with_stdin};
use crate::refs::ROOT;
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

    /// The Gage refs the remote `name` advertises, as ref name to
    /// commit SHA. One `git ls-remote` over `refs/gage/1/*`.
    pub fn ls_remote(&self, name: &str) -> Result<BTreeMap<String, String>, StoreError> {
        let pattern = format!("{ROOT}*");
        let out = run(git_in(
            self.store.path(),
            ["ls-remote", "--refs", name, &pattern],
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

    /// Push `ref_names` to the remote `name`, one refspec per ref and
    /// no force, with `approval` set for the store's `pre-push` hook.
    /// Git's output goes to the terminal as it runs. Returns git's
    /// exit status; a rejection by either hook is a failing status
    /// whose reason git has already printed.
    pub fn push(
        &self,
        name: &str,
        ref_names: &[String],
        approval: &str,
    ) -> Result<ExitStatus, StoreError> {
        let mut cmd = git_in(self.store.path(), ["push", name]);
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

    /// Fetch every Gage ref the remote `name` advertises into its
    /// remote-tracking refs, pruning the ones it no longer has. Git's
    /// output goes to the terminal as it runs.
    pub fn fetch_all(&self, name: &str) -> Result<ExitStatus, StoreError> {
        let mut cmd = git_in(self.store.path(), ["fetch", "--prune", name]);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        cmd.status().map_err(StoreError::Spawn)
    }

    /// Fetch the named Gage refs of the remote `name` into its
    /// remote-tracking refs, forced.
    pub fn fetch_refs(&self, name: &str, ref_names: &[String]) -> Result<ExitStatus, StoreError> {
        let mut cmd = git_in(self.store.path(), ["fetch", name]);
        for ref_name in ref_names {
            let tracking = match ref_name.strip_prefix(ROOT) {
                Some(rest) => format!("{}{rest}", tracking_prefix(name)),
                None => return Err(StoreError::Parse(format!("not a Gage ref: {ref_name}"))),
            };
            cmd.arg(format!("+{ref_name}:{tracking}"));
        }
        cmd.stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        cmd.status().map_err(StoreError::Spawn)
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
        assert!(remotes.ls_remote("share").unwrap().is_empty());

        // The hook rejects a digest for another set
        let wrong = remotes
            .approval_digest(&[ref_name.clone(), "refs/gage/1/tag/x".into()])
            .unwrap();
        let status = remotes.push("share", &[ref_name.clone()], &wrong).unwrap();
        assert!(!status.success());

        let digest = remotes.approval_digest(&[ref_name.clone()]).unwrap();
        let status = remotes.push("share", &[ref_name.clone()], &digest).unwrap();
        assert!(status.success());
        assert_eq!(
            remotes.ls_remote("share").unwrap(),
            BTreeMap::from([(ref_name.clone(), tip.clone())])
        );
        assert_eq!(target.rev_parse(&ref_name).unwrap().unwrap(), tip);
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
