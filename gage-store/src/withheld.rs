//! Objects withheld from push and backup.
//!
//! A fetch keeps what a remote has under the remote's tracking refs,
//! apart from the store's own objects, until a merge. An object of
//! the store can still link one of those: a scan of a remote's
//! sessions writes a dataset that links the sessions and notes that
//! target them. A link is a commit parent, so pushing such an object
//! would carry the remote's session with it, to wherever the push
//! goes. Those objects are withheld: push and backup leave them out.
//!
//! The rule is structural. An object is withheld while its links
//! reach an object the store holds only as fetched from a remote.
//! Merging that object into the store ends the withholding.

use std::collections::BTreeSet;

use crate::git::{git_in, run};
use crate::refs::GENERATION;
use crate::{Store, StoreError};

impl Store {
    /// Ids of this store's objects that push and backup leave out:
    /// every object whose tip reaches, through `first-parent` and link
    /// edges, a commit of an object that has a tracking ref under some
    /// remote and no ref of the store's own.
    pub fn withheld_ids(&self) -> Result<BTreeSet<String>, StoreError> {
        let unreferenced = self.index.unreferenced_ids()?;
        if unreferenced.is_empty() {
            return Ok(BTreeSet::new());
        }
        let fetched = self.fetched_ids()?;
        let remote_only: Vec<String> = unreferenced
            .into_iter()
            .filter(|id| fetched.contains(id))
            .collect();
        Ok(self
            .index
            .refs_reaching(&remote_only)?
            .into_iter()
            .collect())
    }

    /// Ids of the objects any remote's tracking refs hold.
    fn fetched_ids(&self) -> Result<BTreeSet<String>, StoreError> {
        let marker = format!("/gage/{GENERATION}/object/");
        let out = run(git_in(
            self.path(),
            ["for-each-ref", "--format=%(refname)", "refs/remotes/"],
        ))?;
        Ok(out
            .lines()
            .filter_map(|name| name.split_once(&marker))
            .map(|(_, id)| id.to_string())
            .collect())
    }
}
