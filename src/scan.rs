//! Parallel, streaming filesystem scanner.
//!
//! Design goals:
//!  * **Streaming** — every directory we finish reading is published immediately as a
//!    `Batch`, so the UI can render and aggregate partial results long before the whole
//!    tree is walked.
//!  * **SSD-friendly** — directory reads and `lstat` calls are the bottleneck, and they are
//!    latency-bound rather than bandwidth-bound. We run many worker threads (defaulting to
//!    `cores * 4`) so the kernel always has a deep queue of outstanding I/O. On flash this
//!    overlaps thousands of tiny stat latencies; on a spinning disk it would thrash, which is
//!    exactly the trade-off ncdu can't make because it is single-threaded.
//!
//! Work is pulled from a shared **two-tier frontier**: a `hot` queue of jobs whose path lies
//! under the directory the user is currently viewing, and a `cold` queue for everything else.
//! Workers always drain `hot` first, so navigating into a directory makes its subtree finish
//! scanning ahead of unrelated branches (see [`ScanControl::set_focus`]).
//!
//! Shutdown is tracked by a `pending` count (queued + in-flight jobs): when it reaches zero the
//! frontier is marked `done` and all idle workers are woken to exit, which drops every event
//! `Sender` and disconnects the channel — the UI's signal that the scan is complete.

use std::collections::VecDeque;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

use compact_str::CompactString;
use crossbeam_channel::{unbounded, Receiver, Sender};

use crate::glob;
use crate::model::{Excluded, NodeKind};

/// A single freshly-discovered filesystem entry, sent to the UI to be grafted into the tree.
pub struct NewNode {
    /// The name for display and search. Valid UTF-8 always; if the name on disk is not, the
    /// offending bytes are shown as U+FFFD and `raw_name` carries the real ones.
    pub name: CompactString,
    /// The name exactly as the filesystem has it, when it is not valid UTF-8. Everything that
    /// builds a path to act on uses these bytes; a path built from `name` would not exist, or
    /// would be another entry.
    pub raw_name: Option<Box<[u8]>>,
    pub kind: NodeKind,
    /// Logical file size in bytes (`st_size`).
    pub apparent: u64,
    /// Actual blocks consumed on disk in bytes (`st_blocks * 512`).
    pub disk: u64,
    pub dev: u64,
    pub ino: u64,
    /// A file with more than one link. Which of an inode's links is counted is the tree's
    /// decision (see `Tree::graft`), so it holds across scan generations.
    pub hlink: bool,
    pub excluded: Excluded,
    /// Why the entry could not be examined (its `lstat` failed), if it could not. Such an
    /// entry is shown with the read-error flag and without a size.
    pub error: Option<Box<str>>,
    /// For directories we will recurse into: the id future batches use as `parent_id`.
    pub dir_id: Option<u64>,
}

/// One message per directory we finished reading.
pub struct Batch {
    pub parent_id: u64,
    pub nodes: Vec<NewNode>,
    /// Set if the directory could not be read, or not to the end (e.g. permission denied, an
    /// I/O error halfway through). `nodes` then holds what was listed before the error.
    pub error: Option<String>,
}

/// A unit of work: read directory `path`, whose tree id is `id`.
struct DirJob {
    id: u64,
    path: PathBuf,
}

/// How urgently a queued directory should be read. The lower, the sooner.
const FIRST: usize = 0;
const FOCUSED: usize = 1;
const REST: usize = 2;

/// How many jobs one `pop` re-classifies at most after the priorities changed.
const SORT_CHUNK: usize = 256;

/// The shared pool of directories waiting to be read.
///
/// **Priority.** Jobs fall into three classes: under the directory that should be finished
/// first (the selected entry, or one the user is waiting for), under the directory being
/// viewed, and the rest. A class is emptied before the next is touched.
///
/// **Order.** Within a class the newest job is taken first, so the walk goes depth-first:
/// the subdirectories of the directory just read come next. A subtree is therefore finished
/// before its siblings are started, which is what makes sizes final and `d` / `r` available
/// early, and the pool stays small: it holds the unread siblings along the current paths, not
/// a whole level of the tree.
///
/// **Changing the priorities is cheap.** Re-classifying every queued job when the user moves
/// would block the UI thread and stall the workers on a large pool. Instead the jobs are set
/// aside as `unsorted` and classified a chunk at a time by the workers as they take work.
struct Frontier {
    /// The directory to finish first, if any.
    first: Option<PathBuf>,
    /// The directory the user is currently viewing.
    focus: Option<PathBuf>,
    /// Classified jobs, by class. The back of each is taken first; a job that is classified
    /// late goes to the front, below the jobs that were queued after it.
    queues: [VecDeque<DirJob>; 3],
    /// Jobs queued before the priorities last changed, not classified against them yet.
    unsorted: Vec<DirJob>,
    /// Queued + in-flight jobs. The scan is finished when this hits zero.
    pending: usize,
    done: bool,
    /// The most jobs that were ever queued at once.
    #[cfg(test)]
    peak: usize,
}

impl Frontier {
    fn new(root: DirJob) -> Self {
        Frontier {
            first: None,
            focus: None,
            queues: [VecDeque::new(), VecDeque::new(), VecDeque::from([root])],
            unsorted: Vec::new(),
            pending: 1, // the root job
            done: false,
            #[cfg(test)]
            peak: 1,
        }
    }

    fn class_of(&self, path: &Path) -> usize {
        let under = |dir: &Option<PathBuf>| dir.as_ref().is_some_and(|d| path.starts_with(d));
        if under(&self.first) {
            FIRST
        } else if under(&self.focus) {
            FOCUSED
        } else {
            REST
        }
    }

    #[cfg(test)]
    fn queued(&self) -> usize {
        self.unsorted.len() + self.queues.iter().map(VecDeque::len).sum::<usize>()
    }

    fn push(&mut self, job: DirJob) {
        let class = self.class_of(&job.path);
        self.queues[class].push_back(job);
        #[cfg(test)]
        {
            self.peak = self.peak.max(self.queued());
        }
    }

    /// Change what is read first. The queued jobs are only moved aside here (no path is
    /// looked at); `pop` classifies them later.
    fn set_priorities(&mut self, first: Option<PathBuf>, focus: Option<PathBuf>) {
        if self.first == first && self.focus == focus {
            return;
        }
        self.first = first;
        self.focus = focus;
        // Least urgent at the bottom, so that classifying from the top reaches what used to
        // be urgent first. Within each class the newest job stays on top.
        for class in [REST, FOCUSED, FIRST] {
            let queue = std::mem::take(&mut self.queues[class]);
            if self.unsorted.is_empty() {
                // Takes over the queue's memory; nothing is copied when the jobs lie in one
                // piece, as they do unless some were classified late.
                self.unsorted = Vec::from(queue);
            } else {
                self.unsorted.extend(queue);
            }
        }
    }

    /// The next directory to read.
    fn pop(&mut self) -> Option<DirJob> {
        loop {
            // Classify some of the jobs that predate the current priorities, newest first.
            // Each goes below what its class already holds: those jobs are newer still.
            for _ in 0..SORT_CHUNK {
                let Some(job) = self.unsorted.pop() else {
                    break;
                };
                let class = self.class_of(&job.path);
                self.queues[class].push_front(job);
            }
            // Take the most urgent job known. While unsorted jobs remain, one of them could be
            // more urgent than what is found here; that is accepted, a chunk at a time, in
            // exchange for never stalling.
            if let Some(job) = self.queues.iter_mut().find_map(VecDeque::pop_back) {
                return Some(job);
            }
            if self.unsorted.is_empty() {
                return None;
            }
        }
    }
}

