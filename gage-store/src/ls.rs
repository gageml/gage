//! The store as one path hierarchy: the namespaces under `refs/`, the
//! references in them, and the trees and blobs of the commit each
//! reference points at. A listing walks the hierarchy with glob
//! patterns, so one pattern reaches from a namespace into the files
//! of a commit.

use std::collections::BTreeMap;

use wax::{Glob, Program};

use crate::git::{EntryKind, git_in, run};
use crate::refs::{KEY_REFS, OBJECT_REFS, ROOT, TAG_REFS};
use crate::remote::{RemoteStore, tracking_prefix};
use crate::{Store, StoreError};

/// One path of the hierarchy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathEntry {
    /// The full `/`-separated path from the store root.
    pub path: String,
    pub kind: PathKind,
    /// The SHA a reference, tree, blob, or commit entry resolves to;
    /// `None` for a namespace.
    pub sha: Option<String>,
    /// Bytes for a blob; `None` otherwise.
    pub size: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathKind {
    /// A prefix of reference names, such as `refs/gage/1/tag`.
    Namespace,
    /// A reference, pointing at a commit.
    Ref,
    Tree,
    Blob,
    /// A commit entry inside a tree.
    Commit,
}

impl PathKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            PathKind::Namespace => "namespace",
            PathKind::Ref => "ref",
            PathKind::Tree => "tree",
            PathKind::Blob => "blob",
            PathKind::Commit => "commit",
        }
    }

    /// Whether the path has children: a namespace, a reference, or a
    /// tree.
    pub fn is_container(&self) -> bool {
        matches!(self, PathKind::Namespace | PathKind::Ref | PathKind::Tree)
    }
}

impl Store {
    /// Lists paths of the hierarchy. With no patterns, lists the Gage
    /// namespaces under the local root and under each remote's
    /// tracking root. A pattern with no wildcard names one path: a
    /// container lists its children, and a blob lists itself. A
    /// pattern with a wildcard lists what it matches: `*` matches one
    /// name and `**` matches any number of names, on either side of a
    /// reference. The result is path-sorted with one entry per path.
    pub fn ls_paths(&self, patterns: &[&str]) -> Result<Vec<PathEntry>, StoreError> {
        let hierarchy = Hierarchy::load(self)?;
        if patterns.is_empty() {
            return Ok(hierarchy.gage_namespaces());
        }
        let mut out: BTreeMap<String, PathEntry> = BTreeMap::new();
        for pattern in patterns {
            for entry in hierarchy.list(pattern)? {
                out.entry(entry.path.clone()).or_insert(entry);
            }
        }
        Ok(out.into_values().collect())
    }
}

struct Hierarchy<'a> {
    store: &'a Store,
    /// Every reference and the SHA it points at.
    refs: BTreeMap<String, String>,
    /// The Gage namespaces, without a trailing `/`. They exist even
    /// when no reference is under them.
    known: Vec<String>,
}

/// A node of the hierarchy.
#[derive(Debug, Clone)]
struct Node {
    path: String,
    kind: PathKind,
    sha: Option<String>,
}

/// One `/`-separated component of a pattern.
enum Step<'t> {
    /// `**`: any number of components.
    Any,
    Glob(Glob<'t>),
}

impl<'a> Hierarchy<'a> {
    fn load(store: &'a Store) -> Result<Self, StoreError> {
        let out = run(git_in(
            store.path(),
            ["for-each-ref", "--format=%(refname) %(objectname)"],
        ))?;
        let mut refs = BTreeMap::new();
        for line in out.lines() {
            let (name, sha) = line
                .split_once(' ')
                .ok_or_else(|| StoreError::Parse(format!("for-each-ref line: {line}")))?;
            refs.insert(name.to_string(), sha.to_string());
        }
        let mut roots = vec![ROOT.to_string()];
        for remote in RemoteStore::from(store).list()? {
            roots.push(tracking_prefix(&remote.name));
        }
        let known = roots
            .iter()
            .flat_map(|root| {
                [KEY_REFS, OBJECT_REFS, TAG_REFS].map(|namespace| {
                    let child = namespace[ROOT.len()..].trim_end_matches('/');
                    format!("{root}{child}")
                })
            })
            .collect();
        Ok(Hierarchy { store, refs, known })
    }

