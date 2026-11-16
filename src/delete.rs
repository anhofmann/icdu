//! Deleting what the scan saw, and nothing else.
//!
//! `std::fs::remove_dir_all` removes whatever is below a path at the moment it runs. That is
//! more than the user was shown: files created since the scan, the content of directories the
//! scan left out (`--exclude`, `-x`, the mtime filters) or could not read, and whole other
//! filesystems mounted somewhere below.
//!
//! So a deletion is driven by the tree instead. [`Plan::of`] copies the subtree to delete out
//! of the tree (the tree itself never leaves the UI thread), and [`Plan::execute`] walks that
//! copy on a worker thread:
//!
//! * Every entry is `lstat`ed first and removed only if it is still the entry that was
//!   scanned: same type, same device, same inode. A directory that has since become a symlink
//!   or a mount point for something else is left alone.
//! * Files are unlinked, directories removed with `rmdir`, which succeeds only on an empty
//!   directory. Whatever the scan never listed therefore stays, and so does every directory
//!   above it.
//! * A directory that was not read (excluded, or unreadable) is never descended into.
//!
//! What could not be removed is returned in the [`Report`], so the caller can say so and read
//! the directory again.

use std::io::ErrorKind;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use compact_str::CompactString;

use crate::model::{Excluded, NodeIdx, NodeKind, Tree};

/// How many failures a [`Report`] spells out; beyond that they are only counted.
const FAILURE_SAMPLES: usize = 20;

/// One entry to remove: what the scan recorded about it.
struct Entry {
    name: CompactString,
    kind: NodeKind,
    dev: u64,
    ino: u64,
    /// For a directory: its listing is part of the plan. False for a directory the scan did
    /// not read; that one is only removed if it turns out to be empty.
    descend: bool,
    /// False if the scan could not `lstat` the entry, so there is nothing to compare with.
    verifiable: bool,
    /// Index one past the last entry of this entry's subtree.
    end: usize,
}

impl Entry {
    /// Is what `meta` describes the entry the scan recorded: same type, device and inode?
    fn is_still(&self, meta: &std::fs::Metadata) -> bool {
        self.verifiable
            && kind_of(meta) == self.kind
            && meta.dev() == self.dev
            && meta.ino() == self.ino
    }
}

/// A subtree to delete, copied out of the tree in pre-order: every directory is followed by
/// everything below it.
pub struct Plan {
    target: PathBuf,
    entries: Vec<Entry>,
}

/// What a deletion did.
#[derive(Debug, Default)]
pub struct Report {
    /// Entries removed from disk.
    pub removed: u64,
    /// Entries that could not be removed, including directories kept alive by one of them.
    pub failed: u64,
    /// The first few failures: path and reason.
    pub failures: Vec<(PathBuf, String)>,
    /// The entry the deletion was asked for is gone.
    pub complete: bool,
    /// The deletion is incomplete, and its target is still the entry that was scanned: a
    /// directory with things left in it, which can be read again to see what they are.
    pub target_intact: bool,
}

impl Report {
    fn fail(&mut self, path: &Path, reason: impl Into<String>) {
        self.failed += 1;
        if self.failures.len() < FAILURE_SAMPLES {
            self.failures.push((path.to_path_buf(), reason.into()));
        }
    }

    /// A report for a worker that died without sending one: nothing is known.
    pub fn lost() -> Self {
        Report {
            failed: 1,
            failures: vec![(
                PathBuf::new(),
                "the delete worker exited unexpectedly".into(),
            )],
            ..Report::default()
        }
    }
}

