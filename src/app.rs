//! Application state and input handling.
//!
//! Selection tracks a node's *identity*, not its row position, so the highlight stays glued to
//! the same entry even as live size updates reshuffle the sort order during a scan.

use std::cell::{Cell, RefCell};
use std::io::{BufWriter, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::time::{Duration, Instant};

use crossbeam_channel::{unbounded, Receiver};

use crate::delete::{Plan, Report};
use crate::model::{Excluded, NodeIdx, NodeKind, Tree};
use crate::scan::{
    check_identity, start_at, Batch, Opts, Scan, ScanControl, NOT_THE_SCANNED_DIR, SCAN_ID_STRIDE,
};

#[derive(Clone, Copy, PartialEq)]
pub enum SortKey {
    Size,
    Name,
}

/// One row of the top-files popup: a file, its path relative to the directory the popup was
/// opened from, and its size in the metric active when the popup was opened.
pub struct TopEntry {
    pub idx: NodeIdx,
    pub path: String,
    pub size: u64,
}

/// State of the top-files popup (largest files under the viewed directory).
pub struct TopFiles {
    pub items: Vec<TopEntry>,
    pub selected: usize,
    /// First row on screen; see [`scroll_offset`].
    pub offset: Cell<usize>,
}

/// While a scan keeps changing the tree, the viewed directory is sorted again only after this
/// many times the duration of its last sort, so sorting takes at most a tenth of the time. A
/// small directory sorts in microseconds and is in order on every frame; one with a million
/// entries is sorted again about once a second.
const SORT_SHARE: u32 = 10;

/// Whether rows that were sorted `elapsed` ago, in a sort that took `took`, are due for
/// another sort.
fn resort_due(elapsed: Duration, took: Duration) -> bool {
    elapsed >= took * SORT_SHARE
}

/// The rows of the viewed directory in display order, kept between frames. Sorting a
/// directory with a million entries takes a noticeable part of a second; doing it for every
/// frame and every key press made such directories unusable.
struct RowOrder {
    // What the order was computed for.
    cur: NodeIdx,
    sort: SortKey,
    disk_usage: bool,
    /// `Tree::generation` at that moment: equal means nothing has changed since.
    generation: u64,
    /// `Tree::detached` at that moment: a difference means entries were removed.
    detached: u64,
    rows: Rc<[NodeIdx]>,
    /// The largest size among the rows, which scales the size bars.
    max_size: u64,
    /// The frame it was computed in, if the event loop is keeping time.
    at: Option<Instant>,
    /// How long the sort took.
    took: Duration,
}

/// Rows a list keeps between the highlighted row and its top or bottom edge while there is
/// more to scroll to, so the user sees what comes next.
const SCROLL_MARGIN: usize = 2;

/// Where a list has to be scrolled to: the index of the first row to show.
///
/// `offset` is where it is scrolled to now, `selected` the highlighted row, `len` the number
/// of rows in the list and `rows` the number that fit on screen. The list moves only when it
/// has to: when the highlighted row would otherwise come within [`SCROLL_MARGIN`] of an edge
/// (or off screen), and then by as little as possible. Inside the page the highlight moves and
/// the list stands still. Every list keeps the offset this returns and passes it back in on
/// the next frame; starting from 0 each time would pin the highlight to the bottom edge.
pub fn scroll_offset(offset: usize, selected: Option<usize>, len: usize, rows: usize) -> usize {
    if rows == 0 {
        return offset;
    }
    let last_offset = len.saturating_sub(rows);
    let mut offset = offset.min(last_offset);
    let Some(selected) = selected else {
        return offset;
    };
    // A window of one or two rows has no room for a margin.
    let margin = SCROLL_MARGIN.min((rows - 1) / 2);
    if selected < offset + margin {
        offset = selected.saturating_sub(margin);
    } else if selected + margin >= offset + rows {
        offset = selected + margin + 1 - rows;
    }
    // No margin is kept past the ends of the list.
    offset.min(last_offset)
}

/// How many rows the top-files popup shows.
const TOP_FILES_LIMIT: usize = 100;

/// Status shown when an action is asked of a node that a delete or rescan has since removed.
const GONE: &str = "that entry is no longer in the tree";

/// How long the whole-tree search waits after the last keystroke before it runs.
pub const FIND_DEBOUNCE: Duration = Duration::from_millis(100);
/// The most hits the whole-tree search keeps. Broad queries (`e`) can match half the tree;
/// past the cap, matches are only counted, and the popup asks for a narrower query.
pub const FIND_CAP: usize = 10_000;
/// The shortest interval between re-sorts of the hit list while it grows or sizes change.
const FIND_RESORT: Duration = Duration::from_millis(250);

/// State of the search popup (`/`).
///
/// The search is one linear pass over `tree.nodes`, sliced across frames by a cursor so a huge
/// tree never blocks the UI. Nodes are only ever appended, so the same cursor also picks up
/// entries that a running scan or rescan adds later: live results need no special case.
pub struct Find {
    /// The query being typed.
    pub query: String,
    /// The directory being viewed behind the popup. `App::find_tick` keeps it current.
    pub dir: NodeIdx,
    /// Tab switches between the whole tree (false) and what lies below `dir` (true).
    pub scoped: bool,
    /// The query was edited since the last tick; the next tick (re)arms the debounce.
    edited: bool,
    /// When the debounced query is due to run.
    due: Option<Instant>,
    /// The query the current pass matches against; None while nothing is searched.
    pub active: Option<String>,
    /// Next node the pass will look at.
    pub cursor: NodeIdx,
    /// Matching nodes (at most `FIND_CAP`), sorted for display.
    pub hits: Vec<NodeIdx>,
    /// Every match found, including those past the cap.
    pub total: usize,
    /// Highlighted hit, by identity, so re-sorts and live inserts don't move the highlight.
    pub selected: Option<NodeIdx>,
    /// First hit on screen; see [`scroll_offset`].
    pub offset: Cell<usize>,
    sort_dirty: bool,
    last_sort: Option<Instant>,
}

impl Find {
    fn new(dir: NodeIdx) -> Self {
        Find {
            query: String::new(),
            dir,
            scoped: false,
            edited: false,
            due: None,
            active: None,
            cursor: 0,
            hits: Vec::new(),
            total: 0,
            selected: None,
            offset: Cell::new(0),
            sort_dirty: false,
            last_sort: None,
        }
    }

    /// True while the pass has nodes left to look at.
    pub fn busy(&self, tree: &Tree) -> bool {
        self.active.is_some() && self.cursor < tree.nodes.len()
    }

    /// True once more matches were found than the list keeps.
    pub fn capped(&self) -> bool {
        self.total > self.hits.len()
    }

    /// The directory the search is limited to, if it is.
    pub fn scope(&self) -> Option<NodeIdx> {
        self.scoped.then_some(self.dir)
    }

    /// Switch between the whole tree and the viewed directory, and search again.
    fn toggle_scope(&mut self) {
        self.scoped = !self.scoped;
        self.restart();
    }

    /// The viewed directory changed behind the popup (a deletion finished and took it away).
    fn set_dir(&mut self, dir: NodeIdx) {
        if self.dir != dir {
            self.dir = dir;
            if self.scoped {
                self.restart();
            }
        }
    }

    /// Start a fresh pass over the tree for the active query.
    fn restart(&mut self) {
        self.cursor = 0;
        self.hits.clear();
        self.total = 0;
        self.selected = None;
        self.sort_dirty = false;
    }

    /// One frame's worth of work: debounce, then up to `budget` nodes of the pass, then a
    /// (throttled) re-sort. `now` is passed in so tests can drive the clock.
    fn tick(&mut self, tree: &Tree, disk_usage: bool, scanning: bool, now: Instant, budget: usize) {
        if self.edited {
            self.edited = false;
            self.due = Some(now + FIND_DEBOUNCE);
        }
        if self.due.is_some_and(|d| now >= d) {
            self.due = None;
            self.active = (!self.query.is_empty()).then(|| self.query.clone());
            self.restart();
        }
        let Some(q) = &self.active else { return };

        let len = tree.nodes.len();
        let was_busy = self.cursor < len;
        let end = len.min(self.cursor.saturating_add(budget));
        // Tombstones exist only after a delete or rescan; until then skip the parent walk.
        let check_attached = tree.detached > 0;
        // The whole pass stays linear: the walk up to the scope is paid only by names that
        // match. Scoped to the root is the whole tree.
        let scope = self.scope().filter(|&d| d != tree.root);
        for i in self.cursor..end {
            if i != tree.root
                && contains_ci(&tree.nodes[i].name, q)
                && scope.is_none_or(|d| i != d && tree.is_within(i, d))
                && (!check_attached || tree.is_attached(i))
            {
                self.total += 1;
                if self.hits.len() < FIND_CAP {
                    self.hits.push(i);
                    self.sort_dirty = true;
                }
            }
        }
        self.cursor = end;
        let finished = was_busy && end == len;

        // Sizes keep changing while a scan runs, so the order goes stale even without new hits.
        let stale = self.sort_dirty || (scanning && !self.hits.is_empty());
        let throttled = self
            .last_sort
            .is_some_and(|t| now.saturating_duration_since(t) < FIND_RESORT);
        if stale && (finished || !throttled) {
            let size = |i: NodeIdx| {
                let n = &tree.nodes[i];
                if disk_usage {
                    n.disk
                } else {
                    n.apparent
                }
            };
            self.hits.sort_by(|&a, &b| {
                size(b)
                    .cmp(&size(a))
                    .then_with(|| tree.nodes[a].name.cmp(&tree.nodes[b].name))
                    .then_with(|| a.cmp(&b))
            });
            self.sort_dirty = false;
            self.last_sort = Some(now);
        }
        self.ensure_selection();
    }

    /// Drop hits that a delete or rescan turned into tombstones. Past the cap the true count
    /// can't be corrected from the kept hits alone, so the pass starts over instead.
    fn prune(&mut self, tree: &Tree) {
        if self.active.is_none() {
            return;
        }
        if self.capped() {
            self.restart();
            return;
        }
        let before = self.hits.len();
        self.hits.retain(|&i| tree.is_attached(i));
        self.total -= before - self.hits.len();
        self.ensure_selection();
    }

    fn ensure_selection(&mut self) {
        match self.selected {
            Some(s) if self.hits.contains(&s) => {}
            _ => self.selected = self.hits.first().copied(),
        }
    }

    /// Position of the highlighted hit in the list.
    pub fn selected_pos(&self) -> Option<usize> {
        self.selected
            .and_then(|s| self.hits.iter().position(|&h| h == s))
    }

    fn move_selection(&mut self, delta: isize) {
        if self.hits.is_empty() {
            return;
        }
        let pos = self.selected_pos().unwrap_or(0) as isize;
        let new = (pos + delta).clamp(0, self.hits.len() as isize - 1) as usize;
        self.selected = Some(self.hits[new]);
    }
}

/// How raw keys should be decoded right now.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InputMode {
    Normal,
    /// The search popup is open: letters go to its query.
    Find,
}

/// A transient modal overlay.
pub enum Modal {
    None,
    Help,
    /// Confirm deletion of the given node.
    ConfirmDelete(NodeIdx),
    /// Confirm an export while directories are still unread: the dump will be partial.
    ConfirmExport,
    /// The largest files under the directory the popup was opened from.
    TopFiles(Box<TopFiles>),
    /// Details of the selected entry (tree data plus a fresh on-disk stat).
    Info(NodeIdx),
    /// Search by name, in the whole tree or below the viewed directory.
    Find(Box<Find>),
}

/// An in-progress deletion running on a background thread, so the UI stays responsive and shows
/// a spinner while a large directory is removed.
struct PendingDelete {
    idx: NodeIdx,
    /// The real path being removed; the worker reports what it could not remove below it.
    path: PathBuf,
    rx: Receiver<Report>,
}

/// A system-open running on a background thread, so a slow or hung opener can't freeze the UI.
struct PendingOpen {
    path: String,
    rx: Receiver<std::io::Result<()>>,
}

/// One live batch stream: the original scan (`refresh: false`) or a subtree rescan
/// (`refresh: true`). `alive` flips false once the stream's channel disconnects.
struct ScanHandle {
    rx: Receiver<Batch>,
    refresh: bool,
    alive: bool,
    /// When this stream started, and the counted bytes it has delivered, in both metrics:
    /// the footer shows the throughput of the streams that are running.
    started: Instant,
    apparent: u64,
    disk: u64,
}

impl ScanHandle {
    fn new(rx: Receiver<Batch>, refresh: bool, started: Instant) -> Self {
        ScanHandle {
            rx,
            refresh,
            alive: true,
            started,
            apparent: 0,
            disk: 0,
        }
    }
}

pub struct App {
    pub tree: Tree,
    /// Directory currently being viewed.
    pub cur: NodeIdx,
    /// Currently highlighted child node (by identity), if any.
    pub selected: Option<NodeIdx>,
    pub sort: SortKey,
    /// Show on-disk usage (`st_blocks`) vs. apparent size (`st_size`).
    pub disk_usage: bool,
    /// Show the item count column (`c`, `--show-itemcount`): entries below each directory.
    pub show_item_count: bool,
    pub scanning: bool,
    /// Read-only: deletion disabled (ncdu `-r`).
    pub read_only: bool,
    /// The tree was loaded from a dump (`-f`) rather than scanned. Its paths describe whatever
    /// machine and moment the dump was taken on, not the local filesystem as it is now, so
    /// nothing may act on them: no delete, shell, open, rescan or stat. ncdu does the same.
    pub imported: bool,
    pub quit: bool,
    pub modal: Modal,
    /// Transient one-line message shown in the footer until the next key press.
    pub status: Option<String>,
    /// A deletion currently running on a background thread.
    pending_delete: Option<PendingDelete>,
    /// Opens still running on background threads, oldest first.
    pending_open: Vec<PendingOpen>,
    /// True once the user has pressed quit during a deletion; a second press force-quits.
    quit_armed: bool,
    /// Batch streams of every live scan: the original one plus any subtree rescans.
    scans: Vec<ScanHandle>,
    /// Steering handles for every live scan; navigation re-prioritizes each one.
    controls: Vec<ScanControl>,
    /// Scan options, kept so a subtree rescan can reuse them (same threads, excludes, `-x`).
    /// None when browsing a loaded dump (`-f`), where there is nothing to rescan.
    scan_opts: Option<Opts>,
    /// The subtree currently being rescanned in place, if any.
    pub refreshing_idx: Option<NodeIdx>,
    /// A directory to read again as soon as the running rescan has finished: a deletion left
    /// things behind in it while another rescan held the single rescan slot.
    rescan_queued: Option<NodeIdx>,
    /// The running rescan was started to bring the tree back in step after a deletion, not by
    /// the user. Its end is not announced: the footer keeps the message that says what the
    /// deletion could not remove.
    rescan_is_resync: bool,
    /// Generation counter for subtree rescans; each gets a disjoint scanner-id range.
    next_scan_base: u64,
    /// A pending request to suspend the TUI and spawn `$SHELL` in this directory, consumed
    /// once per frame by the event loop (which owns the terminal).
    shell_request: Option<PathBuf>,
    /// Where the export key writes its dump; `None` means the working directory at that
    /// moment. Tests point it at a scratch directory.
    export_dir: Option<PathBuf>,
    /// How many hard-linked files the entry in the delete dialog is or contains, counted
    /// when the dialog opens (a walk of the subtree, too slow to repeat every frame).
    pub confirm_hard_links: u32,
    /// First row of the directory listing on screen; see [`scroll_offset`]. Drawing updates
    /// it, because only drawing knows how many rows fit.
    pub list_offset: Cell<usize>,
    /// The sorted rows of the viewed directory, reused while they are still good.
    row_order: RefCell<Option<RowOrder>>,
    /// When the frame being prepared started. The event loop sets it; without it (in tests)
    /// the rows are sorted again after every change, with it only when `resort_due` says so.
    frame_time: Option<Instant>,
    /// Spinner animation frame.
    pub tick: usize,
}

impl App {
    pub fn new(tree: Tree, disk_usage: bool, read_only: bool, scanning: bool) -> Self {
        let mut tree = tree;
        if !scanning {
            // An imported dump arrives complete: release the scan-only bookkeeping up front.
            tree.finish_scan();
        }
        App {
            cur: tree.root,
            selected: None,
            sort: SortKey::Size,
            disk_usage,
            show_item_count: false,
            scanning,
            read_only,
            imported: false,
            quit: false,
            modal: Modal::None,
            status: None,
            pending_delete: None,
            pending_open: Vec::new(),
            quit_armed: false,
            scans: Vec::new(),
            controls: Vec::new(),
            scan_opts: None,
            refreshing_idx: None,
            rescan_queued: None,
            rescan_is_resync: false,
            next_scan_base: 1,
            shell_request: None,
            export_dir: None,
            confirm_hard_links: 0,
            list_offset: Cell::new(0),
            row_order: RefCell::new(None),
            frame_time: None,
            tick: 0,
            tree,
        }
    }

    /// Attach the initial scan: its batch stream, its steering handle, and the options future
    /// subtree rescans should reuse (interactive scan mode only).
    pub fn attach_scan(&mut self, scan: Scan, opts: Opts) {
        self.scans
            .push(ScanHandle::new(scan.events, false, Instant::now()));
        self.controls.push(scan.control);
        self.scan_opts = Some(opts);
        self.refocus();
    }

    /// Tell every live scan to prioritize the directory currently being viewed.
    fn refocus(&mut self) {
        if !self.scanning {
            return;
        }
        let Some(path) = self.tree.fs_path_of(self.cur) else {
            return;
        };
        let focus = Some(path);
        for ctrl in &self.controls {
            ctrl.set_focus(focus.clone());
        }
    }

    /// Drain every live scan's batch stream into the tree (up to `cap` batches per frame, so a
    /// huge filesystem can't starve input handling). Marks streams dead when their channel
    /// disconnects; when a rescan's stream ends the spinner clears, and when the last stream
    /// ends the scan is over and the scan-only bookkeeping is released.
    pub fn poll_scans(&mut self, cap: usize) {
        let mut applied = 0usize;
        for i in 0..self.scans.len() {
            if !self.scans[i].alive {
                continue;
            }
            loop {
                match self.scans[i].rx.try_recv() {
                    Ok(batch) => {
                        // Count the batch's bytes before `apply` moves it, for the footer's
                        // throughput display.
                        let stream = &mut self.scans[i];
                        for n in batch.nodes.iter().filter(|n| n.excluded == Excluded::No) {
                            stream.apparent = stream.apparent.saturating_add(n.apparent);
                            stream.disk = stream.disk.saturating_add(n.disk);
                        }
                        self.tree.apply(batch);
                        applied += 1;
                        if applied >= cap {
                            break;
                        }
                    }
                    Err(crossbeam_channel::TryRecvError::Empty) => break,
                    Err(crossbeam_channel::TryRecvError::Disconnected) => {
                        self.scans[i].alive = false;
                        break;
                    }
                }
            }
        }

        if self.refreshing_idx.is_some() && !self.scans.iter().any(|h| h.alive && h.refresh) {
            let idx = self.refreshing_idx.take().unwrap();
            if !std::mem::take(&mut self.rescan_is_resync) {
                self.status = Some(format!("rescanned {}", self.tree.path_of(idx)));
            }
            // The rescan slot is free again: read the directory a deletion left out of step.
            if let Some(queued) = self.rescan_queued.take() {
                self.resync(queued);
            }
        }
        let any_alive = self.scans.iter().any(|h| h.alive);
        if self.scanning && !any_alive {
            self.scanning = false;
            self.tree.finish_scan();
        }
    }

