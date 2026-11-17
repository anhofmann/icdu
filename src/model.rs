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
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{PathBuf, MAIN_SEPARATOR};

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
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Excluded {
    #[default]
    No,
    /// Matched a `--exclude` pattern.
    Pattern,
    /// On a different filesystem and `-x` was given.
    OtherFs,
    /// Outside the `--older-than`/`--newer-than` mtime window.
    Age,
    /// A Linux pseudo filesystem (`/proc`, `/sys`, ...), left out by `--exclude-kernfs`.
    Kernfs,
    /// A macOS firmlink, which ncdu does not follow.
    FrmLink,
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

/// The links to one inode that a scan has delivered (files with a link count above 1 only).
/// What `-e` records about an entry besides its sizes: ncdu's "extended information".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Ext {
    /// Modification time, in seconds since the epoch.
    pub mtime: u64,
    pub uid: u32,
    pub gid: u32,
    /// `st_mode`: the file type and the permission bits.
    pub mode: u16,
    /// Which of the four are known (`Ext::UID` and so on). A scan knows all of them or, for
    /// an entry it could not examine, none. A dump may carry any of them.
    pub has: u8,
}

impl Ext {
    pub const UID: u8 = 1;
    pub const GID: u8 = 2;
    pub const MODE: u8 = 4;
    pub const MTIME: u8 = 8;
    pub const ALL: u8 = Ext::UID | Ext::GID | Ext::MODE | Ext::MTIME;

    pub fn uid(&self) -> Option<u32> {
        (self.has & Ext::UID != 0).then_some(self.uid)
    }

    pub fn gid(&self) -> Option<u32> {
        (self.has & Ext::GID != 0).then_some(self.gid)
    }

    pub fn mode(&self) -> Option<u16> {
        (self.has & Ext::MODE != 0).then_some(self.mode)
    }

    pub fn mtime(&self) -> Option<u64> {
        (self.has & Ext::MTIME != 0).then_some(self.mtime)
    }

    /// The time as it is kept for "latest below": one more than the time, so that 0 can
    /// stand for "none known".
    fn stamp(&self) -> u64 {
        self.mtime().map_or(0, |mtime| mtime.saturating_add(1))
    }
}

/// One row of the extended table.
#[derive(Debug, Clone, Copy, Default)]
struct ExtRow {
    ext: Ext,
    /// The latest modification time in the subtree, the entry itself included, as a stamp
    /// (see [`Ext::stamp`]). Kept like the sizes: raised as entries arrive, and what lies
    /// below an excluded directory does not reach the directories above it.
    latest: u64,
}

struct LinkSet {
    /// The link whose size is in the totals.
    counted: NodeIdx,
    /// The other links, in the order they arrived: shown with their size, marked as
    /// duplicates, and counted nowhere. May hold tombstones; they are dropped when the count
    /// is handed over.
    others: Vec<NodeIdx>,
}

pub struct Tree {
    pub nodes: Vec<Node>,
    pub root: NodeIdx,
    /// Maps a scanner directory id to its node index. Used only while scanning; released by
    /// [`Tree::finish_scan`].
    dir_index: HashMap<u64, NodeIdx>,
    /// Batches whose parent hasn't been created yet, keyed by `parent_id`.
    orphans: HashMap<u64, Vec<Batch>>,
    /// The on-disk bytes of the few names that are not valid UTF-8. `Node::name` holds their
    /// lossy form for display; kept here so that `Node` does not grow for a rare case.
    raw_names: HashMap<NodeIdx, Box<[u8]>>,
    /// Why an entry is flagged with a read error, in the words of the OS. Only entries a scan
    /// could not read have one; an imported dump carries the flag without a reason.
    errors: HashMap<NodeIdx, Box<str>>,
    /// The link count of the entries that have more than one link, where it is known. A
    /// dump needs it (`nlink`), a node has no room for it, and few entries have one.
    nlinks: HashMap<NodeIdx, u32>,
    /// How many entries with a read error lie below a directory, for the directories that
    /// have any. It lets a search for errors skip every subtree without one instead of
    /// walking the whole tree, and it stays small: errors are rare. Kept in step by
    /// `set_read_error` and by everything that adds or detaches entries.
    errors_below: HashMap<NodeIdx, u32>,
    /// Hard-linked files by `(dev, ino)`, so that each inode is counted exactly once: across
    /// directories, across scan generations, and after its counted link is deleted. It costs
    /// memory only for files with more than one link. Imported trees do not use it (their
    /// duplicates are marked by the import, and nothing in them is deleted or rescanned).
    links: HashMap<(u64, u64), LinkSet>,
    /// Extended information (`-e`): one row per node, parallel to `nodes`. `None` unless the
    /// mode is on, so that it costs nothing otherwise and `Node` stays as small as it is.
    ext: Option<Vec<ExtRow>>,
    pub total_files: u64,
    pub total_dirs: u64,
    /// How many times a subtree has been detached (deleted or torn down for a rescan). While
    /// this is 0 every node is attached, so callers can skip [`Tree::is_attached`].
    pub detached: u64,
    /// Counts every change to the tree's entries or sizes. Whatever is derived from the tree
    /// and kept between frames (the sorted rows of the viewed directory) compares this to
    /// know whether it is still up to date.
    pub generation: u64,
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
            raw_names: HashMap::new(),
            errors: HashMap::new(),
            nlinks: HashMap::new(),
            errors_below: HashMap::new(),
            links: HashMap::new(),
            ext: None,
            generation: 0,
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
        self.generation += 1;
        // A directory counts as read with its last batch. Until then it stays unscanned:
        // nothing above it passes the delete or rescan gate, and it is listed as incomplete.
        if !batch.more && !self.nodes[parent_idx].scanned {
            self.nodes[parent_idx].scanned = true;
            // This directory is now read: one fewer unscanned dir in every ancestor's subtree.
            self.bump_unscanned(parent_idx, -1);
        }
        if let Some(error) = batch.error {
            self.set_read_error(parent_idx, true);
            self.errors.insert(parent_idx, error.into());
        }
        // Entries of this batch that could not be examined.
        let new_errors = batch.nodes.iter().filter(|nn| nn.error.is_some()).count();
        self.bump_errors(Some(parent_idx), new_errors as i64);

        let mut sum_apparent = 0u64;
        let mut sum_disk = 0u64;
        // Every entry is an item, whether or not its size counts.
        let new_items = u32::try_from(batch.nodes.len()).unwrap_or(u32::MAX);
        let mut new_dir_ids = Vec::new();
        let mut new_subdirs = Vec::new();
        // Extended information comes as a list parallel to the entries, if at all.
        let mut exts = batch.ext.into_iter();
        let mut newest = 0u64;

        // Nearly every directory arrives as one batch, so reserve its children exactly and
        // avoid the Vec's growth slack. (A very large one arrives in several.)
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