    /// The Gage namespaces beneath each root, in root order.
    fn gage_namespaces(&self) -> Vec<PathEntry> {
        self.known
            .iter()
            .map(|path| PathEntry {
                path: path.clone(),
                kind: PathKind::Namespace,
                sha: None,
                size: None,
            })
            .collect()
    }

    fn list(&self, pattern: &str) -> Result<Vec<PathEntry>, StoreError> {
        let trimmed = pattern.strip_suffix('/').unwrap_or(pattern);
        if trimmed.is_empty() {
            return Err(StoreError::ListPattern(format!(
                "{pattern:?}: a pattern names at least one component"
            )));
        }
        let steps = trimmed
            .split('/')
            .map(|component| parse_step(component, pattern))
            .collect::<Result<Vec<_>, _>>()?;
        let invariant = steps
            .iter()
            .all(|step| matches!(step, Step::Glob(glob) if glob.text().is_invariant()));
        let root = Node {
            path: String::new(),
            kind: PathKind::Namespace,
            sha: None,
        };
        let mut matched: BTreeMap<String, Node> = BTreeMap::new();
        self.expand(&root, &steps, &mut matched)?;
        if matched.is_empty() {
            return Err(StoreError::PathNotFound(pattern.to_string()));
        }
        let nodes: Vec<Node> = if invariant {
            let node = matched.into_values().next().unwrap();
            if node.kind.is_container() {
                self.children(&node)?
            } else {
                vec![node]
            }
        } else {
            matched.into_values().collect()
        };
        nodes.into_iter().map(|node| self.entry(node)).collect()
    }