    pub fn is_deleting(&self) -> bool {
        self.pending_delete.is_some()
    }

    /// True if a deletion in progress covers `sel` (the node itself or an ancestor of it).
    fn deletion_covers(&self, sel: NodeIdx) -> bool {
        self.deleting_idx()
            .is_some_and(|d| self.tree.is_within(sel, d))
    }

    /// True if a deletion is in progress anywhere in `sel`'s subtree: `sel` is being deleted,
    /// lies inside what is being deleted, or contains it.
    fn deletion_touches(&self, sel: NodeIdx) -> bool {
        self.deleting_idx()
            .is_some_and(|d| self.tree.is_within(sel, d) || self.tree.is_within(d, sel))
    }

    /// While browsing an imported dump, refuse an action that would touch the filesystem:
    /// sets the status and returns true when `what` must not run.
    fn refused_for_import(&mut self, what: &str) -> bool {
        if self.imported {
            self.status = Some(format!("{what} is disabled when browsing an imported dump"));
        }
        self.imported
    }

    /// Rescan the viewed directory in place (at the top level: the whole tree). The model
    /// tears its subtree down and a new scanner generation re-reads it from disk, grafting
    /// back into the same node. Browsing stays live the whole time; totals of surrounding
    /// directories stay correct throughout.
    fn request_refresh(&mut self) {
        if self.refused_for_import("rescan") {
            return;
        }
        let sel = self.cur;
        if self.scan_opts.is_none() {
            self.status = Some("rescan unavailable: no scan is attached".into());
            return;
        }
        if self.refreshing_idx.is_some() {
            self.status = Some("a rescan is already in progress — please wait".into());
            return;
        }
        // An excluded directory was left out on purpose (`--exclude`, `-x`, the mtime
        // filters). A rescan of it, or of anything below it, would read what the scan skipped.
        if self.tree.in_excluded(sel) {
            self.status = Some("can't rescan: excluded directories are not scanned".into());
            return;
        }
        // Scanning a subtree that is being removed from disk would race the deletion, and the
        // rescan tears down every node below `sel`, the one being deleted included. So this
        // has to hold for a deletion anywhere below `sel` as well as around it.
        if self.deletion_touches(sel) {
            self.status = Some("can't rescan: a deletion is in progress here".into());
            return;
        }
        // The subtree must be fully known before tearing it down — otherwise the original
        // scan could still deliver batches for it and double-count.
        if !self.tree.subtree_complete(sel) {
            self.status = Some("cannot rescan: directory is still being scanned".into());
            return;
        }
        // Last, the disk: the path must still name the directory that was scanned. If it was
        // replaced (by a symlink, or by another directory), reading it would list something
        // else as this directory's content.
        let Some(path) = self.tree.fs_path_of(sel) else {
            self.status = Some(GONE.into());
            return;
        };
        let node = &self.tree.nodes[sel];
        if let Err(why) = check_identity(&path, (node.dev, node.ino)) {
            let hint = match (why == NOT_THE_SCANNED_DIR, node.parent) {
                (true, Some(_)) => " — go up and rescan from there",
                (true, None) => " — restart icdu to scan it",
                (false, _) => "",
            };
            self.status = Some(format!("can't rescan: {why}{hint}"));
            return;
        }
        self.status = Some(match self.start_rescan(sel) {
            Some(path) => format!("rescanning {path}…"),
            None => GONE.into(),
        });
    }

    /// Tear the subtree below `idx` down and start a scanner generation that reads it again.
    /// The caller has checked the gates: `idx` is a fully scanned directory, no other rescan
    /// is running, no deletion touches it. Returns the path being read, for display, or `None`
    /// when `idx` has left the tree or no scan options are attached.
    fn start_rescan(&mut self, idx: NodeIdx) -> Option<String> {
        let opts = self.scan_opts.clone()?;
        let path = self.tree.fs_path_of(idx)?;
        let id_base = self.next_scan_base * SCAN_ID_STRIDE;
        self.next_scan_base += 1;
        let node = &self.tree.nodes[idx];
        let identity = (node.dev, node.ino);
        self.tree.begin_refresh(idx, id_base);
        let scan = start_at(path, opts, id_base, Some(identity));
        self.prune_find();
        self.scans
            .push(ScanHandle::new(scan.events, true, Instant::now()));
        self.controls.push(scan.control);
        self.refreshing_idx = Some(idx);
        self.scanning = true;
        Some(self.tree.path_of(idx))
    }

    /// Bring the tree below `idx` back in step with the disk after a deletion that left
    /// things behind: the directory is read again. Only one rescan runs at a time, so if
    /// that slot is taken the directory waits for it. Returns whether a rescan was started
    /// or queued.
    fn resync(&mut self, idx: NodeIdx) -> bool {
        if !self.tree.is_attached(idx) || !self.tree.nodes[idx].is_dir() {
            return false;
        }
        if self.refreshing_idx.is_some() {
            self.rescan_queued = Some(idx);
            return true;
        }
        self.rescan_is_resync = self.start_rescan(idx).is_some();
        self.rescan_is_resync
    }

    /// Bytes per second the running scans are bringing in, for the footer, in the size
    /// metric on screen. None when no scan is running.
    pub fn scan_rate(&self) -> Option<u64> {
        self.scan_rate_at(Instant::now())
    }

    /// [`App::scan_rate`] at a given moment: the bytes of every stream that is still running,
    /// over the time since the oldest of them started (at least one second, so the first
    /// second reports what has been seen so far). Each stream keeps its own count, so a
    /// rescan neither resets the figure of a scan that is still running nor inherits the
    /// bytes of one that has finished.
    fn scan_rate_at(&self, now: Instant) -> Option<u64> {
        let running = || self.scans.iter().filter(|s| s.alive);
        let started = running().map(|s| s.started).min()?;
        let bytes = running().fold(0u64, |sum, s| {
            sum.saturating_add(if self.disk_usage { s.disk } else { s.apparent })
        });
        Some(bytes / now.saturating_duration_since(started).as_secs().max(1))
    }

    /// Directories the running scans have not read yet, in the whole tree.
    pub fn dirs_left(&self) -> u32 {
        self.tree.nodes[self.tree.root].unscanned
    }

    /// Export the whole tree as it is right now (including deletions made this session) to the
    /// next free `icdu-dump*.json` in the working directory, in the same ncdu format as `-o`.
    /// Synchronous: dumps write fast relative to scanning, and the outcome lands in the
    /// status line.
    ///
    /// While directories are still unread, the dump would be partial yet look like the result
    /// of a finished scan to whoever reads it, so that case is confirmed first.
    fn request_export(&mut self) {
        if self.tree.unread_dirs() > 0 {
            self.modal = Modal::ConfirmExport;
        } else {
            self.export_now();
        }
    }

    /// Write the dump without further questions.
    fn export_now(&mut self) {
        let dir = self
            .export_dir
            .clone()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        self.export_into(&dir);
    }

    fn export_into(&mut self, dir: &Path) {
        let written = create_export_file(dir)
            .and_then(|(path, file)| self.export_tree_to(file).map(|()| path));
        match written {
            Ok(path) => {
                let entries = self.tree.total_dirs + self.tree.total_files;
                let path = path.display();
                self.status = Some(match self.tree.unread_dirs() {
                    0 => format!("exported {entries} entries to {path}"),
                    1 => format!(
                        "partial export ({entries} entries, 1 directory not read yet) to {path}"
                    ),
                    n => format!(
                        "partial export ({entries} entries, {n} directories not read yet) to {path}"
                    ),
                });
            }
            Err(e) => self.status = Some(format!("export failed: {e}")),
        }
    }

    /// Write the tree as ncdu-compatible JSON (the `-o` format, via `dump::export`).
    fn export_tree_to(&self, file: std::fs::File) -> std::io::Result<()> {
        let mut w = BufWriter::new(file);
        crate::dump::export(&self.tree, &mut w)?;
        w.flush()
    }

    /// The aggregated size metric we currently sort and display by.
    pub fn size_of(&self, idx: NodeIdx) -> u64 {
        let n = &self.tree.nodes[idx];
        if self.disk_usage {
            n.disk
        } else {
            n.apparent
        }
    }

    /// The node's own (non-recursive) size in the current metric.
    pub fn own_size_of(&self, idx: NodeIdx) -> u64 {
        if self.disk_usage {
            self.tree.own_disk(idx)
        } else {
            self.tree.own_apparent(idx)
        }
    }

    /// Tell the app when the frame it is about to prepare started. With a frame clock, the
    /// rows of the viewed directory are not sorted again for every change a running scan
    /// makes, only every so often.
    pub fn begin_frame(&mut self, now: Instant) {
        self.frame_time = Some(now);
    }

    /// Children of the current directory, sorted for display (largest first, or by name).
    ///
    /// The order is cached. It is computed again when the user changed what it depends on
    /// (directory, sort key, size metric) and when entries were removed. Changes that only
    /// a running scan makes (new entries, growing sizes) arrive with every frame; for those
    /// the rows are sorted again once ten times the duration of the last sort has passed, so
    /// sorting never takes more than a tenth of the time.
    pub fn sorted_children(&self) -> Rc<[NodeIdx]> {
        self.with_row_order(|order| Rc::clone(&order.rows))
    }

    /// The largest size among the rows of the viewed directory (0 if it has none).
    pub fn max_row_size(&self) -> u64 {
        self.with_row_order(|order| order.max_size)
    }

    fn with_row_order<T>(&self, read: impl FnOnce(&RowOrder) -> T) -> T {
        let mut cache = self.row_order.borrow_mut();
        let order = match cache.take() {
            Some(order) if self.row_order_is_good(&order) => order,
            _ => self.compute_row_order(),
        };
        read(cache.insert(order))
    }

    fn row_order_is_good(&self, order: &RowOrder) -> bool {
        let same_view = order.cur == self.cur
            && order.sort == self.sort
            && order.disk_usage == self.disk_usage
            && order.detached == self.tree.detached;
        if !same_view {
            return false;
        }
        if order.generation == self.tree.generation {
            return true;
        }
        // Only sizes and new entries differ. Good enough until another sort is affordable.
        match (self.frame_time, order.at) {
            (Some(now), Some(at)) => !resort_due(now.saturating_duration_since(at), order.took),
            _ => false,
        }
    }

    fn compute_row_order(&self) -> RowOrder {
        let started = Instant::now();
        let rows: Rc<[NodeIdx]> = self
            .order_children(self.tree.nodes[self.cur].children.clone())
            .into();
        let max_size = match self.sort {
            SortKey::Size => rows.first().map_or(0, |&first| self.size_of(first)),
            SortKey::Name => rows.iter().map(|&k| self.size_of(k)).max().unwrap_or(0),
        };
        RowOrder {
            cur: self.cur,
            sort: self.sort,
            disk_usage: self.disk_usage,
            generation: self.tree.generation,
            detached: self.tree.detached,
            rows,
            max_size,
            at: self.frame_time,
            took: started.elapsed(),
        }
    }

    /// Sort children the way every view presents them (largest first, or by name), so
    /// traversals like the next-error jump match what the user sees on screen.
    fn order_children(&self, mut kids: Vec<NodeIdx>) -> Vec<NodeIdx> {
        let name = |k: NodeIdx| self.tree.nodes[k].name.as_str();
        match self.sort {
            SortKey::Size => {
                // The sizes are fetched once, next to each other, instead of from two nodes
                // somewhere in memory for every comparison.
                let mut keyed: Vec<(u64, NodeIdx)> =
                    kids.iter().map(|&k| (self.size_of(k), k)).collect();
                keyed.sort_unstable_by(|&(size_a, a), &(size_b, b)| {
                    size_b
                        .cmp(&size_a)
                        .then_with(|| name(a).cmp(name(b)))
                        .then_with(|| a.cmp(&b))
                });
                kids.clear();
                kids.extend(keyed.into_iter().map(|(_, k)| k));
            }
            SortKey::Name => {
                kids.sort_unstable_by(|&a, &b| name(a).cmp(name(b)).then_with(|| a.cmp(&b)))
            }
        }
        kids
    }

    /// Jump to the next entry flagged with a read error (`!`), searching forward from the
    /// current selection in display order and wrapping around. The view moves to the entry's
    /// parent directory with it selected.
    fn jump_to_next_error(&mut self) {
        let mut order = Vec::new();
        let mut stack = vec![self.tree.root];
        while let Some(idx) = stack.pop() {
            order.push(idx);
            // Pushed reversed so the pop order is display order.
            stack.extend(
                self.order_children(self.tree.nodes[idx].children.clone())
                    .iter()
                    .rev(),
            );
        }
        let anchor = self.selected.unwrap_or(self.cur);
        let pos = order.iter().position(|&i| i == anchor).unwrap_or(0);
        let mut locked = false;
        for step in 1..=order.len() {
            let idx = order[(pos + step) % order.len()];
            if !self.tree.nodes[idx].read_error {
                continue;
            }
            // Errors inside a subtree that is being deleted are about to disappear, and the
            // subtree is locked: pass over them.
            if self.deletion_covers(idx) {
                locked = true;
                continue;
            }
            if self.jump_to(idx) {
                let path = self.tree.path_of(idx);
                self.status = Some(match self.tree.error_of(idx) {
                    Some(why) => format!("read error: {path}: {why}"),
                    None => format!("read error: {path}"),
                });
            }
            return;
        }
        self.status = Some(if locked {
            "no read errors outside the directory being deleted".into()
        } else {
            "no read errors".into()
        });
    }

    /// Request a shell in the viewed directory. The event loop owns the terminal, so it
    /// consumes the request and handles the suspend/resume; nothing happens here but
    /// recording it.
    fn request_shell(&mut self) {
        if self.refused_for_import("the shell") {
            return;
        }
        match self.tree.fs_path_of(self.cur) {
            Some(path) => self.shell_request = Some(path),
            None => self.status = Some(GONE.into()),
        }
    }

    /// True while a shell request waits for the event loop.
    pub fn shell_requested(&self) -> bool {
        self.shell_request.is_some()
    }

    /// Consume a pending shell request, if any: the directory to start the shell in.
    pub fn take_shell_request(&mut self) -> Option<PathBuf> {
        self.shell_request.take()
    }

    /// Make sure a row of the viewed directory is selected, if it has any: the first row
    /// when nothing is selected yet or the selected entry has left the directory. The event
    /// loop calls this before every frame, so what Enter and `d` would act on is always the
    /// row the user sees highlighted, from the first batch on.
    pub fn settle_selection(&mut self) {
        let in_view = self
            .selected
            .is_some_and(|sel| self.tree.nodes[sel].parent == Some(self.cur));
        if !in_view {
            self.selected = self.sorted_children().first().copied();
        }
    }

    /// Open the top-files popup for the directory currently being viewed.
    fn open_top_files(&mut self) {
        let items = self.top_files(self.cur, TOP_FILES_LIMIT);
        if items.is_empty() {
            self.status = Some("no files under this directory".into());
            return;
        }
        self.modal = Modal::TopFiles(Box::new(TopFiles {
            items,
            selected: 0,
            offset: Cell::new(0),
        }));
    }