/// Handle the UI keeps to steer the scan. Crucially it does NOT hold the event `Sender`, so the
/// channel still disconnects when the workers finish even while the UI keeps this handle.
pub struct ScanControl {
    frontier: Arc<(Mutex<Frontier>, Condvar)>,
}

impl ScanControl {
    /// Prioritize in two steps: `first` is the directory to finish before anything else (the
    /// selected entry, or one the user is waiting for), `focus` the directory being viewed.
    /// Cheap enough to call on every key press: nothing is re-sorted here.
    pub fn set_priorities(&self, first: Option<PathBuf>, focus: Option<PathBuf>) {
        let (lock, _cv) = &*self.frontier;
        lock.lock().unwrap().set_priorities(first, focus);
        // Workers that are mid-directory pick up the new order on their next pop; no need to
        // wake anyone, because no job was added.
    }
}

#[cfg(test)]
impl ScanControl {
    /// The most directories that were waiting to be read at any one time.
    fn peak_queued(&self) -> usize {
        self.frontier.0.lock().unwrap().peak
    }

    /// Prioritize the subtree rooted at `path` only, as when the user navigates into it.
    fn set_focus(&self, path: Option<PathBuf>) {
        self.set_priorities(None, path);
    }

    /// A handle on a pool with nothing in it, for tests of the code that steers scans.
    pub fn idle() -> Self {
        let mut frontier = Frontier::new(DirJob {
            id: 0,
            path: PathBuf::new(),
        });
        drop(frontier.pop());
        ScanControl {
            frontier: Arc::new((Mutex::new(frontier), Condvar::new())),
        }
    }

    /// The priorities last set: the directory to finish first, and the focused one.
    pub fn priorities(&self) -> (Option<PathBuf>, Option<PathBuf>) {
        let f = self.frontier.0.lock().unwrap();
        (f.first.clone(), f.focus.clone())
    }
}

/// What [`start`] returns: the batch stream plus the steering handle.
pub struct Scan {
    pub events: Receiver<Batch>,
    pub control: ScanControl,
}

/// Scan configuration.
#[derive(Clone)]
pub struct Opts {
    pub threads: usize,
    /// Don't cross filesystem boundaries (ncdu `-x`).
    pub one_file_system: bool,
    /// Glob patterns matched against entry basenames (ncdu `--exclude`).
    pub excludes: Vec<String>,
    /// Device id of the root, used for `one_file_system`.
    pub root_dev: u64,
    /// `--older-than`: entries modified before this Unix time (seconds) are excluded.
    pub older_than: Option<i64>,
    /// `--newer-than`: entries modified after this Unix time (seconds) are excluded.
    pub newer_than: Option<i64>,
}

/// State shared by workers only (NOT the UI), so it holds the event `Sender`.
struct Shared {
    ev_tx: Sender<Batch>,
    next_id: AtomicU64,
    one_file_system: bool,
    root_dev: u64,
    excludes: Vec<String>,
    /// `--older-than` cutoff in Unix seconds; entries modified before it are excluded.
    older_than: Option<i64>,
    /// `--newer-than` cutoff in Unix seconds; entries modified after it are excluded.
    newer_than: Option<i64>,
    /// A rescan's starting directory: its job id and what it must still be when it is read.
    start_check: Option<(u64, Identity)>,
    /// The id of the directory the scan starts at.
    start_id: u64,
}

/// True if the error says the entry is gone. Between listing a directory and examining an
/// entry, anything can be removed; in `/proc` that happens all the time. An entry that
/// vanished is not a read error: nothing is wrong, it is just not there any more.
fn vanished(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::NotFound
}

/// Yields the entries of a directory listing and keeps the first error the listing reports.
/// An error in the middle (an I/O error, a stale network handle) must not be dropped: the
/// directory was not read to the end, and has to say so.
struct Listing<I> {
    inner: I,
    error: Option<String>,
}

impl<T, I: Iterator<Item = std::io::Result<T>>> Iterator for Listing<I> {
    type Item = T;

    fn next(&mut self) -> Option<T> {
        loop {
            match self.inner.next()? {
                Ok(entry) => return Some(entry),
                Err(e) => {
                    self.error.get_or_insert_with(|| e.to_string());
                }
            }
        }
    }
}

/// Device and inode of a directory, as the tree recorded them when it was scanned.
pub type Identity = (u64, u64);

/// The message for a path that no longer names the directory the tree has for it.
pub const NOT_THE_SCANNED_DIR: &str = "this is no longer the directory that was scanned";

/// Check that `path` still names the directory known by `identity`. The check is an `lstat`,
/// so a symlink that took the directory's place is seen as a symlink and not followed.
///
/// `read_dir` follows a symlink in the last component. Without this check, rescanning a
/// directory that was replaced by a link would list the link's target as its content.
pub fn check_identity(path: &Path, identity: Identity) -> Result<(), String> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() && (meta.dev(), meta.ino()) == identity => Ok(()),
        Ok(_) => Err(NOT_THE_SCANNED_DIR.into()),
        Err(e) => Err(e.to_string()),
    }
}

type FrontierArc = Arc<(Mutex<Frontier>, Condvar)>;

/// Default worker count. SSDs love a deep I/O queue, so we oversubscribe cores heavily;
/// the work is almost entirely waiting on `readdir`/`lstat`, not CPU.
pub fn default_threads() -> usize {
    let cores = thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    (cores * 4).clamp(4, 64)
}

/// Id-space width of one scan generation. A scan started with `id_base` uses it for its root
/// directory and allocates further ids above it. Subtree rescans run concurrently with the
/// original scan, so each generation gets a disjoint id range (see [`start_at`]).
pub const SCAN_ID_STRIDE: u64 = 1 << 40;

/// Start scanning `root` (already assigned tree id 0).
/// The event channel disconnects when the scan is done.
pub fn start(root: PathBuf, opts: Opts) -> Scan {
    start_at(root, opts, 0, None)
}

/// Start scanning `root`, treating it as tree id `id_base`: a whole-tree scan passes 0, while
/// a subtree rescan passes its generation's base (a multiple of [`SCAN_ID_STRIDE`]) so its ids
/// never collide with another scan streaming into the same tree.
///
/// A rescan also passes the `identity` the tree has for `root`. If the path names something
/// else by the time it is read, nothing is listed and the directory gets a read error.
pub fn start_at(root: PathBuf, opts: Opts, id_base: u64, identity: Option<Identity>) -> Scan {
    let (ev_tx, ev_rx) = unbounded::<Batch>();

    let frontier: FrontierArc = Arc::new((
        Mutex::new(Frontier::new(DirJob {
            id: id_base,
            path: root,
        })),
        Condvar::new(),
    ));

    let shared = Arc::new(Shared {
        ev_tx,
        next_id: AtomicU64::new(id_base + 1),
        one_file_system: opts.one_file_system,
        root_dev: opts.root_dev,
        excludes: opts.excludes,
        older_than: opts.older_than,
        newer_than: opts.newer_than,
        start_check: identity.map(|identity| (id_base, identity)),
        start_id: id_base,
    });

    for _ in 0..opts.threads {
        let frontier = Arc::clone(&frontier);
        let shared = Arc::clone(&shared);
        thread::spawn(move || worker(frontier, shared));
    }

    // Drop our worker-state reference so the event channel is kept alive only by the workers;
    // it disconnects once the last worker exits. The UI keeps `control` (frontier only).
    drop(shared);

    Scan {
        events: ev_rx,
        control: ScanControl { frontier },
    }
}

