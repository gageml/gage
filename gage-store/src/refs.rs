//! The ref layout: every Gage ref lives under `refs/gage/<generation>/`.
//!
//! The generation names the ref layout scheme and the conventions a
//! reader needs before it can reach an object's type: how an id maps
//! to a ref, the marker blobs of an object commit, link files, and
//! tombstones. It is the one store-wide version, stamped into the
//! repository's `gage.version` at init and checked at open. A change
//! to one object type's content is a bump of that type's `type` blob
//! version, not of the generation.
//!
//! The generation is in the ref name because the ref name is the only
//! thing that travels with a ref on push and pull and the only thing a
//! receiving repository's `update` hook can inspect. A repository can
//! hold refs of more than one generation; a build reads its own
//! generation and leaves the others alone.

/// The ref layout generation this build reads and writes.
pub const GENERATION: u32 = 1;

/// The namespace of every Gage ref: `refs/gage/<generation>/`.
pub const ROOT: &str = "refs/gage/1/";

/// The object refs: `refs/gage/<generation>/object/<id>`.
pub const OBJECT_REFS: &str = "refs/gage/1/object/";

/// The tag refs: `refs/gage/<generation>/tag/<name>`.
pub const TAG_REFS: &str = "refs/gage/1/tag/";

/// The key refs: `refs/gage/<generation>/key/<type>/<key>`, one per
/// live object carrying `attrs.key`.
pub const KEY_REFS: &str = "refs/gage/1/key/";

/// Full ref name of the object with the given id.
pub(crate) fn object_ref(id: &str) -> String {
    format!("{OBJECT_REFS}{id}")
}

/// Full ref name of the tag with the given name.
pub(crate) fn tag_ref(name: &str) -> String {
    format!("{TAG_REFS}{name}")
}

/// Full ref name of the key `key` of objects of the bare type name
/// `type_name` (`issue`, `attachment`).
pub(crate) fn key_ref(type_name: &str, key: &str) -> String {
    format!("{KEY_REFS}{type_name}/{key}")
}

/// The ref namespace a store reads its objects, tags, and keys under:
/// the local one, or the tracking refs a fetch of a remote wrote. The
/// two share one layout below the root, so a reader of one reads the
/// other. Only the local namespace is written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefRoot {
    root: String,
    remote: Option<String>,
}

impl RefRoot {
    /// The store's own refs: `refs/gage/<generation>/`.
    pub fn local() -> RefRoot {
        RefRoot {
            root: ROOT.to_string(),
            remote: None,
        }
    }

    /// The tracking refs of the remote `name`:
    /// `refs/remotes/<name>/gage/<generation>/`.
    pub fn tracking(name: &str) -> RefRoot {
        RefRoot {
            root: format!("refs/remotes/{name}/gage/{GENERATION}/"),
            remote: Some(name.to_string()),
        }
    }

    /// The namespace prefix, ending in `/`.
    pub fn as_str(&self) -> &str {
        &self.root
    }

    /// The remote whose tracking refs these are; `None` for the local
    /// namespace.
    pub fn remote(&self) -> Option<&str> {
        self.remote.as_deref()
    }

    pub fn is_local(&self) -> bool {
        self.remote.is_none()
    }

    /// The object ref namespace.
    pub fn objects(&self) -> String {
        format!("{}object/", self.root)
    }

    /// The tag ref namespace.
    pub fn tags(&self) -> String {
        format!("{}tag/", self.root)
    }

    /// The key ref namespace.
    pub fn keys(&self) -> String {
        format!("{}key/", self.root)
    }

    /// Full ref name of the object with the given id.
    pub fn object_ref(&self, id: &str) -> String {
        format!("{}object/{id}", self.root)
    }

    /// Full ref name of the tag with the given name.
    pub fn tag_ref(&self, name: &str) -> String {
        format!("{}tag/{name}", self.root)
    }

    /// Full ref name of the key `key` of objects of the bare type
    /// name `type_name`.
    pub fn key_ref(&self, type_name: &str, key: &str) -> String {
        format!("{}key/{type_name}/{key}", self.root)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_local_root_names_the_constant_namespaces() {
        let refs = RefRoot::local();
        assert!(refs.is_local());
        assert_eq!(refs.as_str(), ROOT);
        assert_eq!(refs.objects(), OBJECT_REFS);
        assert_eq!(refs.tags(), TAG_REFS);
        assert_eq!(refs.keys(), KEY_REFS);
        assert_eq!(refs.object_ref("abc"), object_ref("abc"));
        assert_eq!(refs.tag_ref("a/b"), tag_ref("a/b"));
        assert_eq!(refs.key_ref("issue", "a/b"), key_ref("issue", "a/b"));
    }

    #[test]
    fn a_tracking_root_keeps_the_layout_under_the_remote() {
        let refs = RefRoot::tracking("acme");
        assert_eq!(refs.remote(), Some("acme"));
        assert_eq!(refs.as_str(), "refs/remotes/acme/gage/1/");
        assert_eq!(
            refs.object_ref("abc"),
            "refs/remotes/acme/gage/1/object/abc"
        );
        assert_eq!(refs.tag_ref("a/b"), "refs/remotes/acme/gage/1/tag/a/b");
        assert_eq!(
            refs.key_ref("issue", "k"),
            "refs/remotes/acme/gage/1/key/issue/k"
        );
    }

    #[test]
    fn namespaces_carry_the_generation() {
        assert_eq!(ROOT, format!("refs/gage/{GENERATION}/"));
        assert_eq!(OBJECT_REFS, format!("{ROOT}object/"));
        assert_eq!(TAG_REFS, format!("{ROOT}tag/"));
        assert_eq!(KEY_REFS, format!("{ROOT}key/"));
        assert_eq!(key_ref("issue", "a/b"), "refs/gage/1/key/issue/a/b");
        assert_eq!(object_ref("abc"), "refs/gage/1/object/abc");
        assert_eq!(tag_ref("a/b"), "refs/gage/1/tag/a/b");
    }
}