    /// The largest regular files under `from`, largest first, with paths relative to it.
    /// Computed on demand (a subtree walk) so it costs nothing during the scan; entries are a
    /// snapshot of the current sizes, which is exactly what the popup shows.
    pub fn top_files(&self, from: NodeIdx, limit: usize) -> Vec<TopEntry> {
        let mut out: Vec<TopEntry> = Vec::new();
        let mut stack = vec![(from, String::new())];
        while let Some((dir, prefix)) = stack.pop() {
            for &c in &self.tree.nodes[dir].children {
                let n = &self.tree.nodes[c];
                let path = if prefix.is_empty() {
                    n.name.to_string()
                } else {
                    format!("{prefix}/{}", n.name)
                };
                match n.kind {
                    NodeKind::Dir => stack.push((c, path)),
                    NodeKind::File => out.push(TopEntry {
                        idx: c,
                        path,
                        size: self.size_of(c),
                    }),
                    // Symlinks and special files are not "biggest file" material.
                    _ => {}
                }
            }
        }
        out.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.path.cmp(&b.path)));
        out.truncate(limit);
        out
    }

    /// Open the info popup for the selected entry.
    fn open_info(&mut self) {
        if let Some(sel) = self.selected {
            self.modal = Modal::Info(sel);
        }
    }

    /// Rows for the info popup: tree data plus a lazily-statted on-disk section. The stat is
    /// taken at open time and nothing is stored in the node, keeping the tree lean; if the
    /// path is gone (deleted mid-session, or a dump whose paths no longer exist) only the
    /// tree data is shown.
    pub fn info_pairs(&self, idx: NodeIdx) -> Vec<(String, String)> {
        use crate::ui::fmt_size;
        let n = &self.tree.nodes[idx];
        let kind_name = match n.kind {
            NodeKind::Dir => "directory",
            NodeKind::File => "file",
            NodeKind::Link => "symlink",
            NodeKind::Other => "special",
        };
        let mut rows = vec![
            ("path".to_string(), self.tree.path_of(idx)),
            ("type".to_string(), kind_name.to_string()),
            (
                "total size".to_string(),
                fmt_size(self.size_of(idx)).trim_start().to_string(),
            ),
            (
                "own size".to_string(),
                fmt_size(self.own_size_of(idx)).trim_start().to_string(),
            ),
        ];
        if n.is_dir() {
            rows.push(("entries".to_string(), n.children.len().to_string()));
            // An excluded directory was never read, so it has no meaningful count.
            if n.excluded == Excluded::No {
                rows.push(("total items".to_string(), n.items.to_string()));
            }
        }
        if n.hlink {
            rows.push(("hard links".to_string(), "yes".to_string()));
        }
        if n.shared {
            rows.push((
                "duplicate".to_string(),
                "hard link already counted elsewhere".to_string(),
            ));
        }
        match n.excluded {
            Excluded::Pattern => {
                rows.push((
                    "excluded".to_string(),
                    "matched --exclude pattern".to_string(),
                ));
            }
            Excluded::OtherFs => {
                rows.push((
                    "excluded".to_string(),
                    "different filesystem (-x)".to_string(),
                ));
            }
            Excluded::Age => {
                rows.push((
                    "excluded".to_string(),
                    "outside --older-than/--newer-than window".to_string(),
                ));
            }
            Excluded::No => {}
        }
        if n.read_error {
            // The reason, when the scan recorded one; a dump only carries the flag.
            let why = self.tree.error_of(idx).unwrap_or("yes");
            rows.push(("read error".to_string(), why.to_string()));
        }
        if self.imported {
            // The path may belong to another machine; a local file of the same name would
            // show up here as if it were the entry.
            rows.push((
                "on disk".to_string(),
                "not checked (imported dump)".to_string(),
            ));
            return rows;
        }
        match self.tree.fs_path_of(idx).map(std::fs::symlink_metadata) {
            Some(Ok(m)) => {
                rows.push((
                    "permissions".to_string(),
                    format!("{:o}", m.mode() & 0o7777),
                ));
                rows.push(("owner".to_string(), format!("{}:{}", m.uid(), m.gid())));
                rows.push(("modified".to_string(), fmt_time(m.mtime())));
                rows.push(("inode".to_string(), m.ino().to_string()));
                rows.push(("links".to_string(), m.nlink().to_string()));
            }
            _ => rows.push((
                "on disk".to_string(),
                "unavailable (deleted since scanning, or dump paths are gone)".to_string(),
            )),
        }
        rows
    }

    /// The node currently being deleted (its subtree is locked), if any.
    pub fn deleting_idx(&self) -> Option<NodeIdx> {
        self.pending_delete.as_ref().map(|p| p.idx)
    }

    /// How the event loop should decode the next key.
    pub fn input_mode(&self) -> InputMode {
        if matches!(self.modal, Modal::Find(_)) {
            InputMode::Find
        } else {
            InputMode::Normal
        }
    }

    /// Advance the search, if its popup is open: debounce the query, look at up to `budget`
    /// nodes, and re-sort the hits now and then. Called once per frame, after the scan batches
    /// are applied, so live results show up in the same frame.
    pub fn find_tick(&mut self, now: Instant, budget: usize) {
        if let Modal::Find(f) = &mut self.modal {
            f.set_dir(self.cur);
            f.tick(&self.tree, self.disk_usage, self.scanning, now, budget);
        }
    }

    /// True while the search pass has work left; the event loop then polls input without
    /// waiting, so the pass finishes as fast as the slicing allows.
    pub fn find_busy(&self) -> bool {
        matches!(&self.modal, Modal::Find(f) if f.busy(&self.tree))
    }

    /// When the pending (debounced) search query is due, if one is waiting.
    pub fn find_due(&self) -> Option<Instant> {
        match &self.modal {
            Modal::Find(f) => f.due,
            _ => None,
        }
    }

    /// Drop search hits that a delete or rescan just detached from the tree.
    fn prune_find(&mut self) {
        if let Modal::Find(f) = &mut self.modal {
            f.prune(&self.tree);
        }
    }

    /// Move the view to `idx`: its parent directory, with `idx` selected (the root is shown
    /// itself, with nothing selected). Every jump goes through here: search hit, top file,
    /// next error. Refused with a status message when `idx` is no longer part of the tree or
    /// lies in a subtree that is being deleted, which is locked. Returns whether the view moved.
    fn jump_to(&mut self, idx: NodeIdx) -> bool {
        if !self.tree.is_attached(idx) {
            self.status = Some(GONE.into());
            return false;
        }
        if self.deletion_covers(idx) {
            self.status = Some("can't go there: it is being deleted".into());
            return false;
        }
        match self.tree.nodes[idx].parent {
            Some(parent) => {
                self.cur = parent;
                self.selected = Some(idx);
            }
            None => {
                self.cur = idx;
                self.selected = None;
            }
        }
        // The target may be newer than the cached rows; it has to be among them now.
        *self.row_order.borrow_mut() = None;
        self.refocus();
        true
    }

    /// Text pasted into the terminal. It belongs to the search query when the popup is open,
    /// without line breaks and other control characters: a newline in the clipboard is not
    /// Enter. Anywhere else a paste is dropped; the letters in it are not commands.
    pub fn on_paste(&mut self, text: &str) {
        if let Modal::Find(f) = &mut self.modal {
            f.query.extend(text.chars().filter(|c| !c.is_control()));
            f.edited = true;
        }
    }

    pub fn on_key(&mut self, action: KeyAction) {
        // Note: a background deletion does NOT block input — the user can keep browsing. Only
        // the subtree being deleted is locked (see `descend` and `request_delete`).

        // Modal overlays capture input first. The modal is taken out (popups carry state), so
        // every arm that keeps it open must put it back.
        match std::mem::replace(&mut self.modal, Modal::None) {
            Modal::Help => return,
            Modal::ConfirmDelete(idx) => {
                match action {
                    KeyAction::Confirm => self.perform_delete(idx),
                    KeyAction::Quit | KeyAction::Leave | KeyAction::Cancel => {
                        self.status = Some("delete cancelled".into());
                    }
                    // Any other key: keep waiting in the dialog.
                    _ => self.modal = Modal::ConfirmDelete(idx),
                }
                return;
            }
            Modal::ConfirmExport => {
                match action {
                    KeyAction::Confirm => self.export_now(),
                    KeyAction::Quit | KeyAction::Leave | KeyAction::Cancel => {
                        self.status = Some("export cancelled".into());
                    }
                    _ => self.modal = Modal::ConfirmExport,
                }
                return;
            }
            Modal::TopFiles(mut tf) => {
                let last = tf.items.len().saturating_sub(1);
                // Whether the popup stays open after this key. It was taken out of `modal`
                // above, so staying open means putting it back.
                let keep_open = match action {
                    KeyAction::Down => {
                        tf.selected = (tf.selected + 1).min(last);
                        true
                    }
                    KeyAction::Up => {
                        tf.selected = tf.selected.saturating_sub(1);
                        true
                    }
                    KeyAction::PageDown(page) => {
                        tf.selected = tf.selected.saturating_add(page).min(last);
                        true
                    }
                    KeyAction::PageUp(page) => {
                        tf.selected = tf.selected.saturating_sub(page);
                        true
                    }
                    KeyAction::Top => {
                        tf.selected = 0;
                        true
                    }
                    KeyAction::Bottom => {
                        tf.selected = last;
                        true
                    }
                    KeyAction::Enter => {
                        // The tree may have changed since the popup opened (deletions and
                        // rescans tombstone nodes, a running deletion locks its subtree):
                        // `jump_to` refuses those and says why.
                        self.jump_to(tf.items[tf.selected].idx);
                        false
                    }
                    // q / Esc / h / ? close the popup without changing the view.
                    KeyAction::Quit | KeyAction::Leave | KeyAction::Cancel | KeyAction::Help => {
                        false
                    }
                    // Anything else: keep the popup open exactly as it was.
                    _ => true,
                };
                if keep_open {
                    self.modal = Modal::TopFiles(tf);
                }
                return;
            }
            // Any key closes the info popup (it's read-only), including q — which must not
            // also quit, so it returns before the quit handling below.
            Modal::Info(_) => return,
            Modal::Find(mut f) => {
                // Whether the popup stays open after this key.
                let keep_open = match action {
                    KeyAction::FindChar(c) => {
                        f.query.push(c);
                        f.edited = true;
                        true
                    }
                    KeyAction::FindBackspace => {
                        f.query.pop();
                        f.edited = true;
                        true
                    }
                    KeyAction::FindClear => {
                        f.query.clear();
                        f.edited = true;
                        true
                    }
                    KeyAction::FindDeleteWord => {
                        // The spaces after the last word go with it, as in a shell.
                        let keep = f.query.trim_end().len();
                        f.query.truncate(keep);
                        let word = f.query.rfind(char::is_whitespace).map_or(0, |at| {
                            at + f.query[at..].chars().next().map_or(1, char::len_utf8)
                        });
                        f.query.truncate(word);
                        f.edited = true;
                        true
                    }
                    KeyAction::FindScope => {
                        f.toggle_scope();
                        true
                    }
                    KeyAction::Down => {
                        f.move_selection(1);
                        true
                    }
                    KeyAction::Up => {
                        f.move_selection(-1);
                        true
                    }
                    KeyAction::PageDown(page) => {
                        f.move_selection(page as isize);
                        true
                    }
                    KeyAction::PageUp(page) => {
                        f.move_selection(-(page as isize));
                        true
                    }
                    KeyAction::Top => {
                        f.selected = f.hits.first().copied();
                        true
                    }
                    KeyAction::Bottom => {
                        f.selected = f.hits.last().copied();
                        true
                    }
                    KeyAction::Enter => match f.selected {
                        Some(idx) => {
                            self.jump_to(idx);
                            false
                        }
                        // Nothing to jump to yet.
                        None => true,
                    },
                    // Esc closes the popup and leaves the view as it was. Ctrl-C closes it
                    // too and goes on to the regular quit handling below.
                    KeyAction::Cancel | KeyAction::Quit => false,
                    _ => true,
                };
                if keep_open {
                    self.modal = Modal::Find(f);
                }
                if !matches!(action, KeyAction::Quit) {
                    return;
                }
            }
            Modal::None => {}
        }

        // Any key other than a repeated quit dismisses a stale status and disarms a pending
        // force-quit (so the two-press abort only triggers on consecutive quits).
        if !matches!(action, KeyAction::Quit) {
            self.status = None;
            self.quit_armed = false;
        }

        self.settle_selection();
        let kids = self.sorted_children();
        match action {
            KeyAction::Quit => {
                if self.is_deleting() {
                    // Don't silently abort a destructive operation. Arm on the first press,
                    // force-quit on the second (the removal is interrupted wherever it is and
                    // leaves the rest of the subtree on disk).
                    if self.quit_armed {
                        self.quit = true;
                    } else {
                        self.quit_armed = true;
                        self.status = Some(
                            "deletion in progress — press q / Ctrl-C again to abort it and quit (may leave partial results)"
                                .into(),
                        );
                    }
                } else {
                    self.quit = true;
                }
            }
            KeyAction::Down => self.move_selection(&kids, 1),
            KeyAction::Up => self.move_selection(&kids, -1),
            KeyAction::PageDown(page) => self.move_selection(&kids, page as isize),
            KeyAction::PageUp(page) => self.move_selection(&kids, -(page as isize)),
            KeyAction::Top => self.selected = kids.first().copied(),
            KeyAction::Bottom => self.selected = kids.last().copied(),
            KeyAction::Enter => self.descend(),
            KeyAction::Leave => self.ascend(),
            KeyAction::ToggleSort => {
                self.sort = match self.sort {
                    SortKey::Size => SortKey::Name,
                    SortKey::Name => SortKey::Size,
                };
            }
            KeyAction::ToggleUsage => self.disk_usage = !self.disk_usage,
            KeyAction::ToggleItemCount => self.show_item_count = !self.show_item_count,
            KeyAction::Help => self.modal = Modal::Help,
            KeyAction::Delete => self.request_delete(),
            KeyAction::Open => self.open_selected(),
            KeyAction::TopFiles => self.open_top_files(),
            KeyAction::Refresh => self.request_refresh(),
            KeyAction::Info => self.open_info(),
            KeyAction::Export => self.request_export(),
            KeyAction::NextError => self.jump_to_next_error(),
            KeyAction::Shell => self.request_shell(),
            KeyAction::Find => self.modal = Modal::Find(Box::new(Find::new(self.cur))),
            KeyAction::Moved(to) => self.status = Some(to.into()),
            // Esc and `n` answer dialogs; in the browser they do nothing (as in ncdu).
            KeyAction::Confirm | KeyAction::Cancel => {}
            // Only produced while the search popup is open, which handles them above.
            KeyAction::FindChar(_)
            | KeyAction::FindBackspace
            | KeyAction::FindClear
            | KeyAction::FindDeleteWord
            | KeyAction::FindScope => {}
            // Handled by the event loop, which owns the terminal.
            KeyAction::Redraw | KeyAction::Suspend => {}
        }
    }

    /// Move the selection by `delta` rows, clamped to the list. `delta` is the page size for
    /// the page keys and ±1 for plain up/down.
    fn move_selection(&mut self, kids: &[NodeIdx], delta: isize) {
        if kids.is_empty() {
            return;
        }
        let pos = self
            .selected
            .and_then(|s| kids.iter().position(|&k| k == s))
            .unwrap_or(0) as isize;
        let new = (pos + delta).clamp(0, kids.len() as isize - 1) as usize;
        self.selected = Some(kids[new]);
    }

    fn descend(&mut self) {
        if let Some(sel) = self.selected {
            // The subtree being deleted is locked: refuse to enter it.
            if self.deleting_idx() == Some(sel) {
                self.status = Some("can't enter: this directory is being deleted".into());
                return;
            }
            if self.tree.nodes[sel].is_dir() {
                self.cur = sel;
                self.list_offset.set(0);
                self.selected = self.sorted_children().first().copied();
                // Prioritize scanning the directory we just entered.
                self.refocus();
            }
        }
    }

    fn ascend(&mut self) {
        if let Some(parent) = self.tree.nodes[self.cur].parent {
            let was = self.cur;
            self.cur = parent;
            // Re-select the directory we just came out of.
            self.selected = Some(was);
            self.refocus();
        }
    }

    /// Hand the selected entry to the platform's default handler: files open in their default
    /// app, directories in the file manager. Non-destructive, so unlike delete it also works
    /// in read-only mode and while a scan is still filling in the tree.
    fn open_selected(&mut self) {
        if self.refused_for_import("open") {
            return;
        }
        let Some(sel) = self.selected else { return };
        if self.deleting_idx() == Some(sel) {
            self.status = Some("can't open: this entry is being deleted".into());
            return;
        }
        let Some(path) = self.tree.fs_path_of(sel) else {
            self.status = Some(GONE.into());
            return;
        };
        let (tx, rx) = unbounded();
        // Waiting happens off the UI thread: openers normally return at once, but a wedged
        // handler must not take the TUI down with it. Waiting here still reaps the child.
        std::thread::spawn(move || {
            let _ = tx.send(open_path(&path));
        });
        let shown = self.tree.path_of(sel);
        self.status = Some(format!("opening {shown}"));
        self.pending_open.push(PendingOpen { path: shown, rx });
    }

    /// Check whether a background open has finished and surface its result. Called once per
    /// frame by the event loop.
    pub fn poll_open(&mut self) {
        if self.pending_open.is_empty() {
            return;
        }
        let mut finished = Vec::new();
        self.pending_open.retain_mut(|p| match p.rx.try_recv() {
            Ok(res) => {
                finished.push((p.path.clone(), res));
                false
            }
            Err(crossbeam_channel::TryRecvError::Empty) => true, // still working
            Err(crossbeam_channel::TryRecvError::Disconnected) => {
                finished.push((
                    p.path.clone(),
                    Err(std::io::Error::other("open worker exited unexpectedly")),
                ));
                false
            }
        });
        for (path, res) in finished {
            self.status = Some(match res {
                Ok(()) => format!("opened {path}"),
                Err(e) => format!("open failed: {e}"),
            });
        }
    }

    fn request_delete(&mut self) {
        if self.refused_for_import("delete") {
            return;
        }
        if self.read_only {
            self.status = Some("read-only mode (-r): deletion disabled".into());
            return;
        }
        // One deletion at a time keeps the locking simple; browsing stays available meanwhile.
        if self.pending_delete.is_some() {
            self.status = Some("a deletion is already in progress — please wait".into());
            return;
        }
        let Some(sel) = self.selected else { return };
        if sel == self.cur {
            return;
        }
        let n = &self.tree.nodes[sel];
        if n.is_dir() {
            // The core safety rule: never delete a subtree that is still being scanned, or
            // its contents (and on-disk reality) aren't fully known yet.
            if !self.tree.subtree_complete(sel) {
                self.status = Some(
                    "cannot delete: directory is still being scanned — wait for it to finish"
                        .into(),
                );
                return;
            }
            // The same holds for a directory the scan never listed: nobody has seen what is
            // in it. (Inside a larger deletion such a directory is simply left in place.)
            if n.excluded != Excluded::No {
                self.status = Some(
                    "cannot delete: this directory was excluded from the scan, its contents are unknown"
                        .into(),
                );
                return;
            }
            if n.read_error {
                self.status = Some(
                    "cannot delete: this directory could not be read, its contents are unknown"
                        .into(),
                );
                return;
            }
        }
        self.confirm_hard_links = self.tree.hard_links_in(sel);
        self.modal = Modal::ConfirmDelete(sel);
    }

    fn perform_delete(&mut self, idx: NodeIdx) {
        // Double-check the gate at the moment of action (scan may have advanced).
        if self.tree.nodes[idx].is_dir() && !self.tree.subtree_complete(idx) {
            self.status = Some("cannot delete: subtree not fully scanned".into());
            return;
        }
        // Only what the scan listed is removed: the worker gets a copy of the subtree and
        // checks every entry against it (see `delete.rs`).
        let (Some(path), Some(plan)) = (self.tree.fs_path_of(idx), Plan::of(&self.tree, idx))
        else {
            self.status = Some(GONE.into());
            return;
        };

        // Run the (potentially slow) filesystem removal off the UI thread. The loop polls
        // `poll_delete`, and the UI shows a spinner via `deleting_path` until it completes.
        let (tx, rx) = unbounded();
        std::thread::spawn(move || {
            let _ = tx.send(plan.execute());
        });
        self.status = None;
        self.pending_delete = Some(PendingDelete { idx, path, rx });
    }

    /// Check whether a background deletion has finished and, if so, fold the result into the
    /// tree. Called once per frame by the event loop.
    pub fn poll_delete(&mut self) {
        let Some(pd) = self.pending_delete.as_ref() else {
            return;
        };
        let report = match pd.rx.try_recv() {
            Ok(report) => report,
            Err(crossbeam_channel::TryRecvError::Empty) => return, // still working
            // The worker died without reporting: what it removed is unknown.
            Err(crossbeam_channel::TryRecvError::Disconnected) => Report::lost(),
        };
        let pd = self.pending_delete.take().unwrap();
        // The deletion is over, so a pending force-quit is no longer relevant.
        self.quit_armed = false;

        if report.complete {
            // If the removed node is in the view the user is looking at, land the highlight
            // on a neighbour; if they browsed elsewhere, leave their selection untouched.
            let kids = self.sorted_children();
            let pos = kids.iter().position(|&k| k == pd.idx);
            let parent = self.tree.nodes[pd.idx].parent;
            // A node that is no longer part of the tree has already been taken out of
            // every total; removing it again would subtract it a second time.
            if self.tree.is_attached(pd.idx) {
                self.tree.delete_subtree(pd.idx);
            }
            self.prune_find();
            if !self.rehome_view(parent) {
                if let Some(p) = pos {
                    let kids = self.sorted_children();
                    self.selected = (!kids.is_empty()).then(|| kids[p.min(kids.len() - 1)]);
                }
            }
            self.status = Some(format!("deleted {}", pd.path.display()));
            return;
        }

        // Something was left behind: entries that appeared after the scan, that changed, that
        // the scan never read, or that the OS refused to remove. The tree below the target no
        // longer matches the disk. If the target is still the directory that was scanned,
        // read it again; the status says what stood in the way.
        let rereading = report.target_intact && self.resync(pd.idx);
        self.rehome_view(Some(pd.idx));
        // The footer is one line: name the target by its name and what stood in the way by
        // its path below the target.
        let first = match report.failures.first() {
            None => "unknown reason".to_string(),
            Some((path, reason)) => match path.strip_prefix(&pd.path) {
                Ok(below) if !below.as_os_str().is_empty() => {
                    format!("{}: {reason}", below.display())
                }
                _ => reason.clone(),
            },
        };
        let more = match report.failed.saturating_sub(1) {
            0 => String::new(),
            n => format!(" (and {n} more)"),
        };
        let tail = if rereading {
            " — reading it again"
        } else {
            ""
        };
        self.status = Some(format!(
            "{} could not be removed completely: {first}{more}{tail}",
            self.tree.nodes[pd.idx].name
        ));
    }

    /// If the directory being viewed has left the tree, move the view to `fallback` (or to the
    /// root if that has gone as well) and select its first entry. Staying would show entries
    /// that no longer exist, with no way back up. Returns whether the view moved.
    fn rehome_view(&mut self, fallback: Option<NodeIdx>) -> bool {
        if self.tree.is_attached(self.cur) {
            return false;
        }
        self.cur = fallback
            .filter(|&p| self.tree.is_attached(p))
            .unwrap_or(self.tree.root);
        self.selected = self.sorted_children().first().copied();
        self.refocus();
        true
    }
}