fn worker(frontier: FrontierArc, shared: Arc<Shared>) {
    let (lock, cv) = &*frontier;
    loop {
        // Take the next job, preferring the focused subtree; block until one is available or
        // the whole scan is finished.
        let job = {
            let mut f = lock.lock().unwrap();
            loop {
                if let Some(job) = f.pop() {
                    break job;
                }
                if f.done {
                    return;
                }
                f = cv.wait(f).unwrap();
            }
        };

        process(&job, &frontier, &shared);

        // This job is complete. If it was the last one, finish the scan and wake everyone.
        let mut f = lock.lock().unwrap();
        f.pending -= 1;
        if f.pending == 0 {
            f.done = true;
            cv.notify_all();
        }
    }
}

fn process(job: &DirJob, frontier: &FrontierArc, shared: &Shared) {
    let failed = |error: String| {
        let _ = shared.ev_tx.send(Batch {
            parent_id: job.id,
            nodes: Vec::new(),
            error: Some(error),
        });
    };
    let read = match std::fs::read_dir(&job.path) {
        Ok(r) => r,
        // Removed since its parent was listed: an empty directory, not an error. The
        // directory the scan was asked to start at is different: that one has to exist.
        Err(e) if vanished(&e) && job.id != shared.start_id => {
            let _ = shared.ev_tx.send(Batch {
                parent_id: job.id,
                nodes: Vec::new(),
                error: None,
            });
            return;
        }
        Err(e) => return failed(e.to_string()),
    };
    // Checked after the directory is open: had the path been a symlink when it was opened,
    // it would have to be swapped back within this instant to pass.
    if let Some((id, identity)) = shared.start_check {
        if job.id == id {
            if let Err(e) = check_identity(&job.path, identity) {
                return failed(e);
            }
        }
    }

    let mut nodes = Vec::new();
    let mut child_jobs = Vec::new();

    let mut listing = Listing {
        inner: read,
        error: None,
    };
    for entry in listing.by_ref() {
        let path = entry.path();
        let file_name = entry.file_name();
        let (name, raw_name) = match file_name.to_str() {
            Some(valid) => (CompactString::from(valid), None),
            None => (
                CompactString::from(file_name.to_string_lossy()),
                Some(Box::from(file_name.as_bytes())),
            ),
        };

        // lstat: never follow symlinks, so a symlink to a directory is counted as the link
        // itself and never traversed (matching ncdu's default and avoiding cycles).
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(e) if vanished(&e) => continue,
            Err(e) => {
                // Unreadable entry: record it with a read error, zero size.
                nodes.push(NewNode {
                    name,
                    raw_name,
                    kind: NodeKind::Other,
                    apparent: 0,
                    disk: 0,
                    dev: 0,
                    ino: 0,
                    hlink: false,
                    excluded: Excluded::No,
                    error: Some(e.to_string().into()),
                    dir_id: None,
                });
                continue;
            }
        };

        let ft = meta.file_type();
        let kind = if ft.is_dir() {
            NodeKind::Dir
        } else if ft.is_symlink() {
            NodeKind::Link
        } else if ft.is_file() {
            NodeKind::File
        } else {
            NodeKind::Other
        };
        let dev = meta.dev();
        let ino = meta.ino();
        let apparent = meta.len();
        let disk = meta.blocks().saturating_mul(512);
        let mtime = meta.mtime();
        let hlink = matches!(kind, NodeKind::File) && meta.nlink() > 1;

        // Determine exclusion / whether to recurse.
        let mut excluded = Excluded::No;
        if name_excluded(&name, &shared.excludes) {
            excluded = Excluded::Pattern;
        } else if age_excluded(mtime, shared) {
            excluded = Excluded::Age;
        } else if shared.one_file_system && matches!(kind, NodeKind::Dir) && dev != shared.root_dev
        {
            excluded = Excluded::OtherFs;
        }

        let recurse = matches!(kind, NodeKind::Dir) && excluded == Excluded::No;
        let dir_id = if recurse {
            Some(shared.next_id.fetch_add(1, Ordering::Relaxed))
        } else {
            None
        };

        nodes.push(NewNode {
            name,
            raw_name,
            kind,
            apparent,
            disk,
            dev,
            ino,
            hlink,
            excluded,
            error: None,
            dir_id,
        });
        if let Some(id) = dir_id {
            child_jobs.push(DirJob { id, path });
        }
    }

    // Publish this directory's children BEFORE queueing the subdirectories. This guarantees a
    // node's creation batch reaches the UI before any of its descendants' batches, so the tree
    // is always grafted parent-first.
    let _ = shared.ev_tx.send(Batch {
        parent_id: job.id,
        nodes,
        error: listing.error,
    });

    // Enqueue the subdirectories, prioritizing those under the focused path, and wake idle
    // workers. The readdir/stat work above runs outside the lock; only queueing takes it.
    if !child_jobs.is_empty() {
        let (lock, cv) = &**frontier;
        let mut f = lock.lock().unwrap();
        f.pending += child_jobs.len();
        // Pushed in reverse: the newest job is taken first, and the first subdirectory listed
        // should be the first one read.
        for cj in child_jobs.into_iter().rev() {
            f.push(cj);
        }
        cv.notify_all();
    }
}

fn name_excluded(name: &str, patterns: &[String]) -> bool {
    patterns.iter().any(|p| glob::matches(p, name))
}

