//! Lazy tree state + visible-row projection.
//!
//! `Tree` owns the shape of an n-level tree whose children are loaded
//! on first expansion: which nodes exist, which are expandable, which
//! are expanded, and which have had their children supplied. Row
//! content is composed by the renderer from the node data. Each node
//! carries a stable string key so an identity-stable table can follow
//! the selection across rebuilds.

pub(crate) struct Tree<T> {
    nodes: Vec<Node<T>>,
    roots: Vec<usize>,
    visible: Vec<Row>,
}

struct Node<T> {
    key: String,
    data: T,
    parent: Option<usize>,
    expandable: bool,
    expanded: bool,
    /// `None` until the children have been supplied
    children: Option<Vec<usize>>,
}

/// One visible row of the projection
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Row {
    pub node: usize,
    /// Root rows are level 0
    pub level: usize,
    pub expandable: bool,
    pub expanded: bool,
}

/// What an expand request needs from the caller
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Expand {
    /// The node's children are not loaded. Supply them with
    /// [`Tree::set_children`], which also expands the node.
    NeedsChildren(usize),
    Expanded,
    /// The row is a leaf or already expanded
    Nothing,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Collapse {
    Collapsed,
    /// The row is a leaf or already collapsed: the caller may move
    /// the selection to this parent row index
    SelectParent(usize),
    Nothing,
}

impl<T> Tree<T> {
    pub fn new() -> Self {
        Self {
            nodes: Vec::new(),
            roots: Vec::new(),
            visible: Vec::new(),
        }
    }

    pub fn clear(&mut self) {
        self.nodes.clear();
        self.roots.clear();
        self.visible.clear();
    }

    /// Append a root node. Returns its id.
    pub fn add_root(&mut self, key: String, data: T, expandable: bool) -> usize {
        let id = self.push(key, data, None, expandable);
        self.roots.push(id);
        self.rebuild();
        id
    }

    /// Supply `node`'s children in display order and expand it.
    pub fn set_children(&mut self, node: usize, children: Vec<(String, T, bool)>) {
        let ids: Vec<usize> = children
            .into_iter()
            .map(|(key, data, expandable)| self.push(key, data, Some(node), expandable))
            .collect();
        if let Some(n) = self.nodes.get_mut(node) {
            n.children = Some(ids);
            n.expanded = true;
        }
        self.rebuild();
    }

    pub fn rows(&self) -> &[Row] {
        &self.visible
    }

    pub fn row(&self, idx: usize) -> Option<&Row> {
        self.visible.get(idx)
    }

    pub fn data(&self, node: usize) -> Option<&T> {
        self.nodes.get(node).map(|n| &n.data)
    }

    pub fn key(&self, node: usize) -> Option<&str> {
        self.nodes.get(node).map(|n| n.key.as_str())
    }

    /// Keys of the visible rows, in order
    pub fn visible_keys(&self) -> Vec<&str> {
        self.visible
            .iter()
            .filter_map(|r| self.key(r.node))
            .collect()
    }

    pub fn toggle(&mut self, idx: usize) -> Expand {
        match self.visible.get(idx) {
            Some(row) if row.expanded => {
                self.set_expanded(row.node, false);
                Expand::Nothing
            }
            _ => self.expand(idx),
        }
    }

    pub fn expand(&mut self, idx: usize) -> Expand {
        let Some(row) = self.visible.get(idx) else {
            return Expand::Nothing;
        };
        if !row.expandable || row.expanded {
            return Expand::Nothing;
        }
        let node = row.node;
        match self.nodes.get(node).and_then(|n| n.children.as_ref()) {
            None => Expand::NeedsChildren(node),
            Some(_) => {
                self.set_expanded(node, true);
                Expand::Expanded
            }
        }
    }

    pub fn collapse(&mut self, idx: usize) -> Collapse {
        let Some(row) = self.visible.get(idx) else {
            return Collapse::Nothing;
        };
        if row.expandable && row.expanded {
            self.set_expanded(row.node, false);
            return Collapse::Collapsed;
        }
        match self.parent_row(idx) {
            Some(parent) => Collapse::SelectParent(parent),
            None => Collapse::Nothing,
        }
    }

    fn push(&mut self, key: String, data: T, parent: Option<usize>, expandable: bool) -> usize {
        self.nodes.push(Node {
            key,
            data,
            parent,
            expandable,
            expanded: false,
            children: None,
        });
        self.nodes.len() - 1
    }

    fn set_expanded(&mut self, node: usize, expanded: bool) {
        if let Some(n) = self.nodes.get_mut(node) {
            n.expanded = expanded;
        }
        self.rebuild();
    }

    /// Visible row index of the row's parent node
    fn parent_row(&self, idx: usize) -> Option<usize> {
        let node = self.visible.get(idx)?.node;
        let parent = self.nodes.get(node)?.parent?;
        self.visible.iter().position(|r| r.node == parent)
    }

    fn rebuild(&mut self) {
        let mut rows = Vec::new();
        for &root in &self.roots {
            self.push_rows(root, 0, &mut rows);
        }
        self.visible = rows;
    }

    fn push_rows(&self, node: usize, level: usize, rows: &mut Vec<Row>) {
        let Some(n) = self.nodes.get(node) else {
            return;
        };
        let expanded = n.expandable && n.expanded && n.children.is_some();
        rows.push(Row {
            node,
            level,
            expandable: n.expandable,
            expanded,
        });
        if expanded {
            for &child in n.children.iter().flatten() {
                self.push_rows(child, level + 1, rows);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels<'a>(tree: &Tree<&'a str>) -> Vec<(usize, &'a str)> {
        tree.rows()
            .iter()
            .map(|r| (r.level, *tree.data(r.node).unwrap()))
            .collect()
    }

    #[test]
    fn expand_asks_for_children_once() {
        let mut tree = Tree::new();
        let root = tree.add_root("a".into(), "a", true);
        assert_eq!(tree.expand(0), Expand::NeedsChildren(root));
        tree.set_children(
            root,
            vec![("a/x".into(), "x", false), ("a/y".into(), "y", true)],
        );
        assert_eq!(labels(&tree), [(0, "a"), (1, "x"), (1, "y")]);
        assert_eq!(tree.collapse(0), Collapse::Collapsed);
        assert_eq!(labels(&tree), [(0, "a")]);
        assert_eq!(tree.expand(0), Expand::Expanded);
        assert_eq!(labels(&tree), [(0, "a"), (1, "x"), (1, "y")]);
    }

    #[test]
    fn leaf_collapse_selects_parent() {
        let mut tree = Tree::new();
        let root = tree.add_root("a".into(), "a", true);
        tree.set_children(root, vec![("a/x".into(), "x", false)]);
        assert_eq!(tree.expand(1), Expand::Nothing);
        assert_eq!(tree.collapse(1), Collapse::SelectParent(0));
        assert_eq!(tree.collapse(0), Collapse::Collapsed);
        assert_eq!(tree.collapse(0), Collapse::Nothing);
    }

    #[test]
    fn toggle_round_trips_and_nested_state_survives_collapse() {
        let mut tree = Tree::new();
        let root = tree.add_root("a".into(), "a", true);
        tree.set_children(root, vec![("a/d".into(), "d", true)]);
        let d = tree.row(1).unwrap().node;
        tree.set_children(d, vec![("a/d/f".into(), "f", false)]);
        assert_eq!(labels(&tree), [(0, "a"), (1, "d"), (2, "f")]);
        assert_eq!(tree.toggle(0), Expand::Nothing);
        assert_eq!(labels(&tree), [(0, "a")]);
        assert_eq!(tree.toggle(0), Expand::Expanded);
        assert_eq!(labels(&tree), [(0, "a"), (1, "d"), (2, "f")]);
        assert_eq!(tree.visible_keys(), ["a", "a/d", "a/d/f"]);
    }
}
