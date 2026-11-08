//! Outline tree state + visible-row projection.
//!
//! `Outline` owns the tree *shape* (what's expanded, what's visible). Row
//! content — labels for the session, entry titles, note names — is composed by
//! the renderer from the `Document`, keyed off `RowKind`.

use std::collections::HashSet;

pub struct Outline {
    session_expanded: bool,
    /// Ids of the attachments whose file rows are shown
    attachment_expanded: HashSet<String>,
    entry_expanded: HashSet<usize>,
    /// The attachments that target the session, in order. Rendered as
    /// children of the session row before its notes.
    attachments: Vec<AttachmentFiles>,
    /// Ordered note ids attached to the session itself (no line target).
    /// Rendered as children of the session row.
    session_note_ids: Vec<String>,
    /// `entry_note_ids[i]` is the ordered list of note ids attached to entry
    /// `i`'s line. Mutated when notes are added or removed; outline rebuilds
    /// from this projection.
    entry_note_ids: Vec<Vec<String>>,
    /// `entry_hidden[i]` marks entry `i` as filtered out, omitted from the
    /// rows unless the entry carries notes.
    entry_hidden: Vec<bool>,
    /// `dropped_anchors[j]` is the entry index the `j`th run of dropped
    /// lines precedes, `entry_hidden.len()` for a run after the last
    /// entry. Runs are in line order, so anchors never decrease.
    dropped_anchors: Vec<usize>,
    /// Whether the dropped runs have rows
    dropped_shown: bool,
    visible: Vec<Row>,
}

/// An attachment's rows: its id and its file paths in tree order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttachmentFiles {
    pub id: String,
    pub paths: Vec<String>,
}

/// The rows with children that are open.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Expansion {
    pub session: bool,
    pub attachments: HashSet<String>,
    pub entries: HashSet<usize>,
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
    /// A store attachment that targets the session, under the session row
    Attachment {
        attachment_id: String,
    },
    /// One file of an attachment, under its attachment row
    AttachmentFile {
        attachment_id: String,
        path: String,
    },
    Entry {
        index: usize,
    },
    /// `entry_index` is `None` for a session-level note under the session row
    Note {
        entry_index: Option<usize>,
        note_id: String,
    },
    /// The `index`th run of lines a publish dropped, shown in place
    Dropped {
        index: usize,
    },
}

pub enum CollapseOutcome {
    Collapsed,
    SelectParent(usize),
    None,
}

impl Outline {
    pub fn new(
        attachments: Vec<AttachmentFiles>,
        session_note_ids: Vec<String>,
        entry_note_ids: Vec<Vec<String>>,
        entry_hidden: Vec<bool>,
        dropped_anchors: Vec<usize>,
        dropped_shown: bool,
    ) -> Self {
        let mut o = Self {
            session_expanded: false,
            attachment_expanded: HashSet::new(),
            entry_expanded: HashSet::new(),
            attachments,
            session_note_ids,
            entry_note_ids,
            entry_hidden,
            dropped_anchors,
            dropped_shown,
            visible: Vec::new(),
        };
        o.rebuild();
        o
    }

    /// Replace the filter mask and rebuild, preserving notes and expansion.
    /// Used when a view-option toggle changes which entries qualify.
    pub fn set_hidden(&mut self, entry_hidden: Vec<bool>) {
        self.entry_hidden = entry_hidden;
        self.rebuild();
    }

