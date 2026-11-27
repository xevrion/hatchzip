//! The contents tree. An archive can hold a million entries, so the list view
//! builds rows on demand for what is visible instead of keeping a model per row.
//! The tree changes only the display, not the extraction plan. Rows can be
//! chosen for "Extract selected…", which extracts only those.

use crate::EntryRow;
use hatchzip_core::{archive::Kind, engine::Job, human};
use slint::{Model, ModelNotify, ModelTracker};
use std::cell::RefCell;
use std::collections::HashMap;

pub struct Entry {
    path: Box<str>,
    size: u64,
    kind: Kind,
}

pub fn collect(job: &Job) -> Vec<Entry> {
    job.units
        .iter()
        .rev()
        .flat_map(|u| &u.items)
        .map(|item| Entry {
            path: item.path.to_string_lossy().into(),
            size: item.size,
            kind: item.kind.clone(),
        })
        .collect()
}

#[derive(Default)]
struct Node {
    name: String,
    parent: usize,
    size: u64,
    dir: bool,
    link: bool,
    expanded: bool,
    /// Chosen for "Extract selected…". A folder brings everything inside.
    selected: bool,
    children: Vec<usize>,
}

/// What a drag out of the tree takes: each path with whether it is a
/// folder, the size of it all, and whether it is the selection.
pub type DragRows = (Vec<(String, bool)>, u64, bool);

pub struct Entries {
    nodes: Vec<Node>,
    visible: Vec<(usize, i32)>,
    /// The row a Shift-click selects from: the last one clicked.
    anchor: Option<usize>,
}

impl Entries {
    pub fn new(entries: Vec<Entry>) -> Self {
        let mut tree = Self {
            nodes: vec![Node {
                dir: true,
                ..Default::default()
            }],
            visible: Vec::new(),
            anchor: None,
        };
        let mut folders: HashMap<(usize, String), usize> = HashMap::new();
        for entry in entries {
            let parts: Vec<_> = entry
                .path
                .split(['/', '\\'])
                .filter(|p| !p.is_empty() && *p != ".")
                .collect();
            let mut parent = 0;
            let mut ancestors = vec![0];
            for (i, part) in parts.iter().enumerate() {
                let dir = i + 1 < parts.len() || entry.kind == Kind::Dir;
                let existing = dir
                    .then(|| folders.get(&(parent, (*part).to_string())).copied())
                    .flatten();
                let child = if let Some(child) = existing {
                    child
                } else {
                    let child = tree.nodes.len();
                    tree.nodes.push(Node {
                        name: (*part).to_string(),
                        parent,
                        dir,
                        ..Default::default()
                    });
                    tree.nodes[parent].children.push(child);
                    if dir {
                        folders.insert((parent, (*part).to_string()), child);
                    }
                    child
                };
                parent = child;
                if dir {
                    ancestors.push(child);
                }
            }
            if !parts.is_empty() && entry.kind != Kind::Dir {
                tree.nodes[parent].size = entry.size;
                tree.nodes[parent].link = entry.kind != Kind::File;
                for node in ancestors {
                    tree.nodes[node].size = tree.nodes[node].size.saturating_add(entry.size);
                }
            }
        }
        drop(folders);
        // Names are kept once in the tree. Sort each folder only after all
        // entries have been collected, then discard the temporary sort keys.
        for i in 0..tree.nodes.len() {
            let mut children = std::mem::take(&mut tree.nodes[i].children);
            children.sort_by_cached_key(|&child| {
                let node = &tree.nodes[child];
                (!node.dir, node.name.to_lowercase(), node.name.clone())
            });
            tree.nodes[i].children = children;
        }
        if let [root] = tree.nodes[0].children.as_slice() {
            let root = *root;
            if tree.nodes[root].dir {
                tree.nodes[root].expanded = true;
            }
        }
        tree.rebuild();
        tree
    }

    fn rebuild(&mut self) {
        self.visible.clear();
        let mut stack: Vec<_> = self.nodes[0]
            .children
            .iter()
            .rev()
            .map(|&n| (n, 0))
            .collect();
        while let Some((node, depth)) = stack.pop() {
            self.visible.push((node, depth));
            if self.nodes[node].expanded {
                stack.extend(
                    self.nodes[node]
                        .children
                        .iter()
                        .rev()
                        .map(|&n| (n, depth + 1)),
                );
            }
        }
    }

