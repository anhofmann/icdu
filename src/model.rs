//! The in-memory tree owned by the UI thread.
//!
//! The scanner streams [`Batch`]es of newly discovered entries; [`Tree::apply`] grafts them in
//! and propagates their sizes up every ancestor so totals are always live and correct, even
//! mid-scan. Batches that arrive before their parent node exists (which channel ordering should
//! prevent, but we don't rely on it) are buffered and flushed once the parent appears.
//!
//! Sizes that don't "count" — hard-link duplicates and excluded entries — keep their real own
//! size for display but contribute zero to the aggregated totals, so a tree's grand total
//! dedupes hard links and ignores excluded subtrees, matching ncdu.
//!
//! Every entry, counted or not, is an *item*: each node also carries the number of entries in
//! its subtree, maintained alongside the sizes so item counts are live and cost no walk.
//!
//! Memory: there is one [`Node`] per filesystem entry, so the struct is kept lean. Names use
//! `CompactString` (inlined when ≤24 bytes, the common case, so most entries allocate nothing).
//! A node's *own* size is derived from its children rather than stored, and the scan-only
//! `dir_index` map is released once scanning finishes.

use std::collections::HashMap;
use std::path::MAIN_SEPARATOR;

use compact_str::CompactString;

use crate::scan::Batch;

pub type NodeIdx = usize;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    Dir,
    File,
    Link,
    /// Anything else: socket, fifo, device, ...
    Other,
}

/// Why an entry was not descended into (and excluded from totals).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Excluded {
    No,
    /// Matched a `--exclude` pattern.
    Pattern,
    /// On a different filesystem and `-x` was given.
    OtherFs,
    /// Outside the `--older-than`/`--newer-than` mtime window.
    Age,
}

pub struct Node {
    pub name: CompactString,
    pub kind: NodeKind,
    /// Aggregated, *counted* logical size (own entry + everything beneath it that counts).
    pub apparent: u64,
    /// Aggregated, *counted* on-disk size.
    pub disk: u64,
    pub parent: Option<NodeIdx>,
    pub children: Vec<NodeIdx>,
    pub dev: u64,
    pub ino: u64,
    /// Link count > 1 (a hard-link candidate).
    pub hlink: bool,
    /// A hard-link whose inode was already counted elsewhere — excluded from totals.
    pub shared: bool,
    pub excluded: Excluded,
    pub read_error: bool,
    /// True once this directory's own `readdir` has been applied.
    pub scanned: bool,
    /// Number of directories in this subtree (including self) not yet `readdir`-ed.
    /// A subtree is fully scanned — and only then safe to delete — when this hits 0.
    pub unscanned: u32,
    /// Number of entries in this subtree, not counting the node itself (ncdu's item count).
    /// Every entry counts, including hard-link duplicates and excluded entries. Saturating.
    pub items: u32,
}

impl Node {
    pub fn is_dir(&self) -> bool {
        matches!(self.kind, NodeKind::Dir)
    }

    /// Does this entry contribute to aggregated totals?
    pub fn counts(&self) -> bool {
        !self.shared && self.excluded == Excluded::No
    }
}

pub struct Tree {
    pub nodes: Vec<Node>,
    pub root: NodeIdx,
    /// Maps a scanner directory id to its node index. Used only while scanning; released by
    /// [`Tree::finish_scan`].
    dir_index: HashMap<u64, NodeIdx>,
    /// Batches whose parent hasn't been created yet, keyed by `parent_id`.
    orphans: HashMap<u64, Vec<Batch>>,
    pub total_files: u64,
    pub total_dirs: u64,
    /// How many times a subtree has been detached (deleted or torn down for a rescan). While
    /// this is 0 every node is attached, so callers can skip [`Tree::is_attached`].
    pub detached: u64,
}

impl Tree {
    /// Build a tree containing only the (directory) root, using its own stat info.
    pub fn new(root_name: CompactString, apparent: u64, disk: u64, dev: u64, ino: u64) -> Self {
        let root = Node {
            name: root_name,
            kind: NodeKind::Dir,
            apparent,
            disk,
            parent: None,
            children: Vec::new(),
            dev,
            ino,
            hlink: false,
            shared: false,
            excluded: Excluded::No,
            read_error: false,
            scanned: false,
            // The root itself is an unscanned directory until its batch arrives.
            unscanned: 1,
            items: 0,
        };
        let mut dir_index = HashMap::new();
        dir_index.insert(0, 0);
        Tree {
            nodes: vec![root],
            root: 0,
            dir_index,
            orphans: HashMap::new(),
            total_dirs: 1,
            total_files: 0,
            detached: 0,
        }
    }