/// Outside the `--older-than`/`--newer-than` window: `older_than` excludes entries modified
/// before its cutoff, `newer_than` excludes entries modified after it. Both flag the entry
/// like `--exclude` (visible, not descended into, not counted).
fn age_excluded(mtime: i64, shared: &Shared) -> bool {
    if let Some(cutoff) = shared.older_than {
        if mtime < cutoff {
            return true;
        }
    }
    if let Some(cutoff) = shared.newer_than {
        if mtime > cutoff {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use crate::model::{NodeIdx, Tree};
    use std::fs;

    fn opts(threads: usize, root_dev: u64) -> Opts {
        Opts {
            threads,
            one_file_system: false,
            excludes: Vec::new(),
            root_dev,
            older_than: None,
            newer_than: None,
        }
    }

    /// Scan a known tree and confirm the root aggregates every file's apparent size,
    /// every node is reachable, and out-of-order/orphan buffering never drops a batch.
    #[test]
    fn aggregates_full_tree() {
        // Build a deterministic temp tree (no external deps).
        let base = std::env::temp_dir().join(format!("icdu_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("a/b/c")).unwrap();
        fs::create_dir_all(base.join("d")).unwrap();
        fs::write(base.join("a/f1"), vec![b'x'; 1000]).unwrap();
        fs::write(base.join("a/b/f2"), vec![b'x'; 2000]).unwrap();
        fs::write(base.join("a/b/c/f3"), vec![b'x'; 3000]).unwrap();
        fs::write(base.join("d/f4"), vec![b'x'; 4000]).unwrap();
        fs::write(base.join("top"), vec![b'x'; 500]).unwrap();
        let expected_files_bytes = 1000 + 2000 + 3000 + 4000 + 500;
        let expected_files = 5;
        let expected_dirs = 1 /*root*/ + 4 /*a,b,c,d*/;

        let meta = fs::symlink_metadata(&base).unwrap();
        let mut tree = Tree::new(
            "root".into(),
            meta.len(),
            meta.blocks() * 512,
            meta.dev(),
            meta.ino(),
        );

        let rx = start(base.clone(), opts(8, meta.dev())).events;
        for batch in rx.iter() {
            tree.apply(batch);
        }

        assert_eq!(tree.total_files, expected_files, "file count");
        assert_eq!(tree.total_dirs, expected_dirs, "dir count");
        assert!(tree.orphans_is_empty(), "no batch left buffered");
        // The whole tree is scanned, so the root subtree must report complete.
        assert!(
            tree.subtree_complete(tree.root),
            "subtree should be complete"
        );

        // The aggregated root size must equal the sum of every node's (derived) own size, for
        // both metrics — verifying the aggregation and the own-size derivation agree.
        let total_own_apparent: u64 = (0..tree.nodes.len()).map(|i| tree.own_apparent(i)).sum();
        let total_own_disk: u64 = (0..tree.nodes.len()).map(|i| tree.own_disk(i)).sum();
        assert_eq!(
            tree.nodes[tree.root].apparent, total_own_apparent,
            "apparent aggregation"
        );
        assert_eq!(
            tree.nodes[tree.root].disk, total_own_disk,
            "disk aggregation"
        );

        // The file bytes we wrote must be accounted for exactly (files are leaves: own == size).
        let file_bytes: u64 = (0..tree.nodes.len())
            .filter(|&i| !tree.nodes[i].is_dir())
            .map(|i| tree.own_apparent(i))
            .sum();
        assert_eq!(file_bytes, expected_files_bytes, "file bytes accounted");

        fs::remove_dir_all(&base).unwrap();
    }

    /// `--exclude` keeps an entry visible but stops recursion and zeroes its contribution.
    #[test]
    fn exclude_pattern_stops_recursion() {
        let base = std::env::temp_dir().join(format!("icdu_excl_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("keep")).unwrap();
        fs::create_dir_all(base.join("node_modules/deep")).unwrap();
        fs::write(base.join("keep/f"), vec![b'x'; 1000]).unwrap();
        fs::write(base.join("node_modules/big"), vec![b'x'; 9_000_000]).unwrap();

        let meta = fs::symlink_metadata(&base).unwrap();
        let mut tree = Tree::new(
            "root".into(),
            meta.len(),
            meta.blocks() * 512,
            meta.dev(),
            meta.ino(),
        );
        let mut o = opts(4, meta.dev());
        o.excludes = vec!["node_modules".to_string()];
        let rx = start(base.clone(), o).events;
        for batch in rx.iter() {
            tree.apply(batch);
        }

        // node_modules exists as a node but was not recursed and contributes nothing.
        let nm = tree.nodes[tree.root]
            .children
            .iter()
            .find(|&&c| tree.nodes[c].name == "node_modules")
            .copied()
            .expect("node_modules present");
        assert!(tree.nodes[nm].children.is_empty(), "not recursed");
        assert_eq!(tree.nodes[nm].excluded, Excluded::Pattern);
        // The 9 MB inside node_modules must NOT be in the root total.
        assert!(
            tree.nodes[tree.root].apparent < 9_000_000,
            "excluded bytes counted"
        );

        fs::remove_dir_all(&base).unwrap();
    }

    /// Hard-linked files are counted once; the duplicate is flagged `shared`.
    #[test]
    fn hardlinks_counted_once() {
        let base = std::env::temp_dir().join(format!("icdu_hl_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        fs::write(base.join("orig"), vec![b'x'; 5000]).unwrap();
        fs::hard_link(base.join("orig"), base.join("link")).unwrap();

        let meta = fs::symlink_metadata(&base).unwrap();
        let mut tree = Tree::new(
            "root".into(),
            meta.len(),
            meta.blocks() * 512,
            meta.dev(),
            meta.ino(),
        );
        let rx = start(base.clone(), opts(4, meta.dev())).events;
        for batch in rx.iter() {
            tree.apply(batch);
        }

        let shared_count = tree.nodes.iter().filter(|n| n.shared).count();
        assert_eq!(shared_count, 1, "exactly one of the two links is 'shared'");
        // The 5000 bytes are counted once, not twice.
        let file_bytes: u64 = tree
            .nodes
            .iter()
            .filter(|n| matches!(n.kind, NodeKind::File) && n.counts())
            .map(|n| n.apparent) // files are leaves, so apparent == own size
            .sum();
        assert_eq!(file_bytes, 5000, "hardlinked bytes counted once");

        fs::remove_dir_all(&base).unwrap();
    }

    fn job(id: u64, path: &str) -> DirJob {
        DirJob {
            id,
            path: path.into(),
        }
    }

    /// A pool with the given paths queued (ids are their positions, from 1), after the root.
    fn frontier_with(paths: &[&str]) -> Frontier {
        let mut f = Frontier::new(job(0, "/r"));
        assert_eq!(f.pop().map(|j| j.id), Some(0));
        for (i, path) in paths.iter().enumerate() {
            f.push(job(i as u64 + 1, path));
        }
        f
    }

    fn drain(f: &mut Frontier) -> Vec<u64> {
        std::iter::from_fn(|| f.pop().map(|j| j.id)).collect()
    }

    /// The newest job is taken first: reading a directory is followed by its subdirectories,
    /// not by its siblings. That is a depth-first walk.
    #[test]
    fn frontier_takes_the_newest_job_first() {
        let mut f = frontier_with(&["/r/a", "/r/b"]);
        assert_eq!(f.pop().map(|j| j.id), Some(2), "/r/b, the newest");
        // Reading /r/b found two subdirectories. They come before /r/a.
        f.push(job(10, "/r/b/x"));
        f.push(job(11, "/r/b/y"));
        assert_eq!(drain(&mut f), vec![11, 10, 1]);
    }

    /// Jobs under the focused directory come before the rest, and jobs under the directory
    /// to finish first come before those, wherever that directory lies.
    #[test]
    fn frontier_serves_the_classes_in_order() {
        let paths = ["/r/A/x", "/r/B/y", "/r/A/z", "/r/C", "/r/B/q/deep"];
        let mut f = frontier_with(&paths);
        f.set_priorities(None, Some("/r/B".into()));
        assert_eq!(drain(&mut f), vec![5, 2, 4, 3, 1], "B first, then the rest");

        // A directory to finish first inside the focus...
        let mut f = frontier_with(&paths);
        f.set_priorities(Some("/r/B/q".into()), Some("/r/B".into()));
        assert_eq!(drain(&mut f), vec![5, 2, 4, 3, 1]);
        // ...and one outside it, which still wins.
        let mut f = frontier_with(&paths);
        f.set_priorities(Some("/r/C".into()), Some("/r/B".into()));
        assert_eq!(drain(&mut f), vec![4, 5, 2, 3, 1]);

        // New jobs are classified as they arrive.
        let mut f = frontier_with(&["/r/A/x"]);
        f.set_priorities(None, Some("/r/B".into()));
        f.push(job(7, "/r/B/new"));
        f.push(job(8, "/r/A/new"));
        assert_eq!(drain(&mut f), vec![7, 8, 1]);

        // A path is under a directory by components, not by prefix of its text.
        let mut f = frontier_with(&["/r/Bee", "/r/B"]);
        f.set_priorities(None, Some("/r/B".into()));
        assert_eq!(drain(&mut f), vec![2, 1]);
    }

    /// Changing the priorities does not walk the queued jobs: they are set aside and
    /// classified by `pop`, a chunk at a time. No job is lost or served twice on the way,
    /// also when the priorities change again before the first change has been worked off.
    #[test]
    fn frontier_classifies_lazily_and_loses_nothing() {
        let total = SORT_CHUNK * 4 + 17;
        let paths: Vec<String> = (0..total)
            .map(|i| format!("/r/{}/d{i}", if i % 3 == 0 { "hot" } else { "cold" }))
            .collect();
        let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
        let mut f = frontier_with(&refs);

        f.set_priorities(None, Some("/r/hot".into()));
        assert_eq!(f.unsorted.len(), total, "set aside, not classified");
        assert_eq!(f.queued(), total);

        // One pop classifies one chunk and serves a focused job from it.
        let first = f.pop().unwrap();
        assert!(first.path.starts_with("/r/hot"));
        assert_eq!(f.unsorted.len(), total - SORT_CHUNK);

        // The priorities change again while most jobs are still unsorted.
        f.set_priorities(None, Some("/r/cold".into()));
        assert_eq!(f.queued(), total - 1);
        let mut seen = vec![first.id];
        seen.extend(drain(&mut f));
        seen.sort_unstable();
        assert_eq!(seen, (1..=total as u64).collect::<Vec<_>>());
        assert_eq!(f.queued(), 0);

        // Setting the same priorities again is free: nothing is set aside.
        let mut f = frontier_with(&["/r/hot/a", "/r/cold/b"]);
        f.set_priorities(None, Some("/r/hot".into()));
        drop(f.pop());
        f.set_priorities(None, Some("/r/hot".into()));
        assert!(f.unsorted.is_empty());
    }

    /// Once every queued job has been classified, the order is exact again: nothing from the
    /// rest is served while a focused job waits.
    #[test]
    fn frontier_order_is_exact_once_classified() {
        let total = SORT_CHUNK * 3;
        let paths: Vec<String> = (0..total)
            .map(|i| format!("/r/{}/d{i}", if i < 5 { "hot" } else { "cold" }))
            .collect();
        let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
        let mut f = frontier_with(&refs);
        f.set_priorities(None, Some("/r/hot".into()));
        // The five focused jobs were queued first, so they are classified last. Until then
        // other jobs are served; afterwards the focused ones come before anything else.
        let order = drain(&mut f);
        let last_hot = order.iter().rposition(|&id| id <= 5).unwrap();
        let first_hot = order.iter().position(|&id| id <= 5).unwrap();
        assert_eq!(
            last_hot - first_hot,
            4,
            "served together once they were found"
        );
        assert!(
            first_hot <= 3,
            "at most one job per chunk went ahead of them"
        );
    }

    /// A tree with a large `slow/` branch (150 directories of 100 files) and a small `target/`
    /// branch (15 directories). Scans it with the priorities `steer` sets right after the
    /// start, as the UI does, and returns the number of the last batch from each branch.
    fn finish_order(
        tag: &str,
        threads: usize,
        steer: impl Fn(&ScanControl, &Path),
    ) -> (usize, usize) {
        let base = std::env::temp_dir().join(format!("icdu_prio_{tag}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        for d in 0..150 {
            let sd = base.join("slow").join(format!("s{d}"));
            fs::create_dir_all(&sd).unwrap();
            for i in 0..100 {
                fs::write(sd.join(format!("f{i}")), b"x").unwrap();
            }
        }
        for d in 0..15 {
            fs::create_dir_all(base.join("target").join(format!("t{d}"))).unwrap();
        }

        let meta = fs::symlink_metadata(&base).unwrap();
        let scan = start(base.clone(), opts(threads, meta.dev()));
        steer(&scan.control, &base);

        // Track which directory ids belong to each branch (batches arrive parent-first).
        let mut target_ids: HashSet<u64> = HashSet::new();
        let mut slow_ids: HashSet<u64> = HashSet::new();
        let (mut last_target, mut last_slow) = (0usize, 0usize);

        for (i, batch) in scan.events.iter().enumerate() {
            if target_ids.contains(&batch.parent_id) {
                last_target = i;
            }
            if slow_ids.contains(&batch.parent_id) {
                last_slow = i;
            }
            for n in &batch.nodes {
                if let Some(id) = n.dir_id {
                    if batch.parent_id == 0 {
                        if n.name == "target" {
                            target_ids.insert(id);
                        } else if n.name == "slow" {
                            slow_ids.insert(id);
                        }
                    } else if target_ids.contains(&batch.parent_id) {
                        target_ids.insert(id);
                    } else if slow_ids.contains(&batch.parent_id) {
                        slow_ids.insert(id);
                    }
                }
            }
        }
        fs::remove_dir_all(&base).unwrap();
        assert!(last_target > 0 && last_slow > 0, "both branches scanned");
        (last_target, last_slow)
    }

    /// End-to-end: focusing `target/` makes its subtree finish scanning before `slow/` does.
    #[test]
    fn focus_prioritizes_navigated_subtree() {
        // Prioritize the target subtree immediately, as if the user navigated into it.
        let (last_target, last_slow) = finish_order("focus", 8, |control, base| {
            control.set_focus(Some(base.join("target")))
        });
        assert!(
            last_target < last_slow,
            "focused 'target' subtree should finish before 'slow' (target@{last_target}, slow@{last_slow})"
        );
    }

    /// End-to-end: inside the viewed directory, the directory to finish first is read before
    /// its siblings. One thread, so nothing but the priority decides the order.
    #[test]
    fn first_priority_finishes_a_subtree_before_its_siblings() {
        let (last_target, last_slow) = finish_order("first", 1, |control, base| {
            control.set_priorities(Some(base.join("target")), Some(base.to_path_buf()))
        });
        // The 16 directories of `target/` come right after the root, apart from the few of
        // `slow/` that were read before the priorities arrived. `slow/` has 151.
        assert!(
            last_target < 80 && last_target < last_slow,
            "target@{last_target}, slow@{last_slow}"
        );
    }

    /// What a change of priorities costs with a million directories queued. Not a pass/fail
    /// test:
    ///
    /// ```sh
    /// cargo test --release priority_change_measurement -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "a measurement, run by hand"]
    fn priority_change_measurement() {
        use std::time::Instant;
        const JOBS: usize = 1_000_000;
        let mut f = Frontier::new(job(0, "/data"));
        drop(f.pop());
        for i in 0..JOBS {
            f.push(DirJob {
                id: i as u64 + 1,
                path: format!(
                    "/data/cache/{:03}/{:03}/entry-{i}",
                    i % 512,
                    (i / 512) % 512
                )
                .into(),
            });
        }
        let focus = Some(PathBuf::from("/data/cache/007"));

        let t = Instant::now();
        f.set_priorities(None, focus.clone());
        println!("changing the priorities:          {:>9.2?}", t.elapsed());
        let t = Instant::now();
        let first = f.pop().unwrap();
        println!("the first job taken afterwards:   {:>9.2?}", t.elapsed());
        drop(first);

        // What the old code did in one go, on the UI thread and with the lock held.
        let t = Instant::now();
        let hot = f
            .unsorted
            .iter()
            .filter(|j| f.class_of(&j.path) == FOCUSED)
            .count();
        println!(
            "classifying all the others at once: {:>7.2?} ({hot} of them focused)",
            t.elapsed()
        );
    }

    /// How the order in which directories are read plays out on a real tree. Not a pass/fail
    /// test. It scans `ICDU_MEASURE_ROOT` (default `/usr`) with the focus on
    /// `ICDU_MEASURE_FOCUS` (a directory below it, default `<root>/share`), as if the user
    /// had gone there at once, and reports when things completed:
    ///
    /// ```sh
    /// cargo test --release scan_order_measurement -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "a measurement, run by hand"]
    fn scan_order_measurement() {
        use std::time::Instant;
        let root = PathBuf::from(std::env::var("ICDU_MEASURE_ROOT").unwrap_or("/usr".into()));
        let focus = std::env::var("ICDU_MEASURE_FOCUS")
            .map(PathBuf::from)
            .unwrap_or_else(|_| root.join("share"));
        let threads: usize = std::env::var("ICDU_MEASURE_THREADS")
            .ok()
            .and_then(|t| t.parse().ok())
            .unwrap_or(8);
        let meta = fs::symlink_metadata(&root).unwrap();
        let mut tree = Tree::new(
            root.to_string_lossy().into(),
            meta.len(),
            meta.blocks() * 512,
            meta.dev(),
            meta.ino(),
        );
        // The node of the focused directory, once the scan has delivered it.
        let find_focus = |tree: &Tree| -> Option<NodeIdx> {
            let mut cur = tree.root;
            for part in focus.strip_prefix(&root).unwrap().components() {
                let name = part.as_os_str().to_string_lossy();
                cur = *tree.nodes[cur]
                    .children
                    .iter()
                    .find(|&&c| tree.nodes[c].name == name.as_ref())?;
            }
            Some(cur)
        };

        let started = Instant::now();
        let scan = start(root.clone(), opts(threads, meta.dev()));
        scan.control.set_focus(Some(focus.clone()));
        let (mut first_sub, mut focus_done) = (None, None);
        let mut batches = 0usize;
        for batch in scan.events.iter() {
            tree.apply(batch);
            batches += 1;
            let Some(f) = find_focus(&tree) else { continue };
            if first_sub.is_none() {
                // A subdirectory of some substance, not an empty one.
                let done = tree.nodes[f].children.iter().any(|&c| {
                    let n = &tree.nodes[c];
                    n.is_dir() && n.scanned && n.unscanned == 0 && n.items >= 10
                });
                if done {
                    first_sub = Some((batches, started.elapsed()));
                }
            }
            if focus_done.is_none() && tree.subtree_complete(f) {
                focus_done = Some((batches, started.elapsed()));
            }
        }
        let total = started.elapsed();
        let show = |what: &str, at: Option<(usize, std::time::Duration)>| match at {
            Some((n, t)) => println!(
                "{what:<44} after {n:>7} directories ({:>3}%), {t:>9.2?}",
                n * 100 / batches
            ),
            None => println!("{what:<44} never"),
        };
        println!(
            "scanned {} with {threads} threads, focus on {}",
            root.display(),
            focus.display()
        );
        show("first subdirectory of the focus complete", first_sub);
        show("focused directory complete", focus_done);
        println!(
            "{:<44} after {batches:>7} directories (100%), {total:>9.2?}",
            "scan complete"
        );
        println!(
            "{:<44} {:>13}",
            "most directories queued at once",
            scan.control.peak_queued()
        );
    }

    /// Subtree rescan end-to-end: after the whole tree is scanned, `Tree::begin_refresh` tears
    /// down one subtree (keeping ancestor totals consistent) and `start_at` with a fresh id
    /// base re-scans it in place, picking up on-disk changes and restoring totals.
    #[test]
    fn subtree_rescan_restores_totals_and_picks_up_changes() {
        let base = std::env::temp_dir().join(format!("icdu_rescan_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("target/inner")).unwrap();
        fs::create_dir_all(base.join("other")).unwrap();
        fs::write(base.join("target/f1"), vec![b'x'; 1000]).unwrap();
        fs::write(base.join("target/inner/f2"), vec![b'x'; 2000]).unwrap();
        fs::write(base.join("other/f3"), vec![b'x'; 4000]).unwrap();

        let meta = fs::symlink_metadata(&base).unwrap();
        let mut tree = Tree::new(
            "root".into(),
            meta.len(),
            meta.blocks() * 512,
            meta.dev(),
            meta.ino(),
        );
        let rx = start(base.clone(), opts(4, meta.dev())).events;
        for batch in rx.iter() {
            tree.apply(batch);
        }
        tree.finish_scan();

        let target = tree.nodes[tree.root]
            .children
            .iter()
            .find(|&&c| tree.nodes[c].name == "target")
            .copied()
            .expect("target present");
        let root_total_before = tree.nodes[tree.root].apparent;
        let target_agg_before = tree.nodes[target].apparent;
        let target_own_before = tree.own_apparent(target);
        let files_before = tree.total_files;
        assert!(tree.subtree_complete(target));

        // On-disk change while nobody is looking: a new file appears in the subtree.
        fs::write(base.join("target/inner/new"), vec![b'x'; 7000]).unwrap();

        // Tear the subtree down in the model, then rescan just that directory into it.
        let id_base = SCAN_ID_STRIDE;
        tree.begin_refresh(target, id_base);
        assert!(
            !tree.subtree_complete(target),
            "rescan in progress: subtree incomplete"
        );
        assert!(tree.nodes[target].children.is_empty(), "children detached");
        assert_eq!(
            tree.nodes[target].apparent, target_own_before,
            "aggregate reset to own size"
        );
        assert_eq!(
            tree.nodes[tree.root].apparent,
            root_total_before - target_agg_before + target_own_before,
            "ancestor total stays consistent across the reset"
        );

        let target_meta = fs::symlink_metadata(base.join("target")).unwrap();
        let identity = (target_meta.dev(), target_meta.ino());
        let rx = start_at(
            base.join("target"),
            opts(4, meta.dev()),
            id_base,
            Some(identity),
        )
        .events;
        for batch in rx.iter() {
            tree.apply(batch);
        }
        assert!(tree.subtree_complete(target), "rescan completed");
        assert!(tree.orphans_is_empty(), "no batch left buffered");
        assert!(
            tree.nodes[tree.root].apparent > root_total_before,
            "rescan picked up the added bytes"
        );
        assert_eq!(tree.total_files, files_before + 1);
        // The untouched branch is unaffected.
        assert_eq!(tree.nodes[target].children.len(), 2, "f1 + inner");

        // Ground truth: the refreshed tree must agree exactly with an independent fresh scan
        // of the same on-disk state (this also absorbs directory-size drift from the new
        // file, which makes a hand-computed total unreliable).
        let mut fresh = Tree::new(
            "root".into(),
            meta.len(),
            meta.blocks() * 512,
            meta.dev(),
            meta.ino(),
        );
        let rx = start(base.clone(), opts(4, meta.dev())).events;
        for batch in rx.iter() {
            fresh.apply(batch);
        }
        fresh.finish_scan();
        assert_eq!(
            tree.nodes[tree.root].apparent, fresh.nodes[fresh.root].apparent,
            "root apparent matches a fresh scan"
        );
        assert_eq!(
            tree.nodes[tree.root].disk, fresh.nodes[fresh.root].disk,
            "root disk usage matches a fresh scan"
        );
        assert_eq!(tree.total_files, fresh.total_files);
        assert_eq!(tree.total_dirs, fresh.total_dirs);
        assert_eq!(
            tree.nodes[target].items, 4,
            "f1, inner, inner/f2, inner/new"
        );
        assert_eq!(
            tree.nodes[tree.root].items, fresh.nodes[fresh.root].items,
            "root item count matches a fresh scan"
        );

        fs::remove_dir_all(&base).unwrap();
    }

    /// Entries below `dir` on disk, without following symlinks: `find DIR -mindepth 1 | wc -l`.
    fn entries_on_disk(dir: &Path) -> u32 {
        fs::read_dir(dir)
            .unwrap()
            .map(|e| {
                let e = e.unwrap();
                let below = if e.file_type().unwrap().is_dir() {
                    entries_on_disk(&e.path())
                } else {
                    0
                };
                1 + below
            })
            .sum()
    }

    /// Item counts after a real scan: every directory reports the number of entries below it,
    /// matching `find DIR -mindepth 1 | wc -l`. Hard-link duplicates and symlinks count; an
    /// excluded directory counts as one entry and its contents not at all. The count stays
    /// consistent with the global totals after every batch, not just at the end.
    #[test]
    fn item_counts_match_entries_on_disk() {
        let base = std::env::temp_dir().join(format!("icdu_items_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("a/b/c")).unwrap();
        fs::create_dir_all(base.join("d")).unwrap();
        fs::create_dir_all(base.join("skip/x")).unwrap();
        fs::write(base.join("a/f1"), b"x").unwrap();
        fs::write(base.join("a/b/f2"), b"x").unwrap();
        fs::hard_link(base.join("a/f1"), base.join("d/hl")).unwrap();
        std::os::unix::fs::symlink("../a/f1", base.join("d/ln")).unwrap();
        fs::write(base.join("skip/x/y"), b"x").unwrap();
        fs::write(base.join("top"), b"x").unwrap();

        let meta = fs::symlink_metadata(&base).unwrap();
        let mut tree = Tree::new(
            "root".into(),
            meta.len(),
            meta.blocks() * 512,
            meta.dev(),
            meta.ino(),
        );
        let mut o = opts(4, meta.dev());
        o.excludes = vec!["skip".to_string()];
        for batch in start(base.clone(), o).events.iter() {
            tree.apply(batch);
            let root = &tree.nodes[tree.root];
            assert_eq!(
                root.items as u64,
                tree.total_dirs + tree.total_files - 1,
                "live count"
            );
        }

        let child = |parent: NodeIdx, name: &str| {
            tree.nodes[parent]
                .children
                .iter()
                .copied()
                .find(|&c| tree.nodes[c].name == name)
                .unwrap_or_else(|| panic!("{name} present"))
        };
        let a = child(tree.root, "a");
        let b = child(a, "b");
        let c = child(b, "c");
        let d = child(tree.root, "d");
        let skip = child(tree.root, "skip");
        for (idx, path) in [(a, "a"), (b, "a/b"), (c, "a/b/c"), (d, "d")] {
            assert_eq!(
                tree.nodes[idx].items,
                entries_on_disk(&base.join(path)),
                "{path}"
            );
        }
        assert_eq!(tree.nodes[a].items, 4, "f1, b, b/f2, b/c");
        assert_eq!(tree.nodes[d].items, 2, "hard link and symlink");
        assert!(
            tree.nodes.iter().any(|n| n.shared),
            "one link is a duplicate"
        );
        assert_eq!(tree.nodes[skip].items, 0, "excluded: not descended into");
        // Everything on disk, minus what the excluded directory hides (x and x/y).
        assert_eq!(tree.nodes[tree.root].items, entries_on_disk(&base) - 2);
        assert_eq!(tree.nodes[tree.root].items, 10);

        fs::remove_dir_all(&base).unwrap();
    }

    /// An entry that is gone is not an error; everything else is.
    #[test]
    fn only_a_missing_entry_counts_as_vanished() {
        use std::io::{Error, ErrorKind};
        assert!(vanished(&Error::from(ErrorKind::NotFound)));
        assert!(vanished(
            &fs::symlink_metadata("/nonexistent/icdu/path").unwrap_err()
        ));
        for kind in [
            ErrorKind::PermissionDenied,
            ErrorKind::Other,
            ErrorKind::Interrupted,
            ErrorKind::InvalidInput,
        ] {
            assert!(!vanished(&Error::from(kind)), "{kind:?}");
        }
    }

    /// An error in the middle of a listing is kept, and the entries around it still come out.
    #[test]
    fn listing_keeps_the_first_error() {
        use std::io::Error;
        let items: Vec<std::io::Result<u32>> = vec![
            Ok(1),
            Err(Error::other("first")),
            Ok(2),
            Err(Error::other("second")),
        ];
        let mut listing = Listing {
            inner: items.into_iter(),
            error: None,
        };
        assert_eq!(listing.by_ref().collect::<Vec<_>>(), vec![1, 2]);
        assert_eq!(listing.error.as_deref(), Some("first"));

        let mut clean = Listing {
            inner: vec![Ok::<u32, Error>(7)].into_iter(),
            error: None,
        };
        assert_eq!(clean.by_ref().count(), 1);
        assert_eq!(clean.error, None);
    }

    /// Collect a whole scan into a tree.
    fn scan_tree(root: &Path, threads: usize) -> Tree {
        let meta = fs::symlink_metadata(root).unwrap();
        let mut tree = Tree::new(
            root.to_string_lossy().into(),
            meta.len(),
            meta.blocks() * 512,
            meta.dev(),
            meta.ino(),
        );
        for batch in start(root.to_path_buf(), opts(threads, meta.dev()))
            .events
            .iter()
        {
            tree.apply(batch);
        }
        tree
    }

    /// A directory that cannot be read is flagged, and the tree keeps the reason.
    #[test]
    fn unreadable_directory_keeps_the_os_error() {
        use std::os::unix::fs::PermissionsExt;
        let base = std::env::temp_dir().join(format!("icdu_eacces_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("locked")).unwrap();
        fs::write(base.join("locked/inside"), b"x").unwrap();
        fs::write(base.join("plain"), b"x").unwrap();
        fs::set_permissions(base.join("locked"), fs::Permissions::from_mode(0o000)).unwrap();
        let unlock =
            || fs::set_permissions(base.join("locked"), fs::Permissions::from_mode(0o755)).unwrap();
        if fs::read_dir(base.join("locked")).is_ok() {
            eprintln!("skipped: running with privileges that ignore directory permissions");
            unlock();
            fs::remove_dir_all(&base).unwrap();
            return;
        }

        let tree = scan_tree(&base, 2);
        unlock();
        let named = |name: &str| tree.nodes.iter().position(|n| n.name == name).unwrap();
        let locked = named("locked");
        assert!(tree.nodes[locked].read_error);
        assert!(
            tree.error_of(locked)
                .is_some_and(|e| e.contains("Permission denied")),
            "{:?}",
            tree.error_of(locked)
        );
        assert!(!tree.nodes[named("plain")].read_error);
        assert_eq!(tree.error_of(named("plain")), None);
        assert!(!tree.nodes[tree.root].read_error);
        fs::remove_dir_all(&base).unwrap();
    }

    /// Entries that disappear while the scan runs are not read errors. Which ones the scan
    /// still catches is a race; that none of them is flagged must hold every time.
    #[test]
    fn entries_removed_during_the_scan_are_not_errors() {
        let base = std::env::temp_dir().join(format!("icdu_vanish_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        for d in 0..20 {
            let dir = base.join(format!("d{d:02}"));
            fs::create_dir_all(dir.join("sub")).unwrap();
            for f in 0..100 {
                fs::write(dir.join(format!("f{f:03}")), b"x").unwrap();
            }
        }
        let remover = {
            let base = base.clone();
            thread::spawn(move || {
                for d in (0..20).rev() {
                    let _ = fs::remove_dir_all(base.join(format!("d{d:02}")));
                }
            })
        };
        let tree = scan_tree(&base, 1);
        remover.join().unwrap();
        let flagged: Vec<&str> = tree
            .nodes
            .iter()
            .filter(|n| n.read_error)
            .map(|n| n.name.as_str())
            .collect();
        assert!(flagged.is_empty(), "flagged as read errors: {flagged:?}");
        assert!(tree.subtree_complete(tree.root));
        fs::remove_dir_all(&base).unwrap();
    }

    /// A scan that is told what its starting directory must be lists nothing when the path
    /// names something else: a symlink to another directory, or a different directory.
    #[test]
    fn start_check_refuses_a_replaced_directory() {
        let base = std::env::temp_dir().join(format!("icdu_ident_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("real")).unwrap();
        fs::write(base.join("real/file"), b"x").unwrap();
        fs::create_dir_all(base.join("other")).unwrap();
        fs::write(base.join("other/foreign"), b"x").unwrap();
        std::os::unix::fs::symlink(base.join("other"), base.join("link")).unwrap();

        let meta = fs::symlink_metadata(base.join("real")).unwrap();
        let identity = (meta.dev(), meta.ino());
        assert!(check_identity(&base.join("real"), identity).is_ok());
        for wrong in ["link", "other"] {
            assert_eq!(
                check_identity(&base.join(wrong), identity).unwrap_err(),
                NOT_THE_SCANNED_DIR,
                "{wrong}"
            );
        }
        assert!(check_identity(&base.join("missing"), identity).is_err());

        let scan = |name: &str| -> Vec<Batch> {
            start_at(base.join(name), opts(2, meta.dev()), 0, Some(identity))
                .events
                .iter()
                .collect()
        };
        let batches = scan("real");
        assert_eq!(batches.len(), 1);
        assert!(batches[0].error.is_none());
        assert_eq!(batches[0].nodes.len(), 1);
        for wrong in ["link", "other"] {
            let batches = scan(wrong);
            assert_eq!(batches.len(), 1, "{wrong}");
            assert_eq!(batches[0].error.as_deref(), Some(NOT_THE_SCANNED_DIR));
            assert!(batches[0].nodes.is_empty(), "{wrong}: nothing is listed");
        }
        fs::remove_dir_all(&base).unwrap();
    }

    /// `--older-than` / `--newer-than` exclude entries by mtime: filtered entries stay
    /// visible but are not descended into and don't count toward totals.
    #[test]
    fn age_filters_exclude_entries_at_scan_time() {
        use std::time::{Duration, SystemTime};

        let base = std::env::temp_dir().join(format!("icdu_age_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("olddir")).unwrap();
        fs::write(base.join("oldfile"), vec![b'x'; 1000]).unwrap();
        fs::write(base.join("newfile"), vec![b'x'; 2000]).unwrap();
        fs::write(base.join("olddir/inner"), vec![b'x'; 4000]).unwrap();

        // Age the old entries: 2000-01-01, well before any cutoff below. futimens works
        // through a read-only fd, which is also the only way to open a directory.
        let old = SystemTime::UNIX_EPOCH + Duration::from_secs(946_684_800);
        for path in ["oldfile", "olddir", "olddir/inner"] {
            fs::File::open(base.join(path))
                .unwrap()
                .set_times(fs::FileTimes::new().set_modified(old))
                .unwrap();
        }
        let meta = fs::symlink_metadata(&base).unwrap();
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let cutoff = now - 100 * 86_400; // 100 days ago

        let mut tree = Tree::new(
            "root".into(),
            meta.len(),
            meta.blocks() * 512,
            meta.dev(),
            meta.ino(),
        );
        let mut o = opts(4, meta.dev());
        o.older_than = Some(cutoff);
        let rx = start(base.clone(), o).events;
        for batch in rx.iter() {
            tree.apply(batch);
        }
        assert!(tree.orphans_is_empty());

        let find = |name: &str| {
            tree.nodes[tree.root]
                .children
                .iter()
                .find(|&&c| tree.nodes[c].name == name)
                .copied()
                .unwrap_or_else(|| panic!("{name} present"))
        };
        let old_file = find("oldfile");
        let new_file = find("newfile");
        let old_dir = find("olddir");
        assert_eq!(tree.nodes[old_file].excluded, Excluded::Age);
        assert_eq!(tree.nodes[new_file].excluded, Excluded::No);
        assert_eq!(tree.nodes[old_dir].excluded, Excluded::Age);
        // The old directory was not descended into, so its inner file never entered the tree.
        assert!(tree.nodes[old_dir].children.is_empty());
        // Only the fresh file's bytes count toward the root total.
        assert_eq!(tree.nodes[tree.root].apparent, 2000 + meta.len());

        // And the mirror: --newer-than keeps the old entries and excludes the fresh ones.
        let mut tree = Tree::new(
            "root".into(),
            meta.len(),
            meta.blocks() * 512,
            meta.dev(),
            meta.ino(),
        );
        let mut o = opts(4, meta.dev());
        o.newer_than = Some(cutoff);
        let rx = start(base.clone(), o).events;
        for batch in rx.iter() {
            tree.apply(batch);
        }
        let find = |name: &str| {
            tree.nodes[tree.root]
                .children
                .iter()
                .find(|&&c| tree.nodes[c].name == name)
                .copied()
                .unwrap()
        };
        assert_eq!(tree.nodes[find("oldfile")].excluded, Excluded::No);
        assert_eq!(tree.nodes[find("newfile")].excluded, Excluded::Age);
        assert_eq!(tree.nodes[find("olddir")].excluded, Excluded::No);

        fs::remove_dir_all(&base).unwrap();
    }
}