    pub fn toggle(&mut self, row: usize, expand: Option<bool>) -> Option<(bool, usize)> {
        let &(node, depth) = self.visible.get(row)?;
        if !self.nodes[node].dir {
            return None;
        }
        let expanded = expand.unwrap_or(!self.nodes[node].expanded);
        if self.nodes[node].expanded == expanded {
            return None;
        }
        self.nodes[node].expanded = expanded;
        if !expanded {
            let end = self.visible[row + 1..]
                .iter()
                .position(|&(_, d)| d <= depth)
                .map_or(self.visible.len(), |n| row + 1 + n);
            let count = end - row - 1;
            self.visible.drain(row + 1..end);
            Some((false, count))
        } else {
            let mut stack: Vec<_> = self.nodes[node]
                .children
                .iter()
                .rev()
                .map(|&n| (n, depth + 1))
                .collect();
            let mut added = Vec::new();
            while let Some((child, child_depth)) = stack.pop() {
                added.push((child, child_depth));
                if self.nodes[child].expanded {
                    stack.extend(
                        self.nodes[child]
                            .children
                            .iter()
                            .rev()
                            .map(|&n| (n, child_depth + 1)),
                    );
                }
            }
            let count = added.len();
            self.visible.splice(row + 1..row + 1, added);
            Some((true, count))
        }
    }

    /// Changes the selection with the row at `row`: `mode` 0 chooses
    /// nothing and only makes it where a Shift-click starts, 1 adds it or
    /// takes it out, 2 selects every row from the last one clicked to it,
    /// and 3 empties the selection. Returns the visible rows whose look
    /// changed.
    pub fn choose(&mut self, row: usize, mode: i32) -> Vec<usize> {
        let node = self.visible.get(row).map(|&(n, _)| n);
        if mode == 0 {
            self.anchor = node.or(self.anchor);
            return Vec::new();
        }
        let node = match (mode, node) {
            (3, _) => 0,
            (_, Some(n)) => n,
            (_, None) => return Vec::new(),
        };
        let before: Vec<bool> = self
            .visible
            .iter()
            .map(|&(n, _)| self.nodes[n].selected)
            .collect();
        match mode {
            1 => {
                self.nodes[node].selected = !self.nodes[node].selected;
                self.anchor = Some(node);
            }
            2 => {
                // The anchor may be inside a folder closed since; the range
                // then starts here.
                let from = self
                    .anchor
                    .and_then(|a| self.visible.iter().position(|&(n, _)| n == a))
                    .unwrap_or(row);
                self.clear_selection();
                for i in from.min(row)..=from.max(row) {
                    let n = self.visible[i].0;
                    self.nodes[n].selected = true;
                }
            }
            _ => self.clear_selection(),
        }
        before
            .iter()
            .enumerate()
            .filter(|&(i, was)| self.nodes[self.visible[i].0].selected != *was)
            .map(|(i, _)| i)
            .collect()
    }

    fn clear_selection(&mut self) {
        for n in &mut self.nodes {
            n.selected = false;
        }
    }

    /// How many paths [`Self::chosen`] gives, without building them.
    pub fn chosen_count(&self) -> usize {
        (1..self.nodes.len())
            .filter(|&i| self.nodes[i].selected && !self.inside_chosen(i))
            .count()
    }

    /// The paths to extract: every chosen row, except those inside a chosen
    /// folder, which comes with everything in it anyway.
    pub fn chosen(&self) -> Vec<String> {
        (1..self.nodes.len())
            .filter(|&i| self.nodes[i].selected && !self.inside_chosen(i))
            .map(|i| self.path(i))
            .collect()
    }