    /// Graft a scanner batch into the tree.
    pub fn apply(&mut self, batch: Batch) {
        let Some(&parent_idx) = self.dir_index.get(&batch.parent_id) else {
            // Parent not created yet — stash and retry later.
            self.orphans.entry(batch.parent_id).or_default().push(batch);
            return;
        };
        self.graft(parent_idx, batch);
    }

    /// Release scan-only bookkeeping once no more batches will arrive. Frees the per-directory
    /// `dir_index` map, which can be a large fraction of memory on directory-heavy trees.
    pub fn finish_scan(&mut self) {
        self.dir_index = HashMap::new();
        self.orphans = HashMap::new();
    }

    fn graft(&mut self, parent_idx: NodeIdx, batch: Batch) {
        if !self.nodes[parent_idx].scanned {
            self.nodes[parent_idx].scanned = true;
            // This directory is now read: one fewer unscanned dir in every ancestor's subtree.
            self.bump_unscanned(parent_idx, -1);
        }
        if batch.error.is_some() {
            self.nodes[parent_idx].read_error = true;
        }

        let mut sum_apparent = 0u64;
        let mut sum_disk = 0u64;
        // Every entry is an item, whether or not its size counts.
        let new_items = u32::try_from(batch.nodes.len()).unwrap_or(u32::MAX);
        let mut new_dir_ids = Vec::new();
        let mut new_subdirs = Vec::new();

        // Each dir receives exactly one batch (one readdir), so reserve children exactly to
        // avoid the Vec's growth slack.
        self.nodes[parent_idx]
            .children
            .reserve_exact(batch.nodes.len());

        for nn in batch.nodes {
            let idx = self.nodes.len();
            let is_dir = matches!(nn.kind, NodeKind::Dir);
            if is_dir {
                self.total_dirs += 1;
            } else {
                self.total_files += 1;
            }

            let counts = !nn.shared && nn.excluded == Excluded::No;
            if counts {
                sum_apparent += nn.apparent;
                sum_disk += nn.disk;
            }

            self.nodes.push(Node {
                name: nn.name,
                kind: nn.kind,
                apparent: nn.apparent,
                disk: nn.disk,
                parent: Some(parent_idx),
                children: Vec::new(),
                dev: nn.dev,
                ino: nn.ino,
                hlink: nn.hlink,
                shared: nn.shared,
                excluded: nn.excluded,
                read_error: nn.read_error,
                scanned: false,
                unscanned: 0,
                items: 0,
            });
            self.nodes[parent_idx].children.push(idx);

            if let Some(id) = nn.dir_id {
                // A directory we will recurse into: register it and mark it unscanned.
                self.dir_index.insert(id, idx);
                new_dir_ids.push(id);
                new_subdirs.push(idx);
            }
        }

        // Each recursable subdir adds one unscanned dir to itself and every ancestor.
        for idx in new_subdirs {
            self.bump_unscanned(idx, 1);
        }

        // Push the children's combined counted size and their item count up through every
        // ancestor.
        self.add_totals(parent_idx, sum_apparent, sum_disk, new_items);

        // Flush any batches that were waiting on directories we just created.
        for id in new_dir_ids {
            if let Some(waiting) = self.orphans.remove(&id) {
                let child_idx = self.dir_index[&id];
                for b in waiting {
                    self.graft(child_idx, b);
                }
            }
        }
    }

    /// Add counted sizes and an item count to `start` and every ancestor, in one walk.
    ///
    /// Sizes stop at a node that does not count (see [`Node::counts`]): an excluded directory
    /// shows what it holds itself, but nothing below it reaches its ancestors. Items go all
    /// the way up, because every entry is an item.
    fn add_totals(&mut self, start: NodeIdx, mut apparent: u64, mut disk: u64, items: u32) {
        let mut cur = Some(start);
        while let Some(i) = cur {
            let n = &mut self.nodes[i];
            n.apparent += apparent;
            n.disk += disk;
            n.items = n.items.saturating_add(items);
            if !n.counts() {
                (apparent, disk) = (0, 0);
            }
            cur = n.parent;
        }
    }

    /// The inverse of [`Tree::add_totals`], saturating at zero, with the same stop for sizes.
    fn sub_totals(&mut self, start: NodeIdx, mut apparent: u64, mut disk: u64, items: u32) {
        let mut cur = Some(start);
        while let Some(i) = cur {
            let n = &mut self.nodes[i];
            n.apparent = n.apparent.saturating_sub(apparent);
            n.disk = n.disk.saturating_sub(disk);
            n.items = n.items.saturating_sub(items);
            if !n.counts() {
                (apparent, disk) = (0, 0);
            }
            cur = n.parent;
        }
    }

