# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

Workflow rules (worktrees, branch naming, commits, merging, quality gates, test requirements) are in AGENTS.md and apply in full:

@AGENTS.md

## Commands

```sh
cargo test                                   # all tests (unit tests live in each src/*.rs)
cargo test aggregates_full_tree              # single test by name substring
cargo test scan::tests                       # one module's tests
cargo test -- --nocapture                    # show println! output

cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test && cargo build --release
                                             # exactly what CI runs (ubuntu + macos)

cargo run -- <PATH>                          # interactive TUI
cargo run -- <PATH> -o -                     # headless scan → ncdu JSON on stdout (no TTY needed)
cargo run -- -f dump.json -o -               # import + re-export a dump
./build-portable.sh                          # static musl Linux + native macOS binaries into dist/
```

The TUI needs a real terminal, so check behavior headlessly with `-o -` or through unit tests on `App`/`Tree`.

## Platform

Unix only (`std::os::unix::fs::MetadataExt` for dev/ino/blocks/nlink). Platform-specific code uses `#[cfg(target_os = ...)]`, e.g. the three `opener_command` variants in `app.rs`. Anything you change has to build and pass on both Linux and macOS.

Dependencies are deliberately few (`ratatui`, `crossbeam-channel`, `compact_str`, and `signal-hook`, which crossterm brings into the build anyway). There is no JSON library: `dump.rs` writes and parses the dump format itself. The core never spawns external processes. The only exceptions are `o` (the platform opener) and `b` (`$SHELL`). Nothing in the binary uses the network.

## Architecture

### Data flow: scanner threads → channel → UI-thread-owned tree

- **`scan.rs`**: N worker threads (default `cores × 4`) pull `DirJob`s from a shared `Frontier`. Each worker reads one directory, `lstat`s its entries, pushes subdirectories back onto the frontier, and sends exactly **one `Batch` per directory** over an unbounded crossbeam channel. A `Batch` carries the `parent_id` (a scanner-assigned `u64`, not a `NodeIdx`) and the new nodes. Subdirectories carry a `dir_id` that later batches use as their `parent_id`.
- **Scan completion**: a `pending` count (queued + in-flight jobs) reaches 0 → workers exit → every `Sender` drops → the channel disconnects. A disconnect is the UI's only "scan done" signal. `ScanControl` (the UI's steering handle) intentionally holds no `Sender`, so it can't keep the channel alive.
- **Priority**: the frontier has `hot` and `cold` queues. `ScanControl::set_focus(path)` re-buckets the queued jobs so anything under the viewed directory goes first. `App::refocus()` calls this on every navigation.
- **`model.rs` (`Tree`)**: a flat `Vec<Node>` indexed by `NodeIdx`, owned only by the UI thread. `Tree::apply` maps `parent_id → NodeIdx` through `dir_index`, grafts the nodes, and propagates sizes up every ancestor, so totals are correct at every moment. Batches whose parent doesn't exist yet are buffered in `orphans`. `finish_scan()` frees `dir_index` and `orphans` once all streams end.
- **Counted vs. displayed size**: hard-link duplicates (`shared`) and excluded entries (`Excluded::{Pattern, OtherFs, Age}`) keep their real size for display but contribute 0 to aggregates (`Node::counts()`). A node's own size is derived (aggregate minus counted children), not stored. Keep `Node` lean: there is one per filesystem entry.
- **Hard links**: the scanner only reports `hlink` (link count above 1). Which link of an inode is counted is decided by the tree, in `graft`, through `Tree::links` (`(dev, ino)` → the counted link and the duplicates): the first link to arrive counts. That makes the rule hold across scan generations. When the counted link is detached (`delete_subtree`, `begin_refresh`), `hand_over_links` moves the count, and the size, to a link that is still attached. Imported trees get `shared` from the import and do not use the index.
- **Tombstones**: deleting or rescanning detaches nodes but never removes them from `tree.nodes`. Indices stay stable, and unreachable nodes are simply left in the vector.

### Safety gates (do not weaken)