    /// What dragging the row at `row` takes: the whole selection when the
    /// row is part of it, as file managers do, and the row alone otherwise.
    /// Each path comes with whether it is a folder, then the size of it all
    /// and whether it was the selection.
    pub fn dragged(&self, row: usize) -> Option<DragRows> {
        let &(node, _) = self.visible.get(row)?;
        let chosen = self.nodes[node].selected || self.inside_chosen(node);
        let picked: Vec<usize> = if chosen {
            (1..self.nodes.len())
                .filter(|&i| self.nodes[i].selected && !self.inside_chosen(i))
                .collect()
        } else {
            vec![node]
        };
        let size = picked
            .iter()
            .fold(0u64, |sum, &i| sum.saturating_add(self.nodes[i].size));
        let paths = picked
            .iter()
            .map(|&i| (self.path(i), self.nodes[i].dir))
            .collect();
        Some((paths, size, chosen))
    }

    fn inside_chosen(&self, mut node: usize) -> bool {
        while node != 0 {
            node = self.nodes[node].parent;
            if self.nodes[node].selected {
                return true;
            }
        }
        false
    }

    // Only rows requested by ListView allocate a full path for accessibility.
    fn path(&self, mut node: usize) -> String {
        let mut parts = Vec::new();
        while node != 0 {
            parts.push(self.nodes[node].name.as_str());
            node = self.nodes[node].parent;
        }
        parts.reverse();
        parts.join("/")
    }

    #[cfg(test)]
    pub fn rows(&self) -> Vec<EntryRow> {
        self.visible
            .iter()
            .map(|&(i, depth)| {
                let n = &self.nodes[i];
                EntryRow {
                    name: n.name.as_str().into(),
                    path: self.path(i).into(),
                    size: if n.link {
                        "link".into()
                    } else {
                        human(n.size).into()
                    },
                    dir: n.dir,
                    link: n.link,
                    expanded: n.expanded,
                    depth,
                    selected: n.selected,
                }
            })
            .collect()
    }
}

pub struct EntriesModel {
    tree: RefCell<Entries>,
    notify: ModelNotify,
}

impl EntriesModel {
    pub fn new(tree: Entries) -> Self {
        Self {
            tree: RefCell::new(tree),
            notify: ModelNotify::default(),
        }
    }

    pub fn navigate(&self, row: usize, right: bool) -> usize {
        let tree = self.tree.borrow();
        let Some(&(node, depth)) = tree.visible.get(row) else {
            return row;
        };
        let current = &tree.nodes[node];
        match (right, current.expanded) {
            (true, true) if !current.children.is_empty() => row + 1,
            (false, false) if depth > 0 => {
                // Walk the visible indices without building any display rows.
                tree.visible[..row]
                    .iter()
                    .rposition(|&(id, _)| id == current.parent)
                    .unwrap_or(row)
            }
            _ if current.dir && right != current.expanded => {
                drop(tree);
                self.toggle(row, Some(right));
                row
            }
            _ => row,
        }
    }

    /// See [`Entries::choose`]. Returns how many rows are chosen now.
    pub fn choose(&self, row: usize, mode: i32) -> usize {
        let changed = self.tree.borrow_mut().choose(row, mode);
        for r in changed {
            self.notify.row_changed(r);
        }
        self.tree.borrow().chosen_count()
    }

    pub fn chosen(&self) -> Vec<String> {
        self.tree.borrow().chosen()
    }

    /// See [`Entries::dragged`].
    pub fn dragged(&self, row: usize) -> Option<DragRows> {
        self.tree.borrow().dragged(row)
    }

    pub fn toggle(&self, row: usize, expand: Option<bool>) {
        let change = self.tree.borrow_mut().toggle(row, expand);
        if let Some((added, count)) = change {
            if count > 0 {
                if added {
                    self.notify.row_added(row + 1, count);
                } else {
                    self.notify.row_removed(row + 1, count);
                }
            }
            self.notify.row_changed(row);
        }
    }
}

impl Model for EntriesModel {
    type Data = EntryRow;

    fn row_count(&self) -> usize {
        self.tree.borrow().visible.len()
    }

    fn row_data(&self, row: usize) -> Option<EntryRow> {
        let tree = self.tree.borrow();
        let &(i, depth) = tree.visible.get(row)?;
        let n = &tree.nodes[i];
        Some(EntryRow {
            name: n.name.as_str().into(),
            path: tree.path(i).into(),
            size: if n.link {
                "link".into()
            } else {
                human(n.size).into()
            },
            dir: n.dir,
            link: n.link,
            expanded: n.expanded,
            depth,
            selected: n.selected,
        })
    }