            // Of all the links to one inode, the first to arrive is counted. Excluded entries
            // take no part: they count nowhere anyway.
            let shared = nn.hlink
                && nn.excluded == Excluded::No
                && self.register_link((nn.dev, nn.ino), idx);
            let counts = !shared && nn.excluded == Excluded::No;
            if counts {
                sum_apparent = sum_apparent.saturating_add(nn.apparent);
                sum_disk = sum_disk.saturating_add(nn.disk);
            }
            if let Some(raw) = nn.raw_name {
                self.raw_names.insert(idx, raw);
            }
            if nn.hlink {
                self.set_nlink(idx, nn.nlink);
            }
            let read_error = nn.error.is_some();
            if let Some(error) = nn.error {
                self.errors.insert(idx, error);
            }
            if let Some(rows) = &mut self.ext {
                let ext = exts.next().unwrap_or_default();
                // Like its size, the time of an excluded entry is its own affair.
                if nn.excluded == Excluded::No {
                    newest = newest.max(ext.stamp());
                }
                rows.push(ExtRow {
                    ext,
                    latest: ext.stamp(),
                });
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
                shared,
                excluded: nn.excluded,
                read_error,
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
        self.raise_latest(Some(parent_idx), newest);

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
            n.apparent = n.apparent.saturating_add(apparent);
            n.disk = n.disk.saturating_add(disk);
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

    /// Turn on extended mode (`-e`): from now on the tree keeps owner, group, mode and
    /// modification time of every entry it is given them for.
    pub fn set_extended(&mut self) {
        if self.ext.is_none() {
            self.ext = Some(vec![ExtRow::default(); self.nodes.len()]);
        }
    }

    pub fn is_extended(&self) -> bool {
        self.ext.is_some()
    }

    /// The extended information of an entry, if the tree keeps any and has some for it.
    pub fn ext(&self, idx: NodeIdx) -> Option<Ext> {
        let ext = self.ext.as_ref()?[idx].ext;
        (ext.has != 0).then_some(ext)
    }

    /// Record the extended information of an entry. Without extended mode this does nothing.
    pub fn set_ext(&mut self, idx: NodeIdx, ext: Ext) {
        let Some(rows) = &mut self.ext else { return };
        self.generation += 1;
        rows[idx].ext = ext;
        self.raise_latest(Some(idx), ext.stamp());
    }

    /// The latest modification time in a subtree, the entry itself included: what ncdu shows
    /// and sorts by for a directory. `None` if no time is known there.
    pub fn latest_mtime(&self, idx: NodeIdx) -> Option<u64> {
        self.ext.as_ref()?[idx].latest.checked_sub(1)
    }

    /// Raise "latest below" to `stamp` at `start` and at every ancestor that is behind it.
    ///
    /// An ancestor that is not behind ends the walk: those above it are at least as late.
    /// So does an excluded directory, as for sizes: what it holds does not reach beyond it.
    fn raise_latest(&mut self, start: Option<NodeIdx>, stamp: u64) {
        let Some(rows) = &mut self.ext else { return };
        let mut cur = start;
        while let Some(i) = cur {
            if rows[i].latest >= stamp {
                break;
            }
            rows[i].latest = stamp;
            if self.nodes[i].excluded != Excluded::No {
                break;
            }
            cur = self.nodes[i].parent;
        }
    }

    /// The other direction: something whose latest time was `stamp` has been taken out from
    /// under `start`. Every directory that had its time from there gets the latest of what
    /// it still holds. That reads the children of those directories once, and of no others:
    /// the walk ends at the first ancestor with a later time from elsewhere.
    fn lower_latest(&mut self, start: Option<NodeIdx>, stamp: u64) {
        let Some(rows) = &mut self.ext else { return };
        let mut cur = start;
        while let Some(i) = cur {
            if rows[i].latest != stamp {
                break;
            }
            let node = &self.nodes[i];
            let latest = node
                .children
                .iter()
                .filter(|&&child| self.nodes[child].excluded == Excluded::No)
                .map(|&child| rows[child].latest)
                .fold(rows[i].ext.stamp(), u64::max);
            if latest == stamp || node.excluded != Excluded::No {
                rows[i].latest = latest;
                break;
            }
            rows[i].latest = latest;
            cur = node.parent;
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
        let kids = n
            .children
            .iter()
            .filter(|&&c| self.nodes[c].counts())
            .fold(0u64, |sum, &c| sum.saturating_add(self.nodes[c].apparent));
        n.apparent.saturating_sub(kids)
    }

    pub fn own_disk(&self, idx: NodeIdx) -> u64 {
        let n = &self.nodes[idx];
        let kids = n
            .children
            .iter()
            .filter(|&&c| self.nodes[c].counts())
            .fold(0u64, |sum, &c| sum.saturating_add(self.nodes[c].disk));
        n.disk.saturating_sub(kids)
    }

    /// True if this entire subtree has been fully scanned (every descendant directory read).
    /// Files are trivially complete. This is the gate for deletion.
    pub fn subtree_complete(&self, idx: NodeIdx) -> bool {
        self.nodes[idx].unscanned == 0
    }

    /// Directories in the whole tree that are still waiting to be read. Zero once every scan
    /// and rescan has finished.
    pub fn unread_dirs(&self) -> u32 {
        self.nodes[self.root].unscanned
    }

    /// True for a directory whose listing has not arrived yet: it will be read, but for now
    /// its (empty) child list says nothing about what it holds. Excluded directories are
    /// never read and are not "unread" in this sense.
    pub fn is_unread(&self, idx: NodeIdx) -> bool {
        let n = &self.nodes[idx];
        n.is_dir() && !n.scanned && n.excluded == Excluded::No
    }

    /// Remove a fully-scanned subtree from the tree (after it has been removed from disk).
    /// Returns the number of (dirs, files) removed.
    pub fn delete_subtree(&mut self, idx: NodeIdx) -> (u64, u64) {
        self.generation += 1;
        self.forget_errors(idx, true);
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
            if let (Some(rows), Excluded::No) = (&self.ext, self.nodes[idx].excluded) {
                let stamp = rows[idx].latest;
                self.lower_latest(Some(parent), stamp);
            }
        }
        // Cut the upward link too, so `is_attached` can tell the tombstones apart.
        self.nodes[idx].parent = None;
        self.detached += 1;
        // Walk the subtree to update global counts (nodes become unreachable tombstones).
        let mut dirs = 0u64;
        let mut files = 0u64;
        let mut lost_links = Vec::new();
        let mut stack = vec![idx];
        while let Some(n) = stack.pop() {
            if self.nodes[n].is_dir() {
                dirs += 1;
            } else {
                files += 1;
            }
            if self.is_counted_link(n) {
                lost_links.push(n);
            }
            // The counts of what was detached are of no use any more.
            if !self.errors_below.is_empty() {
                self.errors_below.remove(&n);
            }
            stack.extend_from_slice(&self.nodes[n].children);
        }
        self.total_dirs = self.total_dirs.saturating_sub(dirs);
        self.total_files = self.total_files.saturating_sub(files);
        self.hand_over_links(lost_links);
        (dirs, files)
    }

    /// How many hard-linked files `idx` is or contains. Deleting one of them frees its space
    /// only if it is the last link, and the tree cannot know that: links may exist outside
    /// the scanned directory.
    pub fn hard_links_in(&self, idx: NodeIdx) -> u32 {
        let mut count = 0u32;
        let mut stack = vec![idx];
        while let Some(n) = stack.pop() {
            if self.nodes[n].hlink {
                count = count.saturating_add(1);
            }
            stack.extend_from_slice(&self.nodes[n].children);
        }
        count
    }

    /// True if `idx` is a hard-linked file whose size is in the totals: the one link of its
    /// inode that counts.
    fn is_counted_link(&self, idx: NodeIdx) -> bool {
        let n = &self.nodes[idx];
        n.hlink && n.counts()
    }

    /// Record a link to the inode `key` that is about to become node `idx`. Returns true if
    /// it is a duplicate: another link of that inode is in the tree and counted.
    fn register_link(&mut self, key: (u64, u64), idx: NodeIdx) -> bool {
        match self.links.get(&key).map(|set| set.counted) {
            Some(counted) if self.is_attached(counted) => {
                if let Some(set) = self.links.get_mut(&key) {
                    set.others.push(idx);
                }
                true
            }
            // Never seen, or its links have all left the tree since.
            _ => {
                self.links.insert(
                    key,
                    LinkSet {
                        counted: idx,
                        others: Vec::new(),
                    },
                );
                false
            }
        }
    }

    /// `lost` are counted links that were just detached (deleted, or torn down for a rescan).
    /// Their inodes may live on through other links, which were duplicates until now. Hand
    /// each count to the first link that is still part of the tree: it becomes the counted
    /// one and its size goes into its ancestors. Without this the inode would vanish from
    /// every total although it still occupies the disk.
    fn hand_over_links(&mut self, lost: Vec<NodeIdx>) {
        for idx in lost {
            let key = (self.nodes[idx].dev, self.nodes[idx].ino);
            let Some(mut set) = self.links.remove(&key) else {
                continue;
            };
            if set.counted != idx {
                self.links.insert(key, set);
                continue;
            }
            // No link left in the tree: the entry is dropped with the inode.
            let Some(pos) = set.others.iter().position(|&o| self.is_attached(o)) else {
                continue;
            };
            let heir = set.others[pos];
            // Everything before the heir is a tombstone.
            set.others.drain(..=pos);
            set.counted = heir;
            self.links.insert(key, set);

            let n = &mut self.nodes[heir];
            n.shared = false;
            let (apparent, disk, parent) = (n.apparent, n.disk, n.parent);
            if let Some(parent) = parent {
                self.add_totals(parent, apparent, disk, 0);
            }
        }
    }

    /// Prepare a subtree for an in-place rescan (`r`): its children are detached (and dropped
    /// from the global dir/file counts), its aggregates are reset to its own size and zero
    /// items — so every ancestor's total stays correct while the rescan streams back in — and
    /// the subtree is marked unscanned again. `id_base` registers the node as the root of a new
    /// scan generation (see `scan::start_at`); batches from that scan graft back into this node.
    pub fn begin_refresh(&mut self, idx: NodeIdx, id_base: u64) {
        self.generation += 1;
        debug_assert!(self.nodes[idx].is_dir());
        debug_assert!(
            self.subtree_complete(idx),
            "only a fully scanned subtree may be rescanned"
        );
        // Own size first: the aggregate minus the counted children is this node's own size,
        // which is what the reset aggregate becomes.
        let (own_apparent, own_disk) = (self.own_apparent(idx), self.own_disk(idx));

        self.forget_errors(idx, false);
        let mut stack = std::mem::take(&mut self.nodes[idx].children);
        // Cut the children's upward links, so `is_attached` can tell the tombstones apart.
        for &c in &stack {
            self.nodes[c].parent = None;
        }
        self.detached += 1;
        let mut dirs = 0u64;
        let mut files = 0u64;
        let mut lost_links = Vec::new();
        while let Some(n) = stack.pop() {
            if self.nodes[n].is_dir() {
                dirs += 1;
            } else {
                files += 1;
            }
            if self.is_counted_link(n) {
                lost_links.push(n);
            }
            if !self.errors_below.is_empty() {
                self.errors_below.remove(&n);
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
        // The directory is left with its own modification time, and whatever above it had
        // its latest time from below here looks again.
        if let Some(rows) = &mut self.ext {
            let own = rows[idx].ext.stamp();
            let was = std::mem::replace(&mut rows[idx].latest, own);
            if self.nodes[idx].excluded == Excluded::No {
                let parent = self.nodes[idx].parent;
                self.lower_latest(parent, was);
            }
        }
        let n = &mut self.nodes[idx];
        n.scanned = false;
        self.set_read_error(idx, false);
        self.errors.remove(&idx);
        // This directory is unread again, and every ancestor has to count it: the batch that
        // reads it takes one off the whole chain (see `graft`), and until then nothing above
        // it may pass the delete/rescan gate.
        self.bump_unscanned(idx, 1);
        self.dir_index.insert(id_base, idx);
        // Inodes whose counted link was below `idx` are counted through a link elsewhere in
        // the tree from now on, if they have one. The links the rescan brings back are then
        // duplicates, so nothing is counted twice or lost, whatever the disk looks like now.
        self.hand_over_links(lost_links);
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

    /// The path of a node relative to `base`, one of its ancestors (`sub/dir/file`), for
    /// compact display. Empty for `base` itself. The walk also ends at the root, so a `base`
    /// that is not an ancestor yields the path relative to the root.
    pub fn rel_path_from(&self, idx: NodeIdx, base: NodeIdx) -> String {
        let mut parts = Vec::new();
        let mut cur = idx;
        while cur != base {
            match self.nodes[cur].parent {
                Some(p) => {
                    parts.push(self.nodes[cur].name.as_str());
                    cur = p;
                }
                None => break,
            }
        }
        parts.reverse();
        parts.join("/")
    }

    /// Flag or unflag `idx` as unreadable, and keep the error counts of its ancestors in step.
    /// Never write `Node::read_error` directly: the counts would no longer match.
    pub fn set_read_error(&mut self, idx: NodeIdx, on: bool) {
        if self.nodes[idx].read_error != on {
            self.nodes[idx].read_error = on;
            self.bump_errors(self.nodes[idx].parent, if on { 1 } else { -1 });
        }
    }

    /// The number of entries with a read error below `idx`, not counting `idx` itself.
    pub fn errors_below(&self, idx: NodeIdx) -> u32 {
        if self.errors_below.is_empty() {
            return 0;
        }
        self.errors_below.get(&idx).copied().unwrap_or(0)
    }

    /// Add `delta` to the error count of `start` and of every ancestor.
    fn bump_errors(&mut self, start: Option<NodeIdx>, delta: i64) {
        if delta == 0 {
            return;
        }
        let mut cur = start;
        while let Some(i) = cur {
            let count = i64::from(self.errors_below(i)) + delta;
            if count > 0 {
                self.errors_below
                    .insert(i, u32::try_from(count).unwrap_or(u32::MAX));
            } else {
                self.errors_below.remove(&i);
            }
            cur = self.nodes[i].parent;
        }
    }

    /// The entries of the subtree `idx` are leaving the tree: take their errors off the counts
    /// of what stays. `with_self` says whether `idx` itself leaves (a deletion) or only what
    /// is below it (a rescan). Called before the links are cut.
    fn forget_errors(&mut self, idx: NodeIdx, with_self: bool) {
        let below = i64::from(self.errors_below(idx));
        if with_self {
            let own = i64::from(self.nodes[idx].read_error);
            self.bump_errors(self.nodes[idx].parent, -(below + own));
        } else {
            self.bump_errors(Some(idx), -below);
        }
    }

    /// The link count of `idx`, if it has more than one link and the count is known.
    pub fn nlink(&self, idx: NodeIdx) -> Option<u32> {
        if self.nlinks.is_empty() {
            return None;
        }
        self.nlinks.get(&idx).copied()
    }

    /// Record the link count of an entry. Only counts above 1 are worth keeping.
    pub fn set_nlink(&mut self, idx: NodeIdx, nlink: u32) {
        if nlink > 1 {
            self.nlinks.insert(idx, nlink);
        }
    }

    /// Why `idx` has a read error, if the scan recorded a reason.
    pub fn error_of(&self, idx: NodeIdx) -> Option<&str> {
        self.errors.get(&idx).map(|e| &**e)
    }

    /// The on-disk bytes of a name that is not valid UTF-8; `None` for every other entry.
    pub fn raw_name(&self, idx: NodeIdx) -> Option<&[u8]> {
        if self.raw_names.is_empty() {
            return None;
        }
        self.raw_names.get(&idx).map(|raw| &**raw)
    }

    /// Record the on-disk bytes of a name that `Node::name` can only show lossily.
    pub fn set_raw_name(&mut self, idx: NodeIdx, raw: Box<[u8]>) {
        self.raw_names.insert(idx, raw);
    }

    /// The name of an entry exactly as the filesystem has it.
    pub fn name_bytes(&self, idx: NodeIdx) -> &[u8] {
        self.raw_name(idx)
            .unwrap_or_else(|| self.nodes[idx].name.as_bytes())
    }

    /// The path of a node for acting on the filesystem (delete, open, shell, rescan, stat), or
    /// `None` if the node is no longer part of the tree. A tombstone's parent chain ends
    /// before the root, so [`Tree::path_of`] gives a *relative* path for it, which the OS
    /// would resolve against the working directory and so reach an unrelated file.
    ///
    /// The path is built from the names' on-disk bytes. For a name that is not valid UTF-8,
    /// the display form would name a path that does not exist, or a different entry.
    pub fn fs_path_of(&self, idx: NodeIdx) -> Option<PathBuf> {
        if !self.is_attached(idx) {
            return None;
        }
        let mut parts = Vec::new();
        let mut cur = Some(idx);
        while let Some(i) = cur {
            parts.push(self.name_bytes(i));
            cur = self.nodes[i].parent;
        }
        // The root's name is the absolute path of the scan root; the rest are components.
        let mut path = PathBuf::from(OsStr::from_bytes(parts.pop()?));
        for part in parts.iter().rev() {
            path.push(OsStr::from_bytes(part));
        }
        Some(path)
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
        self.generation += 1;
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
        if let Some(rows) = &mut self.ext {
            rows.push(ExtRow::default());
        }
        self.nodes[parent].children.push(idx);
        // Every entry is an item; only counted entries add to the sizes.
        let (apparent, disk) = if !shared && excluded == Excluded::No {
            (apparent, disk)
        } else {
            (0, 0)
        };
        self.add_totals(parent, apparent, disk, 1);
        self.bump_errors(Some(parent), i64::from(read_error));
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
            raw_name: None,
            kind: NodeKind::Dir,
            apparent: 100,
            disk: 100,
            dev: 1,
            ino: id,
            hlink: false,
            nlink: 1,
            excluded: Excluded::No,
            error: None,
            dir_id: Some(id),
        }
    }

    /// Display paths are relative to an ancestor and never start with its name.
    #[test]
    fn rel_path_from_stops_at_the_base() {
        let mut t = Tree::new("/r".into(), 100, 100, 1, 1);
        t.apply(Batch {
            parent_id: 0,
            nodes: vec![dir("a", 1)],
            error: None,
            more: false,
            ext: Vec::new(),
        });
        t.apply(Batch {
            parent_id: 1,
            nodes: vec![dir("b", 2)],
            error: None,
            more: false,
            ext: Vec::new(),
        });
        let a = t.nodes[t.root].children[0];
        let b = t.nodes[a].children[0];
        assert_eq!(t.rel_path_from(b, t.root), "a/b");
        assert_eq!(t.rel_path_from(b, a), "b");
        assert_eq!(t.rel_path_from(a, a), "");
        assert_eq!(t.rel_path_from(t.root, t.root), "");
        // Not an ancestor: the walk ends at the root.
        assert_eq!(t.rel_path_from(a, b), "a");
    }

    /// The tree keeps why something could not be read, for a directory and for a single
    /// entry, and forgets it when the directory is read again.
    #[test]
    fn read_error_reasons_are_kept_and_cleared_by_a_rescan() {
        let mut t = Tree::new("/r".into(), 100, 100, 1, 1);
        t.apply(Batch {
            parent_id: 0,
            nodes: vec![
                dir("a", 1),
                NewNode {
                    error: Some("Input/output error (os error 5)".into()),
                    ..file("broken", 10)
                },
            ],
            error: None,
            more: false,
            ext: Vec::new(),
        });
        // The listing of `a` broke off after one entry.
        t.apply(Batch {
            parent_id: 1,
            nodes: vec![file("seen", 11)],
            error: Some("Stale file handle (os error 116)".into()),
            more: false,
            ext: Vec::new(),
        });
        let (a, broken) = (idx_of(&t, "a"), idx_of(&t, "broken"));
        assert!(t.nodes[a].read_error && t.nodes[broken].read_error);
        assert_eq!(t.error_of(a), Some("Stale file handle (os error 116)"));
        assert_eq!(t.error_of(broken), Some("Input/output error (os error 5)"));
        assert_eq!(t.nodes[a].children.len(), 1, "what was listed is kept");
        assert_eq!(t.error_of(t.root), None);
        assert!(t.subtree_complete(t.root), "a failed read is still a read");

        t.begin_refresh(a, crate::scan::SCAN_ID_STRIDE);
        assert!(!t.nodes[a].read_error);
        assert_eq!(t.error_of(a), None);
    }

    /// A large directory arrives in several batches. It counts as read only with the last
    /// one: until then it and everything above it stays incomplete, while its entries and
    /// sizes are already there.
    #[test]
    fn directory_is_scanned_only_after_its_last_batch() {
        let mut t = Tree::new("/r".into(), 100, 100, 1, 1);
        t.apply(Batch {
            parent_id: 0,
            nodes: vec![dir("big", 1)],
            error: None,
            more: false,
            ext: Vec::new(),
        });
        let big = idx_of(&t, "big");
        assert!(!t.subtree_complete(t.root) && t.nodes[big].unscanned == 1);

        // First part: two entries, one of them a directory that is read right away.
        t.apply(Batch {
            parent_id: 1,
            nodes: vec![file("f1", 10), dir("sub", 2)],
            error: None,
            more: true,
            ext: Vec::new(),
        });
        t.apply(Batch {
            parent_id: 2,
            nodes: Vec::new(),
            error: None,
            more: false,
            ext: Vec::new(),
        });
        assert_eq!(t.nodes[big].children.len(), 2, "the entries are there");
        assert_eq!(t.nodes[big].items, 2);
        assert_eq!(
            t.nodes[big].apparent,
            100 + 100 + 100,
            "and so are the sizes"
        );
        assert!(!t.nodes[big].scanned, "but the listing is not finished");
        assert!(t.is_unread(big));
        assert!(!t.subtree_complete(big) && !t.subtree_complete(t.root));
        assert_eq!(t.nodes[t.root].unscanned, 1);

        // A second part, still not the last.
        t.apply(Batch {
            parent_id: 1,
            nodes: vec![file("f2", 11)],
            error: None,
            more: true,
            ext: Vec::new(),
        });
        assert!(!t.subtree_complete(big));

        // The last part may be empty.
        t.apply(Batch {
            parent_id: 1,
            nodes: Vec::new(),
            error: None,
            more: false,
            ext: Vec::new(),
        });
        assert!(t.nodes[big].scanned);
        assert!(t.subtree_complete(big) && t.subtree_complete(t.root));
        assert_eq!(t.nodes[big].items, 3);
        assert_eq!(t.nodes[t.root].items, 4);
        assert_eq!(t.nodes[t.root].unscanned, 0, "counted down exactly once");
    }

    /// Parts that arrive before their directory is known are held back together, in order.
    #[test]
    fn parts_of_a_directory_wait_for_their_parent() {
        let mut t = Tree::new("/r".into(), 100, 100, 1, 1);
        for (name, more) in [("f1", true), ("f2", false)] {
            t.apply(Batch {
                parent_id: 1,
                nodes: vec![file(name, 10)],
                error: None,
                more,
                ext: Vec::new(),
            });
        }
        assert!(!t.orphans_is_empty());
        t.apply(Batch {
            parent_id: 0,
            nodes: vec![dir("big", 1)],
            error: None,
            more: false,
            ext: Vec::new(),
        });
        assert!(t.orphans_is_empty());
        let big = idx_of(&t, "big");
        assert_eq!(t.nodes[big].items, 2);
        assert!(t.subtree_complete(t.root));
    }

    /// The count of read errors below each directory follows every way an error can come
    /// and go: with a batch, by flag, with a deletion, with a rescan.
    #[test]
    fn error_counts_follow_the_tree() {
        let mut t = small_tree(); // /r: a/ (x, sub/ (y)), b
        let (a, sub, x) = (idx_of(&t, "a"), idx_of(&t, "sub"), idx_of(&t, "x"));
        let counts = |t: &Tree| {
            (
                t.errors_below(t.root),
                t.errors_below(a),
                t.errors_below(sub),
            )
        };
        assert_eq!(counts(&t), (0, 0, 0));

        // A flag on a file counts for every directory above it, once.
        t.set_read_error(x, true);
        t.set_read_error(x, true);
        assert_eq!(counts(&t), (1, 1, 0));
        // A directory with an error counts for its ancestors, not for itself.
        t.set_read_error(sub, true);
        assert_eq!(counts(&t), (2, 2, 0));
        t.set_read_error(x, false);
        assert_eq!(counts(&t), (1, 1, 0));

        // A batch brings both kinds: a failed listing and entries that could not be examined.
        let mut t = Tree::new("/r".into(), 100, 100, 1, 1);
        t.apply(Batch {
            parent_id: 0,
            nodes: vec![
                dir("d", 1),
                NewNode {
                    error: Some("Input/output error".into()),
                    ..file("broken", 10)
                },
            ],
            error: None,
            more: false,
            ext: Vec::new(),
        });
        let d = idx_of(&t, "d");
        assert_eq!(t.errors_below(t.root), 1);
        t.apply(Batch {
            parent_id: 1,
            nodes: vec![NewNode {
                error: Some("Permission denied".into()),
                ..file("inner", 11)
            }],
            error: Some("Stale file handle".into()),
            more: false,
            ext: Vec::new(),
        });
        assert_eq!((t.errors_below(t.root), t.errors_below(d)), (3, 1));

        // A rescan of `d` takes away what was below it and its own flag...
        t.begin_refresh(d, crate::scan::SCAN_ID_STRIDE);
        assert_eq!((t.errors_below(t.root), t.errors_below(d)), (1, 0));
        // ...and a deletion takes the entry with everything below it.
        let broken = idx_of(&t, "broken");
        t.delete_subtree(broken);
        assert_eq!(t.errors_below(t.root), 0);

        // Imported entries count the same way.
        let mut t = Tree::new_imported("/r".into(), 0, 0, 1, 1);
        let d = t.import_child(
            t.root,
            "d".into(),
            NodeKind::Dir,
            0,
            0,
            1,
            2,
            false,
            false,
            Excluded::No,
            true,
        );
        t.import_child(
            d,
            "f".into(),
            NodeKind::File,
            0,
            0,
            1,
            3,
            false,
            false,
            Excluded::No,
            true,
        );
        assert_eq!((t.errors_below(t.root), t.errors_below(d)), (2, 1));
        t.delete_subtree(d);
        assert_eq!(t.errors_below(t.root), 0);
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
            more: false,
            ext: Vec::new(),
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
            more: false,
            ext: Vec::new(),
        });
        assert!(!t.subtree_complete(a), "a has an unscanned child b");

        // `b` is empty and now scanned → everything is complete.
        t.apply(Batch {
            parent_id: 2,
            nodes: vec![],
            error: None,
            more: false,
            ext: Vec::new(),
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
            more: false,
            ext: Vec::new(),
        });
        t.apply(Batch {
            parent_id: 1,
            nodes: vec![file("x", 11), dir("sub", 2)],
            error: None,
            more: false,
            ext: Vec::new(),
        });
        t.apply(Batch {
            parent_id: 2,
            nodes: vec![file("y", 12)],
            error: None,
            more: false,
            ext: Vec::new(),
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
        assert_eq!(t.fs_path_of(y), Some(PathBuf::from("/r/a/sub/y")));
        assert_eq!(t.fs_path_of(t.root), Some(PathBuf::from("/r")));

        t.delete_subtree(a);
        assert_eq!(t.path_of(y), "a/sub/y", "relative: the chain ends at `a`");
        assert_eq!(t.fs_path_of(y), None);
        assert_eq!(t.fs_path_of(a), None);
        assert_eq!(
            t.fs_path_of(b),
            Some(PathBuf::from("/r/b")),
            "sibling keeps its path"
        );
    }

    /// A name that is not valid UTF-8 is shown lossily, but the path to act on is built from
    /// its real bytes, at any depth.
    #[test]
    fn filesystem_paths_use_the_real_bytes_of_a_name() {
        let mut t = Tree::new("/r".into(), 100, 100, 1, 1);
        let raw_dir = NewNode {
            name: "d\u{FFFD}".into(),
            raw_name: Some(Box::from(&b"d\xff"[..])),
            ..dir("", 1)
        };
        t.apply(Batch {
            parent_id: 0,
            nodes: vec![raw_dir],
            error: None,
            more: false,
            ext: Vec::new(),
        });
        t.apply(Batch {
            parent_id: 1,
            nodes: vec![file("plain", 10)],
            error: None,
            more: false,
            ext: Vec::new(),
        });
        let (d, plain) = (idx_of(&t, "d\u{FFFD}"), idx_of(&t, "plain"));

        assert_eq!(t.path_of(plain), "/r/d\u{FFFD}/plain", "display");
        assert_eq!(
            t.fs_path_of(plain),
            Some(PathBuf::from(OsStr::from_bytes(b"/r/d\xff/plain")))
        );
        assert_eq!(t.raw_name(d), Some(&b"d\xff"[..]));
        assert_eq!(t.raw_name(plain), None);
        assert_eq!(t.name_bytes(plain), b"plain");
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
            more: false,
            ext: Vec::new(),
        });
        t.apply(Batch {
            parent_id: 1,
            nodes: vec![file("x", 11)],
            error: None,
            more: false,
            ext: Vec::new(),
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
            more: false,
            ext: Vec::new(),
        });
        assert_counts_in_step(&t, "rescan read a");
        t.apply(Batch {
            parent_id: base + 1,
            nodes: vec![],
            error: None,
            more: false,
            ext: Vec::new(),
        });
        assert!(t.subtree_complete(a));
        assert_counts_in_step(&t, "rescan done");

        // The original scan still owes two directories: the root is complete only after both.
        t.apply(Batch {
            parent_id: 2,
            nodes: vec![],
            error: None,
            more: false,
            ext: Vec::new(),
        });
        assert!(!t.subtree_complete(t.root), "c has not been read yet");
        assert_counts_in_step(&t, "b read");
        t.apply(Batch {
            parent_id: 3,
            nodes: vec![],
            error: None,
            more: false,
            ext: Vec::new(),
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
            more: false,
            ext: Vec::new(),
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
            more: false,
            ext: Vec::new(),
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

    fn at(mtime: u64) -> Ext {
        Ext {
            mtime,
            uid: 1000,
            gid: 100,
            mode: 0o100644,
            has: Ext::ALL,
        }
    }

    /// A tree in extended mode, with the modification times in brackets:
    ///
    /// ```text
    /// /r (10)
    /// ├── old (20)
    /// ├── d (30)
    /// │   ├── new (90)
    /// │   └── mid (50)
    /// └── skipped (99, excluded)
    /// ```
    fn extended_tree() -> Tree {
        let mut t = Tree::new("/r".into(), 0, 0, 1, 1);
        t.set_extended();
        t.set_ext(t.root, at(10));
        t.apply(Batch {
            parent_id: 0,
            nodes: vec![
                file("old", 2),
                dir("d", 1),
                NewNode {
                    excluded: Excluded::Pattern,
                    ..file("skipped", 3)
                },
            ],
            error: None,
            more: false,
            ext: vec![at(20), at(30), at(99)],
        });
        t.apply(Batch {
            parent_id: 1,
            nodes: vec![file("new", 4), file("mid", 5)],
            error: None,
            more: false,
            ext: vec![at(90), at(50)],
        });
        t
    }

    /// In extended mode every entry keeps what the scan saw of it, and a directory knows the
    /// latest modification time below it at every moment, like it knows its size.
    #[test]
    fn extended_information_follows_the_tree() {
        let mut t = Tree::new("/r".into(), 0, 0, 1, 1);
        t.set_extended();
        t.set_ext(t.root, at(10));
        assert_eq!(t.ext(t.root), Some(at(10)));
        assert_eq!(t.latest_mtime(t.root), Some(10));

        let t = extended_tree();
        assert_eq!(t.ext(idx_of(&t, "old")), Some(at(20)));
        assert_eq!(t.ext(idx_of(&t, "new")), Some(at(90)));
        // A file's latest time is its own; a directory's is the latest below it.
        assert_eq!(t.latest_mtime(idx_of(&t, "old")), Some(20));
        assert_eq!(t.latest_mtime(idx_of(&t, "d")), Some(90));
        assert_eq!(t.latest_mtime(t.root), Some(90));
        // An excluded entry has its time, and keeps it to itself, as it does its size.
        assert_eq!(t.latest_mtime(idx_of(&t, "skipped")), Some(99));
    }

    /// A directory that is newer than everything in it shows its own time.
    #[test]
    fn a_directory_counts_its_own_time_too() {
        let mut t = Tree::new("/r".into(), 0, 0, 1, 1);
        t.set_extended();
        t.set_ext(t.root, at(500));
        t.apply(Batch {
            parent_id: 0,
            nodes: vec![file("f", 2)],
            error: None,
            more: false,
            ext: vec![at(20)],
        });
        assert_eq!(t.latest_mtime(t.root), Some(500));
    }

    /// An entry the scan could not examine has no extended information, and one whose time
    /// is the epoch itself still has one.
    #[test]
    fn unknown_and_zero_times_are_told_apart() {
        let mut t = Tree::new("/r".into(), 0, 0, 1, 1);
        t.set_extended();
        t.apply(Batch {
            parent_id: 0,
            nodes: vec![file("unknown", 2), file("epoch", 3)],
            error: None,
            more: false,
            ext: vec![Ext::default(), at(0)],
        });
        assert_eq!(t.ext(idx_of(&t, "unknown")), None);
        assert_eq!(t.latest_mtime(idx_of(&t, "unknown")), None);
        assert_eq!(t.ext(idx_of(&t, "epoch")), Some(at(0)));
        assert_eq!(t.latest_mtime(idx_of(&t, "epoch")), Some(0));
        // The root itself was given no time; it has the one below it.
        assert_eq!(t.ext(t.root), None);
        assert_eq!(t.latest_mtime(t.root), Some(0));
    }

    /// Deleting what a directory had its latest time from: the time falls back to the
    /// latest of what is left, all the way up.
    #[test]
    fn deleting_the_newest_entry_lowers_the_latest_time() {
        let mut t = extended_tree();
        // Not the newest: nothing changes.
        t.delete_subtree(idx_of(&t, "mid"));
        assert_eq!(t.latest_mtime(idx_of(&t, "d")), Some(90));
        t.delete_subtree(idx_of(&t, "new"));
        assert_eq!(t.latest_mtime(idx_of(&t, "d")), Some(30), "its own time");
        assert_eq!(t.latest_mtime(t.root), Some(30));
        t.delete_subtree(idx_of(&t, "d"));
        assert_eq!(t.latest_mtime(t.root), Some(20), "`old` is what is left");
        // The excluded entry never counted, so removing it changes nothing either.
        t.delete_subtree(idx_of(&t, "skipped"));
        assert_eq!(t.latest_mtime(t.root), Some(20));
        t.delete_subtree(idx_of(&t, "old"));
        assert_eq!(t.latest_mtime(t.root), Some(10), "the root's own time");
    }

    /// A rescan empties the directory: it is left with its own time until the new entries
    /// arrive, and the directories above it follow both ways.
    #[test]
    fn rescan_resets_and_restores_the_latest_time() {
        let mut t = extended_tree();
        let d = idx_of(&t, "d");
        let base = crate::scan::SCAN_ID_STRIDE;
        t.begin_refresh(d, base);
        assert_eq!(t.latest_mtime(d), Some(30));
        assert_eq!(t.latest_mtime(t.root), Some(30));
        assert_eq!(
            t.ext(d),
            Some(at(30)),
            "what is known of the directory stays"
        );
        t.apply(Batch {
            parent_id: base,
            nodes: vec![file("newer", 6)],
            error: None,
            more: false,
            ext: vec![at(95)],
        });
        assert_eq!(t.latest_mtime(d), Some(95));
        assert_eq!(t.latest_mtime(t.root), Some(95));
    }

    /// Without `-e` the tree keeps none of it, whatever a batch carries.
    #[test]
    fn nothing_is_kept_without_extended_mode() {
        let mut t = Tree::new("/r".into(), 0, 0, 1, 1);
        assert!(!t.is_extended());
        t.set_ext(t.root, at(10));
        t.apply(Batch {
            parent_id: 0,
            nodes: vec![file("f", 2)],
            error: None,
            more: false,
            ext: vec![at(20)],
        });
        assert_eq!(t.ext(t.root), None);
        assert_eq!(t.ext(idx_of(&t, "f")), None);
        assert_eq!(t.latest_mtime(t.root), None);
    }

    /// The table is as long as the tree, however the nodes got there, and takes the fields
    /// one by one, as a dump may have them.
    #[test]
    fn extended_rows_stay_parallel_to_the_nodes() {
        let mut t = Tree::new_imported("/r".into(), 0, 0, 1, 1);
        t.set_extended();
        let mut add = |parent, name: &str, kind| {
            t.import_child(
                parent,
                name.into(),
                kind,
                1,
                1,
                1,
                2,
                false,
                false,
                Excluded::No,
                false,
            )
        };
        let d = add(0, "d", NodeKind::Dir);
        let f = add(d, "f", NodeKind::File);
        assert_eq!(t.ext(f), None);
        t.set_ext(f, at(70));
        assert_eq!(t.ext(f), Some(at(70)));
        assert_eq!(t.latest_mtime(d), Some(70));
        assert_eq!(t.latest_mtime(t.root), Some(70));
        let only_time = Ext {
            mtime: 80,
            has: Ext::MTIME,
            ..Ext::default()
        };
        t.set_ext(d, only_time);
        assert_eq!(t.ext(d).unwrap().uid(), None);
        assert_eq!(t.ext(d).unwrap().mtime(), Some(80));
        assert_eq!(t.latest_mtime(t.root), Some(80));
    }

    /// Extended mode costs a row per entry, and only when it is on.
    #[test]
    fn an_extended_row_is_32_bytes() {
        assert_eq!(std::mem::size_of::<ExtRow>(), 32);
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
            more: false,
            ext: Vec::new(),
        });
        assert_eq!(t.nodes[t.root].items, 2);
        assert_eq!(items(&t, "a"), 0, "not read yet");
        t.apply(Batch {
            parent_id: 1,
            nodes: vec![file("x", 11), dir("sub", 2)],
            error: None,
            more: false,
            ext: Vec::new(),
        });
        assert_eq!(items(&t, "a"), 2);
        assert_eq!(t.nodes[t.root].items, 4);
        t.apply(Batch {
            parent_id: 2,
            nodes: vec![file("y", 12)],
            error: None,
            more: false,
            ext: Vec::new(),
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
            more: false,
            ext: Vec::new(),
        });
        assert_eq!(t.nodes[t.root].items, 0, "orphan not grafted yet");
        t.apply(Batch {
            parent_id: 0,
            nodes: vec![dir("a", 1)],
            error: None,
            more: false,
            ext: Vec::new(),
        });
        assert!(t.orphans_is_empty());
        assert_eq!(items(&t, "a"), 2);
        assert_eq!(t.nodes[t.root].items, 3);
    }

    /// A hard link to inode `ino` with a size of 1000.
    fn link(name: &str, ino: u64) -> NewNode {
        NewNode {
            hlink: true,
            nlink: 2,
            apparent: 1000,
            disk: 1000,
            ..file(name, ino)
        }
    }

    /// Root (100) with `a/` and `b/` (100 each). One inode (1000 bytes) is linked from both
    /// as `a/one` and `b/two`; `a` also holds a plain file `a/x`.
    fn tree_with_two_links() -> Tree {
        let mut t = Tree::new("/r".into(), 100, 100, 1, 1);
        t.apply(Batch {
            parent_id: 0,
            nodes: vec![dir("a", 1), dir("b", 2)],
            error: None,
            more: false,
            ext: Vec::new(),
        });
        t.apply(Batch {
            parent_id: 1,
            nodes: vec![link("one", 50), file("x", 11)],
            error: None,
            more: false,
            ext: Vec::new(),
        });
        t.apply(Batch {
            parent_id: 2,
            nodes: vec![link("two", 50)],
            error: None,
            more: false,
            ext: Vec::new(),
        });
        t
    }

    /// The tree decides which link of an inode is counted: the first one it gets. Every
    /// later link is a duplicate.
    #[test]
    fn the_first_link_seen_is_the_counted_one() {
        let t = tree_with_two_links();
        let (one, two) = (idx_of(&t, "one"), idx_of(&t, "two"));
        assert!(!t.nodes[one].shared);
        assert!(t.nodes[two].shared);
        assert_eq!(t.nodes[idx_of(&t, "a")].apparent, 100 + 1000 + 100);
        assert_eq!(t.nodes[idx_of(&t, "b")].apparent, 100);
        assert_eq!(t.nodes[t.root].apparent, 100 + 1200 + 100);
        // Equal inode numbers on different devices are different files.
        let mut t = tree_with_two_links();
        t.apply(Batch {
            parent_id: 0,
            nodes: vec![NewNode {
                dev: 2,
                ..link("elsewhere", 50)
            }],
            error: None,
            more: false,
            ext: Vec::new(),
        });
        assert!(!t.nodes[idx_of(&t, "elsewhere")].shared);
    }

    /// Deleting the link that carries the size leaves the inode on disk: another link still
    /// holds it. The size moves to that link, and the total above both does not change.
    #[test]
    fn deleting_the_counted_link_moves_its_size_to_a_surviving_link() {
        let mut t = tree_with_two_links();
        let (one, two) = (idx_of(&t, "one"), idx_of(&t, "two"));
        let (a, b) = (idx_of(&t, "a"), idx_of(&t, "b"));
        let total = (t.nodes[t.root].apparent, t.nodes[t.root].disk);

        t.delete_subtree(one);
        assert!(!t.nodes[two].shared, "the surviving link is counted now");
        assert_eq!(t.nodes[a].apparent, 100 + 100);
        assert_eq!(t.nodes[b].apparent, 100 + 1000);
        assert_eq!((t.nodes[t.root].apparent, t.nodes[t.root].disk), total);

        // The last link gone, the inode is gone.
        t.delete_subtree(two);
        assert_eq!(t.nodes[t.root].apparent, total.0 - 1000);
        assert_eq!(t.nodes[t.root].disk, total.1 - 1000);
    }

    /// The same when the counted link goes as part of a directory.
    #[test]
    fn deleting_a_directory_with_the_counted_link_moves_its_size() {
        let mut t = tree_with_two_links();
        let (two, a, b) = (idx_of(&t, "two"), idx_of(&t, "a"), idx_of(&t, "b"));
        let before = t.nodes[t.root].apparent;

        t.delete_subtree(a);
        assert!(!t.nodes[two].shared);
        assert_eq!(t.nodes[b].apparent, 100 + 1000);
        // Gone: the directory itself (100) and `x` (100). The inode stays.
        assert_eq!(t.nodes[t.root].apparent, before - 200);
    }

    #[test]
    fn hard_links_in_counts_links_of_any_kind() {
        let t = tree_with_two_links();
        assert_eq!(t.hard_links_in(t.root), 2);
        assert_eq!(t.hard_links_in(idx_of(&t, "a")), 1);
        assert_eq!(
            t.hard_links_in(idx_of(&t, "two")),
            1,
            "a duplicate is one too"
        );
        assert_eq!(t.hard_links_in(idx_of(&t, "x")), 0);
    }

    /// Deleting a duplicate frees nothing and changes no size.
    #[test]
    fn deleting_a_duplicate_link_changes_no_size() {
        let mut t = tree_with_two_links();
        let (one, two) = (idx_of(&t, "one"), idx_of(&t, "two"));
        let before = t.nodes[t.root].apparent;
        t.delete_subtree(two);
        assert!(!t.nodes[one].shared);
        assert_eq!(t.nodes[t.root].apparent, before);
        // With the duplicate gone, deleting the counted link removes the inode.
        t.delete_subtree(one);
        assert_eq!(t.nodes[t.root].apparent, before - 1000);
    }

    /// A rescan must count each inode once too, whichever side of its boundary the links are
    /// on. The counted link inside the rescanned directory: the outside link takes over at
    /// once, and the link that comes back is the duplicate.
    #[test]
    fn rescan_hands_the_count_to_a_link_outside() {
        let mut t = tree_with_two_links();
        let (two, a) = (idx_of(&t, "two"), idx_of(&t, "a"));
        let total = t.nodes[t.root].apparent;
        let base = crate::scan::SCAN_ID_STRIDE;

        t.begin_refresh(a, base);
        assert!(
            !t.nodes[two].shared,
            "counted while `a` is being read again"
        );
        assert_eq!(t.nodes[t.root].apparent, total - 100, "only `x` is missing");
        t.apply(Batch {
            parent_id: base,
            nodes: vec![link("one", 50), file("x", 11)],
            error: None,
            more: false,
            ext: Vec::new(),
        });
        let again = t.nodes[a].children[0];
        assert!(
            t.nodes[again].shared,
            "the link that came back is the duplicate"
        );
        assert_eq!(t.nodes[t.root].apparent, total);
    }

    /// The counted link outside the rescanned directory: the link inside stays a duplicate.
    #[test]
    fn rescan_does_not_count_a_link_twice() {
        let mut t = tree_with_two_links();
        let (one, b) = (idx_of(&t, "one"), idx_of(&t, "b"));
        let total = t.nodes[t.root].apparent;
        let base = crate::scan::SCAN_ID_STRIDE;

        t.begin_refresh(b, base);
        assert_eq!(t.nodes[t.root].apparent, total);
        t.apply(Batch {
            parent_id: base,
            nodes: vec![link("two", 50)],
            error: None,
            more: false,
            ext: Vec::new(),
        });
        assert!(!t.nodes[one].shared);
        assert!(t.nodes[t.nodes[b].children[0]].shared);
        assert_eq!(t.nodes[t.root].apparent, total, "counted once, as before");
    }

    /// An excluded hard link takes no part: it is never the counted link and never takes
    /// the count over.
    #[test]
    fn excluded_links_stay_out_of_the_count() {
        let mut t = Tree::new("/r".into(), 100, 100, 1, 1);
        t.apply(Batch {
            parent_id: 0,
            nodes: vec![
                NewNode {
                    excluded: Excluded::Pattern,
                    ..link("skipped", 50)
                },
                link("kept", 50),
            ],
            error: None,
            more: false,
            ext: Vec::new(),
        });
        let (skipped, kept) = (idx_of(&t, "skipped"), idx_of(&t, "kept"));
        assert!(
            !t.nodes[kept].shared,
            "the excluded link did not take the count"
        );
        assert_eq!(t.nodes[t.root].apparent, 100 + 1000);
        t.delete_subtree(kept);
        assert!(!t.nodes[skipped].counts());
        assert_eq!(t.nodes[t.root].apparent, 100);
    }

    /// Hard-link duplicates and excluded entries don't count toward sizes, but they are
    /// listed entries, so they count as items. An excluded directory contributes just itself.
    #[test]
    fn shared_and_excluded_entries_are_items() {
        let mut t = Tree::new("/r".into(), 100, 100, 1, 1);
        // Two links to one inode: the second is the duplicate.
        let counted = NewNode {
            hlink: true,
            nlink: 2,
            ..file("f", 10)
        };
        let shared = NewNode {
            hlink: true,
            nlink: 2,
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
            nodes: vec![counted, shared, excluded_dir, excluded_file],
            error: None,
            more: false,
            ext: Vec::new(),
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
            more: false,
            ext: Vec::new(),
        });
        assert_eq!(t.nodes[a].items, 3);
        assert_eq!(t.nodes[t.root].items, 5);
        t.apply(Batch {
            parent_id: base + 1,
            nodes: vec![file("y", 12)],
            error: None,
            more: false,
            ext: Vec::new(),
        });
        assert_eq!(t.nodes[a].items, 4);
        assert_eq!(t.nodes[t.root].items, 6);
        assert_eq!(t.nodes[t.root].items, entries_below(&t, t.root));
    }

    /// Sizes saturate instead of wrapping. Apparent sizes near 2^63 are legal (a sparse file
    /// made with `truncate -s 8E`), and two of them in one tree exceed a `u64`.
    #[test]
    fn sizes_saturate() {
        let huge = u64::MAX / 2 + 1;
        let big = |name: &str, ino| NewNode {
            apparent: huge,
            disk: huge,
            ..file(name, ino)
        };
        // Two in one batch, into a directory that sits below the root.
        let mut t = Tree::new("/r".into(), 100, 100, 1, 1);
        t.apply(Batch {
            parent_id: 0,
            nodes: vec![dir("a", 1)],
            error: None,
            more: false,
            ext: Vec::new(),
        });
        t.apply(Batch {
            parent_id: 1,
            nodes: vec![big("x", 10), big("y", 11)],
            error: None,
            more: false,
            ext: Vec::new(),
        });
        let a = idx_of(&t, "a");
        for idx in [a, t.root] {
            assert_eq!(t.nodes[idx].apparent, u64::MAX);
            assert_eq!(t.nodes[idx].disk, u64::MAX);
        }
        // The derived own size stays within bounds as well.
        assert_eq!(t.own_apparent(a), 0);
        assert_eq!(t.own_disk(a), 0);

        // One by one, as an import adds them.
        let mut t = Tree::new_imported("/r".into(), 100, 100, 1, 1);
        for (name, ino) in [("x", 10), ("y", 11)] {
            t.import_child(
                t.root,
                name.into(),
                NodeKind::File,
                huge,
                huge,
                1,
                ino,
                false,
                false,
                Excluded::No,
                false,
            );
        }
        assert_eq!(t.nodes[t.root].apparent, u64::MAX);
        assert_eq!(t.nodes[t.root].disk, u64::MAX);
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
            more: false,
            ext: Vec::new(),
        });
        assert_eq!(t.nodes[a].items, u32::MAX);
        assert_eq!(t.nodes[t.root].items, u32::MAX);
    }
}