/// Launch the platform opener for `path` and wait for it to exit — waiting reaps the child
/// instead of leaving a zombie. Stdio is disconnected so the opener can't scribble on the TUI.
fn open_path(path: &Path) -> std::io::Result<()> {
    let status = opener_command(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "opener exited with {status}"
        )))
    }
}

#[cfg(target_os = "macos")]
fn opener_command(path: &Path) -> Command {
    let mut cmd = Command::new("open");
    cmd.arg(path);
    cmd
}

#[cfg(target_os = "windows")]
fn opener_command(path: &Path) -> Command {
    // `start` treats its first quoted argument as the window title, so pass an empty one.
    let mut cmd = Command::new("cmd");
    cmd.args(["/C", "start", ""]).arg(path);
    cmd
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn opener_command(path: &Path) -> Command {
    let mut cmd = Command::new("xdg-open");
    cmd.arg(path);
    cmd
}

/// A short type indicator for an entry (mirrors ncdu's column).
pub fn indicator(kind: NodeKind, shared: bool, excluded: bool, read_error: bool) -> char {
    if read_error {
        '!'
    } else if excluded {
        '<'
    } else if shared {
        'H'
    } else {
        match kind {
            NodeKind::Dir => '/',
            NodeKind::Link => '@',
            NodeKind::Other => '*',
            NodeKind::File => ' ',
        }
    }
}

/// Logical input actions, decoded from raw keys in `main`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KeyAction {
    Quit,
    Up,
    Down,
    Top,
    Bottom,
    Enter,
    Leave,
    ToggleSort,
    ToggleUsage,
    /// Show or hide the item count column.
    ToggleItemCount,
    Help,
    Delete,
    Open,
    Confirm,
    Cancel,
    TopFiles,
    Refresh,
    Info,
    /// Jump by this many rows (the visible page height).
    PageDown(usize),
    PageUp(usize),
    Export,
    NextError,
    Shell,
    /// Open the search popup.
    Find,
    /// A character typed into the search popup's query.
    FindChar(char),
    FindBackspace,
    /// Empty the search query (Ctrl-U).
    FindClear,
    /// Remove the last word of the search query (Ctrl-W).
    FindDeleteWord,
    /// Switch the search between the whole tree and the viewed directory.
    FindScope,
    /// Repaint the whole screen (Ctrl-L). The event loop does it; the app has nothing to do.
    Redraw,
    /// Stop the process and give the terminal back to the shell (Ctrl-Z). Also the event
    /// loop's job.
    Suspend,
    /// A key whose function moved to another key: the footer says where.
    Moved(&'static str),
}

/// Create the first free `icdu-dump*.json` in `dir`, so consecutive exports never clobber each
/// other (or an unrelated file that happens to share the name). `create_new` (`O_EXCL`) makes
/// the check and the create one atomic step that fails on anything already at the name,
/// including a planted symlink, so the write can never be redirected elsewhere.
fn create_export_file(dir: &Path) -> std::io::Result<(PathBuf, std::fs::File)> {
    for n in 1u32.. {
        let path = match n {
            1 => dir.join("icdu-dump.json"),
            n => dir.join(format!("icdu-dump-{n}.json")),
        };
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => return Ok((path, file)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::other("no free icdu-dump*.json name"))
}

/// Split Unix seconds into UTC civil date/time (Howard Hinnant's civil-from-days).
fn civil(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (h, mi, s) = (
        (tod / 3600) as u32,
        ((tod % 3600) / 60) as u32,
        (tod % 60) as u32,
    );
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m as u32, d as u32, h, mi, s)
}

/// Human-readable UTC timestamp for the info panel (mtime), dependency-free.
pub fn fmt_time(secs: i64) -> String {
    let (y, mo, d, h, mi, s) = civil(secs);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02} UTC")
}

/// Case-insensitive substring test used by the search. ASCII letters compare
/// case-insensitively; other bytes must match exactly (a plain byte-substring test, so
/// multi-byte UTF-8 names can't produce false positives). An empty needle matches everything.
fn contains_ci(haystack: &str, needle: &str) -> bool {
    find_ci(haystack, needle).is_some()
}