    fn model_tracker(&self) -> &dyn ModelTracker {
        &self.notify
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, size: u64) -> Entry {
        Entry {
            path: path.into(),
            size,
            kind: Kind::File,
        }
    }

    #[test]
    fn right_on_an_expanded_empty_folder_keeps_selection() {
        let model = EntriesModel::new(Entries::new(vec![
            Entry {
                path: "empty".into(),
                size: 0,
                kind: Kind::Dir,
            },
            file("sibling", 1),
        ]));
        assert_eq!(model.navigate(0, true), 0);
        assert!(model.row_data(0).unwrap().expanded);
        assert_eq!(model.navigate(0, true), 0);
        assert_eq!(model.row_data(1).unwrap().path, "sibling");
    }

    #[test]
    fn implicit_folders_sort_expand_and_sum() {
        let mut tree = Entries::new(vec![file("root/z.txt", 7), file("root/sub/a.txt", 3)]);
        let rows = tree.rows();
        assert_eq!(
            rows.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            ["root", "sub", "z.txt"]
        );
        assert_eq!(rows[0].size.as_str(), human(10));
        tree.toggle(1, Some(true));
        assert_eq!(
            tree.rows()
                .iter()
                .map(|r| r.name.as_str().to_string())
                .collect::<Vec<_>>(),
            ["root", "sub", "a.txt", "z.txt"]
        );
        tree.toggle(0, Some(false));
        assert_eq!(tree.rows().len(), 1);
    }

    #[test]
    fn multiple_roots_and_nested_expansion() {
        let mut tree = Entries::new(vec![
            file("folder/nested/file", 4),
            file("a.txt", 2),
            Entry {
                path: "folder".into(),
                size: 0,
                kind: Kind::Dir,
            },
        ]);
        assert_eq!(
            tree.rows()
                .iter()
                .map(|r| r.name.as_str().to_string())
                .collect::<Vec<_>>(),
            ["folder", "a.txt"]
        );
        tree.toggle(0, Some(true));
        tree.toggle(1, Some(true));
        tree.toggle(0, Some(false));
        tree.toggle(0, Some(true));
        assert_eq!(
            tree.rows()
                .iter()
                .map(|r| r.name.as_str().to_string())
                .collect::<Vec<_>>(),
            ["folder", "nested", "file", "a.txt"]
        );
    }

    #[test]
    fn repeated_names_keep_their_full_paths() {
        let model = EntriesModel::new(Entries::new(vec![
            file("root/a/bin/tool", 1),
            file("root/b/bin/tool", 2),
        ]));
        for path in ["root/a", "root/a/bin", "root/b", "root/b/bin"] {
            let row = (0..model.row_count())
                .find(|&row| model.row_data(row).unwrap().path == path)
                .unwrap();
            model.toggle(row, Some(true));
        }
        let paths: Vec<_> = (0..model.row_count())
            .map(|row| model.row_data(row).unwrap().path.to_string())
            .collect();
        assert_eq!(
            paths,
            [
                "root",
                "root/a",
                "root/a/bin",
                "root/a/bin/tool",
                "root/b",
                "root/b/bin",
                "root/b/bin/tool"
            ]
        );
    }

    #[test]
    fn names_sort_without_case_affecting_order() {
        let tree = Entries::new(vec![
            file("z.txt", 1),
            file("Beta.txt", 1),
            file("alpha.txt", 1),
        ]);
        assert_eq!(
            tree.rows()
                .iter()
                .map(|r| r.name.as_str().to_string())
                .collect::<Vec<_>>(),
            ["alpha.txt", "Beta.txt", "z.txt"]
        );
    }

    #[test]
    fn folder_total_includes_link_sizes() {
        let tree = Entries::new(vec![
            file("root/file", 5),
            Entry {
                path: "root/link".into(),
                size: 5,
                kind: Kind::HardLink("root/file".into()),
            },
        ]);
        assert_eq!(tree.rows()[0].size.as_str(), human(10));
        assert_eq!(tree.rows()[2].size.as_str(), "link");
        let model = EntriesModel::new(tree);
        assert!(!model.row_data(0).unwrap().link);
        assert!(!model.row_data(1).unwrap().link);
        assert!(model.row_data(2).unwrap().link);
    }

