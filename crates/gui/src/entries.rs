//! The contents tree. An archive can hold a million entries, so the list view
//! builds rows on demand for what is visible instead of keeping a model per row.
//! The tree changes only the display, not the extraction plan.

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
    children: Vec<usize>,
}

pub struct Entries {
    nodes: Vec<Node>,
    visible: Vec<(usize, i32)>,
}

impl Entries {
    pub fn new(entries: Vec<Entry>) -> Self {
        let mut tree = Self {
            nodes: vec![Node {
                dir: true,
                ..Default::default()
            }],
            visible: Vec::new(),
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
                    expanded: n.expanded,
                    depth,
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
            expanded: n.expanded,
            depth,
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