/// Byte offset of the first case-insensitive match of `needle` in `haystack` (same rules as
/// [`contains_ci`]). The match always starts and ends on a char boundary: it begins and ends
/// with the same bytes as the (valid UTF-8) needle.
pub fn find_ci(haystack: &str, needle: &str) -> Option<usize> {
    let (h, n) = (haystack.as_bytes(), needle.as_bytes());
    if n.is_empty() {
        return Some(0);
    }
    h.windows(n.len()).position(|w| w.eq_ignore_ascii_case(n))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Excluded, NodeKind};
    use crossbeam_channel::Sender;

    fn app() -> App {
        App::new(Tree::new("/r".into(), 0, 0, 1, 1), true, false, false)
    }

    fn app_with_entries() -> App {
        let mut t = Tree::new_imported("/r".into(), 0, 0, 1, 1);
        for (name, kind, size) in [
            ("alpha", NodeKind::File, 10u64),
            ("Beta", NodeKind::File, 30),
            ("banner", NodeKind::File, 20),
            ("gamma", NodeKind::Dir, 5),
        ] {
            t.import_child(
                t.root,
                name.into(),
                kind,
                size,
                size,
                1,
                1,
                false,
                false,
                Excluded::No,
                false,
            );
        }
        App::new(t, true, false, false)
    }

    #[test]
    fn contains_ci_matches_ascii_case_insensitively() {
        assert!(contains_ci("Hello, World", "WORLD"));
        assert!(contains_ci("node_modules", "ODE_MOD"));
        assert!(contains_ci("anything", ""));
        assert!(!contains_ci("short", "shorter"));
        assert!(
            !contains_ci("résumé", "RESUME"),
            "non-ASCII compares exactly"
        );
        assert!(contains_ci("résumé", "sum"));
    }

    fn app_with_tree_for_top_files() -> App {
        let mut t = Tree::new_imported("/r".into(), 0, 0, 1, 1);
        t.import_child(
            t.root,
            "big.bin".into(),
            NodeKind::File,
            900,
            900,
            1,
            1,
            false,
            false,
            Excluded::No,
            false,
        );
        let sub = t.import_child(
            t.root,
            "sub".into(),
            NodeKind::Dir,
            500,
            500,
            1,
            2,
            false,
            false,
            Excluded::No,
            false,
        );
        t.import_child(
            sub,
            "mid.bin".into(),
            NodeKind::File,
            500,
            500,
            1,
            3,
            false,
            false,
            Excluded::No,
            false,
        );
        t.import_child(
            t.root,
            "ln".into(),
            NodeKind::Link,
            100,
            100,
            1,
            4,
            false,
            false,
            Excluded::No,
            false,
        );
        App::new(t, true, false, false)
    }

    #[test]
    fn top_files_lists_files_largest_first_with_relative_paths() {
        let app = app_with_tree_for_top_files();
        let top = app.top_files(app.tree.root, 10);
        assert_eq!(
            top.iter().map(|e| e.path.as_str()).collect::<Vec<_>>(),
            vec!["big.bin", "sub/mid.bin"],
            "files only (no dirs, no symlinks), largest first, relative paths"
        );
        assert_eq!(top[0].size, 900);
        assert_eq!(app.top_files(app.tree.root, 1).len(), 1, "limit applies");
    }

    fn row_names(app: &App) -> Vec<String> {
        app.sorted_children()
            .iter()
            .map(|&k| app.tree.nodes[k].name.to_string())
            .collect()
    }

    /// A batch for the root with one file, as a running scan would deliver it.
    fn file_batch(name: &str, size: u64) -> Batch {
        Batch {
            parent_id: 0,
            nodes: vec![crate::scan::NewNode {
                name: name.into(),
                raw_name: None,
                kind: NodeKind::File,
                apparent: size,
                disk: size,
                dev: 1,
                ino: size,
                hlink: false,
                excluded: Excluded::No,
                error: None,
                dir_id: None,
            }],
            error: None,
        }
    }

    /// The display order is kept between frames and only computed again when something it
    /// depends on has changed. Without a frame clock (as in every other test) it is exact.
    #[test]
    fn row_order_is_cached_until_something_changes() {
        let mut app = app_with_entries();
        let first = app.sorted_children();
        assert_eq!(row_names(&app), vec!["Beta", "banner", "alpha", "gamma"]);
        assert!(
            Rc::ptr_eq(&first, &app.sorted_children()),
            "nothing changed: the same list is handed out"
        );

        // A sort change.
        app.on_key(KeyAction::ToggleSort);
        assert_eq!(row_names(&app), vec!["Beta", "alpha", "banner", "gamma"]);
        app.on_key(KeyAction::ToggleSort);

        // A deletion.
        let banner = idx_named(&app, "banner");
        app.tree.delete_subtree(banner);
        assert_eq!(row_names(&app), vec!["Beta", "alpha", "gamma"]);

        // The size metric.
        let alpha = idx_named(&app, "alpha");
        app.tree.nodes[alpha].apparent = 1_000;
        app.on_key(KeyAction::ToggleUsage);
        assert_eq!(row_names(&app), vec!["alpha", "Beta", "gamma"]);
        app.on_key(KeyAction::ToggleUsage);

        // Navigation: another directory has another list.
        app.selected = Some(idx_named(&app, "gamma"));
        app.on_key(KeyAction::Enter);
        assert!(row_names(&app).is_empty());
        app.on_key(KeyAction::Leave);
        assert_eq!(row_names(&app), vec!["Beta", "alpha", "gamma"]);
    }

    /// New entries from a scan show up at once when no frame clock throttles the sorting.
    #[test]
    fn row_order_follows_new_batches() {
        let mut app = App::new(Tree::new("/r".into(), 0, 0, 1, 1), true, false, true);
        assert!(row_names(&app).is_empty());
        app.tree.apply(file_batch("first", 10));
        assert_eq!(row_names(&app), vec!["first"]);
    }

    /// A stale order is kept for ten times as long as the last sort took, and no longer.
    #[test]
    fn resort_is_due_after_ten_sort_durations() {
        let ms = Duration::from_millis;
        assert!(!resort_due(ms(399), ms(40)));
        assert!(resort_due(ms(400), ms(40)));
        // A sort that takes no time is due on every frame.
        assert!(resort_due(Duration::ZERO, Duration::ZERO));
        assert!(resort_due(ms(80), Duration::from_micros(50)));
    }

    /// Pretend the last sort of the viewed directory took `took`, as it would in a directory
    /// with very many entries.
    fn pretend_the_sort_took(app: &App, took: Duration) {
        app.row_order
            .borrow_mut()
            .as_mut()
            .expect("rows were sorted")
            .took = took;
    }

    /// While a scan runs, every batch changes sizes somewhere. Sorting a huge directory on
    /// every frame would stall the UI, so with a frame clock a slow sort is not repeated
    /// until it is due. Deletions are never held back: a removed entry must not linger.
    #[test]
    fn a_slow_sort_is_not_repeated_for_every_frame_of_a_scan() {
        let mut t = Tree::new("/r".into(), 0, 0, 1, 1);
        t.apply(Batch {
            parent_id: 0,
            nodes: vec![
                file_batch("small", 10).nodes.remove(0),
                file_batch("large", 500).nodes.remove(0),
            ],
            error: None,
        });
        let mut app = App::new(t, true, false, true);
        let t0 = Instant::now();
        let ms = Duration::from_millis;

        app.begin_frame(t0);
        assert_eq!(row_names(&app), vec!["large", "small"]);
        pretend_the_sort_took(&app, ms(40));

        // A change shortly after: the rows stay as they are.
        let small = idx_named(&app, "small");
        app.tree.apply(file_batch("newcomer", 100));
        app.tree.nodes[small].disk = 9_999;
        app.begin_frame(t0 + ms(80));
        assert_eq!(row_names(&app), vec!["large", "small"], "too early");
        app.begin_frame(t0 + ms(399));
        assert_eq!(row_names(&app), vec!["large", "small"], "still too early");

        // Once ten sort durations have passed, the order is brought up to date.
        app.begin_frame(t0 + ms(400));
        assert_eq!(row_names(&app), vec!["small", "large", "newcomer"]);
        pretend_the_sort_took(&app, ms(40));

        // A deletion counts at once.
        let large = idx_named(&app, "large");
        app.tree.delete_subtree(large);
        app.begin_frame(t0 + ms(401));
        assert_eq!(row_names(&app), vec!["small", "newcomer"]);
        pretend_the_sort_took(&app, ms(40));

        // So does anything the user asks for: a sort change, a jump.
        app.tree.apply(file_batch("zz-last", 1));
        app.on_key(KeyAction::ToggleSort);
        assert_eq!(row_names(&app), vec!["newcomer", "small", "zz-last"]);
    }

    /// A directory that sorts in no time is in order on every frame of a scan, as it was
    /// before the rows were cached.
    #[test]
    fn a_fast_sort_keeps_the_rows_in_order_on_every_frame() {
        let mut app = App::new(Tree::new("/r".into(), 0, 0, 1, 1), true, false, true);
        app.tree.apply(file_batch("a", 10));
        let t0 = Instant::now();
        app.begin_frame(t0);
        assert_eq!(row_names(&app), vec!["a"]);
        pretend_the_sort_took(&app, Duration::from_micros(20));

        app.tree.apply(file_batch("b", 500));
        app.begin_frame(t0 + Duration::from_millis(80)); // the next frame of a scan
        assert_eq!(row_names(&app), vec!["b", "a"]);
    }

    /// A jump to an entry that arrived after the rows were last sorted still lands on it.
    #[test]
    fn jump_reaches_an_entry_newer_than_the_cached_order() {
        let mut app = App::new(Tree::new("/r".into(), 0, 0, 1, 1), true, false, true);
        app.tree.apply(file_batch("old", 10));
        let t0 = Instant::now();
        app.begin_frame(t0);
        assert_eq!(row_names(&app), vec!["old"]);

        pretend_the_sort_took(&app, Duration::from_secs(1));
        app.tree.apply(file_batch("new", 5));
        app.begin_frame(t0 + Duration::from_millis(10));
        assert_eq!(
            row_names(&app),
            vec!["old"],
            "the cached rows are still in use"
        );
        let new = idx_named(&app, "new");
        assert!(app.jump_to(new));
        assert_eq!(app.selected, Some(new));
        app.on_key(KeyAction::Help); // any key: the selection must survive it
        assert_eq!(app.selected, Some(new));
        assert!(row_names(&app).contains(&"new".to_string()));
    }

    /// The largest size among the rows scales the bars; it comes with the cached order.
    #[test]
    fn largest_row_size_is_known_without_a_scan_of_the_rows() {
        let mut app = app_with_entries();
        assert_eq!(app.max_row_size(), 30);
        app.on_key(KeyAction::ToggleSort); // by name: the largest is no longer first
        assert_eq!(app.max_row_size(), 30);
        app.tree.delete_subtree(idx_named(&app, "Beta"));
        assert_eq!(app.max_row_size(), 20);
        app.cur = idx_named(&app, "gamma");
        assert_eq!(app.max_row_size(), 0, "an empty directory");
    }

    /// A list that fits on the screen never scrolls.
    #[test]
    fn scroll_offset_stays_at_the_top_when_everything_fits() {
        for sel in [None, Some(0), Some(4), Some(9)] {
            assert_eq!(scroll_offset(0, sel, 10, 10), 0);
            assert_eq!(scroll_offset(0, sel, 10, 40), 0);
            assert_eq!(
                scroll_offset(7, sel, 10, 40),
                0,
                "a stale offset is dropped"
            );
        }
        assert_eq!(scroll_offset(3, None, 0, 10), 0, "empty list");
    }

    /// Inside the page the cursor moves and the list stands still. The list only scrolls
    /// when the cursor comes within the margin of an edge, and then by as little as needed.
    #[test]
    fn scroll_offset_moves_the_list_only_near_an_edge() {
        let (len, rows) = (100, 10);
        // Going down from the top: rows 0..=7 are free, row 8 is in the bottom margin.
        for sel in 0..=7 {
            assert_eq!(scroll_offset(0, Some(sel), len, rows), 0, "row {sel}");
        }
        assert_eq!(scroll_offset(0, Some(8), len, rows), 1);
        assert_eq!(scroll_offset(1, Some(9), len, rows), 2);
        // Going up again from there moves the cursor, not the list. This is what a fresh
        // offset on every frame got wrong: it pinned the cursor to the bottom edge.
        for sel in (4..=9).rev() {
            assert_eq!(scroll_offset(2, Some(sel), len, rows), 2, "row {sel}");
        }
        assert_eq!(
            scroll_offset(2, Some(3), len, rows),
            1,
            "top margin reached"
        );
        assert_eq!(scroll_offset(1, Some(2), len, rows), 0);
        // The ends of the list are reachable: no margin is kept beyond them.
        assert_eq!(scroll_offset(0, Some(99), len, rows), 90);
        assert_eq!(scroll_offset(90, Some(98), len, rows), 90);
        assert_eq!(scroll_offset(90, Some(0), len, rows), 0);
        // A jump lands with the margin on the side it came from.
        assert_eq!(scroll_offset(0, Some(50), len, rows), 43);
        assert_eq!(scroll_offset(90, Some(50), len, rows), 48);
    }

    /// The offset stays valid when the list or the window changes under it.
    #[test]
    fn scroll_offset_copes_with_shrinking_lists_and_tiny_windows() {
        assert_eq!(scroll_offset(90, Some(5), 20, 10), 3, "list got shorter");
        assert_eq!(
            scroll_offset(90, None, 20, 10),
            10,
            "nothing selected: clamp only"
        );
        // Windows too small for the margin still show the selection.
        for rows in 1..=4 {
            for sel in 0..30 {
                let offset = scroll_offset(0, Some(sel), 30, rows);
                assert!(
                    offset <= sel && sel < offset + rows,
                    "rows {rows}, row {sel}: offset {offset}"
                );
            }
        }
        assert_eq!(
            scroll_offset(4, Some(9), 30, 0),
            4,
            "no rows: nothing to do"
        );
    }

    /// Whatever the starting point, the selected row ends up on screen, and the offset never
    /// runs past the end of the list.
    #[test]
    fn scroll_offset_always_shows_the_selection() {
        for len in [1usize, 5, 12, 50] {
            for rows in [1usize, 3, 10, 60] {
                for start in [0usize, 3, 49, 500] {
                    for sel in 0..len {
                        let offset = scroll_offset(start, Some(sel), len, rows);
                        assert!(offset <= sel && sel < offset + rows);
                        assert!(offset <= len.saturating_sub(rows));
                    }
                }
            }
        }
    }

    /// The first row is highlighted as soon as there is one, without a key press, and the
    /// first Enter then acts on what the user can see.
    #[test]
    fn selection_settles_before_the_first_key() {
        use crate::scan::NewNode;
        let node = |name: &str, kind, size, dir_id| NewNode {
            name: name.into(),
            raw_name: None,
            kind,
            apparent: size,
            disk: size,
            dev: 1,
            ino: size,
            hlink: false,
            excluded: Excluded::No,
            error: None,
            dir_id,
        };
        let mut app = App::new(Tree::new("/r".into(), 0, 0, 1, 1), true, false, true);
        app.settle_selection();
        assert_eq!(app.selected, None, "nothing to select yet");

        app.tree.apply(Batch {
            parent_id: 0,
            nodes: vec![
                node("small", NodeKind::File, 10, None),
                node("big", NodeKind::Dir, 500, Some(1)),
            ],
            error: None,
        });
        app.settle_selection();
        let big = idx_named(&app, "big");
        assert_eq!(app.selected, Some(big), "the first row in display order");

        // It is a selection like any other: later frames leave it alone...
        app.selected = Some(idx_named(&app, "small"));
        app.settle_selection();
        assert_eq!(app.selected, Some(idx_named(&app, "small")));
        // ...and the first `j` moves on from the row that was highlighted.
        app.selected = Some(big);
        app.on_key(KeyAction::Down);
        assert_eq!(app.selected, Some(idx_named(&app, "small")));
    }

    /// A selection that no longer points into the viewed directory is replaced.
    #[test]
    fn selection_settles_again_when_its_entry_is_gone() {
        let mut app = app_with_entries();
        app.settle_selection();
        let first = app.selected.expect("first row");
        assert_eq!(app.tree.nodes[first].name, "Beta");
        app.tree.delete_subtree(first);
        app.settle_selection();
        assert_eq!(app.tree.nodes[app.selected.unwrap()].name, "banner");

        // In an empty directory there is nothing to select.
        let gamma = idx_named(&app, "gamma");
        app.cur = gamma;
        app.settle_selection();
        assert_eq!(app.selected, None);
    }

    /// The row highlighted in the open top-files popup.
    fn top_selected(app: &App) -> usize {
        match &app.modal {
            Modal::TopFiles(tf) => tf.selected,
            _ => panic!("the top-files popup is not open"),
        }
    }

    /// Every navigation key moves the highlight and leaves the popup open. (They used to
    /// close it: the popup is taken out of `modal` for the key and was not put back.)
    #[test]
    fn top_files_popup_navigation_keeps_it_open() {
        let mut t = Tree::new_imported("/r".into(), 0, 0, 1, 1);
        let dir = t.import_child(
            t.root,
            "many".into(),
            NodeKind::Dir,
            0,
            0,
            1,
            2,
            false,
            false,
            Excluded::No,
            false,
        );
        // f00 is the largest, f29 the smallest.
        for i in 0..30u64 {
            file_under(&mut t, dir, &format!("f{i:02}"), 1000 - i);
        }
        let mut app = App::new(t, true, false, false);
        app.on_key(KeyAction::TopFiles);
        assert_eq!(top_selected(&app), 0);

        for (key, want) in [
            (KeyAction::Down, 1),
            (KeyAction::Down, 2),
            (KeyAction::Up, 1),
            (KeyAction::PageDown(10), 11),
            (KeyAction::PageDown(10), 21),
            (KeyAction::PageDown(10), 29),
            (KeyAction::PageUp(10), 19),
            (KeyAction::Top, 0),
            (KeyAction::Up, 0),
            (KeyAction::PageUp(10), 0),
            (KeyAction::Bottom, 29),
            (KeyAction::Down, 29),
        ] {
            app.on_key(key);
            assert_eq!(top_selected(&app), want, "after {key:?}");
        }
        // A key that means nothing here changes nothing.
        app.on_key(KeyAction::ToggleSort);
        assert_eq!(top_selected(&app), 29);
    }

    /// Enter jumps to the highlighted file. The second row is used, in a tree where a plain
    /// Enter in the browser would have led somewhere else, so a popup that closed early
    /// cannot pass.
    #[test]
    fn top_files_popup_navigation_and_jump() {
        let mut t = Tree::new_imported("/r".into(), 0, 0, 1, 1);
        let add_dir = |t: &mut Tree, name: &str, ino| {
            t.import_child(
                t.root,
                name.into(),
                NodeKind::Dir,
                0,
                0,
                1,
                ino,
                false,
                false,
                Excluded::No,
                false,
            )
        };
        let big = add_dir(&mut t, "big", 2);
        let small = add_dir(&mut t, "small", 3);
        file_under(&mut t, big, "largest.bin", 900);
        file_under(&mut t, big, "filler.bin", 100);
        file_under(&mut t, small, "second.bin", 500);
        let mut app = App::new(t, true, false, false);

        app.on_key(KeyAction::TopFiles);
        let rows: Vec<String> = match &app.modal {
            Modal::TopFiles(tf) => tf.items.iter().map(|e| e.path.clone()).collect(),
            _ => panic!("top-files popup did not open"),
        };
        assert_eq!(
            rows,
            vec!["big/largest.bin", "small/second.bin", "big/filler.bin"]
        );
        app.on_key(KeyAction::Down); // small/second.bin
        assert_eq!(top_selected(&app), 1);
        app.on_key(KeyAction::Enter);
        assert!(matches!(app.modal, Modal::None), "jump closes the popup");
        assert_eq!(app.cur, small, "landed in the file's directory");
        assert_eq!(
            app.tree.nodes[app.selected.unwrap()].name,
            "second.bin",
            "file is selected"
        );
    }

    /// A finished open lands in the status line and drains its slot, so the UI thread never
    /// waited on the opener.
    #[test]
    fn finished_open_reports_result_and_drains() {
        let mut a = app();
        let (tx, rx) = unbounded();
        tx.send(Err(std::io::Error::other("no handler"))).unwrap();
        drop(tx);
        a.pending_open.push(PendingOpen {
            path: "/r/a".into(),
            rx,
        });

        a.poll_open();
        assert_eq!(a.status.as_deref(), Some("open failed: no handler"));
        assert!(a.pending_open.is_empty());
    }

    /// An opener that hasn't exited yet keeps its slot (and reports nothing), then resolves on
    /// a later frame.
    #[test]
    fn in_flight_open_waits_and_then_resolves() {
        let mut a = app();
        let (tx, rx) = unbounded();
        a.pending_open.push(PendingOpen {
            path: "/r/a".into(),
            rx,
        });

        a.poll_open();
        assert_eq!(a.pending_open.len(), 1);
        assert_eq!(a.status, None);

        tx.send(Ok(())).unwrap();
        drop(tx);
        a.poll_open();
        assert_eq!(a.status.as_deref(), Some("opened /r/a"));
        assert!(a.pending_open.is_empty());
    }

    /// PageUp/PageDown jump by the given page size and clamp at the ends.
    #[test]
    fn page_keys_jump_and_clamp() {
        let mut app = app_with_entries();
        // Sorted by size: Beta(30), banner(20), alpha(10), gamma(5).
        app.on_key(KeyAction::PageDown(2));
        assert_eq!(app.tree.nodes[app.selected.unwrap()].name, "alpha");
        app.on_key(KeyAction::PageDown(99));
        assert_eq!(app.tree.nodes[app.selected.unwrap()].name, "gamma");
        app.on_key(KeyAction::PageUp(2));
        assert_eq!(app.tree.nodes[app.selected.unwrap()].name, "banner");
        app.on_key(KeyAction::PageUp(99));
        assert_eq!(app.tree.nodes[app.selected.unwrap()].name, "Beta");
    }

    /// `c` toggles the item count column. It starts hidden, and toggling leaves the selection
    /// alone.
    #[test]
    fn c_toggles_item_count_column() {
        let mut app = app_with_entries();
        assert!(!app.show_item_count, "hidden by default");
        app.on_key(KeyAction::Down);
        let sel = app.selected;
        app.on_key(KeyAction::ToggleItemCount);
        assert!(app.show_item_count);
        assert_eq!(app.selected, sel);
        app.on_key(KeyAction::ToggleItemCount);
        assert!(!app.show_item_count);
    }

    /// Attach a scan stream that started at `started`; the sender feeds it.
    fn add_stream(app: &mut App, refresh: bool, started: Instant) -> Sender<Batch> {
        let (tx, rx) = unbounded();
        app.scans.push(ScanHandle::new(rx, refresh, started));
        app.scanning = true;
        tx
    }

    /// A batch with one file of the given sizes, for the directory with id `parent_id`.
    fn sized_batch(parent_id: u64, name: &str, apparent: u64, disk: u64) -> Batch {
        let mut batch = file_batch(name, apparent);
        batch.parent_id = parent_id;
        batch.nodes[0].disk = disk;
        batch
    }

    /// The footer's throughput figure: bytes per second of the streams that are running, in
    /// the size metric on screen; none when nothing is being scanned.
    #[test]
    fn scan_rate_counts_in_the_metric_on_screen() {
        let mut app = App::new(Tree::new("/r".into(), 0, 0, 1, 1), true, false, true);
        let t0 = Instant::now();
        assert_eq!(app.scan_rate_at(t0), None, "no scan attached");

        let tx = add_stream(&mut app, false, t0);
        // A sparse file: 4 GB by its length, 8 MB on disk.
        tx.send(sized_batch(0, "sparse", 4_000_000_000, 8_000_000))
            .unwrap();
        app.poll_scans(100);
        // Within the first second the rate is what has been seen so far.
        assert_eq!(app.scan_rate_at(t0), Some(8_000_000), "disk usage");
        assert_eq!(
            app.scan_rate_at(t0 + Duration::from_secs(4)),
            Some(2_000_000)
        );
        app.on_key(KeyAction::ToggleUsage);
        assert_eq!(
            app.scan_rate_at(t0 + Duration::from_secs(4)),
            Some(1_000_000_000),
            "apparent size"
        );

        drop(tx);
        app.poll_scans(100);
        assert!(!app.scanning);
        assert_eq!(app.scan_rate_at(t0 + Duration::from_secs(5)), None);
    }

    /// A rescan that starts while the first scan is still running does not reset the figure:
    /// both streams count. When the first scan ends, the figure is the rescan's own.
    #[test]
    fn scan_rate_spans_a_rescan() {
        let mut app = App::new(Tree::new("/r".into(), 0, 0, 1, 1), true, false, true);
        let t0 = Instant::now();
        let secs = Duration::from_secs;
        let first = add_stream(&mut app, false, t0);
        first.send(sized_batch(0, "a", 6_000, 6_000)).unwrap();
        app.poll_scans(100);
        assert_eq!(app.scan_rate_at(t0 + secs(2)), Some(3_000));

        // Two seconds in, a rescan of some directory starts and delivers as well.
        let base = SCAN_ID_STRIDE;
        app.tree.begin_refresh(app.tree.root, base);
        let rescan = add_stream(&mut app, true, t0 + secs(2));
        rescan.send(sized_batch(base, "b", 2_000, 2_000)).unwrap();
        app.poll_scans(100);
        assert_eq!(
            app.scan_rate_at(t0 + secs(4)),
            Some(2_000),
            "8,000 bytes in the 4 seconds since the older stream started"
        );

        // The first scan is done: what remains is the rescan, measured from its own start.
        drop(first);
        app.poll_scans(100);
        assert!(app.scanning, "the rescan is still running");
        assert_eq!(app.scan_rate_at(t0 + secs(4)), Some(1_000));
        drop(rescan);
        app.poll_scans(100);
        assert_eq!(app.scan_rate_at(t0 + secs(4)), None);
    }

    /// The info popup's timestamps (dependency-free UTC formatting, leap days included).
    #[test]
    fn fmt_time_formats_utc() {
        assert_eq!(fmt_time(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(fmt_time(86_400), "1970-01-02 00:00:00 UTC");
        assert_eq!(fmt_time(951_782_400), "2000-02-29 00:00:00 UTC");
        assert_eq!(fmt_time(1_000_000_000), "2001-09-09 01:46:40 UTC");
        assert_eq!(fmt_time(1_700_000_000), "2023-11-14 22:13:20 UTC");
    }

    /// With no matching on-disk file (tree paths under "/r" don't exist here), the popup shows
    /// tree data and degrades gracefully instead of failing.
    #[test]
    fn info_rows_show_tree_data_and_note_missing_disk_entry() {
        let app = app_with_entries();
        let beta = app.tree.nodes[app.tree.root].children[1];
        let rows = app.info_pairs(beta);
        assert!(rows.contains(&("path".to_string(), "/r/Beta".to_string())));
        assert!(rows.contains(&("type".to_string(), "file".to_string())));
        let on_disk = rows
            .iter()
            .find(|(k, _)| k == "on disk")
            .expect("on-disk row present");
        assert!(on_disk.1.starts_with("unavailable"), "got {on_disk:?}");
    }

    /// For a live file, the on-disk section is statted: octal permissions, owner uid:gid, and
    /// a formatted modification time.
    #[test]
    fn info_rows_stat_live_files() {
        use std::fs;
        let base = std::env::temp_dir().join(format!("icdu_info_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        fs::write(base.join("f"), b"0123456789").unwrap();
        let meta = fs::symlink_metadata(&base).unwrap();
        let mut t = Tree::new(
            base.to_string_lossy().into(),
            meta.len(),
            meta.blocks() * 512,
            meta.dev(),
            meta.ino(),
        );
        let f = t.import_child(
            t.root,
            "f".into(),
            NodeKind::File,
            10,
            4096,
            meta.dev(),
            1,
            false,
            false,
            Excluded::No,
            false,
        );
        let mut app = App::new(t, true, false, false);
        let rows = app.info_pairs(f);

        let perms = rows.iter().find(|(k, _)| k == "permissions").unwrap();
        assert!(i64::from_str_radix(&perms.1, 8).is_ok(), "got {perms:?}");
        let owner = rows.iter().find(|(k, _)| k == "owner").unwrap();
        assert!(owner.1.contains(':'), "got {owner:?}");
        let modified = rows.iter().find(|(k, _)| k == "modified").unwrap();
        assert_eq!(
            modified.1.len(),
            23,
            "YYYY-MM-DD HH:MM:SS UTC: {modified:?}"
        );
        assert!(
            rows.iter().all(|(k, _)| k != "entries" && k != "on disk"),
            "no dir-only or error rows for a live file"
        );

        // The same tree as an imported dump: the path exists on this machine, and still must
        // not be looked at, because the dump may describe another one.
        app.imported = true;
        let rows = app.info_pairs(f);
        assert!(
            rows.iter()
                .all(|(k, _)| k != "permissions" && k != "owner" && k != "inode"),
            "nothing is read from the local disk: {rows:?}"
        );
        let on_disk = rows.iter().find(|(k, _)| k == "on disk").unwrap();
        assert!(on_disk.1.contains("imported"), "got {on_disk:?}");

        fs::remove_dir_all(&base).unwrap();
    }

    /// The info popup is read-only: any key closes it, and q must not quit through it.
    #[test]
    fn info_modal_closes_on_any_key_without_quitting() {
        let mut app = app_with_entries();
        app.on_key(KeyAction::Info);
        assert!(matches!(app.modal, Modal::Info(_)));
        app.on_key(KeyAction::Quit);
        assert!(matches!(app.modal, Modal::None));
        assert!(!app.quit, "closing info must not quit");
    }

    /// A dump describes some machine at some moment, not the local filesystem as it is now.
    /// While browsing one, every key that would act on a path is refused, as in ncdu.
    #[test]
    fn imported_dumps_refuse_filesystem_actions() {
        let mut app = app_with_entries();
        app.imported = true;
        app.selected = Some(idx_named(&app, "gamma")); // a directory
        for (key, what) in [
            (KeyAction::Delete, "delete"),
            (KeyAction::Shell, "shell"),
            (KeyAction::Open, "open"),
            (KeyAction::Refresh, "rescan"),
        ] {
            app.status = None;
            app.on_key(key);
            let status = app.status.clone().unwrap_or_default();
            assert!(status.contains("imported dump"), "{what}: {status:?}");
        }
        assert!(matches!(app.modal, Modal::None), "no delete dialog");
        assert!(!app.is_deleting());
        assert_eq!(app.take_shell_request(), None);
        assert!(app.pending_open.is_empty());
        assert_eq!(app.refreshing_idx, None);
    }

    /// An exported dump is readable again by the importer, with the same totals and children.
    #[test]
    fn export_writes_an_importable_dump() {
        let app = app_with_entries();
        let dir = std::env::temp_dir().join(format!("icdu_export_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("dump.json");
        app.export_tree_to(std::fs::File::create(&path).unwrap())
            .unwrap();
        let re = crate::dump::import(std::io::BufReader::new(std::fs::File::open(&path).unwrap()))
            .unwrap();
        assert_eq!(
            re.nodes[re.root].apparent, app.tree.nodes[app.tree.root].apparent,
            "totals survive the round-trip"
        );
        assert_eq!(re.nodes[re.root].children.len(), 4);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Consecutive exports pick the first free `icdu-dump*.json` name instead of clobbering.
    #[test]
    fn export_paths_avoid_clobbering() {
        let dir = std::env::temp_dir().join(format!("icdu_export_name_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let name = |dir: &Path| {
            create_export_file(dir)
                .unwrap()
                .0
                .file_name()
                .unwrap()
                .to_owned()
        };
        assert_eq!(name(&dir), "icdu-dump.json");
        assert_eq!(name(&dir), "icdu-dump-2.json");
        std::fs::write(dir.join("icdu-dump-4.json"), b"x").unwrap();
        assert_eq!(name(&dir), "icdu-dump-3.json");
        assert_eq!(name(&dir), "icdu-dump-5.json");
        assert_eq!(
            std::fs::read(dir.join("icdu-dump-4.json")).unwrap(),
            b"x",
            "an existing file is never truncated"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A symlink planted at the export name (even a dangling one, which `exists()` reports as
    /// absent) must not redirect the write: the export skips the name instead (issue #3).
    #[test]
    fn export_never_writes_through_a_symlink() {
        let dir = std::env::temp_dir().join(format!("icdu_export_link_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let victim = dir.join("victim");
        std::os::unix::fs::symlink(&victim, dir.join("icdu-dump.json")).unwrap();

        let mut app = app_with_entries();
        app.export_into(&dir);

        assert!(!victim.exists(), "the symlink target must not be created");
        let link = std::fs::symlink_metadata(dir.join("icdu-dump.json")).unwrap();
        assert!(
            link.file_type().is_symlink(),
            "the planted link is left alone"
        );
        let out = dir.join("icdu-dump-2.json");
        assert!(
            app.status.as_deref().unwrap().ends_with("icdu-dump-2.json"),
            "{:?}",
            app.status
        );
        assert!(crate::dump::import(std::fs::File::open(&out).unwrap()).is_ok());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A tree whose scan is still running: `done/` (with one file) has been read, `pending/`
    /// (scanner id 2) has not.
    fn app_mid_scan() -> App {
        use crate::scan::NewNode;
        let node = |name: &str, kind, dir_id| NewNode {
            name: name.into(),
            raw_name: None,
            kind,
            apparent: 10,
            disk: 10,
            dev: 1,
            ino: 1,
            hlink: false,
            excluded: Excluded::No,
            error: None,
            dir_id,
        };
        let mut app = App::new(Tree::new("/r".into(), 0, 0, 1, 1), true, false, true);
        app.tree.apply(Batch {
            parent_id: 0,
            nodes: vec![
                node("done", NodeKind::Dir, Some(1)),
                node("pending", NodeKind::Dir, Some(2)),
            ],
            error: None,
        });
        app.tree.apply(Batch {
            parent_id: 1,
            nodes: vec![node("f", NodeKind::File, None)],
            error: None,
        });
        app
    }

    /// An empty directory of its own for a test to export into.
    fn scratch_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("icdu_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn files_in(dir: &Path) -> usize {
        std::fs::read_dir(dir).unwrap().count()
    }

    /// Exporting while directories are still unread is asked about first: to whoever reads
    /// the dump later, it would look like the result of a finished scan.
    #[test]
    fn export_during_a_scan_asks_first() {
        let dir = scratch_dir("export_ask");
        let mut app = app_mid_scan();
        app.export_dir = Some(dir.clone());

        app.on_key(KeyAction::Export);
        assert!(
            matches!(app.modal, Modal::ConfirmExport),
            "asks before writing"
        );
        assert_eq!(files_in(&dir), 0, "nothing written yet");

        app.on_key(KeyAction::Cancel);
        assert!(matches!(app.modal, Modal::None));
        assert_eq!(app.status.as_deref(), Some("export cancelled"));
        assert_eq!(files_in(&dir), 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A confirmed partial export says so, and the dump tells unread directories apart from
    /// empty ones: they carry the read-error flag, which every reader shows as incomplete.
    #[test]
    fn confirmed_partial_export_marks_unread_directories() {
        let dir = scratch_dir("export_partial");
        let mut app = app_mid_scan();
        app.export_dir = Some(dir.clone());

        app.on_key(KeyAction::Export);
        app.on_key(KeyAction::Confirm);
        let status = app.status.clone().unwrap_or_default();
        assert!(status.contains("partial"), "{status:?}");

        let dump =
            crate::dump::import(std::fs::File::open(dir.join("icdu-dump.json")).unwrap()).unwrap();
        let named = |name: &str| dump.nodes.iter().find(|n| n.name == name).unwrap();
        assert!(named("pending").read_error, "unread: shown as incomplete");
        assert!(!named("done").read_error, "read: not flagged");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Once everything has been read there is nothing to ask and nothing to mark.
    #[test]
    fn export_of_a_finished_scan_needs_no_question() {
        let dir = scratch_dir("export_done");
        let mut app = app_mid_scan();
        app.export_dir = Some(dir.clone());
        app.tree.apply(Batch {
            parent_id: 2,
            nodes: vec![],
            error: None,
        });

        app.on_key(KeyAction::Export);
        assert!(matches!(app.modal, Modal::None));
        let status = app.status.clone().unwrap_or_default();
        assert!(
            status.starts_with("exported") && !status.contains("partial"),
            "{status:?}"
        );
        let dump =
            crate::dump::import(std::fs::File::open(dir.join("icdu-dump.json")).unwrap()).unwrap();
        assert!(dump.nodes.iter().all(|n| !n.read_error));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// An app whose entries include read errors, in display order (by size):
    /// big(500), broken(400, error), brokendir(300, error, has a child), small(100).
    fn app_with_errors() -> App {
        let mut t = Tree::new_imported("/r".into(), 0, 0, 1, 1);
        for (name, kind, size, err) in [
            ("big", NodeKind::File, 500u64, false),
            ("broken", NodeKind::File, 400, true),
            ("brokendir", NodeKind::Dir, 300, true),
            ("small", NodeKind::File, 100, false),
        ] {
            t.import_child(
                t.root,
                name.into(),
                kind,
                size,
                size,
                1,
                1,
                false,
                false,
                Excluded::No,
                err,
            );
        }
        App::new(t, true, false, false)
    }

    /// The jump lands on each error entry in display order, moves the view to its parent,
    /// and wraps around after the last one.
    #[test]
    fn next_error_jumps_in_display_order_and_wraps() {
        let mut app = app_with_errors();
        // Initial selection is the first row ("big"); the first error after it is "broken".
        app.on_key(KeyAction::NextError);
        assert_eq!(app.tree.nodes[app.selected.unwrap()].name, "broken");
        assert_eq!(app.tree.nodes[app.cur].name, "/r");
        assert_eq!(app.status.as_deref(), Some("read error: /r/broken"));

        app.on_key(KeyAction::NextError);
        assert_eq!(app.tree.nodes[app.selected.unwrap()].name, "brokendir");

        // Wrapped: both errors seen, back to the first.
        app.on_key(KeyAction::NextError);
        assert_eq!(app.tree.nodes[app.selected.unwrap()].name, "broken");
    }

    /// The reason a directory could not be read is shown where the error is reported: in the
    /// info popup and in the footer when `x` jumps to it. Without a recorded reason (an
    /// imported dump has only the flag) both fall back to the bare fact.
    #[test]
    fn read_errors_show_their_reason() {
        use crate::scan::NewNode;
        let dir = |name: &str, id| NewNode {
            name: name.into(),
            raw_name: None,
            kind: NodeKind::Dir,
            apparent: 0,
            disk: 0,
            dev: 1,
            ino: id,
            hlink: false,
            excluded: Excluded::No,
            error: None,
            dir_id: Some(id),
        };
        let mut app = App::new(Tree::new("/r".into(), 0, 0, 1, 1), true, false, true);
        app.tree.apply(Batch {
            parent_id: 0,
            nodes: vec![dir("locked", 1), dir("flagged", 2)],
            error: None,
        });
        app.tree.apply(Batch {
            parent_id: 1,
            nodes: Vec::new(),
            error: Some("Permission denied (os error 13)".into()),
        });
        app.tree.apply(Batch {
            parent_id: 2,
            nodes: Vec::new(),
            error: None,
        });
        let (locked, flagged) = (idx_named(&app, "locked"), idx_named(&app, "flagged"));
        app.tree.nodes[flagged].read_error = true; // as an import would

        let row = |app: &App, idx| {
            app.info_pairs(idx)
                .into_iter()
                .find(|(k, _)| k == "read error")
                .map(|(_, v)| v)
        };
        assert_eq!(
            row(&app, locked).as_deref(),
            Some("Permission denied (os error 13)")
        );
        assert_eq!(row(&app, flagged).as_deref(), Some("yes"));
        assert_eq!(row(&app, app.tree.root), None);

        app.selected = Some(flagged);
        app.on_key(KeyAction::NextError);
        assert_eq!(app.selected, Some(locked));
        assert_eq!(
            app.status.as_deref(),
            Some("read error: /r/locked: Permission denied (os error 13)")
        );
        app.on_key(KeyAction::NextError);
        assert_eq!(app.selected, Some(flagged));
        assert_eq!(app.status.as_deref(), Some("read error: /r/flagged"));
    }

    /// With no errors anywhere, nothing moves and the footer says so.
    #[test]
    fn next_error_without_errors_reports_none() {
        let mut app = app_with_entries();
        app.on_key(KeyAction::Down); // settle the selection (first key always selects)
        let before = app.selected;
        app.on_key(KeyAction::NextError);
        assert_eq!(app.selected, before);
        assert_eq!(app.status.as_deref(), Some("no read errors"));
    }

    /// `b` opens the shell in the directory on screen, whatever is selected in it (as ncdu
    /// does).
    #[test]
    fn shell_opens_in_the_viewed_directory() {
        let mut app = app_with_entries();
        app.on_key(KeyAction::Shell);
        assert_eq!(app.take_shell_request().as_deref(), Some(Path::new("/r")));
        assert!(app.take_shell_request().is_none(), "request is consumed");

        // "gamma" is a directory. Selected, the shell still opens where the user is.
        app.selected = Some(idx_named(&app, "gamma"));
        app.on_key(KeyAction::Shell);
        assert_eq!(app.take_shell_request().as_deref(), Some(Path::new("/r")));

        // Inside it, the shell opens there.
        app.on_key(KeyAction::Enter);
        assert_eq!(app.cur, idx_named(&app, "gamma"));
        app.on_key(KeyAction::Shell);
        assert_eq!(
            app.take_shell_request().as_deref(),
            Some(Path::new("/r/gamma"))
        );
    }

    /// `r` reads the directory on screen again, not the selected entry (as ncdu does).
    #[test]
    fn rescan_reads_the_viewed_directory() {
        let (root, mut app) = tree_with_a_decoy("rescan_viewed");
        let dir = node_at(&app, &root.join("d"));
        let file = node_at(&app, &root.join("d/own.txt"));
        app.cur = dir;
        app.selected = Some(file);
        app.on_key(KeyAction::Refresh);
        assert_eq!(app.refreshing_idx, Some(dir));
        assert!(
            !app.tree.is_attached(file),
            "everything below it is read again"
        );
        assert!(app.tree.is_attached(dir));
        pump(&mut app, "the rescan", |a| !a.scanning);
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// At the top level, `r` reads the whole tree again.
    #[test]
    fn rescan_at_the_top_reads_the_whole_tree() {
        let (root, mut app) = tree_with_a_decoy("rescan_top");
        let top = app.tree.root;
        let items = app.tree.nodes[top].items;
        app.on_key(KeyAction::Refresh);
        assert_eq!(app.refreshing_idx, Some(top));
        assert!(app.tree.nodes[top].children.is_empty());
        assert_eq!(app.tree.nodes[top].items, 0);
        pump(&mut app, "the rescan", |a| !a.scanning);
        assert_eq!(app.tree.nodes[top].items, items, "and finds it all again");
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// Esc closes popups and nothing else. It used to quit, so a second tap on the key that
    /// closed a popup threw the whole scan away.
    #[test]
    fn esc_does_not_quit() {
        let mut app = app_with_entries();
        app.on_key(KeyAction::Help);
        app.on_key(KeyAction::Cancel);
        assert!(matches!(app.modal, Modal::None), "the popup is closed");
        app.on_key(KeyAction::Cancel);
        assert!(!app.quit);
        app.on_key(KeyAction::Quit);
        assert!(app.quit, "q still quits");
    }

    /// A key whose function moved says where it went and does nothing else.
    #[test]
    fn a_moved_key_only_tells_where_its_function_went() {
        let mut app = app_with_entries();
        app.on_key(KeyAction::Down); // settle the selection (first key always selects)
        let before = (app.cur, app.selected);
        app.on_key(KeyAction::Moved("export is now on E"));
        assert_eq!(app.status.as_deref(), Some("export is now on E"));
        assert!(matches!(app.modal, Modal::None));
        assert_eq!((app.cur, app.selected), before);
    }

    // ---- Whole-tree search (`f`) ------------------------------------------------------

    fn file_under(t: &mut Tree, parent: NodeIdx, name: &str, size: u64) -> NodeIdx {
        t.import_child(
            parent,
            name.into(),
            NodeKind::File,
            size,
            size,
            1,
            1,
            false,
            false,
            Excluded::No,
            false,
        )
    }

    /// The info popup shows a directory's direct entries and its recursive item count; an
    /// excluded directory (never read) gets no item count.
    #[test]
    fn info_rows_show_recursive_item_count_for_dirs() {
        let mut app = app_for_find();
        let row = |app: &App, idx, key: &str| {
            app.info_pairs(idx)
                .into_iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v)
        };
        let docs = idx_named(&app, "docs");
        assert_eq!(row(&app, docs, "entries").as_deref(), Some("2"));
        assert_eq!(row(&app, docs, "total items").as_deref(), Some("3"));
        let file = idx_named(&app, "readme");
        assert_eq!(row(&app, file, "total items"), None, "files have none");

        let ex = app.tree.import_child(
            app.tree.root,
            "node_modules".into(),
            NodeKind::Dir,
            0,
            0,
            1,
            9,
            false,
            false,
            Excluded::Pattern,
            false,
        );
        assert_eq!(row(&app, ex, "total items"), None, "excluded: never read");
    }

    /// /r: docs/ { readme.md (50), sub/ { README.txt (200) } }, readme (10), other.bin (1000).
    fn app_for_find() -> App {
        let mut t = Tree::new_imported("/r".into(), 0, 0, 1, 1);
        let docs = t.import_child(
            t.root,
            "docs".into(),
            NodeKind::Dir,
            0,
            0,
            1,
            2,
            false,
            false,
            Excluded::No,
            false,
        );
        file_under(&mut t, docs, "readme.md", 50);
        let sub = t.import_child(
            docs,
            "sub".into(),
            NodeKind::Dir,
            0,
            0,
            1,
            3,
            false,
            false,
            Excluded::No,
            false,
        );
        file_under(&mut t, sub, "README.txt", 200);
        file_under(&mut t, 0, "readme", 10);
        file_under(&mut t, 0, "other.bin", 1000);
        App::new(t, true, false, false)
    }

    fn find_state(app: &App) -> &Find {
        match &app.modal {
            Modal::Find(f) => f,
            _ => panic!("search popup is not open"),
        }
    }

    fn type_query(app: &mut App, q: &str) {
        for c in q.chars() {
            app.on_key(KeyAction::FindChar(c));
        }
    }

    fn hit_names(app: &App) -> Vec<String> {
        find_state(app)
            .hits
            .iter()
            .map(|&i| app.tree.nodes[i].name.to_string())
            .collect()
    }

    /// Open the popup, type `q`, and run the debounced pass to completion.
    fn search(app: &mut App, q: &str) -> Instant {
        let t0 = Instant::now();
        app.on_key(KeyAction::Find);
        type_query(app, q);
        app.find_tick(t0, usize::MAX);
        app.find_tick(t0 + FIND_DEBOUNCE, usize::MAX);
        t0 + FIND_DEBOUNCE
    }

    fn idx_named(app: &App, name: &str) -> NodeIdx {
        app.tree.nodes.iter().position(|n| n.name == name).unwrap()
    }

    /// Matches come from every level, case-insensitively, largest first; the root (whose
    /// name is the absolute scan path) is never a hit.
    #[test]
    fn find_matches_across_whole_tree() {
        let mut app = app_for_find();
        search(&mut app, "readme");
        assert_eq!(hit_names(&app), vec!["README.txt", "readme.md", "readme"]);
        assert_eq!(find_state(&app).total, 3);
        assert!(!app.find_busy());
        assert_eq!(
            find_state(&app).selected,
            Some(idx_named(&app, "README.txt")),
            "first hit selected"
        );

        let mut app = app_for_find();
        search(&mut app, "/r");
        assert!(hit_names(&app).is_empty(), "root excluded");
    }

    /// The pass runs only once the query has been left alone for the debounce interval, and
    /// every keystroke restarts the wait.
    #[test]
    fn find_debounces_100ms() {
        let mut app = app_for_find();
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        app.on_key(KeyAction::Find);
        app.on_key(KeyAction::FindChar('r'));
        app.find_tick(t0, usize::MAX);
        assert_eq!(app.find_due(), Some(t0 + FIND_DEBOUNCE));
        app.find_tick(t0 + ms(99), usize::MAX);
        assert!(find_state(&app).active.is_none(), "too early");

        // Another key at 99 ms pushes the deadline out again.
        app.on_key(KeyAction::FindChar('e'));
        app.find_tick(t0 + ms(99), usize::MAX);
        app.find_tick(t0 + ms(100), usize::MAX);
        assert!(
            find_state(&app).active.is_none(),
            "re-armed by the second key"
        );
        app.find_tick(t0 + ms(199), usize::MAX);
        assert_eq!(find_state(&app).active.as_deref(), Some("re"));
        assert_eq!(app.find_due(), None);
        assert_eq!(find_state(&app).total, 3);

        // Erasing the query clears the results once the debounce fires.
        app.on_key(KeyAction::FindBackspace);
        app.on_key(KeyAction::FindBackspace);
        app.find_tick(t0 + ms(200), usize::MAX);
        app.find_tick(t0 + ms(300), usize::MAX);
        assert!(find_state(&app).active.is_none());
        assert!(hit_names(&app).is_empty());
    }

    /// With a small budget the pass is spread over several ticks, reporting busy meanwhile.
    #[test]
    fn find_slices_with_budget() {
        let mut app = app_for_find();
        let t0 = Instant::now();
        app.on_key(KeyAction::Find);
        type_query(&mut app, "readme");
        app.find_tick(t0, 0);
        let mut now = t0 + FIND_DEBOUNCE;
        let mut ticks = 0;
        loop {
            app.find_tick(now, 2);
            ticks += 1;
            if !app.find_busy() {
                break;
            }
            assert!(find_state(&app).cursor < app.tree.nodes.len());
            now += Duration::from_millis(1);
        }
        assert_eq!(ticks, app.tree.nodes.len().div_ceil(2));
        assert_eq!(hit_names(&app), vec!["README.txt", "readme.md", "readme"]);
    }

    /// A broad query keeps at most FIND_CAP hits but still counts all matches.
    #[test]
    fn find_caps_hits_but_counts_all() {
        let mut t = Tree::new_imported("/r".into(), 0, 0, 1, 1);
        for i in 0..FIND_CAP + 5 {
            file_under(&mut t, 0, &format!("hit{i}"), i as u64);
        }
        let mut app = App::new(t, true, false, false);
        search(&mut app, "hit");
        let f = find_state(&app);
        assert_eq!(f.hits.len(), FIND_CAP);
        assert_eq!(f.total, FIND_CAP + 5);
        assert!(f.capped());
    }

    /// The footer's byte counter saturates like the totals do.
    #[test]
    fn scan_rate_saturates() {
        use crate::scan::NewNode;
        let huge = |name: &str, ino| NewNode {
            name: name.into(),
            raw_name: None,
            kind: NodeKind::File,
            apparent: u64::MAX / 2 + 1,
            disk: 4096,
            dev: 1,
            ino,
            hlink: false,
            excluded: Excluded::No,
            error: None,
            dir_id: None,
        };
        let mut app = App::new(Tree::new("/r".into(), 0, 0, 1, 1), false, false, true);
        let (tx, rx) = crossbeam_channel::unbounded();
        let started = Instant::now();
        app.scans.push(ScanHandle::new(rx, false, started));
        tx.send(Batch {
            parent_id: 0,
            nodes: vec![huge("x", 2), huge("y", 3)],
            error: None,
        })
        .unwrap();
        app.poll_scans(100);
        assert_eq!(app.scan_rate_at(started), Some(u64::MAX));
        drop(tx);
        app.poll_scans(100);
        assert_eq!(app.size_of(app.tree.root), u64::MAX);
    }

    /// Entries a running scan adds after the pass are picked up by later ticks.
    #[test]
    fn find_picks_up_live_inserts() {
        use crate::scan::NewNode;
        let node = |name: &str, kind, dir_id| NewNode {
            name: name.into(),
            raw_name: None,
            kind,
            apparent: 10,
            disk: 10,
            dev: 1,
            ino: 1,
            hlink: false,
            excluded: Excluded::No,
            error: None,
            dir_id,
        };
        let mut app = App::new(Tree::new("/r".into(), 0, 0, 1, 1), true, false, true);
        app.tree.apply(Batch {
            parent_id: 0,
            nodes: vec![
                node("log", NodeKind::Dir, Some(1)),
                node("a.log", NodeKind::File, None),
            ],
            error: None,
        });
        let now = search(&mut app, ".log");
        assert_eq!(hit_names(&app), vec!["a.log"]);

        app.tree.apply(Batch {
            parent_id: 1,
            nodes: vec![
                node("b.log", NodeKind::File, None),
                node("x", NodeKind::File, None),
            ],
            error: None,
        });
        assert!(app.find_busy(), "new nodes to look at");
        app.find_tick(now + Duration::from_secs(1), usize::MAX);
        assert_eq!(hit_names(&app), vec!["a.log", "b.log"]);
        assert!(!app.find_busy());
    }

    /// Deleting a subtree drops its hits, and a new pass doesn't find the tombstones.
    #[test]
    fn find_drops_deleted_entries() {
        let mut app = app_for_find();
        search(&mut app, "readme");
        app.tree.delete_subtree(idx_named(&app, "docs"));
        app.prune_find();
        assert_eq!(hit_names(&app), vec!["readme"]);
        assert_eq!(find_state(&app).total, 1);
        assert_eq!(find_state(&app).selected, Some(idx_named(&app, "readme")));

        // Retype: the fresh pass walks over the tombstones but skips them.
        app.on_key(KeyAction::FindBackspace);
        app.on_key(KeyAction::FindChar('e'));
        let t = Instant::now();
        app.find_tick(t, usize::MAX);
        app.find_tick(t + FIND_DEBOUNCE, usize::MAX);
        assert_eq!(hit_names(&app), vec!["readme"]);
    }

    /// A rescan tears the old subtree down: its hits go; the rescanned directory stays.
    #[test]
    fn find_drops_rescanned_entries() {
        let mut app = app_for_find();
        search(&mut app, "d");
        // docs/ aggregates its children (250), so it sorts first.
        assert_eq!(
            hit_names(&app),
            vec!["docs", "README.txt", "readme.md", "readme"]
        );
        let docs = idx_named(&app, "docs");
        app.tree.begin_refresh(docs, SCAN_ID_STRIDE);
        app.prune_find();
        assert_eq!(hit_names(&app), vec!["docs", "readme"]);
        assert_eq!(find_state(&app).total, 2);
    }

    /// While a directory is being rescanned, nothing above it may be deleted: its content is
    /// not known again until the rescan has delivered it.
    #[test]
    fn delete_is_refused_above_a_directory_being_rescanned() {
        let mut app = app_for_find();
        let (docs, sub) = (idx_named(&app, "docs"), idx_named(&app, "sub"));
        app.tree.begin_refresh(sub, SCAN_ID_STRIDE);

        app.selected = Some(docs);
        app.on_key(KeyAction::Delete);
        assert!(
            matches!(app.modal, Modal::None),
            "no confirm dialog for a directory with a rescan running below it"
        );
        assert!(app
            .status
            .as_deref()
            .is_some_and(|s| s.contains("still being scanned")));
    }

    /// Scan options for tests that need the rescan key to get past "imported dump".
    fn test_opts() -> Opts {
        Opts {
            threads: 1,
            one_file_system: false,
            excludes: Vec::new(),
            root_dev: 1,
            older_than: None,
            newer_than: None,
        }
    }

    /// Mark `idx` as being deleted by a worker that has not reported yet; the returned sender
    /// delivers its report.
    fn start_fake_delete(app: &mut App, idx: NodeIdx) -> Sender<Report> {
        let (tx, rx) = unbounded();
        app.pending_delete = Some(PendingDelete {
            idx,
            path: app.tree.path_of(idx).into(),
            rx,
        });
        tx
    }

    /// The report of a worker that removed everything it was asked to.
    fn all_removed() -> Report {
        Report {
            complete: true,
            ..Report::default()
        }
    }

    /// A rescan tears down everything below its directory. That must not happen to a
    /// directory that is being removed from disk, wherever in the subtree it sits.
    #[test]
    fn rescan_is_refused_above_a_directory_being_deleted() {
        let mut app = app_for_find();
        app.scan_opts = Some(test_opts());
        let (docs, sub) = (idx_named(&app, "docs"), idx_named(&app, "sub"));
        let _worker = start_fake_delete(&mut app, sub);

        // `docs` contains the directory being deleted.
        app.cur = docs;
        app.on_key(KeyAction::Refresh);
        assert_eq!(app.refreshing_idx, None, "no rescan was started");
        assert!(app.tree.is_attached(sub), "the subtree was not torn down");
        assert!(app
            .status
            .as_deref()
            .is_some_and(|s| s.contains("deletion is in progress")));
    }

    /// If the node is no longer part of the tree when its deletion finishes, there is nothing
    /// left to take out: the global counts must not be reduced a second time.
    #[test]
    fn finished_delete_of_a_detached_node_changes_nothing() {
        let mut app = app_for_find();
        let (docs, sub) = (idx_named(&app, "docs"), idx_named(&app, "sub"));
        let worker = start_fake_delete(&mut app, sub);
        // Something tore the subtree down while the worker was still running.
        app.tree.begin_refresh(docs, SCAN_ID_STRIDE);
        let (dirs, files) = (app.tree.total_dirs, app.tree.total_files);

        worker.send(all_removed()).unwrap();
        app.poll_delete();
        assert!(!app.is_deleting());
        assert_eq!((app.tree.total_dirs, app.tree.total_files), (dirs, files));
    }

    /// A subtree that is being removed from disk is locked. The top-files popup lists files
    /// from everywhere below the viewed directory, so its jump must respect the lock too.
    #[test]
    fn top_files_jump_does_not_enter_a_subtree_being_deleted() {
        let mut app = app_for_find();
        let (docs, sub) = (idx_named(&app, "docs"), idx_named(&app, "sub"));
        let _worker = start_fake_delete(&mut app, sub);

        // Viewing docs/: its largest file is sub/README.txt, inside the directory being
        // deleted, so it is the first row of the popup.
        app.cur = docs;
        app.selected = None;
        app.on_key(KeyAction::TopFiles);
        app.on_key(KeyAction::Enter);
        assert_eq!(app.cur, docs, "the view did not move into sub/");
        assert!(app
            .status
            .as_deref()
            .is_some_and(|s| s.contains("being deleted")));
    }

    /// The next-error jump walks the whole tree; it skips errors inside a locked subtree.
    #[test]
    fn next_error_skips_a_subtree_being_deleted() {
        let mut app = app_for_find();
        let (docs, inside, outside) = (
            idx_named(&app, "docs"),
            idx_named(&app, "README.txt"),
            idx_named(&app, "readme"),
        );
        app.tree.nodes[inside].read_error = true;
        app.tree.nodes[outside].read_error = true;
        let _worker = start_fake_delete(&mut app, docs);

        for _ in 0..3 {
            app.on_key(KeyAction::NextError);
            assert_eq!(app.cur, app.tree.root);
            assert_eq!(app.selected, Some(outside), "only the error outside docs/");
        }

        // With nothing but locked errors left, the view stays put and the footer says why.
        app.tree.nodes[outside].read_error = false;
        app.on_key(KeyAction::NextError);
        assert_eq!(app.cur, app.tree.root);
        assert!(app
            .status
            .as_deref()
            .is_some_and(|s| s.contains("being deleted")));
    }

    /// When a deletion completes while the view is inside the deleted subtree, the view moves
    /// out to the surviving parent instead of staying on nodes that no longer exist.
    #[test]
    fn view_leaves_a_subtree_when_its_deletion_completes() {
        let mut app = app_for_find();
        let (docs, sub) = (idx_named(&app, "docs"), idx_named(&app, "sub"));
        let worker = start_fake_delete(&mut app, docs);
        // However the view got there: it is inside the subtree when the worker reports.
        app.cur = sub;
        app.selected = Some(idx_named(&app, "README.txt"));

        worker.send(all_removed()).unwrap();
        app.poll_delete();
        assert_eq!(
            app.cur, app.tree.root,
            "moved to the parent of the deleted directory"
        );
        assert!(
            app.selected.is_some_and(|s| app.tree.is_attached(s)),
            "an entry that still exists is selected"
        );
        assert_eq!(
            app.tree.fs_path_of(app.cur).as_deref(),
            Some(Path::new("/r"))
        );
    }

    /// A node that is no longer part of the tree has no usable path: the one built from it is
    /// relative and would resolve against the working directory. No action may run on it.
    #[test]
    fn actions_refuse_entries_that_left_the_tree() {
        let mut app = app_for_find();
        app.scan_opts = Some(test_opts());
        let (docs, sub) = (idx_named(&app, "docs"), idx_named(&app, "sub"));
        app.tree.delete_subtree(docs);
        // The state the view could be left in before: looking at the deleted directory.
        app.cur = docs;
        app.selected = Some(sub);

        app.perform_delete(sub);
        assert!(!app.is_deleting(), "delete");
        app.on_key(KeyAction::Open);
        assert!(app.pending_open.is_empty(), "open");
        app.on_key(KeyAction::Shell);
        assert_eq!(app.take_shell_request(), None, "shell");
        app.on_key(KeyAction::Refresh);
        assert_eq!(app.refreshing_idx, None, "rescan");
        assert_eq!(app.status.as_deref(), Some(GONE));

        let on_disk = app.info_pairs(sub);
        assert!(
            on_disk
                .iter()
                .any(|(k, v)| k == "on disk" && v.starts_with("unavailable")),
            "info does not stat a relative path"
        );
    }

    /// An app over a real directory, its tree filled by the real scanner run to completion.
    fn scanned_app(root: &Path, excludes: &[&str]) -> App {
        let meta = std::fs::symlink_metadata(root).unwrap();
        let opts = Opts {
            excludes: excludes.iter().map(|s| s.to_string()).collect(),
            root_dev: meta.dev(),
            ..test_opts()
        };
        let mut tree = Tree::new(
            root.to_string_lossy().into(),
            meta.len(),
            meta.blocks() * 512,
            meta.dev(),
            meta.ino(),
        );
        for batch in crate::scan::start(root.to_path_buf(), opts.clone())
            .events
            .iter()
        {
            tree.apply(batch);
        }
        let mut app = App::new(tree, true, false, false);
        app.scan_opts = Some(opts);
        app
    }

    /// A scratch directory under its canonical path, as the scanner would be given it.
    fn real_dir(tag: &str) -> PathBuf {
        std::fs::canonicalize(scratch_dir(tag)).unwrap()
    }

    /// Poll like the event loop does until `done`, or fail after ten seconds.
    fn pump(app: &mut App, what: &str, done: impl Fn(&App) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !done(app) {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(2));
            app.poll_scans(20_000);
            app.poll_delete();
        }
    }

    /// Select `name` in the root view and confirm its deletion.
    fn delete_by_keys(app: &mut App, name: &str) {
        app.selected = Some(idx_named(app, name));
        app.on_key(KeyAction::Delete);
        assert!(
            matches!(app.modal, Modal::ConfirmDelete(_)),
            "confirm dialog for {name}: {:?}",
            app.status
        );
        app.on_key(KeyAction::Confirm);
    }

    /// Deleting removes what the scan saw. A file that appeared afterwards was never shown
    /// to the user and survives, together with the directories that hold it; the tree is
    /// read again so it shows what is really left.
    #[test]
    fn delete_leaves_what_the_scan_never_saw() {
        let root = real_dir("delete_unseen");
        std::fs::create_dir_all(root.join("victim/sub")).unwrap();
        std::fs::write(root.join("victim/known.txt"), b"known").unwrap();
        std::fs::write(root.join("victim/sub/deep.txt"), b"deep").unwrap();
        let mut app = scanned_app(&root, &[]);

        std::fs::write(root.join("victim/sub/new.txt"), b"created after the scan").unwrap();
        delete_by_keys(&mut app, "victim");
        pump(&mut app, "the deletion", |a| !a.is_deleting());

        assert!(root.join("victim/sub/new.txt").exists(), "unseen file kept");
        assert!(
            !root.join("victim/known.txt").exists(),
            "scanned file removed"
        );
        assert!(!root.join("victim/sub/deep.txt").exists());
        let status = app.status.clone().unwrap_or_default();
        assert!(status.contains("could not be removed"), "{status:?}");

        // The directory is read again, so the tree shows exactly what is left.
        pump(&mut app, "the rescan", |a| !a.scanning);
        let victim = idx_named(&app, "victim");
        assert!(app.tree.is_attached(victim));
        assert_eq!(app.tree.nodes[victim].items, 2, "sub/ and sub/new.txt");
        // The end of that rescan is not announced: the footer still says what was left and why.
        let status = app.status.clone().unwrap_or_default();
        assert!(status.contains("could not be removed"), "{status:?}");
        assert_eq!(app.refreshing_idx, None);
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A fully scanned directory is removed completely and leaves the tree.
    #[test]
    fn delete_removes_a_scanned_directory() {
        let root = real_dir("delete_all");
        std::fs::create_dir_all(root.join("victim/sub")).unwrap();
        std::fs::write(root.join("victim/sub/deep.txt"), b"deep").unwrap();
        std::fs::write(root.join("keep.txt"), b"keep").unwrap();
        let mut app = scanned_app(&root, &[]);

        delete_by_keys(&mut app, "victim");
        pump(&mut app, "the deletion", |a| !a.is_deleting());
        assert!(!root.join("victim").exists());
        assert!(root.join("keep.txt").exists());
        assert!(!app.tree.is_attached(idx_named(&app, "victim")));
        assert!(app
            .status
            .as_deref()
            .is_some_and(|s| s.starts_with("deleted")));
        assert!(!app.scanning, "nothing to read again");
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A directory the scan left out or could not read has unknown content: `d` is refused.
    #[test]
    fn delete_is_refused_on_directories_that_were_not_read() {
        let root = real_dir("delete_excluded");
        std::fs::create_dir_all(root.join("skipme")).unwrap();
        std::fs::write(root.join("skipme/secret.txt"), b"never listed").unwrap();
        let mut app = scanned_app(&root, &["skipme"]);

        app.selected = Some(idx_named(&app, "skipme"));
        app.on_key(KeyAction::Delete);
        assert!(matches!(app.modal, Modal::None), "no confirm dialog");
        assert!(app
            .status
            .as_deref()
            .is_some_and(|s| s.contains("excluded")));
        assert!(root.join("skipme/secret.txt").exists());
        std::fs::remove_dir_all(&root).unwrap();

        let mut app = app_with_errors();
        app.selected = Some(idx_named(&app, "brokendir"));
        app.on_key(KeyAction::Delete);
        assert!(matches!(app.modal, Modal::None), "no confirm dialog");
        assert!(app
            .status
            .as_deref()
            .is_some_and(|s| s.contains("could not be read")));
    }

    /// An excluded directory inside the one being deleted is not emptied: only what the scan
    /// listed goes, and the excluded directory keeps its parents alive.
    #[test]
    fn delete_does_not_empty_an_excluded_directory_inside() {
        let root = real_dir("delete_inner_excluded");
        std::fs::create_dir_all(root.join("victim/skipme")).unwrap();
        std::fs::write(root.join("victim/skipme/secret.txt"), b"never listed").unwrap();
        std::fs::write(root.join("victim/known.txt"), b"known").unwrap();
        let mut app = scanned_app(&root, &["skipme"]);

        delete_by_keys(&mut app, "victim");
        pump(&mut app, "the deletion", |a| !a.is_deleting());
        assert!(root.join("victim/skipme/secret.txt").exists());
        assert!(!root.join("victim/known.txt").exists());
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// `dir/<name>`, where the name is `prefix` followed by a byte that is not valid UTF-8.
    fn non_utf8_path(dir: &Path, prefix: &str) -> PathBuf {
        use std::os::unix::ffi::OsStrExt;
        let mut name = prefix.as_bytes().to_vec();
        name.push(0xff);
        dir.join(std::ffi::OsStr::from_bytes(&name))
    }

    /// Create something at a non-UTF-8 path, or report that this filesystem refuses such
    /// names (APFS on macOS does), in which case the test has nothing to check.
    fn create_or_skip(root: &Path, create: impl FnOnce() -> std::io::Result<()>) -> bool {
        if create().is_ok() {
            return true;
        }
        eprintln!("skipped: this filesystem refuses names that are not valid UTF-8");
        std::fs::remove_dir_all(root).unwrap();
        false
    }

    /// The node the scanner created for the entry at `path`, found by its inode.
    fn node_at(app: &App, path: &Path) -> NodeIdx {
        let ino = std::fs::symlink_metadata(path).unwrap().ino();
        (0..app.tree.nodes.len())
            .find(|&i| i != app.tree.root && app.tree.nodes[i].ino == ino)
            .unwrap()
    }

    /// A name that is not valid UTF-8 is shown with a replacement character, but everything
    /// that touches the filesystem uses its real bytes. Deleting it removes that file, not a
    /// sibling whose real name happens to be the displayed one.
    #[test]
    fn delete_reaches_a_file_whose_name_is_not_utf8() {
        let root = real_dir("raw_delete");
        let raw = non_utf8_path(&root, "bad_");
        if !create_or_skip(&root, || std::fs::write(&raw, b"x")) {
            return;
        }
        let lookalike = root.join("bad_\u{FFFD}");
        std::fs::write(&lookalike, b"not the one").unwrap();
        let mut app = scanned_app(&root, &[]);

        let target = node_at(&app, &raw);
        assert_eq!(app.tree.nodes[target].name, "bad_\u{FFFD}", "shown lossily");
        app.selected = Some(target);
        app.on_key(KeyAction::Delete);
        app.on_key(KeyAction::Confirm);
        pump(&mut app, "the deletion", |a| !a.is_deleting());

        assert!(std::fs::symlink_metadata(&raw).is_err(), "the file is gone");
        assert!(lookalike.exists(), "its look-alike is untouched");
        assert!(app
            .status
            .as_deref()
            .is_some_and(|s| s.starts_with("deleted")));
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// The shell and a rescan get the real path of a directory whose name is not valid UTF-8.
    #[test]
    fn shell_and_rescan_use_the_real_bytes_of_a_name() {
        let root = real_dir("raw_dir");
        let raw = non_utf8_path(&root, "dir_");
        if !create_or_skip(&root, || std::fs::create_dir(&raw)) {
            return;
        }
        std::fs::write(raw.join("inside.txt"), b"x").unwrap();
        let mut app = scanned_app(&root, &[]);
        let dir = node_at(&app, &raw);
        assert_eq!(app.tree.nodes[dir].items, 1, "the scan read it");

        app.cur = dir;
        app.on_key(KeyAction::Shell);
        assert_eq!(app.take_shell_request(), Some(raw.clone()));

        app.on_key(KeyAction::Refresh);
        pump(&mut app, "the rescan", |a| !a.scanning);
        assert!(!app.tree.nodes[dir].read_error, "the rescan found it");
        assert_eq!(app.tree.nodes[dir].items, 1, "and read it again");
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// `a/one` and `b/two` are links to one 5000-byte file; `a` also has a plain file.
    fn tree_with_a_hard_link(tag: &str) -> (PathBuf, App) {
        let root = real_dir(tag);
        std::fs::create_dir(root.join("a")).unwrap();
        std::fs::create_dir(root.join("b")).unwrap();
        std::fs::write(root.join("a/one"), vec![b'x'; 5000]).unwrap();
        std::fs::hard_link(root.join("a/one"), root.join("b/two")).unwrap();
        std::fs::write(root.join("a/plain"), b"x").unwrap();
        let app = scanned_app(&root, &[]);
        (root, app)
    }

    /// Deleting the counted link of a file does not free the file: the other link keeps it.
    /// The total above both links must stay what it was, whichever link the scan counted.
    #[test]
    fn deleting_one_hard_link_keeps_the_inode_in_the_totals() {
        let (root, mut app) = tree_with_a_hard_link("hl_delete");
        let one = node_at_path(&app, &["a", "one"]);
        let two = node_at_path(&app, &["b", "two"]);
        let (counted, other) = if app.tree.nodes[one].shared {
            (two, one)
        } else {
            (one, two)
        };
        let total = app.size_of(app.tree.root);
        let file_size = app.size_of(counted);

        app.cur = app.tree.nodes[counted].parent.unwrap();
        app.selected = Some(counted);
        app.on_key(KeyAction::Delete);
        assert!(matches!(app.modal, Modal::ConfirmDelete(_)));
        assert_eq!(app.confirm_hard_links, 1, "the dialog can say so");
        app.on_key(KeyAction::Confirm);
        pump(&mut app, "the deletion", |a| !a.is_deleting());

        assert!(
            !app.tree.nodes[other].shared,
            "the other link is counted now"
        );
        assert_eq!(app.size_of(app.tree.root), total, "the file is still there");
        assert!(root.join("a/one").exists() != root.join("b/two").exists());

        // The last link gone, the space is free.
        app.cur = app.tree.nodes[other].parent.unwrap();
        app.selected = Some(other);
        app.on_key(KeyAction::Delete);
        app.on_key(KeyAction::Confirm);
        pump(&mut app, "the deletion", |a| !a.is_deleting());
        assert_eq!(app.size_of(app.tree.root), total - file_size);
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// Rescanning a directory that holds one link of a file must not count the file a second
    /// time, and rescanning the whole tree gives the same total as the first scan.
    #[test]
    fn rescans_count_a_hard_linked_file_once() {
        let (root, mut app) = tree_with_a_hard_link("hl_rescan");
        let total = app.size_of(app.tree.root);
        for dir in ["a", "b"] {
            app.cur = node_at_path(&app, &[dir]);
            app.on_key(KeyAction::Refresh);
            pump(&mut app, "the rescan", |a| !a.scanning);
            assert_eq!(app.size_of(app.tree.root), total, "after rescanning {dir}");
        }
        app.cur = app.tree.root;
        app.on_key(KeyAction::Refresh);
        pump(&mut app, "the rescan", |a| !a.scanning);
        assert_eq!(
            app.size_of(app.tree.root),
            total,
            "after rescanning everything"
        );
        let dups = app
            .tree
            .nodes
            .iter()
            .enumerate()
            .filter(|(i, n)| n.shared && app.tree.is_attached(*i))
            .count();
        assert_eq!(dups, 1, "exactly one link is the duplicate");
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// The attached node at `path` (names from the root down).
    fn node_at_path(app: &App, path: &[&str]) -> NodeIdx {
        let mut cur = app.tree.root;
        for name in path {
            cur = *app.tree.nodes[cur]
                .children
                .iter()
                .find(|&&c| app.tree.nodes[c].name == *name)
                .unwrap_or_else(|| panic!("{name} not found"));
        }
        cur
    }

    /// A directory tree for the rescan identity tests: `d/` with one file, and `elsewhere/`
    /// with files that must never show up below `d`.
    fn tree_with_a_decoy(tag: &str) -> (PathBuf, App) {
        let root = real_dir(tag);
        std::fs::create_dir(root.join("d")).unwrap();
        std::fs::write(root.join("d/own.txt"), b"x").unwrap();
        std::fs::create_dir(root.join("elsewhere")).unwrap();
        std::fs::write(root.join("elsewhere/foreign-1"), b"x").unwrap();
        std::fs::write(root.join("elsewhere/foreign-2"), b"x").unwrap();
        let app = scanned_app(&root, &[]);
        (root, app)
    }

    /// The outcome every refused rescan must have: nothing started, the tree as it was, and
    /// a message that names the reason.
    fn assert_rescan_refused(app: &App, dir: NodeIdx) {
        assert_eq!(app.refreshing_idx, None, "no rescan was started");
        let names: Vec<&str> = app.tree.nodes[dir]
            .children
            .iter()
            .map(|&c| app.tree.nodes[c].name.as_str())
            .collect();
        assert_eq!(names, vec!["own.txt"], "the tree is unchanged");
        let status = app.status.as_deref().unwrap_or_default();
        assert!(
            status.contains("no longer the directory that was scanned"),
            "{status}"
        );
    }

    /// `read_dir` follows a symlink in the last component. A directory that was replaced by a
    /// link to another one must not be rescanned: the link's target would be listed, and
    /// counted, as the content of the original directory.
    #[test]
    fn rescan_is_refused_when_the_directory_became_a_symlink() {
        let (root, mut app) = tree_with_a_decoy("rescan_symlink");
        let dir = node_at(&app, &root.join("d"));
        std::fs::remove_dir_all(root.join("d")).unwrap();
        std::os::unix::fs::symlink(root.join("elsewhere"), root.join("d")).unwrap();

        app.cur = dir;
        app.on_key(KeyAction::Refresh);
        assert_rescan_refused(&app, dir);
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// The same for a directory that was replaced by a different directory.
    #[test]
    fn rescan_is_refused_when_the_directory_was_replaced() {
        let (root, mut app) = tree_with_a_decoy("rescan_replaced");
        let dir = node_at(&app, &root.join("d"));
        // Rename instead of remove-and-create, so the old inode number cannot be reused.
        std::fs::rename(root.join("d"), root.join("d.old")).unwrap();
        std::fs::rename(root.join("elsewhere"), root.join("d")).unwrap();

        app.cur = dir;
        app.on_key(KeyAction::Refresh);
        assert_rescan_refused(&app, dir);
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A directory that is still itself is rescanned as before.
    #[test]
    fn rescan_of_an_unchanged_directory_still_works() {
        let (root, mut app) = tree_with_a_decoy("rescan_same");
        let dir = node_at(&app, &root.join("d"));
        std::fs::write(root.join("d/later.txt"), b"x").unwrap();

        app.cur = dir;
        app.on_key(KeyAction::Refresh);
        pump(&mut app, "the rescan", |a| !a.scanning);
        assert!(!app.tree.nodes[dir].read_error);
        assert_eq!(app.tree.nodes[dir].items, 2);
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// An excluded directory is deliberately not scanned, and `r` does not get around that:
    /// neither on the directory itself nor on anything below it.
    #[test]
    fn rescan_is_refused_on_excluded_directories() {
        let mut t = Tree::new_imported("/r".into(), 0, 0, 1, 1);
        let mut add_dir = |parent, name: &str, excluded| {
            t.import_child(
                parent,
                name.into(),
                NodeKind::Dir,
                4096,
                4096,
                2,
                2,
                false,
                false,
                excluded,
                false,
            )
        };
        let mount = add_dir(0, "mnt", Excluded::OtherFs);
        let below = add_dir(mount, "below", Excluded::No);
        let mut app = App::new(t, true, false, false);
        app.scan_opts = Some(test_opts());

        for view in [mount, below] {
            app.cur = view;
            app.selected = None;
            app.status = None;
            app.on_key(KeyAction::Refresh);
            assert_eq!(app.refreshing_idx, None, "no rescan was started");
            assert!(app
                .status
                .as_deref()
                .is_some_and(|s| s.contains("excluded")));
        }
    }

    /// Past the cap the count can't be fixed up from the kept hits: the pass starts over.
    #[test]
    fn find_prune_restarts_a_capped_pass() {
        let mut t = Tree::new_imported("/r".into(), 0, 0, 1, 1);
        let dir = t.import_child(
            t.root,
            "dir".into(),
            NodeKind::Dir,
            0,
            0,
            1,
            2,
            false,
            false,
            Excluded::No,
            false,
        );
        for i in 0..FIND_CAP + 5 {
            file_under(&mut t, dir, &format!("hit{i}"), 1);
        }
        file_under(&mut t, 0, "hit-root", 1);
        let mut app = App::new(t, true, false, false);
        let now = search(&mut app, "hit");
        assert!(find_state(&app).capped());

        app.tree.delete_subtree(dir);
        app.prune_find();
        assert!(app.find_busy(), "restarted");
        app.find_tick(now, usize::MAX);
        assert_eq!(hit_names(&app), vec!["hit-root"]);
        assert_eq!(find_state(&app).total, 1);
    }

    /// Tab limits the search to what lies below the viewed directory, and Tab again widens
    /// it back to the whole tree. The viewed directory itself is never a hit.
    #[test]
    fn find_scope_limits_hits_to_the_viewed_directory() {
        let mut app = app_for_find();
        app.cur = idx_named(&app, "docs");
        let now = search(&mut app, "readme");
        assert_eq!(find_state(&app).scope(), None, "whole tree by default");
        assert_eq!(hit_names(&app), vec!["README.txt", "readme.md", "readme"]);

        app.on_key(KeyAction::FindScope);
        assert_eq!(find_state(&app).scope(), Some(app.cur));
        assert!(app.find_busy(), "the pass starts over");
        app.find_tick(now, usize::MAX);
        assert_eq!(hit_names(&app), vec!["README.txt", "readme.md"]);
        assert_eq!(find_state(&app).total, 2);
        assert_eq!(find_state(&app).query, "readme", "the query is kept");

        app.on_key(KeyAction::FindScope);
        app.find_tick(now, usize::MAX);
        assert_eq!(hit_names(&app), vec!["README.txt", "readme.md", "readme"]);

        // "docs" matches "doc" in the whole tree, but not below itself.
        let mut app = app_for_find();
        app.cur = idx_named(&app, "docs");
        let now = search(&mut app, "doc");
        assert_eq!(hit_names(&app), vec!["docs"]);
        app.on_key(KeyAction::FindScope);
        app.find_tick(now, usize::MAX);
        assert!(hit_names(&app).is_empty());
    }

    /// Scoped to the scan root, the search covers the whole tree.
    #[test]
    fn find_scoped_to_the_root_is_the_whole_tree() {
        let mut app = app_for_find();
        let now = search(&mut app, "readme");
        app.on_key(KeyAction::FindScope);
        app.find_tick(now, usize::MAX);
        assert_eq!(find_state(&app).total, 3);
    }

    /// Entries a running scan adds are matched against the scope as well.
    #[test]
    fn find_scope_applies_to_live_inserts() {
        use crate::scan::NewNode;
        let node = |name: &str, kind, dir_id| NewNode {
            name: name.into(),
            raw_name: None,
            kind,
            apparent: 10,
            disk: 10,
            dev: 1,
            ino: 1,
            hlink: false,
            excluded: Excluded::No,
            error: None,
            dir_id,
        };
        let mut app = App::new(Tree::new("/r".into(), 0, 0, 1, 1), true, false, true);
        app.tree.apply(Batch {
            parent_id: 0,
            nodes: vec![
                node("in", NodeKind::Dir, Some(1)),
                node("out", NodeKind::Dir, Some(2)),
            ],
            error: None,
        });
        app.cur = idx_named(&app, "in");
        let now = search(&mut app, ".log");
        app.on_key(KeyAction::FindScope);
        app.find_tick(now, usize::MAX);
        assert!(hit_names(&app).is_empty());

        for (parent_id, name) in [(1, "inside.log"), (2, "outside.log")] {
            app.tree.apply(Batch {
                parent_id,
                nodes: vec![node(name, NodeKind::File, None)],
                error: None,
            });
        }
        app.find_tick(now + Duration::from_secs(1), usize::MAX);
        assert_eq!(hit_names(&app), vec!["inside.log"]);
    }

    /// Enter on a scoped hit goes to its parent, like on any other hit.
    #[test]
    fn find_enter_jumps_to_a_scoped_hit() {
        let mut app = app_for_find();
        app.cur = idx_named(&app, "docs");
        let now = search(&mut app, "readme");
        app.on_key(KeyAction::FindScope);
        app.find_tick(now, usize::MAX);
        app.on_key(KeyAction::Enter); // README.txt, in docs/sub
        assert!(matches!(app.modal, Modal::None));
        assert_eq!(app.cur, idx_named(&app, "sub"));
        assert_eq!(app.selected, Some(idx_named(&app, "README.txt")));
    }

    /// The scope is the directory on screen. When a finished deletion moves the view while
    /// the popup is open, the scoped search follows it.
    #[test]
    fn find_scope_follows_the_viewed_directory() {
        let mut app = app_for_find();
        app.cur = idx_named(&app, "sub");
        let now = search(&mut app, "readme");
        app.on_key(KeyAction::FindScope);
        app.find_tick(now, usize::MAX);
        assert_eq!(hit_names(&app), vec!["README.txt"]);

        app.cur = idx_named(&app, "docs");
        app.find_tick(now, usize::MAX);
        assert_eq!(find_state(&app).scope(), Some(app.cur));
        assert_eq!(hit_names(&app), vec!["README.txt", "readme.md"]);
    }

    /// A jump always lands on a visible, highlighted row: nothing hides entries any more.
    #[test]
    fn jump_targets_are_always_listed() {
        let mut app = app_for_find();
        search(&mut app, "other");
        app.on_key(KeyAction::Enter);
        let sel = app.selected.expect("the hit is selected");
        assert!(app.sorted_children().contains(&sel));
    }

    /// Ctrl-U empties the query, Ctrl-W removes the last word, and each restarts the search.
    #[test]
    fn find_line_editing() {
        let mut app = app_for_find();
        let t0 = search(&mut app, "read me now");
        app.on_key(KeyAction::FindDeleteWord);
        assert_eq!(find_state(&app).query, "read me ");
        app.on_key(KeyAction::FindDeleteWord);
        assert_eq!(
            find_state(&app).query,
            "read ",
            "trailing spaces go with the word"
        );
        app.on_key(KeyAction::FindDeleteWord);
        assert_eq!(find_state(&app).query, "");
        app.on_key(KeyAction::FindDeleteWord);
        assert_eq!(find_state(&app).query, "", "nothing left to delete");

        type_query(&mut app, "readme");
        app.find_tick(t0, usize::MAX);
        app.find_tick(t0 + FIND_DEBOUNCE, usize::MAX);
        assert_eq!(find_state(&app).total, 3);
        app.on_key(KeyAction::FindClear);
        assert_eq!(find_state(&app).query, "");
        app.find_tick(t0 + FIND_DEBOUNCE, usize::MAX);
        app.find_tick(t0 + FIND_DEBOUNCE * 2, usize::MAX);
        assert!(
            find_state(&app).active.is_none(),
            "an empty query searches nothing"
        );
        assert!(hit_names(&app).is_empty());
    }

    /// Pasted text goes into the query as typed text would, minus line breaks and other
    /// control characters: a newline in the clipboard must not act as Enter.
    #[test]
    fn paste_goes_into_the_search_query() {
        let mut app = app_for_find();
        app.on_key(KeyAction::Down); // settle the selection (first key always selects)
        let (cur, sel) = (app.cur, app.selected);
        let t0 = search(&mut app, "");
        app.on_paste("read\r\nme\t\u{7}");
        assert_eq!(find_state(&app).query, "readme");
        assert!(matches!(app.modal, Modal::Find(_)), "still open: no jump");
        assert_eq!((app.cur, app.selected), (cur, sel));
        app.find_tick(t0, usize::MAX);
        app.find_tick(t0 + FIND_DEBOUNCE, usize::MAX);
        assert_eq!(find_state(&app).total, 3, "and the search runs");

        // Outside the search a paste is not a sequence of key presses.
        let mut app = app_with_entries();
        app.on_key(KeyAction::Down);
        let before = (app.cur, app.selected);
        app.on_paste("dqy\n");
        assert!(matches!(app.modal, Modal::None));
        assert_eq!((app.cur, app.selected), before);
        assert!(!app.quit);
    }

    /// Enter moves the view to the hit's parent with the hit selected and closes the popup.
    #[test]
    fn find_enter_jumps_to_parent_with_match_selected() {
        let mut app = app_for_find();
        search(&mut app, "readme");
        app.on_key(KeyAction::Down); // readme.md, in docs/
        app.on_key(KeyAction::Enter);
        assert!(matches!(app.modal, Modal::None));
        assert_eq!(app.cur, idx_named(&app, "docs"));
        assert_eq!(app.selected, Some(idx_named(&app, "readme.md")));
    }

    /// Esc closes the popup and leaves the view untouched; Enter without hits does nothing.
    #[test]
    fn find_esc_leaves_view_unchanged() {
        let mut app = app_for_find();
        app.on_key(KeyAction::Down);
        let (cur, sel) = (app.cur, app.selected);
        search(&mut app, "zzz");
        app.on_key(KeyAction::Enter);
        assert!(matches!(app.modal, Modal::Find(_)), "nothing to jump to");
        app.on_key(KeyAction::Cancel);
        assert!(matches!(app.modal, Modal::None));
        assert_eq!((app.cur, app.selected), (cur, sel));
        assert!(!app.quit);
    }

    /// Ctrl-C (Quit) still quits while the popup is open; letters like `q` only type.
    #[test]
    fn find_quit_key_quits_but_q_types() {
        let mut app = app_for_find();
        app.on_key(KeyAction::Find);
        app.on_key(KeyAction::FindChar('q'));
        assert!(!app.quit);
        assert_eq!(find_state(&app).query, "q");
        app.on_key(KeyAction::Quit);
        assert!(app.quit);
    }

    /// The highlight follows the selected entry when a re-sort moves it, and navigation
    /// clamps at both ends.
    #[test]
    fn find_selection_tracks_identity_across_resort() {
        let mut app = app_for_find();
        let now = search(&mut app, "readme");
        app.on_key(KeyAction::Down);
        let md = idx_named(&app, "readme.md");
        assert_eq!(find_state(&app).selected, Some(md));

        // A bigger match appears (appended, as a scan would) and sorts to the top.
        let root = app.tree.root;
        file_under(&mut app.tree, root, "readme.huge", 9999);
        app.find_tick(now + Duration::from_secs(1), usize::MAX);
        assert_eq!(hit_names(&app)[0], "readme.huge");
        assert_eq!(
            find_state(&app).selected,
            Some(md),
            "same entry still selected"
        );
        assert_eq!(find_state(&app).selected_pos(), Some(2));

        app.on_key(KeyAction::PageDown(99));
        assert_eq!(find_state(&app).selected_pos(), Some(3));
        app.on_key(KeyAction::PageUp(99));
        assert_eq!(find_state(&app).selected_pos(), Some(0));
    }
}
