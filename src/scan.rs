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

use std::collections::{HashSet, VecDeque};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

use compact_str::CompactString;
use crossbeam_channel::{unbounded, Receiver, Sender, TryRecvError};

use crate::glob;
use crate::model::{Excluded, Ext, NodeKind};

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
    /// An entry other than a directory with more than one link. Which of an inode's links is
    /// counted is the tree's decision (see `Tree::graft`), so it holds across scan generations.
    pub hlink: bool,
    /// Its link count (`st_nlink`); 1 where it is not known.
    pub nlink: u32,
    pub excluded: Excluded,
    /// Why the entry could not be examined (its `lstat` failed), if it could not. Such an
    /// entry is shown with the read-error flag and without a size.
    pub error: Option<Box<str>>,
    /// For directories we will recurse into: the id future batches use as `parent_id`.
    pub dir_id: Option<u64>,
    /// With `-L`, for a symlink that is counted as the file it points to: the device and
    /// inode of the link itself. Everything else in the entry is then the target's. Boxed,
    /// because nearly no entry has one.
    pub followed: Option<Box<(u64, u64)>>,
}

/// Entries of one directory, on their way to the tree. A directory usually arrives as one
/// batch. One with very many entries arrives in several, so that its first entries are on
/// screen while the rest is still being read, and no single message holds millions of entries.
pub struct Batch {
    pub parent_id: u64,
    pub nodes: Vec<NewNode>,
    /// Set if the directory could not be read, or not to the end (e.g. permission denied, an
    /// I/O error halfway through). `nodes` then holds what was listed before the error.
    pub error: Option<String>,
    /// More batches for this directory follow. The directory counts as read with the batch
    /// that says `false`.
    pub more: bool,
    /// With `Opts::extended`: owner, group, mode and modification time of each of `nodes`,
    /// in the same order. Empty otherwise, so that an entry in transit is no larger for it.
    pub ext: Vec<Ext>,
}

/// The most entries of one directory sent in one batch.
pub const BATCH_ENTRIES: usize = 4096;

/// The most entries a scan may have on their way to the UI. Beyond this the workers wait.
/// The UI takes batches off every frame, so in normal operation the limit is never reached;
/// it holds memory down when the UI is busy elsewhere for a while (in the shell, say).
/// About 100 bytes per entry: a few hundred megabytes at most.
const QUEUED_ENTRIES: usize = 2_000_000;

/// Counts the entries that have been sent and not yet received, and makes senders wait while
/// there are too many. The channel itself is unbounded: its capacity would count batches,
/// and a batch can hold one entry or thousands.
struct Gate {
    limit: usize,
    state: Mutex<GateState>,
    changed: Condvar,
}

struct GateState {
    queued: usize,
    /// False once the receiving side is gone: nobody will make room, so nobody waits.
    open: bool,
}

impl Gate {
    fn new(limit: usize) -> Self {
        Gate {
            limit,
            state: Mutex::new(GateState {
                queued: 0,
                open: true,
            }),
            changed: Condvar::new(),
        }
    }

    /// Account for `entries` about to be sent, waiting first while the limit is reached. A
    /// batch always gets through when nothing is queued, however large it is.
    fn enter(&self, entries: usize) {
        let mut state = self.state.lock().unwrap();
        while state.open && state.queued > 0 && state.queued + entries > self.limit {
            state = self.changed.wait(state).unwrap();
        }
        state.queued += entries;
    }

    /// `entries` have been received.
    fn leave(&self, entries: usize) {
        let mut state = self.state.lock().unwrap();
        state.queued = state.queued.saturating_sub(entries);
        drop(state);
        self.changed.notify_all();
    }

    fn close(&self) {
        self.state.lock().unwrap().open = false;
        self.changed.notify_all();
    }
}

/// The receiving end of a scan: its batches, in the order they were sent. Taking a batch
/// makes room for the workers to send more.
pub struct Events {
    rx: Receiver<Batch>,
    gate: Option<Arc<Gate>>,
}

impl Events {
    /// A stream fed by hand, without a limit. For tests of the code that consumes one.
    #[cfg(test)]
    pub fn from_channel(rx: Receiver<Batch>) -> Self {
        Events { rx, gate: None }
    }

    fn received(&self, batch: &Batch) {
        if let Some(gate) = &self.gate {
            gate.leave(batch.nodes.len());
        }
    }

    /// The next batch if one is waiting. `Disconnected` means the scan is over.
    pub fn try_recv(&self) -> Result<Batch, TryRecvError> {
        let batch = self.rx.try_recv()?;
        self.received(&batch);
        Ok(batch)
    }

    /// Every batch until the scan is over, waiting for each.
    pub fn iter(&self) -> impl Iterator<Item = Batch> + '_ {
        self.rx.iter().inspect(|batch| self.received(batch))
    }
}

impl Drop for Events {
    fn drop(&mut self) {
        // Nobody will receive any more: let waiting workers through, so they can finish.
        if let Some(gate) = &self.gate {
            gate.close();
        }
    }
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

/// The queued jobs of one priority class.
#[derive(Default)]
struct Queue {
    /// Take the newest job first (a depth-first walk) instead of the oldest (breadth-first).
    newest_first: bool,
    /// Jobs that were queued under earlier priorities and classified into this queue later.
    /// They are older than everything in `fresh`.
    late: VecDeque<DirJob>,
    /// Jobs queued under the current priorities, oldest at the front.
    fresh: VecDeque<DirJob>,
}

impl Queue {
    fn len(&self) -> usize {
        self.late.len() + self.fresh.len()
    }

    fn pop(&mut self) -> Option<DirJob> {
        if self.newest_first {
            self.fresh.pop_back().or_else(|| self.late.pop_back())
        } else {
            self.late.pop_front().or_else(|| self.fresh.pop_front())
        }
    }
}

/// The shared pool of directories waiting to be read.
///
/// **Priority.** Jobs fall into three classes: under a directory the user is waiting for
/// (`first`), under the directory being viewed (`focus`), and the rest. A class is emptied
/// before the next is touched.
///
/// **Order.** The focused directory and the rest are read oldest job first, which is a
/// breadth-first walk: every entry on screen gets a size early, and the readers spread over
/// the whole tree, and with it over all its disks. Measurements on a machine with several
/// filesystems of very different speed decided this: a depth-first walk sent every reader
/// into the first big directory while the rows next to it stayed empty, and letting the rows
/// take turns tied the fast filesystems to the pace of the slowest one (see #38).
///
/// A directory the user is waiting for is different. There the aim is to have that one
/// subtree complete as soon as possible, so it is read newest job first, depth-first.
///
/// **Changing the priorities is cheap.** Re-classifying every queued job when the user moves
/// would block the UI thread and stall the workers on a large pool. Instead the queues are
/// set aside as they are (`stale`), and the workers classify their jobs a chunk at a time as
/// they take work.
struct Frontier {
    /// A directory to finish before anything else, if any.
    first: Option<PathBuf>,
    /// The directory the user is currently viewing.
    focus: Option<PathBuf>,
    /// The jobs classified against the current priorities, by class.
    classes: [Queue; 3],
    /// Queues classified against earlier priorities. The last one is worked off first.
    stale: Vec<Queue>,
    /// Queued + in-flight jobs. The scan is finished when this hits zero.
    pending: usize,
    done: bool,
    /// The most jobs that were ever queued at once.
    #[cfg(test)]
    peak: usize,
}

impl Frontier {
    fn new(root: DirJob) -> Self {
        let mut frontier = Frontier {
            first: None,
            focus: None,
            classes: Self::empty_classes(),
            stale: Vec::new(),
            pending: 1, // the root job
            done: false,
            #[cfg(test)]
            peak: 0,
        };
        frontier.push(root);
        frontier
    }