    fn bump_unscanned(&mut self, start: NodeIdx, delta: i32) {
        let mut cur = Some(start);
        while let Some(i) = cur {
            let v = self.nodes[i].unscanned as i64 + delta as i64;
            self.nodes[i].unscanned = v.max(0) as u32;
            cur = self.nodes[i].parent;
        }
    }

    /// This entry's own size, excluding children — derived rather than stored.
    /// (`apparent` of a node equals its own size plus the counted sizes of its children.)
    pub fn own_apparent(&self, idx: NodeIdx) -> u64 {
        let n = &self.nodes[idx];
        let kids: u64 = n
            .children
            .iter()
            .filter(|&&c| self.nodes[c].counts())
            .map(|&c| self.nodes[c].apparent)
            .sum();
        n.apparent.saturating_sub(kids)
    }

    pub fn own_disk(&self, idx: NodeIdx) -> u64 {
        let n = &self.nodes[idx];
        let kids: u64 = n
            .children
            .iter()
            .filter(|&&c| self.nodes[c].counts())
            .map(|&c| self.nodes[c].disk)
            .sum();
        n.disk.saturating_sub(kids)
    }

    /// True if this entire subtree has been fully scanned (every descendant directory read).
    /// Files are trivially complete. This is the gate for deletion.
    pub fn subtree_complete(&self, idx: NodeIdx) -> bool {
        self.nodes[idx].unscanned == 0
    }

    /// Remove a fully-scanned subtree from the tree (after it has been removed from disk).
    /// Returns the number of (dirs, files) removed.
    pub fn delete_subtree(&mut self, idx: NodeIdx) -> (u64, u64) {
        // Detach from parent and subtract its counted size and its items (the node itself plus
        // everything below it) from all ancestors.
        if let Some(parent) = self.nodes[idx].parent {
            self.nodes[parent].children.retain(|&c| c != idx);
            let n = &self.nodes[idx];
            let (ap, dk) = if n.counts() {
                (n.apparent, n.disk)
            } else {
                (0, 0)
            };
            let items = n.items.saturating_add(1);
            self.sub_totals(parent, ap, dk, items);
        }
        // Cut the upward link too, so `is_attached` can tell the tombstones apart.
        self.nodes[idx].parent = None;
        self.detached += 1;
        // Walk the subtree to update global counts (nodes become unreachable tombstones).
        let mut dirs = 0u64;
        let mut files = 0u64;
        let mut stack = vec![idx];
        while let Some(n) = stack.pop() {
            if self.nodes[n].is_dir() {
                dirs += 1;
            } else {
                files += 1;
            }
            stack.extend_from_slice(&self.nodes[n].children);
        }
        self.total_dirs = self.total_dirs.saturating_sub(dirs);
        self.total_files = self.total_files.saturating_sub(files);
        (dirs, files)
    }

    /// Prepare a subtree for an in-place rescan (`r`): its children are detached (and dropped
    /// from the global dir/file counts), its aggregates are reset to its own size and zero
    /// items — so every ancestor's total stays correct while the rescan streams back in — and
    /// the subtree is marked unscanned again. `id_base` registers the node as the root of a new
    /// scan generation (see `scan::start_at`); batches from that scan graft back into this node.
    pub fn begin_refresh(&mut self, idx: NodeIdx, id_base: u64) {
        debug_assert!(self.nodes[idx].is_dir());
        debug_assert!(
            self.subtree_complete(idx),
            "only a fully scanned subtree may be rescanned"
        );
        // Own size first: the aggregate minus the counted children is this node's own size,
        // which is what the reset aggregate becomes.
        let (own_apparent, own_disk) = (self.own_apparent(idx), self.own_disk(idx));

        let mut stack = std::mem::take(&mut self.nodes[idx].children);
        // Cut the children's upward links, so `is_attached` can tell the tombstones apart.
        for &c in &stack {
            self.nodes[c].parent = None;
        }
        self.detached += 1;
        let mut dirs = 0u64;
        let mut files = 0u64;
        while let Some(n) = stack.pop() {
            if self.nodes[n].is_dir() {
                dirs += 1;
            } else {
                files += 1;
            }
            // Detach the grandchildren too: the nodes stay as tombstones (like delete), but
            // they no longer hold on to their subtree.
            let grandkids = std::mem::take(&mut self.nodes[n].children);
            stack.extend_from_slice(&grandkids);
        }
        self.total_dirs = self.total_dirs.saturating_sub(dirs);
        self.total_files = self.total_files.saturating_sub(files);

        // The detached children's counted sizes and items are baked into this directory's
        // aggregate and every ancestor's — remove them, so totals stay correct while the
        // rescan streams them back in. That leaves the directory with its own size and no
        // items. `sub_totals` mirrors how they were added, including where sizes stop.
        let n = &self.nodes[idx];
        let removed_apparent = n.apparent.saturating_sub(own_apparent);
        let removed_disk = n.disk.saturating_sub(own_disk);
        let removed_items = n.items;
        self.sub_totals(idx, removed_apparent, removed_disk, removed_items);
        let n = &mut self.nodes[idx];
        n.scanned = false;
        n.read_error = false;
        // This directory is unread again, and every ancestor has to count it: the batch that
        // reads it takes one off the whole chain (see `graft`), and until then nothing above
        // it may pass the delete/rescan gate.
        self.bump_unscanned(idx, 1);
        self.dir_index.insert(id_base, idx);
    }