    /// Show or hide the dropped runs and rebuild. Used when the
    /// changes view option toggles.
    pub fn set_dropped_shown(&mut self, shown: bool) {
        self.dropped_shown = shown;
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

    /// Replace the attachment/entry/note projection in place. Preserves
    /// expansion state (session, per-attachment, per-entry) so a reload
    /// doesn't collapse the tree. Attachment ids and entry indices that
    /// no longer exist drop out of the expanded sets.
    pub fn reload(
        &mut self,
        attachments: Vec<AttachmentFiles>,
        session_note_ids: Vec<String>,
        entry_note_ids: Vec<Vec<String>>,
        entry_hidden: Vec<bool>,
        dropped_anchors: Vec<usize>,
    ) {
        self.attachment_expanded
            .retain(|id| attachments.iter().any(|a| &a.id == id));
        self.entry_expanded.retain(|i| *i < entry_note_ids.len());
        self.attachments = attachments;
        self.session_note_ids = session_note_ids;
        self.entry_note_ids = entry_note_ids;
        self.entry_hidden = entry_hidden;
        self.dropped_anchors = dropped_anchors;
        self.rebuild();
    }

    /// Snapshot of the expansion state, for hosts that restore a
    /// previously viewed session's tree shape.
    pub fn expansion(&self) -> Expansion {
        Expansion {
            session: self.session_expanded,
            attachments: self.attachment_expanded.clone(),
            entries: self.entry_expanded.clone(),
        }
    }

    /// Replace the expansion state and rebuild. Attachment ids and entry
    /// indices beyond the current projection drop out, as in
    /// [`Outline::reload`].
    pub fn set_expansion(&mut self, expansion: Expansion) {
        self.session_expanded = expansion.session;
        self.attachment_expanded = expansion.attachments;
        self.entry_expanded = expansion.entries;
        self.attachment_expanded
            .retain(|id| self.attachments.iter().any(|a| &a.id == id));
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
            RowKind::Attachment { attachment_id } => {
                if expanded {
                    self.attachment_expanded.insert(attachment_id.clone());
                } else {
                    self.attachment_expanded.remove(attachment_id);
                }
            }
            RowKind::Entry { index } => {
                let index = *index;
                if expanded {
                    self.entry_expanded.insert(index);
                } else {
                    self.entry_expanded.remove(&index);
                }
            }
            RowKind::AttachmentFile { .. } | RowKind::Note { .. } | RowKind::Dropped { .. } => {
                return;
            }
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
        let session_has_children =
            !self.attachments.is_empty() || !self.session_note_ids.is_empty();
        let session_expanded = session_has_children && self.session_expanded;
        rows.push(Row {
            level: 1,
            has_children: session_has_children,
            expanded: session_expanded,
            kind: RowKind::Session,
        });
        if session_expanded {
            for attachment in &self.attachments {
                let has_children = !attachment.paths.is_empty();
                let expanded = has_children && self.attachment_expanded.contains(&attachment.id);
                rows.push(Row {
                    level: 2,
                    has_children,
                    expanded,
                    kind: RowKind::Attachment {
                        attachment_id: attachment.id.clone(),
                    },
                });
                if expanded {
                    for path in &attachment.paths {
                        rows.push(Row {
                            level: 3,
                            has_children: false,
                            expanded: false,
                            kind: RowKind::AttachmentFile {
                                attachment_id: attachment.id.clone(),
                                path: path.clone(),
                            },
                        });
                    }
                }
            }
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
        let mut dropped = self.dropped_anchors.iter().enumerate().peekable();
        let mut push_dropped_before = |entry: usize, rows: &mut Vec<Row>| {
            if !self.dropped_shown {
                return;
            }
            while let Some((index, _)) = dropped.next_if(|(_, anchor)| **anchor <= entry) {
                rows.push(Row {
                    level: 1,
                    has_children: false,
                    expanded: false,
                    kind: RowKind::Dropped { index },
                });
            }
        };
        for (i, notes) in self.entry_note_ids.iter().enumerate() {
            push_dropped_before(i, &mut rows);
            // A noted entry always shows: a note declares the entry
            // mattered to someone
            if self.entry_hidden.get(i).copied().unwrap_or(false) && notes.is_empty() {
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
        push_dropped_before(entry_count, &mut rows);
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
        let o = Outline::new(Vec::new(), Vec::new(), notes, hidden, Vec::new(), false);
        assert_eq!(entry_indices(&o), [0, 2]);
    }

    #[test]
    fn set_hidden_revises_the_filter() {
        let notes = vec![Vec::new(), Vec::new()];
        let hidden = vec![true, true];
        let mut o = Outline::new(Vec::new(), Vec::new(), notes, hidden, Vec::new(), false);
        assert_eq!(entry_indices(&o), [] as [usize; 0]);
        o.set_hidden(vec![false, false]);
        assert_eq!(entry_indices(&o), [0, 1]);
    }

    #[test]
    fn expansion_round_trips() {
        let notes = vec![vec!["n1".to_string()], vec!["n2".to_string()]];
        let hidden = vec![false, false];
        let attachments = vec![AttachmentFiles {
            id: "a1".to_string(),
            paths: vec!["f".to_string()],
        }];
        let mut o = Outline::new(
            attachments.clone(),
            vec!["s1".to_string()],
            notes.clone(),
            hidden.clone(),
            Vec::new(),
            false,
        );
        let session_row = 0;
        o.toggle(session_row);
        let attachment_row = 1;
        o.toggle(attachment_row);
        let entry1_row = 4;
        o.toggle(entry1_row);
        let expansion = o.expansion();

        let mut fresh = Outline::new(
            attachments,
            vec!["s1".to_string()],
            notes,
            hidden,
            Vec::new(),
            false,
        );
        fresh.set_expansion(expansion);
        let expanded: Vec<bool> = fresh.rows().iter().map(|r| r.expanded).collect();
        // session (expanded), a1 (expanded), f, s1, entry 0 (expanded),
        // n1, entry 1
        assert_eq!(expanded, [true, true, false, false, true, false, false]);
    }

    #[test]
    fn set_expansion_drops_stale_entries() {
        let notes = vec![vec!["n1".to_string()]];
        let mut o = Outline::new(
            Vec::new(),
            Vec::new(),
            notes,
            vec![false],
            Vec::new(),
            false,
        );
        o.set_expansion(Expansion {
            session: false,
            attachments: HashSet::from(["gone".to_string()]),
            entries: HashSet::from([0, 5]),
        });
        let expansion = o.expansion();
        assert_eq!(expansion.entries, HashSet::from([0]));
        assert!(expansion.attachments.is_empty());
    }

    /// The session row's children are its attachments, each opening
    /// to its files, then its notes. A reload keeps an attachment open
    /// while it remains and forgets one that is gone.
    #[test]
    fn attachments_precede_session_notes_and_open_to_their_files() {
        let attachments = vec![
            AttachmentFiles {
                id: "a1".to_string(),
                paths: vec!["settings.json".to_string(), "skills/x.md".to_string()],
            },
            AttachmentFiles {
                id: "a2".to_string(),
                paths: Vec::new(),
            },
        ];
        let mut o = Outline::new(
            attachments,
            vec!["s1".to_string()],
            vec![Vec::new()],
            vec![false],
            Vec::new(),
            false,
        );
        assert_eq!(kinds(&o), ["session", "entry 0"]);
        assert!(o.rows()[0].has_children);
        o.toggle(0);
        assert_eq!(
            kinds(&o),
            [
                "session",
                "attachment a1",
                "attachment a2",
                "note",
                "entry 0"
            ]
        );
        assert!(!o.toggle(2), "an attachment without files has no children");
        o.toggle(1);
        assert_eq!(
            kinds(&o),
            [
                "session",
                "attachment a1",
                "file a1 settings.json",
                "file a1 skills/x.md",
                "attachment a2",
                "note",
                "entry 0",
            ]
        );
        let levels: Vec<usize> = o.rows().iter().map(|r| r.level).collect();
        assert_eq!(levels, [1, 2, 3, 3, 2, 2, 1]);
        assert!(matches!(o.collapse(2), CollapseOutcome::SelectParent(1)));

        o.reload(
            vec![AttachmentFiles {
                id: "a1".to_string(),
                paths: vec!["settings.json".to_string()],
            }],
            Vec::new(),
            vec![Vec::new()],
            vec![false],
            Vec::new(),
        );
        assert_eq!(
            kinds(&o),
            [
                "session",
                "attachment a1",
                "file a1 settings.json",
                "entry 0"
            ]
        );
        assert_eq!(o.expansion().attachments, HashSet::from(["a1".to_string()]));

        o.reload(
            Vec::new(),
            Vec::new(),
            vec![Vec::new()],
            vec![false],
            Vec::new(),
        );
        assert_eq!(kinds(&o), ["session", "entry 0"]);
        assert!(!o.rows()[0].has_children);
        assert!(o.expansion().attachments.is_empty());
    }

    fn kinds(o: &Outline) -> Vec<String> {
        o.rows()
            .iter()
            .map(|r| match &r.kind {
                RowKind::Session => "session".to_string(),
                RowKind::Attachment { attachment_id } => format!("attachment {attachment_id}"),
                RowKind::AttachmentFile {
                    attachment_id,
                    path,
                } => {
                    format!("file {attachment_id} {path}")
                }
                RowKind::Entry { index } => format!("entry {index}"),
                RowKind::Note { .. } => "note".to_string(),
                RowKind::Dropped { index } => format!("dropped {index}"),
            })
            .collect()
    }

    /// A dropped run's row sits before its anchor entry, after the
    /// last entry when anchored past the end, and only while shown.
    /// A run anchored to a hidden entry still shows in its place.
    #[test]
    fn dropped_runs_sit_before_their_anchor_entries() {
        let notes = vec![Vec::new(), Vec::new(), Vec::new()];
        let hidden = vec![false, true, false];
        let mut o = Outline::new(
            Vec::new(),
            Vec::new(),
            notes,
            hidden,
            vec![0, 1, 1, 3],
            false,
        );
        assert_eq!(kinds(&o), ["session", "entry 0", "entry 2"]);
        o.set_dropped_shown(true);
        assert_eq!(
            kinds(&o),
            [
                "session",
                "dropped 0",
                "entry 0",
                "dropped 1",
                "dropped 2",
                "entry 2",
                "dropped 3",
            ]
        );
        assert!(!o.toggle(1), "a dropped row has no children");
        o.set_dropped_shown(false);
        assert_eq!(kinds(&o), ["session", "entry 0", "entry 2"]);
    }

    #[test]
    fn noted_entry_survives_filter() {
        let notes = vec![vec!["n1".to_string()], Vec::new()];
        let hidden = vec![true, true];
        let o = Outline::new(Vec::new(), Vec::new(), notes, hidden, Vec::new(), false);
        assert_eq!(entry_indices(&o), [0]);
    }
}