    #[test]
    fn rows_are_added_by_range_and_cleared() {
        let model = EntriesModel::new(Entries::new(vec![
            file("a.txt", 1),
            file("b.txt", 1),
            file("c.txt", 1),
            file("d/e.txt", 1),
        ]));
        let names = || {
            (0..model.row_count())
                .filter(|&r| model.row_data(r).unwrap().selected)
                .map(|r| model.row_data(r).unwrap().path.to_string())
                .collect::<Vec<_>>()
        };
        // The folder sorts first: d, a.txt, b.txt, c.txt.
        assert_eq!(model.choose(1, 0), 0, "a plain click chooses nothing");
        assert_eq!(model.choose(1, 1), 1);
        assert_eq!(model.choose(3, 1), 2);
        assert_eq!(names(), ["a.txt", "c.txt"]);
        assert_eq!(model.choose(3, 1), 1, "a second Ctrl-click takes it out");
        assert_eq!(model.choose(1, 0), 1, "nor does it clear the selection");
        assert_eq!(model.choose(3, 2), 3, "Shift selects from the click");
        assert_eq!(names(), ["a.txt", "b.txt", "c.txt"]);
        assert_eq!(model.choose(0, 3), 0, "Escape empties it");
        assert!(names().is_empty());
        assert_eq!(model.choose(0, 1), 1);
        assert_eq!(names(), ["d"]);
        // Inside a chosen folder, a chosen file adds nothing to extract.
        model.toggle(0, Some(true));
        assert_eq!(model.choose(1, 1), 1);
        assert_eq!(names(), ["d", "d/e.txt"]);
        assert_eq!(model.chosen(), ["d"]);
        // A closed folder keeps what was chosen inside it.
        model.choose(0, 1);
        model.toggle(0, Some(false));
        assert_eq!(model.chosen(), ["d/e.txt"]);
        assert!(names().is_empty());
        // Escape works with no row under the cursor too.
        assert_eq!(model.choose(99, 3), 0);
        assert!(model.chosen().is_empty());
    }

    #[test]
    fn dragging_takes_the_selection_or_the_row_alone() {
        let model = EntriesModel::new(Entries::new(vec![
            file("a.txt", 1),
            file("b.txt", 2),
            file("d/e.txt", 4),
        ]));
        // d, a.txt, b.txt; nothing chosen drags the row under the pointer.
        assert_eq!(
            model.dragged(1),
            Some((vec![("a.txt".to_string(), false)], 1, false))
        );
        assert_eq!(
            model.dragged(0),
            Some((vec![("d".to_string(), true)], 4, false))
        );
        model.choose(0, 1);
        model.choose(2, 1);
        let (mut paths, size, chosen) = model.dragged(2).unwrap();
        paths.sort();
        assert_eq!(
            paths,
            [("b.txt".to_string(), false), ("d".to_string(), true)]
        );
        assert_eq!((size, chosen), (6, true));
        // A row outside the selection goes alone, and leaves it as it is.
        assert_eq!(
            model.dragged(1),
            Some((vec![("a.txt".to_string(), false)], 1, false))
        );
        assert_eq!(model.chosen().len(), 2);
        // A row inside a chosen folder takes the selection with it.
        model.toggle(0, Some(true));
        assert_eq!(model.row_data(1).unwrap().path, "d/e.txt");
        assert!(model.dragged(1).unwrap().2);
        assert_eq!(model.dragged(99), None);
    }

    #[test]
    fn large_archive_starts_with_only_visible_rows() {
        let entries = (0..100_000)
            .map(|i| file(&format!("root/{i:06}/file"), 1))
            .collect();
        let tree = Entries::new(entries);
        assert_eq!(tree.visible.len(), 100_001);
        assert_eq!(tree.nodes[1].size, 100_000);
        assert!(tree.rows().iter().all(|r| r.name != "file"));
    }
}