impl Plan {
    /// Copy the subtree at `idx` out of the tree. `None` if `idx` is no longer part of it.
    pub fn of(tree: &Tree, idx: NodeIdx) -> Option<Plan> {
        let target = PathBuf::from(tree.fs_path_of(idx)?);
        let entry = |i: NodeIdx| {
            let n = &tree.nodes[i];
            Entry {
                name: n.name.clone(),
                kind: n.kind,
                dev: n.dev,
                ino: n.ino,
                descend: n.is_dir() && n.scanned && !n.read_error && n.excluded == Excluded::No,
                // The scanner records an entry it could not `lstat` without device and inode.
                verifiable: !(n.read_error && !n.is_dir() && n.dev == 0 && n.ino == 0),
                end: 0,
            }
        };

        let mut entries = vec![entry(idx)];
        // (node, next child to visit, index of the node's entry)
        let mut stack = vec![(idx, 0usize, 0usize)];
        while let Some(top) = stack.last_mut() {
            let (node, next, at) = *top;
            let children = &tree.nodes[node].children;
            if entries[at].descend && next < children.len() {
                top.1 += 1;
                let child = children[next];
                entries.push(entry(child));
                stack.push((child, 0, entries.len() - 1));
            } else {
                entries[at].end = entries.len();
                stack.pop();
            }
        }
        Some(Plan { target, entries })
    }

    /// Number of entries the plan covers, the target included.
    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }

    /// Remove the planned entries from disk. Runs for as long as the filesystem takes; call
    /// it off the UI thread.
    pub fn execute(&self) -> Report {
        let mut report = Report::default();
        let mut path = self.target.clone();
        report.complete = self.remove(0, &mut path, &mut report);
        report.target_intact = !report.complete
            && std::fs::symlink_metadata(&self.target)
                .is_ok_and(|meta| self.entries[0].is_still(&meta));
        report
    }

    /// Remove entry `at`, which lives at `path`. Returns whether it is gone afterwards.
    fn remove(&self, at: usize, path: &mut PathBuf, report: &mut Report) -> bool {
        let entry = &self.entries[at];
        let meta = match std::fs::symlink_metadata(&*path) {
            Ok(meta) => meta,
            // Already gone, with everything below it: that is the state asked for.
            Err(e) if e.kind() == ErrorKind::NotFound => return true,
            Err(e) => {
                report.fail(path, e.to_string());
                return false;
            }
        };
        if !entry.verifiable {
            report.fail(path, "could not be examined during the scan");
            return false;
        }
        if !entry.is_still(&meta) {
            report.fail(path, "is not the entry that was scanned any more");
            return false;
        }

        if entry.kind != NodeKind::Dir {
            // Files, symlinks (the link itself, never its target) and special files.
            return match std::fs::remove_file(&*path) {
                Ok(()) => {
                    report.removed += 1;
                    true
                }
                Err(e) if e.kind() == ErrorKind::NotFound => true,
                Err(e) => {
                    report.fail(path, e.to_string());
                    false
                }
            };
        }

        let mut emptied = true;
        if entry.descend {
            let mut child = at + 1;
            while child < entry.end {
                path.push(self.entries[child].name.as_str());
                emptied &= self.remove(child, path, report);
                path.pop();
                child = self.entries[child].end;
            }
        }
        if !emptied {
            // Something the scan listed is still inside and has been reported; `rmdir` could
            // only fail. The directory stays, and counts as not removed.
            report.failed += 1;
            return false;
        }
        match std::fs::remove_dir(&*path) {
            Ok(()) => {
                report.removed += 1;
                true
            }
            Err(e) if e.kind() == ErrorKind::NotFound => true,
            Err(e) => {
                let reason = if !entry.descend {
                    format!("was not read by the scan, so it is only removed when empty ({e})")
                } else if e.kind() == ErrorKind::DirectoryNotEmpty {
                    "holds entries that were not there when it was scanned".to_string()
                } else {
                    e.to_string()
                };
                report.fail(path, reason);
                false
            }
        }
    }
}