    fn empty_classes() -> [Queue; 3] {
        let mut classes: [Queue; 3] = Default::default();
        classes[FIRST].newest_first = true;
        classes
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
        self.classes.iter().chain(&self.stale).map(Queue::len).sum()
    }

    fn push(&mut self, job: DirJob) {
        let class = self.class_of(&job.path);
        self.classes[class].fresh.push_back(job);
        #[cfg(test)]
        {
            self.peak = self.peak.max(self.queued());
        }
    }

    /// Change what is read first. No job is looked at here: the queues are set aside as they
    /// are, and `pop` classifies their jobs later.
    fn set_priorities(&mut self, first: Option<PathBuf>, focus: Option<PathBuf>) {
        if self.first == first && self.focus == focus {
            return;
        }
        self.first = first;
        self.focus = focus;
        let old = std::mem::replace(&mut self.classes, Self::empty_classes());
        // The last queue is worked off first: what used to be most urgent goes last.
        self.stale
            .extend(old.into_iter().rev().filter(|queue| queue.len() > 0));
    }

    /// The next directory to read.
    fn pop(&mut self) -> Option<DirJob> {
        loop {
            // Classify some of the jobs that predate the current priorities, in the order
            // they would have been read in.
            let mut budget = SORT_CHUNK;
            while budget > 0 {
                let Some(queue) = self.stale.last_mut() else {
                    break;
                };
                match queue.pop() {
                    Some(job) => {
                        let class = self.class_of(&job.path);
                        self.classes[class].late.push_back(job);
                        budget -= 1;
                    }
                    None => {
                        self.stale.pop();
                    }
                }
            }
            // Take the most urgent job known. While stale jobs remain, one of them could be
            // more urgent than what is found here; that is accepted, a chunk at a time, in
            // exchange for never stalling.
            if let Some(job) = self.classes.iter_mut().find_map(Queue::pop) {
                return Some(job);
            }
            if self.stale.is_empty() {
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
    /// Prioritize in two steps: `first` is a directory to finish before anything else (one
    /// the user is waiting for), `focus` the directory being viewed. Cheap enough to call on
    /// every key press: nothing is re-sorted here.
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
    pub events: Events,
    pub control: ScanControl,
}

/// Scan configuration.
#[derive(Clone)]
pub struct Opts {
    pub threads: usize,
    /// Don't cross filesystem boundaries (ncdu `-x`).
    pub one_file_system: bool,
    /// Glob patterns for entries to leave out (ncdu `--exclude`), matched against names and
    /// paths as described in `glob.rs`.
    pub excludes: Vec<String>,
    /// Device id of the root, used for `one_file_system`.
    pub root_dev: u64,
    /// `--older-than`: entries modified before this Unix time (seconds) are excluded.
    pub older_than: Option<i64>,
    /// `--newer-than`: entries modified after this Unix time (seconds) are excluded.
    pub newer_than: Option<i64>,
    /// Record owner, group, mode and modification time of every entry (ncdu `-e`).
    pub extended: bool,
    /// Leave out directories tagged as caches (ncdu `--exclude-caches`).
    pub exclude_caches: bool,
    /// Leave out Linux pseudo filesystems (ncdu `--exclude-kernfs`). No effect elsewhere.
    pub exclude_kernfs: bool,
    /// Count a symlink to a file as that file (ncdu `-L`). Links to directories are never
    /// followed.
    pub follow_symlinks: bool,
}

/// State shared by workers only (NOT the UI), so it holds the event `Sender`.
struct Shared {
    ev_tx: Sender<Batch>,
    /// Bounds what is on its way to the UI.
    gate: Arc<Gate>,
    next_id: AtomicU64,
    one_file_system: bool,
    root_dev: u64,
    /// The `--exclude` patterns, compiled.
    excludes: Vec<glob::Pattern>,
    /// `--older-than` cutoff in Unix seconds; entries modified before it are excluded.
    older_than: Option<i64>,
    /// `--newer-than` cutoff in Unix seconds; entries modified after it are excluded.
    newer_than: Option<i64>,
    /// Send extended information with every batch.
    extended: bool,
    /// Do not enter directories that have a valid `CACHEDIR.TAG`.
    exclude_caches: bool,
    /// The devices of the kernel filesystems mounted when the scan started, if those are to
    /// be left out; empty otherwise.
    kernfs: HashSet<u64>,
    /// Count symlinks to files as their targets.
    follow_symlinks: bool,
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
    start_limited(root, opts, id_base, identity, QUEUED_ENTRIES)
}

/// [`start_at`] with a given limit on the entries in transit.
fn start_limited(
    root: PathBuf,
    opts: Opts,
    id_base: u64,
    identity: Option<Identity>,
    queued_entries: usize,
) -> Scan {
    let (ev_tx, ev_rx) = unbounded::<Batch>();
    let gate = Arc::new(Gate::new(queued_entries));

    let frontier: FrontierArc = Arc::new((
        Mutex::new(Frontier::new(DirJob {
            id: id_base,
            path: root,
        })),
        Condvar::new(),
    ));

    let shared = Arc::new(Shared {
        ev_tx,
        gate: Arc::clone(&gate),
        next_id: AtomicU64::new(id_base + 1),
        one_file_system: opts.one_file_system,
        root_dev: opts.root_dev,
        excludes: opts
            .excludes
            .iter()
            .map(|pattern| glob::Pattern::new(pattern))
            .collect(),
        older_than: opts.older_than,
        newer_than: opts.newer_than,
        extended: opts.extended,
        exclude_caches: opts.exclude_caches,
        follow_symlinks: opts.follow_symlinks,
        kernfs: if opts.exclude_kernfs {
            mounted_kernfs()
        } else {
            HashSet::new()
        },
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
        events: Events {
            rx: ev_rx,
            gate: Some(gate),
        },
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
    // Send entries of this directory to the UI, waiting if too much is on its way already.
    let send = |nodes: Vec<NewNode>, ext: Vec<Ext>, error: Option<String>, more: bool| {
        debug_assert!(ext.is_empty() || ext.len() == nodes.len());
        shared.gate.enter(nodes.len());
        let _ = shared.ev_tx.send(Batch {
            parent_id: job.id,
            nodes,
            error,
            more,
            ext,
        });
    };
    let failed = |error: String| send(Vec::new(), Vec::new(), Some(error), false);
    let read = match std::fs::read_dir(&job.path) {
        Ok(r) => r,
        // Removed since its parent was listed: an empty directory, not an error. The
        // directory the scan was asked to start at is different: that one has to exist.
        Err(e) if vanished(&e) && job.id != shared.start_id => {
            return send(Vec::new(), Vec::new(), None, false);
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
    let mut exts = Vec::new();
    let mut child_jobs = Vec::new();
    // For the exclude patterns: this directory's path, and room to build an entry's path in.
    let dir_bytes = job.path.as_os_str().as_bytes();
    let mut scratch = Vec::new();

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
                    nlink: 1,
                    excluded: Excluded::No,
                    error: Some(e.to_string().into()),
                    dir_id: None,
                    followed: None,
                });
                if shared.extended {
                    // Nothing is known about an entry that could not be examined.
                    exts.push(Ext::default());
                }
                continue;
            }
        };

        // Patterns see the entry's path as well as its name, as in ncdu.
        let by_pattern = shared
            .excludes
            .iter()
            .any(|p| p.excludes(dir_bytes, file_name.as_bytes(), &mut scratch));

        // With `-L` a symlink to anything but a directory is counted as what it points to: the
        // entry gets the target's type, sizes, device and inode, as in ncdu. What stays of the
        // link is its name, and its own identity, which a delete has to check. A link to a
        // directory, a broken link and a loop stay what they are, and what a pattern excludes
        // is not looked at any further.
        let mut followed = None;
        let meta = if shared.follow_symlinks && !by_pattern && meta.file_type().is_symlink() {
            match std::fs::metadata(&path) {
                Ok(target) if !target.is_dir() => {
                    followed = Some(Box::new((meta.dev(), meta.ino())));
                    target
                }
                _ => meta,
            }
        } else {
            meta
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
        let nlink = u32::try_from(meta.nlink()).unwrap_or(u32::MAX);
        // Anything but a directory can have several names: files, and symlinks, devices,
        // sockets and fifos too. (A directory's link count is about its subdirectories.)
        let hlink = !matches!(kind, NodeKind::Dir) && nlink > 1;

        // Determine exclusion / whether to recurse.
        let mut excluded = Excluded::No;
        // A file on another filesystem can only turn up through a followed link.
        let may_leave = matches!(kind, NodeKind::Dir) || followed.is_some();
        if by_pattern {
            excluded = Excluded::Pattern;
        } else if age_excluded(mtime, shared) {
            excluded = Excluded::Age;
        } else if shared.one_file_system && may_leave && dev != shared.root_dev {
            excluded = Excluded::OtherFs;
        } else if matches!(kind, NodeKind::Dir) && shared.kernfs.contains(&dev) {
            excluded = Excluded::Kernfs;
        } else if shared.exclude_caches && matches!(kind, NodeKind::Dir) && has_cachedir_tag(&path)
        {
            // The dump format has no reason of its own for this; ncdu writes `pattern` too.
            excluded = Excluded::Pattern;
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
            nlink,
            excluded,
            error: None,
            dir_id,
            followed,
        });
        if shared.extended {
            exts.push(ext_of(&meta));
        }
        if let Some(id) = dir_id {
            child_jobs.push(DirJob { id, path });
        }
        // A directory with very many entries goes out in parts: what has been read so far is
        // on screen, and its subdirectories are being read, while the listing continues.
        if nodes.len() >= BATCH_ENTRIES {
            send(
                std::mem::take(&mut nodes),
                std::mem::take(&mut exts),
                None,
                true,
            );
            enqueue(frontier, std::mem::take(&mut child_jobs));
        }
    }

    // The last (usually the only) batch says that the directory has been read.
    send(nodes, exts, listing.error, false);
    enqueue(frontier, child_jobs);
}

/// The filesystems that are a view into the kernel and not storage: ncdu's list for
/// `--exclude-kernfs`, by the names the kernel gives them in the mount table.
#[cfg(any(target_os = "linux", test))]
const KERNFS_TYPES: &[&str] = &[
    "binfmt_misc",
    "bpf",
    "cgroup",
    "cgroup2",
    "debugfs",
    "devpts",
    "proc",
    "pstore",
    "securityfs",
    "selinuxfs",
    "sysfs",
    "tracefs",
];

/// The device numbers of the kernel filesystems in a mount table of the form of
/// `/proc/self/mountinfo`:
///
/// ```text
/// 23 30 0:22 / /proc rw,nosuid,nodev,noexec,relatime shared:13 - proc proc rw
/// ```
///
/// The third field is the device (`major:minor`), and the filesystem type is the first
/// field after the lone dash. Lines that do not look like that are passed over.
///
/// ncdu asks the kernel about every directory instead (`statfs`), which `std` cannot do.
/// A directory is on one of these filesystems exactly if it has one of these devices, so the
/// scanner, which knows every entry's device anyway, only has to look it up here.
#[cfg(any(target_os = "linux", test))]
fn kernfs_devices(mountinfo: &str) -> HashSet<u64> {
    mountinfo
        .lines()
        .filter_map(|line| {
            let (mount, filesystem) = line.split_once(" - ")?;
            let fstype = filesystem.split(' ').next()?;
            if !KERNFS_TYPES.contains(&fstype) {
                return None;
            }
            let (major, minor) = mount.split(' ').nth(2)?.split_once(':')?;
            Some(makedev(major.parse().ok()?, minor.parse().ok()?))
        })
        .collect()
}

/// A device number as `st_dev` has it on Linux, from its two parts.
#[cfg(any(target_os = "linux", test))]
fn makedev(major: u64, minor: u64) -> u64 {
    ((major & 0xffff_f000) << 32)
        | ((major & 0x0000_0fff) << 8)
        | ((minor & 0xffff_ff00) << 12)
        | (minor & 0x0000_00ff)
}

/// The devices of the kernel filesystems that are mounted right now. Empty where there is no
/// mount table to read: on other systems than Linux, and without `/proc`.
#[cfg(target_os = "linux")]
fn mounted_kernfs() -> HashSet<u64> {
    std::fs::read_to_string("/proc/self/mountinfo")
        .map(|table| kernfs_devices(&table))
        .unwrap_or_default()
}

#[cfg(not(target_os = "linux"))]
fn mounted_kernfs() -> HashSet<u64> {
    HashSet::new()
}

/// How a `CACHEDIR.TAG` has to begin to mark its directory as a cache, by the Cache Directory
/// Tagging Specification (<https://bford.info/cachedir/>).
const CACHEDIR_TAG_SIGNATURE: &[u8; 43] = b"Signature: 8a477f597d28d172789f06886806bc55";

/// True if `dir` holds a `CACHEDIR.TAG` that begins with the signature.
///
/// Like ncdu this follows a symlink of that name. Unlike ncdu it opens nothing but a regular
/// file: opening a fifo called `CACHEDIR.TAG` would stop the scan until someone writes to it.
fn has_cachedir_tag(dir: &Path) -> bool {
    use std::io::Read;
    let tag = dir.join("CACHEDIR.TAG");
    if !std::fs::metadata(&tag).is_ok_and(|meta| meta.is_file()) {
        return false;
    }
    let mut start = [0u8; CACHEDIR_TAG_SIGNATURE.len()];
    std::fs::File::open(&tag)
        .and_then(|mut file| file.read_exact(&mut start))
        .is_ok()
        && start == *CACHEDIR_TAG_SIGNATURE
}

/// The extended information (`-e`) in what `lstat` returned.
pub fn ext_of(meta: &std::fs::Metadata) -> Ext {
    Ext {
        // The dump format has no negative times; one before 1970 becomes 1970.
        mtime: u64::try_from(meta.mtime()).unwrap_or(0),
        uid: meta.uid(),
        gid: meta.gid(),
        // The file type and the permission bits are the low 16 bits everywhere.
        mode: meta.mode() as u16,
        has: Ext::ALL,
    }
}

/// Queue the subdirectories found, and wake idle workers. Always called AFTER the batch that
/// holds those subdirectories was sent: a node's creation then reaches the UI before any
/// batch of its descendants, so the tree is grafted parent-first. The readdir/stat work runs
/// outside the lock; only queueing takes it.
fn enqueue(frontier: &FrontierArc, child_jobs: Vec<DirJob>) {
    if child_jobs.is_empty() {
        return;
    }
    let (lock, cv) = &**frontier;
    let mut f = lock.lock().unwrap();
    f.pending += child_jobs.len();
    for cj in child_jobs {
        f.push(cj);
    }
    cv.notify_all();
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
            extended: false,
            exclude_caches: false,
            exclude_kernfs: false,
            follow_symlinks: false,
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

    /// Without priorities the oldest job is taken first: a breadth-first walk. Every entry
    /// of a directory is read before any of their subdirectories.
    #[test]
    fn frontier_reads_breadth_first() {
        let mut f = frontier_with(&["/r/a", "/r/b"]);
        assert_eq!(f.pop().map(|j| j.id), Some(1), "/r/a, the oldest");
        // Reading /r/a found two subdirectories. /r/b comes before them.
        f.push(job(10, "/r/a/x"));
        f.push(job(11, "/r/a/y"));
        assert_eq!(drain(&mut f), vec![2, 10, 11]);
    }

    /// A directory the user is waiting for is read depth-first: its newest job first, so its
    /// subtrees are finished one after the other.
    #[test]
    fn frontier_reads_the_first_class_depth_first() {
        let mut f = frontier_with(&["/r/a"]);
        f.set_priorities(Some("/r/wait".into()), None);
        f.push(job(10, "/r/wait/x"));
        f.push(job(11, "/r/wait/y"));
        assert_eq!(f.pop().map(|j| j.id), Some(11), "the newest");
        f.push(job(12, "/r/wait/y/deep"));
        assert_eq!(drain(&mut f), vec![12, 10, 1], "then the rest of the tree");
    }

    /// Jobs under the focused directory come before the rest, and jobs under the directory
    /// to finish first come before those, wherever that directory lies.
    #[test]
    fn frontier_serves_the_classes_in_order() {
        let paths = ["/r/A/x", "/r/B/y", "/r/A/z", "/r/C", "/r/B/q/deep"];
        let mut f = frontier_with(&paths);
        f.set_priorities(None, Some("/r/B".into()));
        assert_eq!(drain(&mut f), vec![2, 5, 1, 3, 4], "B first, then the rest");

        // A directory to finish first inside the focus...
        let mut f = frontier_with(&paths);
        f.set_priorities(Some("/r/B/q".into()), Some("/r/B".into()));
        assert_eq!(drain(&mut f), vec![5, 2, 1, 3, 4]);
        // ...and one outside it, which still wins.
        let mut f = frontier_with(&paths);
        f.set_priorities(Some("/r/C".into()), Some("/r/B".into()));
        assert_eq!(drain(&mut f), vec![4, 2, 5, 1, 3]);

        // New jobs are classified as they arrive, behind the older ones of their class.
        let mut f = frontier_with(&["/r/A/x"]);
        f.set_priorities(None, Some("/r/B".into()));
        f.push(job(7, "/r/B/new"));
        f.push(job(8, "/r/A/new"));
        assert_eq!(drain(&mut f), vec![7, 1, 8]);

        // A path is under a directory by components, not by prefix of its text.
        let mut f = frontier_with(&["/r/Bee", "/r/B"]);
        f.set_priorities(None, Some("/r/B".into()));
        assert_eq!(drain(&mut f), vec![2, 1]);
    }

    /// Jobs that are still classified against earlier priorities.
    fn stale_jobs(f: &Frontier) -> usize {
        f.stale.iter().map(Queue::len).sum()
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
        assert_eq!(stale_jobs(&f), total, "set aside, not classified");
        assert_eq!(f.queued(), total);

        // One pop classifies one chunk and serves a focused job from it.
        let first = f.pop().unwrap();
        assert!(first.path.starts_with("/r/hot"));
        assert_eq!(stale_jobs(&f), total - SORT_CHUNK);

        // The priorities change again while most jobs are still stale.
        f.set_priorities(None, Some("/r/cold".into()));
        assert_eq!(f.queued(), total - 1);
        let mut seen = vec![first.id];
        seen.extend(drain(&mut f));
        seen.sort_unstable();
        assert_eq!(seen, (1..=total as u64).collect::<Vec<_>>());
        assert_eq!(f.queued(), 0);
        assert!(f.stale.is_empty());

        // Setting the same priorities again is free: nothing is set aside.
        let mut f = frontier_with(&["/r/hot/a", "/r/cold/b"]);
        f.set_priorities(None, Some("/r/hot".into()));
        drop(f.pop());
        f.set_priorities(None, Some("/r/hot".into()));
        assert!(f.stale.is_empty());
    }

    /// Classifying late keeps the order: jobs that were queued earlier are still read
    /// earlier, and once everything is classified nothing from the rest is served while a
    /// focused job waits.
    #[test]
    fn frontier_order_survives_a_change_of_priorities() {
        let total = SORT_CHUNK * 3;
        // The five focused jobs are the last ones queued, so they are classified last.
        let paths: Vec<String> = (0..total)
            .map(|i| format!("/r/{}/d{i}", if i >= total - 5 { "hot" } else { "cold" }))
            .collect();
        let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
        let mut f = frontier_with(&refs);
        f.set_priorities(None, Some("/r/hot".into()));
        let order = drain(&mut f);
        let hot: Vec<u64> = (total as u64 - 4..=total as u64).collect();
        // Two jobs of the rest went ahead, one per chunk classified before the focused jobs
        // were found; from then on the focused ones come first, oldest first.
        assert_eq!(order[..2], [1, 2]);
        assert_eq!(order[2..7], hot[..]);
        // The rest follows in the order it was queued.
        let rest: Vec<u64> = (3..=total as u64 - 5).collect();
        assert_eq!(order[7..], rest[..]);
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
            .stale
            .iter()
            .flat_map(|queue| queue.late.iter().chain(&queue.fresh))
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

    /// A directory with more entries than fit in a batch arrives in parts: every part but
    /// the last says that more follows, none exceeds the limit, and together they hold every
    /// entry once. Subdirectories found in an early part are read without waiting for the
    /// rest of the listing.
    #[test]
    fn huge_directory_arrives_in_parts() {
        let base = std::env::temp_dir().join(format!("icdu_parts_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let big = base.join("big");
        fs::create_dir_all(&big).unwrap();
        let files = BATCH_ENTRIES * 2 + 500;
        for i in 0..files {
            fs::write(big.join(format!("f{i:05}")), b"x").unwrap();
        }
        for d in 0..3 {
            fs::create_dir(big.join(format!("sub{d}"))).unwrap();
            fs::write(big.join(format!("sub{d}/inside")), b"x").unwrap();
        }
        let entries = files + 3;

        let meta = fs::symlink_metadata(&base).unwrap();
        let batches: Vec<Batch> = start(base.clone(), opts(2, meta.dev()))
            .events
            .iter()
            .collect();
        // The id of `big` comes with the root's batch.
        let big_id = batches
            .iter()
            .find(|b| b.parent_id == 0)
            .and_then(|b| b.nodes[0].dir_id)
            .unwrap();
        let parts: Vec<&Batch> = batches.iter().filter(|b| b.parent_id == big_id).collect();
        assert_eq!(parts.len(), 3, "two full parts and the remainder");
        assert!(parts.iter().all(|b| b.nodes.len() <= BATCH_ENTRIES));
        let more: Vec<bool> = parts.iter().map(|b| b.more).collect();
        assert_eq!(more, vec![true, true, false]);
        assert_eq!(parts.iter().map(|b| b.nodes.len()).sum::<usize>(), entries);
        let mut names: Vec<&str> = parts
            .iter()
            .flat_map(|b| b.nodes.iter().map(|n| n.name.as_str()))
            .collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), entries, "every entry exactly once");
        // Every other directory still arrives in one batch.
        assert!(batches
            .iter()
            .filter(|b| b.parent_id != big_id)
            .all(|b| !b.more));

        // In the tree: complete, with the right counts.
        let tree = scan_tree(&base, 4);
        assert!(tree.subtree_complete(tree.root));
        assert_eq!(tree.nodes[tree.root].items as usize, 1 + entries + 3);
        assert_eq!(tree.total_files as usize, files + 3);
        assert_eq!(tree.total_dirs, 1 + 1 + 3);
        fs::remove_dir_all(&base).unwrap();
    }

    /// The gate lets entries through up to its limit and makes the sender wait beyond it,
    /// until the receiver has taken some.
    #[test]
    fn gate_holds_senders_back_until_there_is_room() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::Duration;
        let gate = Arc::new(Gate::new(10));
        gate.enter(6);
        gate.enter(4); // exactly full: still no wait
        let through = Arc::new(AtomicBool::new(false));
        let sender = {
            let (gate, through) = (Arc::clone(&gate), Arc::clone(&through));
            thread::spawn(move || {
                gate.enter(3);
                through.store(true, Ordering::SeqCst);
            })
        };
        thread::sleep(Duration::from_millis(50));
        assert!(!through.load(Ordering::SeqCst), "over the limit: it waits");
        gate.leave(2);
        thread::sleep(Duration::from_millis(50));
        assert!(!through.load(Ordering::SeqCst), "8 + 3 is still too many");
        gate.leave(4);
        sender.join().unwrap();
        assert!(through.load(Ordering::SeqCst));
        assert_eq!(gate.state.lock().unwrap().queued, 7);
    }

    /// A batch larger than the limit goes through when nothing else is queued (otherwise it
    /// could never be sent), and a closed gate holds nobody.
    #[test]
    fn gate_never_blocks_forever() {
        let gate = Gate::new(10);
        gate.enter(1_000); // nothing queued: goes through
        gate.leave(1_000);

        let gate = Arc::new(Gate::new(10));
        gate.enter(10);
        let sender = {
            let gate = Arc::clone(&gate);
            thread::spawn(move || gate.enter(5))
        };
        thread::sleep(std::time::Duration::from_millis(30));
        gate.close(); // the receiver is gone
        sender.join().unwrap();
    }

    /// While nobody takes batches, a scan stops producing once the limit is reached: memory
    /// stays bounded however long the UI is busy elsewhere. Taking them lets it finish.
    #[test]
    fn scan_waits_when_its_batches_are_not_taken() {
        let base = std::env::temp_dir().join(format!("icdu_gate_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        for d in 0..200 {
            let dir = base.join(format!("d{d:03}"));
            fs::create_dir_all(&dir).unwrap();
            for f in 0..20 {
                fs::write(dir.join(format!("f{f}")), b"x").unwrap();
            }
        }
        let total = 200 + 200 * 20;
        let limit = 300;
        let meta = fs::symlink_metadata(&base).unwrap();
        let scan = start_limited(base.clone(), opts(4, meta.dev()), 0, None, limit);

        // Leave it alone until it has sent something, and then a while longer. It cannot
        // have got far.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while scan.events.rx.is_empty() {
            assert!(
                std::time::Instant::now() < deadline,
                "the scan sent nothing"
            );
            thread::sleep(std::time::Duration::from_millis(2));
        }
        thread::sleep(std::time::Duration::from_millis(300));
        let waiting = scan.events.rx.len();
        let queued = scan
            .events
            .gate
            .as_ref()
            .unwrap()
            .state
            .lock()
            .unwrap()
            .queued;
        // The root's batch (200 entries) may go through alone; after it, at most the limit.
        assert!(queued <= limit + 200, "{queued} entries queued");
        assert!(queued < total, "the scan is waiting, not finished");
        assert!(waiting > 0);

        // Now take everything: the scan runs to its end and nothing is lost.
        let received: usize = scan.events.iter().map(|b| b.nodes.len()).sum();
        assert_eq!(received, total);
        fs::remove_dir_all(&base).unwrap();
    }

    /// A consumer that goes away does not leave the workers waiting at the gate.
    #[test]
    fn dropping_the_events_releases_the_workers() {
        let base = std::env::temp_dir().join(format!("icdu_drop_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        for d in 0..50 {
            let dir = base.join(format!("d{d:02}"));
            fs::create_dir_all(&dir).unwrap();
            for f in 0..10 {
                fs::write(dir.join(format!("f{f}")), b"x").unwrap();
            }
        }
        let meta = fs::symlink_metadata(&base).unwrap();
        let scan = start_limited(base.clone(), opts(4, meta.dev()), 0, None, 20);
        let control = scan.control;
        drop(scan.events);
        // With nobody receiving, the workers must still drain the frontier and stop.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if control.frontier.0.lock().unwrap().done {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "workers are stuck");
            thread::sleep(std::time::Duration::from_millis(5));
        }
        fs::remove_dir_all(&base).unwrap();
    }

    /// The scan records the link count of entries that have more than one name, so that a
    /// dump can carry it (`nlink`), and writes such a dump the way ncdu does.
    #[test]
    fn link_counts_are_recorded_and_exported() {
        let base = std::env::temp_dir().join(format!("icdu_nlink_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("sub")).unwrap();
        fs::write(base.join("one"), b"12345").unwrap();
        fs::hard_link(base.join("one"), base.join("sub/two")).unwrap();
        fs::hard_link(base.join("one"), base.join("sub/three")).unwrap();
        fs::write(base.join("plain"), b"x").unwrap();
        std::os::unix::fs::symlink("plain", base.join("link")).unwrap();

        let tree = scan_tree(&base, 2);
        let named = |name: &str| tree.nodes.iter().position(|n| n.name == name).unwrap();
        for name in ["one", "two", "three"] {
            assert!(tree.nodes[named(name)].hlink, "{name}");
            assert_eq!(tree.nlink(named(name)), Some(3), "{name}");
        }
        assert!(!tree.nodes[named("plain")].hlink);
        assert_eq!(tree.nlink(named("plain")), None);
        // A directory has several links too, but they are not hard links to a file.
        assert!(!tree.nodes[named("sub")].hlink);
        assert_eq!(tree.nlink(named("sub")), None);

        let mut out = Vec::new();
        crate::dump::export(&tree, &mut out).unwrap();
        let dump = String::from_utf8(out).unwrap();
        let ino = tree.nodes[named("one")].ino;
        for name in ["one", "two", "three"] {
            let entry = format!(
                r#"{{"name":"{name}","asize":5,"dsize":4096,"ino":{ino},"hlnkc":true,"nlink":3}}"#
            );
            // The block size is the filesystem's; only compare where it is the usual 4096.
            if tree.own_disk(named(name)) == 4096 {
                assert!(dump.contains(&entry), "{entry}\nnot in\n{dump}");
            }
        }
        assert!(dump.contains(r#"{"name":"plain","asize":1,"#), "{dump}");
        assert!(!dump.contains(&format!(
            r#""name":"plain","asize":1,"dsize":{},"ino""#,
            4096
        )));
        assert!(dump.contains(r#""name":"link","asize":5,"#) && dump.contains(r#""notreg":true"#));
        assert_eq!(
            dump.matches("\"dev\":").count(),
            1,
            "only the root says its device"
        );
        fs::remove_dir_all(&base).unwrap();
    }

    /// `--exclude` patterns against a real tree, with the results ncdu 1.18 gives for the
    /// same tree and patterns (recorded with `ncdu -o - --exclude PATTERN`). A pattern is
    /// tried against the whole path of an entry and against every tail of it, and what is
    /// excluded is not entered, so nothing below it is listed.
    #[test]
    fn excludes_match_paths_like_ncdu() {
        let parent = std::env::temp_dir().join(format!("icdu_excl_paths_{}", std::process::id()));
        let _ = fs::remove_dir_all(&parent);
        // The name of the scanned directory is part of every path, so it is fixed.
        let base = parent.join("excl");
        for dir in ["a/sub/deep", "b/sub", "sub", "a/other", "pad dir", "c"] {
            fs::create_dir_all(base.join(dir)).unwrap();
        }
        for file in [
            "a/sub/deep/f1",
            "a/sub/f2",
            "b/sub/f3",
            "sub/f4",
            "a/other/f5",
            "a/x.o",
            "b/y.o",
            "c/file1",
            "c/file2",
            "c/File3",
            "pad dir/f6",
            "#hash",
            " lead",
            "trail ",
        ] {
            fs::write(base.join(file), b"x").unwrap();
        }
        let root = base.to_string_lossy().to_string();
        let absolute_a = format!("{root}/a");
        let absolute_sub = format!("{root}/a/sub");
        let table: &[(&str, &[&str])] = &[
            ("sub", &["a/sub", "b/sub", "sub"]),
            ("a/sub", &["a/sub"]),
            ("*/sub", &["a/sub", "b/sub", "sub"]),
            ("sub/*", &["a/sub/deep", "a/sub/f2", "b/sub/f3", "sub/f4"]),
            ("a/*", &["a/other", "a/sub", "a/x.o"]),
            ("*.o", &["a/x.o", "b/y.o"]),
            ("a/*.o", &["a/x.o"]),
            (&absolute_a, &["a"]),
            (&absolute_sub, &["a/sub"]),
            // A leading slash is the root of the filesystem, not of the scan.
            ("/a", &[]),
            ("file[12]", &["c/file1", "c/file2"]),
            ("file[!1]", &["c/file2"]),
            ("[Ff]ile?", &["c/File3", "c/file1", "c/file2"]),
            ("file[[:digit:]]", &["c/file1", "c/file2"]),
            ("f?le*", &["c/file1", "c/file2"]),
            ("deep", &["a/sub/deep"]),
            ("sub/deep", &["a/sub/deep"]),
            ("b*", &["b"]),
            // `a/sub` and `sub` because a star also takes slashes: "…/a/su" + "b".
            ("*b", &["a/sub", "b", "sub"]),
            ("a/sub/deep/f1", &["a/sub/deep/f1"]),
            // The scanned directory's own name is part of the path.
            ("excl/a", &["a"]),
            ("pad dir", &["pad dir"]),
            ("pad*", &["pad dir"]),
            ("su", &[]),
            (r"a\/sub", &["a/sub"]),
            (r"fil\e1", &["c/file1"]),
            ("[a-c]", &["a", "b", "c"]),
            (
                "*",
                &[" lead", "#hash", "a", "b", "c", "pad dir", "sub", "trail "],
            ),
            // No entry's path ends in a slash.
            ("c/", &[]),
            // The scanned directory itself is never excluded.
            ("excl", &[]),
        ];
        let meta = fs::symlink_metadata(&base).unwrap();
        for (pattern, expected) in table {
            let mut o = opts(4, meta.dev());
            o.excludes = vec![pattern.to_string()];
            let mut tree = Tree::new(
                root.clone().into(),
                meta.len(),
                meta.blocks() * 512,
                meta.dev(),
                meta.ino(),
            );
            for batch in start(base.clone(), o).events.iter() {
                tree.apply(batch);
            }
            let mut excluded: Vec<String> = (0..tree.nodes.len())
                .filter(|&i| tree.nodes[i].excluded == Excluded::Pattern)
                .map(|i| tree.rel_path_from(i, tree.root))
                .collect();
            excluded.sort();
            assert_eq!(excluded, *expected, "--exclude {pattern:?}");
        }
        fs::remove_dir_all(&parent).unwrap();
    }

    /// With `-e` the scan delivers owner, group, mode and modification time of every entry,
    /// and the tree keeps them; a directory then knows the latest change below it.
    #[test]
    fn extended_scan_records_owner_mode_and_time() {
        use std::os::unix::fs::PermissionsExt;
        use std::time::{Duration, UNIX_EPOCH};
        let base = std::env::temp_dir().join(format!("icdu_extended_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("sub")).unwrap();
        fs::write(base.join("f"), b"12345").unwrap();
        fs::write(base.join("sub/g"), b"1").unwrap();
        std::os::unix::fs::symlink("f", base.join("link")).unwrap();
        fs::set_permissions(base.join("f"), fs::Permissions::from_mode(0o640)).unwrap();
        let touch = |path: &str, secs: u64| {
            fs::File::open(base.join(path))
                .unwrap()
                .set_modified(UNIX_EPOCH + Duration::from_secs(secs))
                .unwrap();
        };
        touch("f", 1_000_000_000);
        touch("sub/g", 1_500_000_000);
        // The directory itself is older than what is in it.
        touch("sub", 1_200_000_000);

        let scan = |extended: bool| {
            let meta = fs::symlink_metadata(&base).unwrap();
            let mut o = opts(2, meta.dev());
            o.extended = extended;
            let mut tree = Tree::new(
                base.to_string_lossy().into(),
                meta.len(),
                meta.blocks() * 512,
                meta.dev(),
                meta.ino(),
            );
            if extended {
                tree.set_extended();
                tree.set_ext(tree.root, ext_of(&meta));
            }
            for batch in start(base.clone(), o).events.iter() {
                tree.apply(batch);
            }
            tree
        };
        let tree = scan(true);
        let named = |name: &str| tree.nodes.iter().position(|n| n.name == name).unwrap();
        // Every entry has what `lstat` says about it.
        for (name, path) in [("f", "f"), ("g", "sub/g"), ("sub", "sub"), ("link", "link")] {
            let on_disk = ext_of(&fs::symlink_metadata(base.join(path)).unwrap());
            assert_eq!(tree.ext(named(name)), Some(on_disk), "{name}");
        }
        let f = tree.ext(named("f")).unwrap();
        assert_eq!(f.mode, 0o100640);
        assert_eq!(f.mtime, 1_000_000_000);
        assert_eq!(tree.ext(named("link")).unwrap().mode & 0o170000, 0o120000);
        assert_eq!(tree.ext(named("sub")).unwrap().mtime, 1_200_000_000);
        assert_eq!(tree.latest_mtime(named("sub")), Some(1_500_000_000));
        assert!(tree.latest_mtime(tree.root) >= Some(1_500_000_000));

        // And the dump has the four fields, between the device and the inode as in ncdu.
        let mut out = Vec::new();
        crate::dump::export(&tree, &mut out).unwrap();
        let dump = String::from_utf8(out).unwrap();
        let fields = format!(
            r#""uid":{},"gid":{},"mode":33184,"mtime":1000000000}}"#,
            f.uid, f.gid
        );
        assert!(dump.contains(&fields), "{fields}\nnot in\n{dump}");

        // Without the flag nothing of it is kept or written.
        let plain = scan(false);
        assert!(!plain.is_extended());
        assert_eq!(plain.ext(named("f")), None);
        let mut out = Vec::new();
        crate::dump::export(&plain, &mut out).unwrap();
        assert!(!String::from_utf8(out).unwrap().contains("\"mtime\""));
        fs::remove_dir_all(&base).unwrap();
    }

    /// `--exclude-caches`: a directory with a valid `CACHEDIR.TAG` is listed, marked as
    /// excluded, not entered and not counted. The cases and their results are what ncdu 1.18
    /// does with the same tree.
    #[test]
    fn cache_directories_are_left_out_on_request() {
        let base = std::env::temp_dir().join(format!("icdu_caches_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let signature = "Signature: 8a477f597d28d172789f06886806bc55";
        let tags: &[(&str, &str, &str)] = &[
            // The usual form: the signature, then a comment.
            (
                "valid",
                "CACHEDIR.TAG",
                "Signature: 8a477f597d28d172789f06886806bc55\n# cache\n",
            ),
            // Exactly the signature and nothing after it.
            ("exact", "CACHEDIR.TAG", signature),
            ("nested/inner", "CACHEDIR.TAG", signature),
            // Not tags: another signature, one byte short, and (where case matters) a name
            // in lower case.
            (
                "invalid",
                "CACHEDIR.TAG",
                "Signature: 00000000000000000000000000000000\n",
            ),
            ("short", "CACHEDIR.TAG", &signature[..42]),
            ("lower", "cachedir.tag", signature),
        ];
        for (dir, name, content) in tags {
            fs::create_dir_all(base.join(dir)).unwrap();
            fs::write(base.join(dir).join(name), content).unwrap();
            fs::write(base.join(dir).join("data"), vec![b'x'; 5000]).unwrap();
        }
        // A tag that is a symlink to a real one counts, one that is a directory does not.
        fs::create_dir_all(base.join("symlinked")).unwrap();
        std::os::unix::fs::symlink("../valid/CACHEDIR.TAG", base.join("symlinked/CACHEDIR.TAG"))
            .unwrap();
        fs::create_dir_all(base.join("dirtag/CACHEDIR.TAG")).unwrap();
        fs::create_dir_all(base.join("valid/deep")).unwrap();
        fs::write(base.join("valid/deep/f"), b"y").unwrap();

        let scan = |root: &Path, exclude_caches: bool| {
            let meta = fs::symlink_metadata(root).unwrap();
            let mut o = opts(2, meta.dev());
            o.exclude_caches = exclude_caches;
            let mut tree = Tree::new(
                root.to_string_lossy().into(),
                meta.len(),
                meta.blocks() * 512,
                meta.dev(),
                meta.ino(),
            );
            for batch in start(root.to_path_buf(), o).events.iter() {
                tree.apply(batch);
            }
            tree
        };
        let excluded = |tree: &Tree| {
            let mut names: Vec<String> = (0..tree.nodes.len())
                .filter(|&i| tree.nodes[i].excluded != Excluded::No)
                .map(|i| tree.rel_path_from(i, tree.root))
                .collect();
            names.sort();
            names
        };

        // A filesystem that ignores case (the usual one on macOS) finds `cachedir.tag` under
        // the name the scan asks for, so there that directory is tagged too.
        let folds_case = base.join("lower/CACHEDIR.TAG").exists();
        let mut tagged = vec!["exact", "nested/inner", "symlinked", "valid"];
        if folds_case {
            tagged.insert(1, "lower");
        }
        let with = scan(&base, true);
        assert_eq!(excluded(&with), tagged);
        let named = |tree: &Tree, name: &str| tree.nodes.iter().position(|n| n.name == name);
        let valid = named(&with, "valid").unwrap();
        assert_eq!(with.nodes[valid].excluded, Excluded::Pattern);
        assert!(with.nodes[valid].children.is_empty(), "not entered");
        assert_eq!(named(&with, "deep"), None);
        assert!(
            with.subtree_complete(with.root),
            "and nothing is left to read"
        );

        // Without the option the same tree is read in full, and is larger by what the four
        // directories hold.
        let without = scan(&base, false);
        assert_eq!(excluded(&without), [] as [&str; 0]);
        assert!(named(&without, "deep").is_some());
        let apparent = |tree: &Tree| tree.nodes[tree.root].apparent;
        assert!(
            apparent(&without) >= apparent(&with) + 4 * 5000,
            "{} vs {}",
            apparent(&without),
            apparent(&with)
        );

        // The directory the scan starts at is read even if it is tagged itself.
        let inside = scan(&base.join("valid"), true);
        assert_eq!(excluded(&inside), [] as [&str; 0]);
        assert!(named(&inside, "deep").is_some());
        fs::remove_dir_all(&base).unwrap();
    }

    /// A mount table as the kernel prints it: the kernel filesystems are picked out by their
    /// type, whatever they are mounted on, and their device is the third field.
    #[test]
    fn kernfs_devices_are_read_from_the_mount_table() {
        let table = "\
22 29 0:21 / /sys rw,nosuid,nodev,noexec,relatime shared:7 - sysfs sysfs rw
23 29 0:22 / /proc rw,nosuid,nodev,noexec,relatime shared:13 - proc proc rw
24 29 0:5 / /dev rw,nosuid,relatime shared:2 - devtmpfs udev rw,size=65842540k,mode=755
25 24 0:23 / /dev/pts rw,nosuid,noexec,relatime shared:3 - devpts devpts rw,gid=5,mode=620
29 1 8:2 / / rw,relatime shared:1 - ext4 /dev/sda2 rw,errors=remount-ro
31 22 0:26 / /sys/fs/cgroup rw,nosuid,nodev,noexec,relatime shared:9 - cgroup2 cgroup2 rw
33 22 0:27 / /sys/fs/bpf rw,nosuid,nodev,noexec,relatime shared:11 - bpf bpf rw,mode=700
34 23 0:28 / /proc/sys/fs/binfmt_misc rw,relatime shared:14 - autofs systemd-1 rw,fd=29
35 24 0:19 / /dev/mqueue rw,nosuid,nodev,noexec,relatime shared:15 - mqueue mqueue rw
36 22 0:7 / /sys/kernel/debug rw,nosuid,nodev,noexec,relatime shared:16 - debugfs debugfs rw
37 22 0:12 / /sys/kernel/tracing rw,nosuid,nodev,noexec,relatime shared:17 - tracefs tracefs rw
40 29 0:300 / /mnt/a\\040-\\040b rw,relatime shared:20 master:1 - proc proc rw
41 29 0:41 / /mnt/data rw,relatime shared:21 - zfs tank/data rw,xattr,posixacl
42 34 0:42 / /proc/sys/fs/binfmt_misc rw,relatime shared:22 - binfmt_misc binfmt_misc rw
43 22 0:43 / /sys/fs/pstore rw,relatime - pstore pstore rw
44 22 0:44 / /sys/kernel/security rw,relatime - securityfs securityfs rw
45 22 0:45 / /sys/fs/selinux rw,relatime - selinuxfs selinuxfs rw
46 22 0:46 / /sys/fs/cgroup/systemd rw,relatime - cgroup cgroup rw,name=systemd
not a line of the table
47 22 x:y / /broken rw - proc proc rw
";
        let minors = |devices: &HashSet<u64>| {
            let mut minors: Vec<u64> = devices.iter().copied().collect();
            minors.sort();
            minors
        };
        // With major 0 and a minor below 256 the device number is the minor; 300 is the one
        // that shows how a larger minor is spread.
        assert_eq!(
            minors(&kernfs_devices(table)),
            [
                7,
                12,
                21,
                22,
                23,
                26,
                27,
                42,
                43,
                44,
                45,
                46,
                makedev(0, 300)
            ]
        );
        assert!(kernfs_devices("").is_empty());
    }

    /// Major and minor number make up `st_dev` the way the C library does it (the expected
    /// values are those of Python's `os.makedev`).
    #[test]
    fn makedev_matches_the_c_library() {
        assert_eq!(makedev(0, 22), 22);
        assert_eq!(makedev(8, 2), 0x802);
        assert_eq!(makedev(253, 1), 0xfd01);
        assert_eq!(makedev(0, 300), 0x10002c);
        assert_eq!(makedev(0x12345, 0x6789a), 0x0001_2000_6783_459a);
    }

    /// On Linux, with the option: `/proc` itself is read when it is what the scan was asked
    /// for, and every directory in it is left out as a kernel filesystem, like ncdu does it.
    /// Without the option they are entered.
    #[cfg(target_os = "linux")]
    #[test]
    fn kernel_filesystems_are_left_out_on_request() {
        let proc_dir = Path::new("/proc");
        let Ok(meta) = fs::metadata(proc_dir) else {
            return; // no /proc here
        };
        // The device numbers computed from the mount table are those `stat` reports.
        assert!(mounted_kernfs().contains(&meta.dev()), "{:?}", meta.dev());

        let mut o = opts(2, meta.dev());
        o.exclude_kernfs = true;
        let mut tree = Tree::new("/proc".into(), 0, 0, meta.dev(), meta.ino());
        for batch in start(proc_dir.to_path_buf(), o).events.iter() {
            tree.apply(batch);
        }
        let dirs: Vec<NodeIdx> = (1..tree.nodes.len())
            .filter(|&i| tree.nodes[i].is_dir())
            .collect();
        assert!(dirs.len() > 3, "/proc has directories: {}", dirs.len());
        for &dir in &dirs {
            let n = &tree.nodes[dir];
            assert_eq!(n.excluded, Excluded::Kernfs, "{}", n.name);
            assert!(n.children.is_empty(), "{} was entered", n.name);
        }
        // Everything listed is directly in /proc: nothing below it was read.
        assert!(tree.nodes[1..].iter().all(|n| n.parent == Some(tree.root)));
        assert!(tree.subtree_complete(tree.root));
    }

    /// `-L`: a symlink to a file is counted as that file, with the file's type, sizes, device
    /// and inode, and each such link counts on its own. A link to a directory, a broken link
    /// and a loop stay links. These are the results of ncdu 1.18 for the same tree.
    #[test]
    fn follow_symlinks_counts_the_files_that_links_point_to() {
        use std::os::unix::fs::symlink;
        let base = std::env::temp_dir().join(format!("icdu_follow_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("sub")).unwrap();
        fs::create_dir_all(base.join("links")).unwrap();
        fs::write(base.join("big"), vec![0u8; 100_000]).unwrap();
        fs::write(base.join("sub/small"), b"abc").unwrap();
        fs::write(base.join("hl1"), b"hl").unwrap();
        fs::hard_link(base.join("hl1"), base.join("hl2")).unwrap();
        for (target, name) in [
            ("../big", "to-big"),
            ("to-big", "chain"),
            ("../sub", "to-dir"),
            ("nowhere", "broken"),
            ("loop-b", "loop-a"),
            ("loop-a", "loop-b"),
            ("../hl1", "to-hardlinked"),
            ("/dev/null", "to-device"),
        ] {
            symlink(target, base.join("links").join(name)).unwrap();
        }
        let scan = |follow_symlinks: bool, one_file_system: bool| {
            let meta = fs::symlink_metadata(&base).unwrap();
            let mut o = opts(2, meta.dev());
            o.follow_symlinks = follow_symlinks;
            o.one_file_system = one_file_system;
            let mut tree = Tree::new(
                base.to_string_lossy().into(),
                meta.len(),
                meta.blocks() * 512,
                meta.dev(),
                meta.ino(),
            );
            for batch in start(base.clone(), o).events.iter() {
                tree.apply(batch);
            }
            tree
        };
        let named = |tree: &Tree, name: &str| tree.nodes.iter().position(|n| n.name == name);
        let link_itself = |name: &str| fs::symlink_metadata(base.join("links").join(name)).unwrap();
        let identity = |meta: &fs::Metadata| (meta.dev(), meta.ino());
        let big = fs::metadata(base.join("big")).unwrap();

        let followed = scan(true, false);
        // Straight and through another link: the entry is the file.
        for name in ["to-big", "chain"] {
            let i = named(&followed, name).unwrap();
            let n = &followed.nodes[i];
            assert_eq!(n.kind, NodeKind::File, "{name}");
            assert_eq!(
                (n.apparent, n.disk),
                (100_000, big.blocks() * 512),
                "{name}"
            );
            assert_eq!((n.dev, n.ino), identity(&big), "{name}");
            assert!(!n.hlink && !n.shared, "{name}: counted on its own");
            // What a delete has to find there is the link.
            assert_eq!(
                followed.followed_link(i),
                Some(identity(&link_itself(name))),
                "{name}"
            );
        }
        // Not followed: a directory, nothing, a loop.
        for name in ["to-dir", "broken", "loop-a", "loop-b"] {
            let i = named(&followed, name).unwrap();
            let n = &followed.nodes[i];
            assert_eq!(n.kind, NodeKind::Link, "{name}");
            assert_eq!(n.apparent, link_itself(name).len(), "{name}");
            assert_eq!((n.dev, n.ino), identity(&link_itself(name)), "{name}");
            assert_eq!(followed.followed_link(i), None, "{name}");
        }
        let smalls = followed.nodes.iter().filter(|n| n.name == "small").count();
        assert_eq!(smalls, 1, "the linked directory was not entered");
        // A link to a file with several names is one more name of that inode: of the three,
        // one is counted.
        let names = ["hl1", "hl2", "to-hardlinked"].map(|name| named(&followed, name).unwrap());
        assert!(names.iter().all(|&i| followed.nodes[i].hlink));
        let counted = names.iter().filter(|&&i| !followed.nodes[i].shared);
        assert_eq!(counted.count(), 1);
        assert_eq!(followed.nlink(names[2]), Some(2));
        // A special file: the entry is that, and on its device.
        let device = named(&followed, "to-device").unwrap();
        let null = fs::metadata("/dev/null").unwrap();
        assert_eq!(followed.nodes[device].kind, NodeKind::Other);
        assert_eq!(followed.nodes[device].dev, null.dev());
        assert_eq!(followed.nodes[device].excluded, Excluded::No);

        // Without the option every one of them is the link, and nothing is noted.
        let plain = scan(false, false);
        for name in [
            "to-big",
            "chain",
            "to-dir",
            "broken",
            "to-hardlinked",
            "to-device",
        ] {
            let i = named(&plain, name).unwrap();
            assert_eq!(plain.nodes[i].kind, NodeKind::Link, "{name}");
            assert_eq!(plain.nodes[i].apparent, link_itself(name).len(), "{name}");
            assert_eq!(plain.followed_link(i), None, "{name}");
        }
        // The difference in the total: twice the big file instead of the two links to it.
        // The hard-linked file is counted once either way, and /dev/null is empty; of those
        // two only the links' own sizes go.
        let total = |tree: &Tree| tree.nodes[tree.root].apparent;
        let gone: u64 = ["to-big", "chain", "to-hardlinked", "to-device"]
            .iter()
            .map(|name| link_itself(name).len())
            .sum();
        assert_eq!(total(&followed), total(&plain) + 2 * 100_000 - gone);

        // With `-x` a file that a link leads to on another filesystem is left out, like a
        // directory there. The others are on this one.
        if null.dev() != big.dev() {
            let one_fs = scan(true, true);
            let device = named(&one_fs, "to-device").unwrap();
            assert_eq!(one_fs.nodes[device].excluded, Excluded::OtherFs);
            let to_big = named(&one_fs, "to-big").unwrap();
            assert_eq!(one_fs.nodes[to_big].excluded, Excluded::No);
            assert_eq!(one_fs.nodes[to_big].apparent, 100_000);
        }
        fs::remove_dir_all(&base).unwrap();
    }

    /// What a pattern excludes is not looked at, so a link among it is not followed either.
    #[test]
    fn an_excluded_symlink_is_not_followed() {
        use std::os::unix::fs::symlink;
        let base = std::env::temp_dir().join(format!("icdu_follow_excl_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        fs::write(base.join("big"), vec![0u8; 50_000]).unwrap();
        symlink("big", base.join("skipped")).unwrap();
        let meta = fs::symlink_metadata(&base).unwrap();
        let mut o = opts(2, meta.dev());
        o.follow_symlinks = true;
        o.excludes = vec!["skipped".to_string()];
        let mut tree = Tree::new(base.to_string_lossy().into(), 0, 0, meta.dev(), meta.ino());
        for batch in start(base.clone(), o).events.iter() {
            tree.apply(batch);
        }
        let i = tree.nodes.iter().position(|n| n.name == "skipped").unwrap();
        assert_eq!(tree.nodes[i].kind, NodeKind::Link);
        assert_eq!(tree.nodes[i].excluded, Excluded::Pattern);
        assert_eq!(tree.followed_link(i), None);
        assert_eq!(tree.nodes[tree.root].apparent, 50_000);
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