- `Node::unscanned` counts the directories not yet read in a subtree. `Tree::subtree_complete(idx)` (`unscanned == 0`) gates both **delete** (`d`) and **rescan** (`r`). Deleting a partially scanned subtree could remove things the user hasn't seen. Rescanning one could double-count batches still arriving from the original scan.
- Deletion removes **only what the scan listed** (`delete.rs`). `perform_delete` copies the subtree into a `delete::Plan`; a worker thread walks it, `lstat`s every entry and removes it only if type, device and inode still match the scan, with `unlink` for files and `rmdir` for directories. Never bring back a recursive remove (`remove_dir_all`): it deletes files created after the scan, the content of excluded or unreadable directories, and filesystems mounted below. Directories the scan did not read are never descended into, and `d` on one is refused.
- A deletion that leaves things behind returns them in a `delete::Report`; `poll_delete` then reads the directory again (`resync`) so the tree matches the disk.
- Deletion runs on a background thread (`perform_delete` → `poll_delete`). Only one runs at a time, the subtree being deleted is locked, and quitting during a delete needs two presses (`quit_armed`).
- An imported dump (`-f`, `App.imported`) is not the local filesystem. Delete, shell, open, rescan and the info popup's `lstat` are refused there (`refused_for_import`); any new action that touches a path needs the same check.
- Filesystem actions take their path from `Tree::fs_path_of`, which is `None` for a tombstone. `Tree::path_of` is for display only: for a detached node it returns a relative path.
- `Node::name` is the display form of a name and always valid UTF-8. A name that is not valid UTF-8 on disk also has its real bytes in `Tree::raw_names` (a side table, so `Node` does not grow). `fs_path_of`, the delete plan and the dump export use `Tree::name_bytes`; never build a path to act on from `Node::name`.

### In-place rescan (`r`)

`Tree::begin_refresh` detaches the subtree and resets its aggregates. `scan::start_at(path, opts, id_base, identity)` then starts a new scanner **generation**. `identity` is the directory's device and inode from the tree: `read_dir` follows a symlink in the last path component, so both `request_refresh` and the scanner's first job check with `scan::check_identity` (an `lstat`) that the path still names that directory. Each generation gets a disjoint id range (`id_base = n × SCAN_ID_STRIDE`, where `SCAN_ID_STRIDE` is 2⁴⁰) so it can stream into the same tree alongside the original scan. `App` keeps a `Vec<ScanHandle>` plus a `Vec<ScanControl>`, and `poll_scans` drains all of them.

### UI / event loop

- `main.rs::run` loops: `poll_scans(20_000)` (a cap, so input never starves) → `poll_delete` / `poll_open` → handle any `take_shell_request()` (tear down the TUI, run `$SHELL`, re-init) → draw → `event::poll` with an 80 ms timeout while busy, 250 ms otherwise.
- Long-running side effects (delete, open) go to background threads and are polled once per frame through channels. Keep the UI thread non-blocking.
- `signals.rs`: SIGTERM, SIGHUP, SIGTSTP and SIGCONT only set flags, which the loop checks at the top of every iteration. It restores the terminal before the signal takes effect (`die_by`, `suspend`). Ctrl-Z is a key press in raw mode and takes the same `suspend` path. Always enter and leave the TUI through `init_terminal` / `restore_terminal` (they also switch bracketed paste).
- `App.selected` tracks node **identity** (`NodeIdx`), not row position, so the highlight stays on the same entry as live updates re-sort the list.
- `App.status` is a one-line footer message that clears on the next key press. `App.modal` holds popups (help, confirm, top files, info).

### Adding a key binding

Touch all of these in one change:
1. `KeyAction` variant (`app.rs`)
2. `decode_key` mapping (`main.rs`; watch the match-arm order, e.g. `Ctrl-d` must come before plain `d`)
3. `App::on_key` handling (`app.rs`)
4. The help popup `render_help` (`ui.rs`)
5. The Keys table and a feature section in README.md

### ncdu compatibility

`dump.rs` reads and writes ncdu JSON format version 1.2 (`[1, 2, {meta}, <root>]`). On import, hard-link dedup is re-derived from `(dev, ino)` so totals match a live scan. The importer is a pull parser for exactly this format (`dump::Parser`), not a general JSON parser: it streams from the reader into the tree, treats strings as bytes (names need not be UTF-8), keeps open directories on a heap stack instead of recursing, skips unknown fields, and is strict about the types of the fields it knows. A new field is one more arm in `Parser::field`. CLI flags mirror ncdu's (`-x`, `-r`, `-o`, `-f`, `-X`, `--exclude`), so keep new flags consistent with ncdu where an equivalent exists. Arguments are parsed by hand in `main.rs::parse_args`; there is no clap.

## Tests

Tests live in `#[cfg(test)]` modules at the bottom of each file. Patterns to reuse:
- **`scan.rs`**: builds a real temporary directory tree, runs the real scanner to completion, and asserts on aggregates. Covers concurrency, hard links, excludes, focus priority and rescan.
- **`app.rs`**: builds a `Tree` in memory with `Tree::new_imported` + `import_child` (see `app_with_entries()`), wraps it in `App::new(tree, disk_usage, read_only, scanning)`, and drives it through `on_key(KeyAction::…)`. No terminal is needed.
- Rendering in `ui.rs` isn't unit-tested. Per AGENTS.md, say so in the PR and verify it manually.