/// The type of an entry on disk, in the scanner's terms.
fn kind_of(meta: &std::fs::Metadata) -> NodeKind {
    let ft = meta.file_type();
    if ft.is_dir() {
        NodeKind::Dir
    } else if ft.is_symlink() {
        NodeKind::Link
    } else if ft.is_file() {
        NodeKind::File
    } else {
        NodeKind::Other
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::{self, Opts};
    use std::fs;

    /// A fresh directory under its canonical path.
    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rcdu_plan_{tag}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::canonicalize(dir).unwrap()
    }

    /// Scan `root` to completion with the real scanner.
    fn scan_tree(root: &Path, excludes: &[&str]) -> Tree {
        let meta = fs::symlink_metadata(root).unwrap();
        let opts = Opts {
            threads: 2,
            one_file_system: false,
            excludes: excludes.iter().map(|s| s.to_string()).collect(),
            root_dev: meta.dev(),
            older_than: None,
            newer_than: None,
        };
        let mut tree = Tree::new(
            root.to_string_lossy().into(),
            meta.len(),
            meta.blocks() * 512,
            meta.dev(),
            meta.ino(),
        );
        for batch in scan::start(root.to_path_buf(), opts).events.iter() {
            tree.apply(batch);
        }
        tree
    }

    fn plan_for(tree: &Tree, name: &str) -> Plan {
        let idx = tree.nodes.iter().position(|n| n.name == name).unwrap();
        Plan::of(tree, idx).unwrap()
    }

    /// Everything the scan listed goes: files, nested directories, and symlinks as links.
    #[test]
    fn removes_exactly_what_was_scanned() {
        let root = scratch("all");
        fs::create_dir_all(root.join("victim/a/b")).unwrap();
        fs::write(root.join("victim/a/b/deep"), b"x").unwrap();
        fs::write(root.join("victim/top"), b"x").unwrap();
        fs::write(root.join("outside"), b"keep me").unwrap();
        std::os::unix::fs::symlink(root.join("outside"), root.join("victim/link")).unwrap();
        fs::create_dir(root.join("victim/empty")).unwrap();

        let plan = plan_for(&scan_tree(&root, &[]), "victim");
        assert_eq!(plan.len(), 7, "victim, a, b, deep, top, link, empty");
        let report = plan.execute();
        assert!(report.complete, "{report:?}");
        assert_eq!((report.removed, report.failed), (7, 0));
        assert!(!root.join("victim").exists());
        assert_eq!(
            fs::read(root.join("outside")).unwrap(),
            b"keep me",
            "the symlink's target is untouched"
        );
        fs::remove_dir_all(&root).unwrap();
    }

    /// A file created after the scan stays, and with it every directory above it. Everything
    /// else the scan listed is removed.
    #[test]
    fn keeps_what_appeared_after_the_scan() {
        let root = scratch("new");
        fs::create_dir_all(root.join("victim/sub")).unwrap();
        fs::write(root.join("victim/sub/old"), b"x").unwrap();
        fs::write(root.join("victim/old"), b"x").unwrap();
        let plan = plan_for(&scan_tree(&root, &[]), "victim");

        fs::write(root.join("victim/sub/new"), b"not in the scan").unwrap();
        let report = plan.execute();
        assert!(!report.complete);
        assert!(root.join("victim/sub/new").exists());
        assert!(!root.join("victim/sub/old").exists());
        assert!(!root.join("victim/old").exists());
        assert_eq!(report.removed, 2, "the two old files");
        assert_eq!(report.failed, 2, "sub/ and victim/ stay");
        let (path, reason) = &report.failures[0];
        assert_eq!(path, &root.join("victim/sub"));
        assert!(reason.contains("not there when it was scanned"), "{reason}");
        fs::remove_dir_all(&root).unwrap();
    }

    /// A directory that has turned into a symlink since the scan is not followed: neither the
    /// link nor what it points at is touched.
    #[test]
    fn does_not_follow_a_directory_replaced_by_a_symlink() {
        let root = scratch("swap");
        fs::create_dir_all(root.join("victim/sub")).unwrap();
        fs::write(root.join("victim/sub/file"), b"x").unwrap();
        fs::create_dir(root.join("elsewhere")).unwrap();
        fs::write(root.join("elsewhere/file"), b"precious").unwrap();
        let plan = plan_for(&scan_tree(&root, &[]), "victim");

        fs::remove_dir_all(root.join("victim/sub")).unwrap();
        std::os::unix::fs::symlink(root.join("elsewhere"), root.join("victim/sub")).unwrap();
        let report = plan.execute();
        assert!(!report.complete);
        assert_eq!(fs::read(root.join("elsewhere/file")).unwrap(), b"precious");
        assert!(
            fs::symlink_metadata(root.join("victim/sub")).is_ok(),
            "the link itself is not what was scanned and stays"
        );
        assert!(report.failures[0]
            .1
            .contains("not the entry that was scanned"));
        fs::remove_dir_all(&root).unwrap();
    }

    /// An excluded directory was never read: it is not emptied, only removed if empty.
    #[test]
    fn never_empties_a_directory_the_scan_left_out() {
        let root = scratch("excluded");
        fs::create_dir_all(root.join("victim/full.skip")).unwrap();
        fs::write(root.join("victim/full.skip/secret"), b"never listed").unwrap();
        fs::create_dir(root.join("victim/empty.skip")).unwrap();
        fs::write(root.join("victim/known"), b"x").unwrap();
        let tree = scan_tree(&root, &["*.skip"]);
        let plan = plan_for(&tree, "victim");
        assert_eq!(plan.len(), 4, "victim, the two excluded directories, known");

        let report = plan.execute();
        assert!(!report.complete);
        assert!(root.join("victim/full.skip/secret").exists());
        assert!(
            !root.join("victim/empty.skip").exists(),
            "empty: rmdir is safe"
        );
        assert!(!root.join("victim/known").exists());
        assert!(report.failures[0].1.contains("was not read by the scan"));
        fs::remove_dir_all(&root).unwrap();
    }

    /// An entry on another device, or with another inode, than the scan recorded is not the
    /// scanned entry: a mount point that appeared, a file that was replaced. It stays.
    #[test]
    fn keeps_entries_whose_identity_changed() {
        let root = scratch("identity");
        fs::create_dir_all(root.join("victim/mnt")).unwrap();
        fs::write(root.join("victim/mnt/data"), b"x").unwrap();
        fs::write(root.join("victim/file"), b"x").unwrap();
        let tree = scan_tree(&root, &[]);
        let mut plan = plan_for(&tree, "victim");

        // As if `mnt` now had another filesystem mounted on it, and `file` had been replaced.
        for entry in &mut plan.entries {
            match entry.name.as_str() {
                "mnt" => entry.dev += 1,
                "file" => entry.ino += 1,
                _ => {}
            }
        }
        let report = plan.execute();
        assert!(!report.complete);
        assert!(root.join("victim/mnt/data").exists(), "not descended into");
        assert!(root.join("victim/file").exists());
        assert_eq!(report.removed, 0);
        fs::remove_dir_all(&root).unwrap();
    }

    /// A target that vanished on its own counts as done.
    #[test]
    fn a_target_that_is_already_gone_is_complete() {
        let root = scratch("gone");
        fs::create_dir(root.join("victim")).unwrap();
        let plan = plan_for(&scan_tree(&root, &[]), "victim");
        fs::remove_dir(root.join("victim")).unwrap();
        let report = plan.execute();
        assert!(report.complete);
        assert_eq!((report.removed, report.failed), (0, 0));
        fs::remove_dir_all(&root).unwrap();
    }

    /// A node that has left the tree has no path, and so no plan.
    #[test]
    fn no_plan_for_a_tombstone() {
        let root = scratch("tombstone");
        fs::create_dir(root.join("victim")).unwrap();
        let mut tree = scan_tree(&root, &[]);
        let idx = tree.nodes.iter().position(|n| n.name == "victim").unwrap();
        tree.delete_subtree(idx);
        assert!(Plan::of(&tree, idx).is_none());
        fs::remove_dir_all(&root).unwrap();
    }
}
