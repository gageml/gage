//! Outline tree state + visible-row projection.
//!
//! `Outline` owns the tree *shape* (what's expanded, what's visible). Row
//! content — labels for the session, entry titles, note names — is composed by
//! the renderer from the `Document`, keyed off `RowKind`.

use std::collections::HashSet;

pub struct Outline {
    session_expanded: bool,
    entry_expanded: HashSet<usize>,
    /// Ordered note ids attached to the session itself (no line target).
    /// Rendered as children of the session row.
    session_note_ids: Vec<String>,
    /// `entry_note_ids[i]` is the ordered list of note ids attached to entry
    /// `i`'s line. Mutated when notes are added or removed; outline rebuilds
    /// from this projection.
    entry_note_ids: Vec<Vec<String>>,
    /// `entry_hidden[i]` marks entry `i` as low signal, omitted from the
    /// rows unless `detail` is on or the entry carries notes.
    entry_hidden: Vec<bool>,
    /// Show all entries, overriding `entry_hidden`.
    detail: bool,
    visible: Vec<Row>,
}

pub struct Row {
    pub level: usize,
    pub has_children: bool,
    pub expanded: bool,
    pub kind: RowKind,
}

#[derive(Clone)]
pub enum RowKind {
    Session,
    Entry {
        index: usize,
    },
    /// `entry_index` is `None` for a session-level note under the session row
    Note {
        entry_index: Option<usize>,
        note_id: String,
    },
}

pub enum CollapseOutcome {
    Collapsed,
    SelectParent(usize),
    None,
}

impl Outline {
    pub fn new(
        session_note_ids: Vec<String>,
        entry_note_ids: Vec<Vec<String>>,
        entry_hidden: Vec<bool>,
        detail: bool,
    ) -> Self {
        let mut o = Self {
            session_expanded: false,
            entry_expanded: HashSet::new(),
            session_note_ids,
            entry_note_ids,
            entry_hidden,
            detail,
            visible: Vec::new(),
        };
        o.rebuild();
        o
    }

    pub fn detail(&self) -> bool {
        self.detail
    }

    pub fn set_detail(&mut self, detail: bool) {
        self.detail = detail;
        self.rebuild();
    }

    pub fn rows(&self) -> &[Row] {
        &self.visible
    }

    pub fn row(&self, idx: usize) -> Option<&Row> {
        self.visible.get(idx)
    }

    pub fn len(&self) -> usize {
        self.visible.len()
    }

    pub fn toggle(&mut self, idx: usize) -> bool {
        let Some(row) = self.visible.get(idx) else {
            return false;
        };
        if !row.has_children {
            return false;
        }
        let expanded = row.expanded;
        self.set_expanded(idx, !expanded);
        true
    }

    pub fn expand(&mut self, idx: usize) -> bool {
        let Some(row) = self.visible.get(idx) else {
            return false;
        };
        if !row.has_children || row.expanded {
            return false;
        }
        self.set_expanded(idx, true);
        true
    }

    pub fn collapse(&mut self, idx: usize) -> CollapseOutcome {
        let Some(row) = self.visible.get(idx) else {
            return CollapseOutcome::None;
        };
        if row.has_children && row.expanded {
            self.set_expanded(idx, false);
            return CollapseOutcome::Collapsed;
        }
        if row.level > 1 {
            return CollapseOutcome::SelectParent(self.parent_of(idx));
        }
        CollapseOutcome::None
    }

    /// Replace the entry/note projection in place. Preserves expansion state
    /// (session + per-entry) so a reload doesn't collapse the tree. Entry
    /// indices that no longer exist drop out of the expanded set.
    pub fn reload(
        &mut self,
        session_note_ids: Vec<String>,
        entry_note_ids: Vec<Vec<String>>,
        entry_hidden: Vec<bool>,
    ) {
        self.entry_expanded.retain(|i| *i < entry_note_ids.len());
        self.session_note_ids = session_note_ids;
        self.entry_note_ids = entry_note_ids;
        self.entry_hidden = entry_hidden;
        self.rebuild();
    }

    /// Snapshot of the expansion state (session + per-entry), for hosts
    /// that restore a previously viewed session's tree shape.
    pub fn expansion(&self) -> (bool, HashSet<usize>) {
        (self.session_expanded, self.entry_expanded.clone())
    }

    /// Replace the expansion state and rebuild. Entry indices beyond the
    /// current projection drop out, as in [`Outline::reload`].
    pub fn set_expansion(&mut self, session_expanded: bool, entry_expanded: HashSet<usize>) {
        self.session_expanded = session_expanded;
        self.entry_expanded = entry_expanded;
        self.entry_expanded
            .retain(|i| *i < self.entry_note_ids.len());
        self.rebuild();
    }

    /// Append a note id under `entry_index` (`None` targets the session row),
    /// ensure the parent is expanded so the new note is visible, and rebuild.
    /// Returns the visible-row index of the new note row.
    pub fn add_note(&mut self, entry_index: Option<usize>, note_id: String) -> Option<usize> {
        match entry_index {
            Some(i) => {
                self.entry_note_ids.get_mut(i)?.push(note_id.clone());
                self.entry_expanded.insert(i);
            }
            None => {
                self.session_note_ids.push(note_id.clone());
                self.session_expanded = true;
            }
        }
        self.rebuild();
        self.visible.iter().position(|r| match &r.kind {
            RowKind::Note { note_id: id, .. } => id == &note_id,
            _ => false,
        })
    }

