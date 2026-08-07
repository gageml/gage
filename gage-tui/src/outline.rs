//! Outline tree state + visible-row projection.
//!
//! `Outline` owns the tree *shape* (what's expanded, what's visible). Row
//! content — labels for the session, entry titles, note names — is composed by
//! the renderer from the `Document`, keyed off `RowKind`.

use std::collections::HashSet;

pub struct Outline {
    entry_expanded: HashSet<usize>,
    /// `entry_note_ids[i]` is the ordered list of note ids attached to entry
    /// `i`'s line. Mutated when notes are added or removed; outline rebuilds
    /// from this projection.
    entry_note_ids: Vec<Vec<String>>,
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
    Entry { index: usize },
    Note { entry_index: usize, note_id: String },
}

pub enum CollapseOutcome {
    Collapsed,
    SelectParent(usize),
    None,
}

impl Outline {
    pub fn new(entry_note_ids: Vec<Vec<String>>) -> Self {
        let mut o = Self {
            entry_expanded: HashSet::new(),
            entry_note_ids,
            visible: Vec::new(),
        };
        o.rebuild();
        o
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
    pub fn reload(&mut self, entry_note_ids: Vec<Vec<String>>) {
        self.entry_expanded.retain(|i| *i < entry_note_ids.len());
        self.entry_note_ids = entry_note_ids;
        self.rebuild();
    }

    /// Append a note id under `entry_index`, ensure the entry is expanded so
    /// the new note is visible, and rebuild. Returns the visible-row index of
    /// the new note row.
    pub fn add_note(&mut self, entry_index: usize, note_id: String) -> Option<usize> {
        self.entry_note_ids
            .get_mut(entry_index)?
            .push(note_id.clone());
        self.entry_expanded.insert(entry_index);
        self.rebuild();
        self.visible.iter().position(|r| match &r.kind {
            RowKind::Note { note_id: id, .. } => id == &note_id,
            _ => false,
        })
    }

    /// Remove a note id wherever it appears, then rebuild. Returns the entry
    /// index that previously owned the note, if any.
    pub fn remove_note(&mut self, note_id: &str) -> Option<usize> {
        let mut found: Option<usize> = None;
        for (i, ids) in self.entry_note_ids.iter_mut().enumerate() {
            if let Some(pos) = ids.iter().position(|n| n == note_id) {
                ids.remove(pos);
                found = Some(i);
                break;
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
            RowKind::Session => return,
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
        rows.push(Row {
            level: 1,
            has_children: false,
            expanded: false,
            kind: RowKind::Session,
        });
        for (i, notes) in self.entry_note_ids.iter().enumerate() {
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
                            entry_index: i,
                            note_id: id.clone(),
                        },
                    });
                }
            }
        }
        self.visible = rows;
    }
}