    /// Adds to `out` every node `steps` reaches from `node`.
    fn expand(
        &self,
        node: &Node,
        steps: &[Step<'_>],
        out: &mut BTreeMap<String, Node>,
    ) -> Result<(), StoreError> {
        let Some((step, rest)) = steps.split_first() else {
            out.insert(node.path.clone(), node.clone());
            return Ok(());
        };
        match step {
            Step::Any => {
                self.expand(node, rest, out)?;
                for child in self.children(node)? {
                    self.expand(&child, steps, out)?;
                }
            }
            Step::Glob(glob) => {
                for child in self.children(node)? {
                    if glob.is_match(child.name()) {
                        self.expand(&child, rest, out)?;
                    }
                }
            }
        }
        Ok(())
    }

    /// The nodes directly beneath `node`, name-sorted.
    fn children(&self, node: &Node) -> Result<Vec<Node>, StoreError> {
        match node.kind {
            PathKind::Namespace => Ok(self.namespace_children(&node.path)),
            PathKind::Ref | PathKind::Tree => {
                let sha = node
                    .sha
                    .as_deref()
                    .expect("a ref or tree node carries its SHA");
                let entries = self.store.read_tree(sha)?;
                Ok(entries
                    .into_iter()
                    .map(|entry| Node {
                        path: format!("{}/{}", node.path, entry.name),
                        kind: match entry.kind {
                            EntryKind::Tree => PathKind::Tree,
                            EntryKind::Blob => PathKind::Blob,
                            EntryKind::Commit => PathKind::Commit,
                        },
                        sha: Some(entry.sha),
                    })
                    .collect())
            }
            PathKind::Blob | PathKind::Commit => Ok(Vec::new()),
        }
    }

    /// The references and namespaces directly beneath the namespace
    /// `path`; the root is the empty path.
    fn namespace_children(&self, path: &str) -> Vec<Node> {
        let prefix = if path.is_empty() {
            String::new()
        } else {
            format!("{path}/")
        };
        let mut children: BTreeMap<String, Node> = BTreeMap::new();
        for (name, sha) in self.refs.range(prefix.clone()..) {
            let Some(rest) = name.strip_prefix(&prefix) else {
                break;
            };
            let (child, node) = match rest.split_once('/') {
                Some((child, _)) => (
                    child,
                    Node {
                        path: format!("{prefix}{child}"),
                        kind: PathKind::Namespace,
                        sha: None,
                    },
                ),
                None => (
                    rest,
                    Node {
                        path: name.clone(),
                        kind: PathKind::Ref,
                        sha: Some(sha.clone()),
                    },
                ),
            };
            children.entry(child.to_string()).or_insert(node);
        }
        for known in &self.known {
            let Some(rest) = known.strip_prefix(&prefix) else {
                continue;
            };
            let child = rest.split('/').next().unwrap();
            children.entry(child.to_string()).or_insert(Node {
                path: format!("{prefix}{child}"),
                kind: PathKind::Namespace,
                sha: None,
            });
        }
        children.into_values().collect()
    }

    fn entry(&self, node: Node) -> Result<PathEntry, StoreError> {
        let size = match (node.kind, &node.sha) {
            (PathKind::Blob, Some(sha)) => self.store.object_info(sha)?.map(|info| info.size),
            _ => None,
        };
        Ok(PathEntry {
            path: node.path,
            kind: node.kind,
            sha: node.sha,
            size,
        })
    }
}

impl Node {
    /// The last component of the path.
    fn name(&self) -> &str {
        self.path.rsplit('/').next().unwrap()
    }
}

fn parse_step<'t>(component: &'t str, pattern: &str) -> Result<Step<'t>, StoreError> {
    if component.is_empty() {
        return Err(StoreError::ListPattern(format!(
            "{pattern:?}: empty path component"
        )));
    }
    if component == "**" {
        return Ok(Step::Any);
    }
    let glob =
        Glob::new(component).map_err(|e| StoreError::ListPattern(format!("{pattern:?}: {e}")))?;
    Ok(Step::Glob(glob))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::open_store;
    use crate::{NoteInput, NoteStore, NoteValue, TagStore};

    fn note(store: &Store, value: &str) -> String {
        NoteStore::from(store)
            .create(NoteInput {
                name: "comment",
                value: NoteValue::Text(value.to_string()),
                author: "user:test",
                target: None,
                metadata: None,
                work_id: None,
            })
            .unwrap()
    }

    fn paths(entries: &[PathEntry]) -> Vec<&str> {
        entries.iter().map(|e| e.path.as_str()).collect()
    }

    fn kinds(entries: &[PathEntry]) -> Vec<PathKind> {
        entries.iter().map(|e| e.kind).collect()
    }

    #[test]
    fn no_pattern_lists_the_gage_namespaces_of_every_root() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());

        let entries = store.ls_paths(&[]).unwrap();
        assert_eq!(
            paths(&entries),
            ["refs/gage/1/key", "refs/gage/1/object", "refs/gage/1/tag"]
        );
        assert!(kinds(&entries).iter().all(|k| *k == PathKind::Namespace));

        RemoteStore::from(&store)
            .add("share", "/srv/share.git", false)
            .unwrap();
        let entries = store.ls_paths(&[]).unwrap();
        assert_eq!(
            paths(&entries),
            [
                "refs/gage/1/key",
                "refs/gage/1/object",
                "refs/gage/1/tag",
                "refs/remotes/share/gage/1/key",
                "refs/remotes/share/gage/1/object",
                "refs/remotes/share/gage/1/tag",
            ]
        );
    }

    #[test]
    fn a_literal_namespace_lists_its_children() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let id = note(&store, "v1");
        let (_, tip) = store.resolve_id(&id).unwrap();
        TagStore::from(&store).add("db/test", &id, false).unwrap();

        let entries = store.ls_paths(&["refs/gage/1"]).unwrap();
        assert_eq!(
            paths(&entries),
            ["refs/gage/1/key", "refs/gage/1/object", "refs/gage/1/tag"]
        );

        let entries = store.ls_paths(&["refs/gage/1/object/"]).unwrap();
        assert_eq!(paths(&entries), [format!("refs/gage/1/object/{id}")]);
        assert_eq!(entries[0].kind, PathKind::Ref);
        assert_eq!(entries[0].sha.as_deref(), Some(tip.as_str()));

        let entries = store.ls_paths(&["refs/gage/1/tag/db"]).unwrap();
        assert_eq!(paths(&entries), ["refs/gage/1/tag/db/test"]);
        assert_eq!(kinds(&entries), [PathKind::Ref]);

        let entries = store.ls_paths(&["refs/gage/1/key"]).unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn a_reference_lists_the_root_tree_of_its_commit() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let id = note(&store, "v1");
        TagStore::from(&store).add("db/test", &id, false).unwrap();

        let entries = store.ls_paths(&["refs/gage/1/tag/db/test"]).unwrap();
        assert!(
            paths(&entries).contains(&"refs/gage/1/tag/db/test/attrs.json"),
            "{entries:?}"
        );
        assert!(
            entries
                .iter()
                .all(|e| e.kind == PathKind::Blob && e.size.is_some())
        );

        let one_level = store.ls_paths(&["refs/gage/1/tag/db/test/*"]).unwrap();
        assert_eq!(one_level, entries);

        let file = store
            .ls_paths(&["refs/gage/1/tag/db/test/attrs.json"])
            .unwrap();
        assert_eq!(paths(&file), ["refs/gage/1/tag/db/test/attrs.json"]);
        assert_eq!(file[0].kind, PathKind::Blob);
        assert!(file[0].size.unwrap() > 0);
    }

    #[test]
    fn a_wildcard_lists_its_matches_and_a_tree_wildcard_crosses_into_commits() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let id = note(&store, "v1");
        TagStore::from(&store).add("db/test", &id, false).unwrap();

        let entries = store.ls_paths(&["refs/gage/1/*"]).unwrap();
        assert_eq!(
            paths(&entries),
            ["refs/gage/1/key", "refs/gage/1/object", "refs/gage/1/tag"]
        );

        let entries = store.ls_paths(&["refs/gage/1/tag/**/*"]).unwrap();
        assert_eq!(
            paths(&entries)[..2],
            ["refs/gage/1/tag/db", "refs/gage/1/tag/db/test"]
        );
        assert_eq!(kinds(&entries)[..2], [PathKind::Namespace, PathKind::Ref]);
        assert!(paths(&entries).contains(&"refs/gage/1/tag/db/test/attrs.json"));

        let prefix = &id[..4];
        let entries = store
            .ls_paths(&[&format!("refs/gage/1/object/{prefix}*/attrs.json")])
            .unwrap();
        assert_eq!(
            paths(&entries),
            [format!("refs/gage/1/object/{id}/attrs.json")]
        );

        let entries = store.ls_paths(&["refs/**/attrs.json"]).unwrap();
        assert_eq!(
            paths(&entries),
            [
                format!("refs/gage/1/object/{id}/attrs.json"),
                "refs/gage/1/tag/db/test/attrs.json".to_string(),
            ]
        );
    }

    #[test]
    fn patterns_union_with_one_entry_per_path() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());

        let entries = store
            .ls_paths(&["refs/gage/1/*", "refs/gage/1/tag", "refs/gage/1/[ot]*"])
            .unwrap();
        assert_eq!(
            paths(&entries),
            ["refs/gage/1/key", "refs/gage/1/object", "refs/gage/1/tag"]
        );
    }

    #[test]
    fn a_missing_path_and_an_unmatched_pattern_are_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        note(&store, "v1");

        let err = store.ls_paths(&["refs/gage/1/tag/nope"]).unwrap_err();
        assert!(matches!(err, StoreError::PathNotFound(p) if p == "refs/gage/1/tag/nope"));

        let err = store.ls_paths(&["refs/gage/1/object/zzzz*"]).unwrap_err();
        assert!(matches!(err, StoreError::PathNotFound(p) if p == "refs/gage/1/object/zzzz*"));
    }

    #[test]
    fn a_malformed_pattern_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());

        for pattern in ["", "/", "refs//gage", "refs/gage/1/a**b", "refs/gage/1/[a"] {
            let err = store.ls_paths(&[pattern]).unwrap_err();
            assert!(
                matches!(err, StoreError::ListPattern(_)),
                "{pattern:?}: {err}"
            );
        }
    }
}