    /// Remove a note id wherever it appears, then rebuild. Returns the owner
    /// if the note was found — `Some(entry_index)` for a line note, `None`
    /// for a session-level note.
    pub fn remove_note(&mut self, note_id: &str) -> Option<Option<usize>> {
        let mut found: Option<Option<usize>> = None;
        if let Some(pos) = self.session_note_ids.iter().position(|n| n == note_id) {
            self.session_note_ids.remove(pos);
            found = Some(None);
        } else {
            for (i, ids) in self.entry_note_ids.iter_mut().enumerate() {
                if let Some(pos) = ids.iter().position(|n| n == note_id) {
                    ids.remove(pos);
                    found = Some(Some(i));
                    break;
                }
            }
        }
        self.rebuild();
        found
    }

    fn set_expanded(&mut self, idx: usize, expanded: bool) {
        let Some(row) = self.visible.get(idx) else {
            return;
        };
        match &row.kind {
            RowKind::Session => {
                self.session_expanded = expanded;
            }
            RowKind::Entry { index } => {
                let index = *index;
                if expanded {
                    self.entry_expanded.insert(index);
                } else {
                    self.entry_expanded.remove(&index);
                }
            }
            RowKind::Note { .. } => return,
        }
        self.rebuild();
    }

    fn parent_of(&self, idx: usize) -> usize {
        let Some(row) = self.visible.get(idx) else {
            return 0;
        };
        let parent_level = row.level.saturating_sub(1);
        for j in (0..idx).rev() {
            if let Some(r) = self.visible.get(j)
                && r.level <= parent_level
            {
                return j;
            }
        }
        0
    }

    fn rebuild(&mut self) {
        let entry_count = self.entry_note_ids.len();
        let mut rows = Vec::with_capacity(1 + entry_count);
        let session_has_children = !self.session_note_ids.is_empty();
        let session_expanded = session_has_children && self.session_expanded;
        rows.push(Row {
            level: 1,
            has_children: session_has_children,
            expanded: session_expanded,
            kind: RowKind::Session,
        });
        if session_expanded {
            for id in &self.session_note_ids {
                rows.push(Row {
                    level: 2,
                    has_children: false,
                    expanded: false,
                    kind: RowKind::Note {
                        entry_index: None,
                        note_id: id.clone(),
                    },
                });
            }
        }
        for (i, notes) in self.entry_note_ids.iter().enumerate() {
            // A noted entry always shows: a note declares the entry
            // mattered to someone
            if !self.detail
                && self.entry_hidden.get(i).copied().unwrap_or(false)
                && notes.is_empty()
            {
                continue;
            }
            let has_children = !notes.is_empty();
            let expanded = has_children && self.entry_expanded.contains(&i);
            rows.push(Row {
                level: 1,
                has_children,
                expanded,
                kind: RowKind::Entry { index: i },
            });
            if expanded {
                for id in notes {
                    rows.push(Row {
                        level: 2,
                        has_children: false,
                        expanded: false,
                        kind: RowKind::Note {
                            entry_index: Some(i),
                            note_id: id.clone(),
                        },
                    });
                }
            }
        }
        self.visible = rows;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry_indices(o: &Outline) -> Vec<usize> {
        o.rows()
            .iter()
            .filter_map(|r| match r.kind {
                RowKind::Entry { index } => Some(index),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn filter_hides_marked_entries() {
        let notes = vec![Vec::new(), Vec::new(), Vec::new()];
        let hidden = vec![false, true, false];
        let o = Outline::new(Vec::new(), notes, hidden, false);
        assert_eq!(entry_indices(&o), [0, 2]);
    }

    #[test]
    fn detail_shows_all_entries() {
        let notes = vec![Vec::new(), Vec::new()];
        let hidden = vec![true, true];
        let mut o = Outline::new(Vec::new(), notes, hidden, false);
        assert_eq!(entry_indices(&o), [] as [usize; 0]);
        o.set_detail(true);
        assert_eq!(entry_indices(&o), [0, 1]);
    }

    #[test]
    fn expansion_round_trips() {
        let notes = vec![vec!["n1".to_string()], vec!["n2".to_string()]];
        let hidden = vec![false, false];
        let mut o = Outline::new(vec!["s1".to_string()], notes.clone(), hidden.clone(), false);
        let session_row = 0;
        let entry1_row = 2;
        o.toggle(session_row);
        o.toggle(entry1_row);
        let (session_expanded, entry_expanded) = o.expansion();

        let mut fresh = Outline::new(vec!["s1".to_string()], notes, hidden, false);
        fresh.set_expansion(session_expanded, entry_expanded);
        let expanded: Vec<bool> = fresh.rows().iter().map(|r| r.expanded).collect();
        // session (expanded), s1, entry 0 (expanded), n1, entry 1
        assert_eq!(expanded, [true, false, true, false, false]);
    }

    #[test]
    fn set_expansion_drops_stale_entries() {
        let notes = vec![vec!["n1".to_string()]];
        let mut o = Outline::new(Vec::new(), notes, vec![false], false);
        o.set_expansion(false, HashSet::from([0, 5]));
        let (_, entry_expanded) = o.expansion();
        assert_eq!(entry_expanded, HashSet::from([0]));
    }

    #[test]
    fn noted_entry_survives_filter() {
        let notes = vec![vec!["n1".to_string()], Vec::new()];
        let hidden = vec![true, true];
        let o = Outline::new(Vec::new(), notes, hidden, false);
        assert_eq!(entry_indices(&o), [0]);
    }
}
