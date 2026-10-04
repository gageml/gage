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

/// Full ref name of the object with the given id.
pub(crate) fn object_ref(id: &str) -> String {
    format!("{OBJECT_REFS}{id}")
}

/// Full ref name of the tag with the given name.
pub(crate) fn tag_ref(name: &str) -> String {
    format!("{TAG_REFS}{name}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespaces_carry_the_generation() {
        assert_eq!(ROOT, format!("refs/gage/{GENERATION}/"));
        assert_eq!(OBJECT_REFS, format!("{ROOT}object/"));
        assert_eq!(TAG_REFS, format!("{ROOT}tag/"));
        assert_eq!(object_ref("abc"), "refs/gage/1/object/abc");
        assert_eq!(tag_ref("a/b"), "refs/gage/1/tag/a/b");
    }
}