    /// True if `idx` is still part of the tree: its parent chain reaches the root. Deleted and
    /// rescanned-away nodes stay in `nodes` as tombstones, but their chain ends early.
    pub fn is_attached(&self, idx: NodeIdx) -> bool {
        let mut cur = idx;
        while let Some(p) = self.nodes[cur].parent {
            cur = p;
        }
        cur == self.root
    }

    /// True if `idx` or one of its ancestors is excluded (`--exclude`, `-x`, the mtime
    /// filters): the scan left that part of the filesystem out on purpose.
    pub fn in_excluded(&self, idx: NodeIdx) -> bool {
        let mut cur = Some(idx);
        while let Some(i) = cur {
            if self.nodes[i].excluded != Excluded::No {
                return true;
            }
            cur = self.nodes[i].parent;
        }
        false
    }

    /// True if `idx` is `ancestor` itself or lies somewhere below it.
    pub fn is_within(&self, idx: NodeIdx, ancestor: NodeIdx) -> bool {
        let mut cur = Some(idx);
        while let Some(i) = cur {
            if i == ancestor {
                return true;
            }
            cur = self.nodes[i].parent;
        }
        false
    }

    /// The path of a node relative to the root (`sub/dir/file`), for compact display. Empty
    /// for the root itself.
    pub fn rel_path_of(&self, idx: NodeIdx) -> String {
        let mut parts = Vec::new();
        let mut cur = idx;
        while cur != self.root {
            parts.push(self.nodes[cur].name.as_str());
            match self.nodes[cur].parent {
                Some(p) => cur = p,
                None => break,
            }
        }
        parts.reverse();
        parts.join("/")
    }

    /// The path of a node for acting on the filesystem (delete, open, shell, rescan, stat), or
    /// `None` if the node is no longer part of the tree. A tombstone's parent chain ends
    /// before the root, so [`Tree::path_of`] gives a *relative* path for it, which the OS
    /// would resolve against the working directory and so reach an unrelated file.
    pub fn fs_path_of(&self, idx: NodeIdx) -> Option<String> {
        self.is_attached(idx).then(|| self.path_of(idx))
    }

    /// Build the path of a node for display, walking back to the root (whose name is
    /// absolute). Anything that touches the filesystem goes through [`Tree::fs_path_of`].
    pub fn path_of(&self, idx: NodeIdx) -> String {
        let mut parts = Vec::new();
        let mut cur = Some(idx);
        while let Some(i) = cur {
            parts.push(self.nodes[i].name.as_str());
            cur = self.nodes[i].parent;
        }
        parts.reverse();
        // The root name is already an absolute path; join the rest with separators.
        let mut out = String::new();
        for (i, p) in parts.iter().enumerate() {
            if i > 0 && !out.ends_with(MAIN_SEPARATOR) {
                out.push(MAIN_SEPARATOR);
            }
            out.push_str(p);
        }
        out
    }

    // ---- Import support (see dump.rs) -------------------------------------------------

    /// Create a tree whose root carries imported metadata; it is considered fully scanned.
    pub fn new_imported(name: CompactString, apparent: u64, disk: u64, dev: u64, ino: u64) -> Self {
        let mut t = Tree::new(name, apparent, disk, dev, ino);
        t.nodes[t.root].scanned = true;
        t.nodes[t.root].unscanned = 0;
        t
    }

    /// Append an imported child under `parent`, propagating its counted size and its item.
    /// Returns its index.
    #[allow(clippy::too_many_arguments)]
    pub fn import_child(
        &mut self,
        parent: NodeIdx,
        name: CompactString,
        kind: NodeKind,
        apparent: u64,
        disk: u64,
        dev: u64,
        ino: u64,
        hlink: bool,
        shared: bool,
        excluded: Excluded,
        read_error: bool,
    ) -> NodeIdx {
        let idx = self.nodes.len();
        if matches!(kind, NodeKind::Dir) {
            self.total_dirs += 1;
        } else {
            self.total_files += 1;
        }
        self.nodes.push(Node {
            name,
            kind,
            apparent,
            disk,
            parent: Some(parent),
            children: Vec::new(),
            dev,
            ino,
            hlink,
            shared,
            excluded,
            read_error,
            scanned: true,
            unscanned: 0,
            items: 0,
        });
        self.nodes[parent].children.push(idx);
        // Every entry is an item; only counted entries add to the sizes.
        let (apparent, disk) = if !shared && excluded == Excluded::No {
            (apparent, disk)
        } else {
            (0, 0)
        };
        self.add_totals(parent, apparent, disk, 1);
        idx
    }

    /// True if no batch is still waiting for a missing parent (used in tests).
    #[cfg(test)]
    pub fn orphans_is_empty(&self) -> bool {
        self.orphans.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::NewNode;

    fn dir(name: &str, id: u64) -> NewNode {
        NewNode {
            name: name.into(),
            kind: NodeKind::Dir,
            apparent: 100,
            disk: 100,
            dev: 1,
            ino: id,
            hlink: false,
            shared: false,
            excluded: Excluded::No,
            read_error: false,
            dir_id: Some(id),
        }
    }

    /// A directory is not deletable until its entire subtree has been scanned.
    #[test]
    fn delete_gate_tracks_unscanned_subtree() {
        let mut t = Tree::new("/r".into(), 100, 100, 1, 1);
        // Root not scanned yet → incomplete.
        assert!(!t.subtree_complete(t.root));

        // Apply root's batch: it declares one subdir `a` (id 1), still unscanned.
        t.apply(Batch {
            parent_id: 0,
            nodes: vec![dir("a", 1)],
            error: None,
        });
        let a = t.nodes[t.root].children[0];
        assert!(
            !t.subtree_complete(t.root),
            "root has an unscanned child dir"
        );
        assert!(!t.subtree_complete(a), "a itself not scanned yet");

        // `a` declares a nested subdir `b` (id 2): still incomplete deeper.
        t.apply(Batch {
            parent_id: 1,
            nodes: vec![dir("b", 2)],
            error: None,
        });
        assert!(!t.subtree_complete(a), "a has an unscanned child b");

        // `b` is empty and now scanned → everything is complete.
        t.apply(Batch {
            parent_id: 2,
            nodes: vec![],
            error: None,
        });
        assert!(t.subtree_complete(t.nodes[a].children[0]), "b complete");
        assert!(t.subtree_complete(a), "a complete");
        assert!(t.subtree_complete(t.root), "root complete");
    }

    fn file(name: &str, ino: u64) -> NewNode {
        NewNode {
            kind: NodeKind::File,
            dir_id: None,
            ..dir(name, ino)
        }
    }

    /// Root with `a/` (containing `a/x`, `a/sub/`, `a/sub/y`) and a sibling file `b`.
    fn small_tree() -> Tree {
        let mut t = Tree::new("/r".into(), 100, 100, 1, 1);
        t.apply(Batch {
            parent_id: 0,
            nodes: vec![dir("a", 1), file("b", 10)],
            error: None,
        });
        t.apply(Batch {
            parent_id: 1,
            nodes: vec![file("x", 11), dir("sub", 2)],
            error: None,
        });
        t.apply(Batch {
            parent_id: 2,
            nodes: vec![file("y", 12)],
            error: None,
        });
        t
    }

    fn idx_of(t: &Tree, name: &str) -> NodeIdx {
        t.nodes.iter().position(|n| n.name == name).unwrap()
    }

    /// Deleting a subtree turns every node in it into a detectable tombstone.
    #[test]
    fn deleted_subtree_is_detached() {
        let mut t = small_tree();
        assert_eq!(t.detached, 0);
        assert!((0..t.nodes.len()).all(|i| t.is_attached(i)));

        t.delete_subtree(idx_of(&t, "a"));
        assert_eq!(t.detached, 1);
        for gone in ["a", "x", "sub", "y"] {
            assert!(!t.is_attached(idx_of(&t, gone)), "{gone} is a tombstone");
        }
        assert!(t.is_attached(idx_of(&t, "b")), "sibling stays");
        assert!(t.is_attached(t.root));
    }

    /// A tombstone has no filesystem path. The display path built from it is relative, which
    /// is exactly why nothing may act on it.
    #[test]
    fn tombstones_have_no_filesystem_path() {
        let mut t = small_tree();
        let (a, y, b) = (idx_of(&t, "a"), idx_of(&t, "y"), idx_of(&t, "b"));
        assert_eq!(t.fs_path_of(y).as_deref(), Some("/r/a/sub/y"));
        assert_eq!(t.fs_path_of(t.root).as_deref(), Some("/r"));

        t.delete_subtree(a);
        assert_eq!(t.path_of(y), "a/sub/y", "relative: the chain ends at `a`");
        assert_eq!(t.fs_path_of(y), None);
        assert_eq!(t.fs_path_of(a), None);
        assert_eq!(
            t.fs_path_of(b).as_deref(),
            Some("/r/b"),
            "sibling keeps its path"
        );
    }

    /// A rescan tears down the subtree below the refreshed directory, which itself stays.
    #[test]
    fn refreshed_subtree_is_detached_but_its_root_stays() {
        let mut t = small_tree();
        t.begin_refresh(idx_of(&t, "a"), 1 << 40);
        assert_eq!(t.detached, 1);
        assert!(t.is_attached(idx_of(&t, "a")));
        assert!(t.is_attached(idx_of(&t, "b")));
        for gone in ["x", "sub", "y"] {
            assert!(!t.is_attached(idx_of(&t, gone)), "{gone} is a tombstone");
        }
    }

    /// Directories at or below `idx` that are still waiting for their batch, counted by walking
    /// the subtree: what `unscanned` must equal at every moment.
    fn unread_dirs(t: &Tree, idx: NodeIdx) -> u32 {
        let n = &t.nodes[idx];
        let own = (n.is_dir() && !n.scanned && n.excluded == Excluded::No) as u32;
        own + n.children.iter().map(|&c| unread_dirs(t, c)).sum::<u32>()
    }

    /// `unscanned` matches the real number of unread directories for every attached directory.
    fn assert_counts_in_step(t: &Tree, when: &str) {
        for idx in 0..t.nodes.len() {
            if t.nodes[idx].is_dir() && t.is_attached(idx) {
                assert_eq!(
                    t.nodes[idx].unscanned,
                    unread_dirs(t, idx),
                    "{when}: unscanned of {:?}",
                    t.nodes[idx].name
                );
            }
        }
    }

    /// A rescan makes its directory unread again, and every ancestor has to know: their
    /// `unscanned` count is the gate for delete and rescan. While the original scan is still
    /// reading other directories, the count must not reach zero one directory early.
    #[test]
    fn rescan_keeps_ancestor_counts_in_step() {
        let mut t = Tree::new("/r".into(), 100, 100, 1, 1);
        // `a` is read; `b` and `c` are still waiting for the original scan.
        t.apply(Batch {
            parent_id: 0,
            nodes: vec![dir("a", 1), dir("b", 2), dir("c", 3)],
            error: None,
        });
        t.apply(Batch {
            parent_id: 1,
            nodes: vec![file("x", 11)],
            error: None,
        });
        let a = idx_of(&t, "a");
        assert!(t.subtree_complete(a));
        assert_counts_in_step(&t, "before the rescan");

        let base = 1 << 40;
        t.begin_refresh(a, base);
        assert!(!t.subtree_complete(a), "a is unread again");
        assert!(!t.subtree_complete(t.root));
        assert_counts_in_step(&t, "rescan started");

        // The rescan reads `a`, finding a new subdirectory, then reads that.
        t.apply(Batch {
            parent_id: base,
            nodes: vec![file("x", 11), dir("sub", base + 1)],
            error: None,
        });
        assert_counts_in_step(&t, "rescan read a");
        t.apply(Batch {
            parent_id: base + 1,
            nodes: vec![],
            error: None,
        });
        assert!(t.subtree_complete(a));
        assert_counts_in_step(&t, "rescan done");

        // The original scan still owes two directories: the root is complete only after both.
        t.apply(Batch {
            parent_id: 2,
            nodes: vec![],
            error: None,
        });
        assert!(!t.subtree_complete(t.root), "c has not been read yet");
        assert_counts_in_step(&t, "b read");
        t.apply(Batch {
            parent_id: 3,
            nodes: vec![],
            error: None,
        });
        assert!(t.subtree_complete(t.root));
        assert_counts_in_step(&t, "c read");
    }

    /// With nothing else pending, the parent of a directory being rescanned is incomplete from
    /// the moment the rescan starts, not only once its first batch arrives.
    #[test]
    fn rescan_makes_ancestors_incomplete_at_once() {
        let mut t = small_tree();
        let (a, sub) = (idx_of(&t, "a"), idx_of(&t, "sub"));
        assert!(t.subtree_complete(t.root));

        t.begin_refresh(sub, 1 << 40);
        assert!(!t.subtree_complete(sub));
        assert!(!t.subtree_complete(a), "parent of the rescanned directory");
        assert!(!t.subtree_complete(t.root));
        assert_counts_in_step(&t, "rescan started");
    }

    fn sizes(t: &Tree, idx: NodeIdx) -> (u64, u64) {
        (t.nodes[idx].apparent, t.nodes[idx].disk)
    }

    /// Sizes found below a directory that does not count stay below it: an excluded directory
    /// contributes nothing to its ancestors, whatever turns up inside it.
    #[test]
    fn sizes_stop_at_a_directory_that_does_not_count() {
        let mut t = Tree::new_imported("/r".into(), 100, 100, 1, 1);
        let ex = t.import_child(
            t.root,
            "ex".into(),
            NodeKind::Dir,
            4096,
            4096,
            2,
            2,
            false,
            false,
            Excluded::OtherFs,
            false,
        );
        assert_eq!(
            sizes(&t, t.root),
            (100, 100),
            "the excluded directory adds 0"
        );

        let inside = t.import_child(
            ex,
            "big".into(),
            NodeKind::File,
            1000,
            1000,
            2,
            5,
            false,
            false,
            Excluded::No,
            false,
        );
        assert_eq!(sizes(&t, t.root), (100, 100), "nor does anything inside it");
        assert_eq!(sizes(&t, ex), (5096, 5096), "it shows what it holds itself");
        assert_eq!(t.nodes[t.root].items, 2, "every entry is still an item");

        // Taking the entry out again must not subtract what was never added.
        t.delete_subtree(inside);
        assert_eq!(sizes(&t, t.root), (100, 100));
        assert_eq!(sizes(&t, ex), (4096, 4096));
        assert_eq!(t.nodes[t.root].items, 1);
    }

    /// The same through the scanner path: a batch grafted below an excluded directory, which
    /// is what rescanning one used to do.
    #[test]
    fn batches_below_an_excluded_directory_leave_ancestors_alone() {
        let mut t = Tree::new("/r".into(), 100, 100, 1, 1);
        let excluded = NewNode {
            excluded: Excluded::Pattern,
            dir_id: None,
            ..dir("ex", 9)
        };
        t.apply(Batch {
            parent_id: 0,
            nodes: vec![excluded, file("f", 10)],
            error: None,
        });
        let ex = idx_of(&t, "ex");
        let before = sizes(&t, t.root);
        assert_eq!(before, (200, 200), "root plus f; ex is excluded");

        let base = 1 << 40;
        t.begin_refresh(ex, base);
        assert_eq!(
            sizes(&t, t.root),
            before,
            "tearing it down subtracts nothing"
        );
        t.apply(Batch {
            parent_id: base,
            nodes: vec![file("inside", 11)],
            error: None,
        });
        assert_eq!(sizes(&t, t.root), before, "its content adds nothing");
    }

    fn items(t: &Tree, name: &str) -> u32 {
        t.nodes[idx_of(t, name)].items
    }

    /// Ground truth for `items`: walk the attached subtree and count every entry below `idx`.
    fn entries_below(t: &Tree, idx: NodeIdx) -> u32 {
        t.nodes[idx]
            .children
            .iter()
            .map(|&c| 1 + entries_below(t, c))
            .sum()
    }

    /// The item count must not cost memory: `items` fits in existing padding.
    #[test]
    #[cfg(target_pointer_width = "64")]
    fn node_stays_lean() {
        assert_eq!(std::mem::size_of::<Node>(), 112);
    }

    /// Item counts grow live with every batch, at the parent and at every ancestor.
    #[test]
    fn item_counts_grow_with_batches() {
        let mut t = Tree::new("/r".into(), 100, 100, 1, 1);
        assert_eq!(t.nodes[t.root].items, 0);
        t.apply(Batch {
            parent_id: 0,
            nodes: vec![dir("a", 1), file("b", 10)],
            error: None,
        });
        assert_eq!(t.nodes[t.root].items, 2);
        assert_eq!(items(&t, "a"), 0, "not read yet");
        t.apply(Batch {
            parent_id: 1,
            nodes: vec![file("x", 11), dir("sub", 2)],
            error: None,
        });
        assert_eq!(items(&t, "a"), 2);
        assert_eq!(t.nodes[t.root].items, 4);
        t.apply(Batch {
            parent_id: 2,
            nodes: vec![file("y", 12)],
            error: None,
        });
        assert_eq!(items(&t, "sub"), 1);
        assert_eq!(items(&t, "a"), 3);
        assert_eq!(t.nodes[t.root].items, 5);
        assert_eq!(items(&t, "b"), 0, "files have no items");
        for i in 0..t.nodes.len() {
            assert_eq!(
                t.nodes[i].items,
                entries_below(&t, i),
                "{}",
                t.nodes[i].name
            );
        }
    }

    /// A batch that arrives before its parent counts once it is grafted, not before.
    #[test]
    fn item_counts_include_orphans_once_flushed() {
        let mut t = Tree::new("/r".into(), 100, 100, 1, 1);
        t.apply(Batch {
            parent_id: 1,
            nodes: vec![file("x", 11), file("y", 12)],
            error: None,
        });
        assert_eq!(t.nodes[t.root].items, 0, "orphan not grafted yet");
        t.apply(Batch {
            parent_id: 0,
            nodes: vec![dir("a", 1)],
            error: None,
        });
        assert!(t.orphans_is_empty());
        assert_eq!(items(&t, "a"), 2);
        assert_eq!(t.nodes[t.root].items, 3);
    }

    /// Hard-link duplicates and excluded entries don't count toward sizes, but they are
    /// listed entries, so they count as items. An excluded directory contributes just itself.
    #[test]
    fn shared_and_excluded_entries_are_items() {
        let mut t = Tree::new("/r".into(), 100, 100, 1, 1);
        let shared = NewNode {
            shared: true,
            hlink: true,
            ..file("dup", 10)
        };
        let excluded_dir = NewNode {
            excluded: Excluded::Pattern,
            dir_id: None,
            ..dir("node_modules", 2)
        };
        let excluded_file = NewNode {
            excluded: Excluded::Age,
            ..file("old", 11)
        };
        t.apply(Batch {
            parent_id: 0,
            nodes: vec![shared, excluded_dir, excluded_file, file("f", 12)],
            error: None,
        });
        assert_eq!(t.nodes[t.root].items, 4);
        assert_eq!(items(&t, "node_modules"), 0);
        assert_eq!(
            t.nodes[t.root].apparent,
            100 + 100,
            "only `f` counts toward the size"
        );

        // The same rules apply to imported entries.
        let mut t = Tree::new_imported("/r".into(), 0, 0, 1, 1);
        let d = t.import_child(
            t.root,
            "d".into(),
            NodeKind::Dir,
            10,
            10,
            1,
            2,
            false,
            false,
            Excluded::No,
            false,
        );
        for (name, shared, excluded) in [
            ("a", false, Excluded::No),
            ("dup", true, Excluded::No),
            ("ex", false, Excluded::OtherFs),
        ] {
            t.import_child(
                d,
                name.into(),
                NodeKind::File,
                10,
                10,
                1,
                3,
                shared,
                shared,
                excluded,
                false,
            );
        }
        assert_eq!(t.nodes[d].items, 3);
        assert_eq!(t.nodes[t.root].items, 4);
        assert_eq!(
            t.nodes[t.root].apparent, 20,
            "d and a count; dup and ex don't"
        );
    }

    /// Deleting an entry removes it and everything below it from every ancestor's count.
    #[test]
    fn delete_subtracts_items() {
        let mut t = small_tree();
        assert_eq!(t.nodes[t.root].items, 5);
        t.delete_subtree(idx_of(&t, "sub"));
        assert_eq!(items(&t, "a"), 1, "x is left");
        assert_eq!(t.nodes[t.root].items, 3);
        t.delete_subtree(idx_of(&t, "b"));
        assert_eq!(t.nodes[t.root].items, 2);
        t.delete_subtree(idx_of(&t, "a"));
        assert_eq!(t.nodes[t.root].items, 0);
    }

    /// A rescan resets the subtree's count; the new batches bring it back, live.
    #[test]
    fn refresh_resets_and_restores_items() {
        let mut t = small_tree();
        let a = idx_of(&t, "a");
        let base = 1 << 40;
        t.begin_refresh(a, base);
        assert_eq!(t.nodes[a].items, 0);
        assert_eq!(t.nodes[t.root].items, 2, "a and b stay");

        t.apply(Batch {
            parent_id: base,
            nodes: vec![file("x", 11), dir("sub", base + 1), file("new", 13)],
            error: None,
        });
        assert_eq!(t.nodes[a].items, 3);
        assert_eq!(t.nodes[t.root].items, 5);
        t.apply(Batch {
            parent_id: base + 1,
            nodes: vec![file("y", 12)],
            error: None,
        });
        assert_eq!(t.nodes[a].items, 4);
        assert_eq!(t.nodes[t.root].items, 6);
        assert_eq!(t.nodes[t.root].items, entries_below(&t, t.root));
    }

    /// Counts saturate instead of wrapping.
    #[test]
    fn item_counts_saturate() {
        let mut t = small_tree();
        let a = idx_of(&t, "a");
        t.nodes[a].items = u32::MAX - 1;
        t.nodes[t.root].items = u32::MAX - 1;
        t.apply(Batch {
            parent_id: 2,
            nodes: vec![file("p", 20), file("q", 21)],
            error: None,
        });
        assert_eq!(t.nodes[a].items, u32::MAX);
        assert_eq!(t.nodes[t.root].items, u32::MAX);
    }
}
