# icdu

An [ncdu](https://dev.yorhel.nl/ncdu)-style interactive disk usage explorer, in Rust — with two
differences that matter:

1. **Interactive immediately.** It does *not* wait for the scan to finish. The UI opens at once
   and you can browse, sort, and descend into directories while the scan is still running. Sizes
   fill in and aggregate upward live.
2. **Optimized for SSDs.** The scanner is a parallel work-stealing walker (default `cores × 4`
   threads). Directory reads and `lstat` calls are latency-bound, not bandwidth-bound, so keeping
   a deep queue of outstanding I/O lets flash storage overlap thousands of tiny stat latencies.
   (This is the opposite of what you'd want on a spinning disk — and exactly the trade-off ncdu,
   being single-threaded, can't make.)

3. **Navigation steers the scan.** When you enter a directory, its subtree is *prioritized* —
   the scanner finishes the branch you're looking at before unrelated ones, so the sizes you
   care about fill in first. Moving around never holds the scan up, however much work is
   queued.

It is a **drop-in ncdu replacement**: the common ncdu flags work, and it reads and writes the
ncdu JSON export format, so existing dumps and scripts keep working.

<p align="center">
  <img src=".github/assets/screenshot.png" alt="icdu browsing a scanned directory" width="97%">
</p>

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/anhofmann/icdu/main/install.sh | sh
```

The script detects your platform (macOS Apple Silicon/Intel, Linux x86-64/ARM64), downloads the
static binary from [Releases](https://github.com/anhofmann/icdu/releases), verifies its
SHA-256 checksum, and installs it to `~/.local/bin`. Override the destination with
`ICDU_INSTALL_DIR=...` or pin a version with `ICDU_VERSION=v0.5.0`. From source with a Rust
toolchain instead: `cargo install --git https://github.com/anhofmann/icdu`.

To upgrade, run the install script again: it replaces the binary with the latest release. There
is no built-in updater, and icdu itself never connects to the network.

### Upgrading from rcdu

Up to version 0.4.1 this program was called rcdu. Coming from it:

- Install icdu with the script above, then remove the old binary (`rm ~/.local/bin/rcdu`).
  `rcdu update` cannot do this step: it does not find releases published under the new name.
- The self-update command is gone. Upgrade by running the install script again.
- The install script reads `ICDU_VERSION` and `ICDU_INSTALL_DIR`. Releases up to v0.4.1 stay
  available: asked for one of them, the script installs it under its old name.
- Some keys changed to match ncdu: `Esc` no longer quits, `/` is the only search key (the
  filter is part of the search now), export is on `E`, the largest files on `T`, and `r` and `b`
  act on the directory you are viewing. The full list is under [Keys](#keys).
- Exports written from the UI are named `icdu-dump.json`, and dumps carry `icdu` as the program
  name. Dumps written by rcdu load as before.

## Usage

```
icdu [PATH] [OPTIONS]

ARGS:
    PATH                     directory to scan (default: current directory)

OPTIONS:
    -t, --threads N          scanner threads (default: cores*4, tuned for SSDs)
    -a, --apparent-size      show apparent size (st_size) instead of on-disk usage
    -x, --one-file-system    do not cross filesystem boundaries
        --exclude PATTERN    exclude entries matching a glob (repeatable)
        --older-than DUR     exclude entries older than DUR (e.g. 30d, 2w, 1y)
        --newer-than DUR     exclude entries newer than DUR (surfaces stale data)
    -X, --exclude-from FILE  read exclude patterns from FILE (one per line)
    -r, --read-only          disable the delete feature
        --show-itemcount     show the item count column at start (toggle with c)
        --hide-itemcount     hide the item count column at start (default)
    -o, --output FILE        scan without UI and write ncdu-compatible JSON ('-' = stdout)
    -f, --file FILE          load a tree from ncdu JSON instead of scanning ('-' = stdin)
    -h, --help               show help
    -V, --version            show version
```

Examples:

```sh
icdu /                          # browse / live, on-disk usage
icdu ~ --exclude .cache --exclude node_modules
icdu / -x -o backup.json        # headless scan of the root filesystem only → JSON
icdu -f backup.json             # browse a previous dump, no scanning
ncdu -o - / | icdu -f -         # pipe an ncdu scan straight into icdu
icdu -f old.json -o new.json    # re-read and rewrite a dump (recomputes totals/dedup)
```

### Keys

| Key | Action |
|-----|--------|
| `j` / `k`, `↓` / `↑` | move selection |
| `l` / `Enter` / `→` | enter directory |
| `h` / `<` / `Backspace` / `←` | go up |
| `Home` / `End`, `G` | jump to top / bottom |
| `PgUp` / `PgDn`, `Ctrl-d` / `Ctrl-u` | page up / down |
| `s` | toggle sort (size ↔ name) |
| `/` | search by name; `Enter` jumps to the selected match, `Tab` limits the search to the current directory |
| `a` | toggle apparent size ↔ on-disk usage |
| `c` | toggle the item count column (entries below each directory) |
| `d` | delete selected entry (asks to confirm) |
| `T` | show the largest files under the current directory |
| `r` | rescan the current directory in place |
| `i` | show details of the selected entry |
| `E` | export the tree to an ncdu-format JSON file |
| `x` | jump to the next read-error entry (`!` marker), wrapping around; the footer shows the reason |
| `b` | spawn `$SHELL` in the current directory |
| `o` | open selected entry (default app; directories in the file manager) |
| `?` | show the help dialog (it scrolls with `j` / `k` when the terminal is too small for it) |
| `q` | quit |
| `Esc` | close a popup (it never quits) |
| `Ctrl-l` | redraw the screen |
| `Ctrl-z` | suspend to the shell; `fg` brings the UI back |

A letter typed with Ctrl or Alt is never taken for the plain letter: `Ctrl-e` does not export and
`Alt-d` does not delete. The only such combinations that do anything are `Ctrl-c`, `Ctrl-d`,
`Ctrl-u`, `Ctrl-l` and `Ctrl-z`.

When icdu is ended from outside (`kill`, a closed terminal window), it gives the terminal back
in a usable state first.

Row indicators: `/` dir · `@` symlink · `*` special file (device, socket, fifo) · `H` hard-link
duplicate · `<` excluded · `!` read error.

### While the scan is running

Sizes are live, so the screen says which of them are final:

- A directory whose size is followed by `…` and shown dimmed has not been read completely
  below. Its size is still growing, and `d` and `r` are not available on it yet. If you
  press one of them anyway, icdu says how many directories are left in it, reads that
  directory before anything else, and tells you in the footer when it is ready. It does not
  delete on its own: you press `d` again.
- The header shows how many directories below the one you are viewing are still to be read
  (`2,281 directories left to read`). The number shrinks to nothing as the scan gets there.
- The title of the list describes the directory itself: `reading…` until its entries have
  arrived, then the number of items, `(empty)`, `(unreadable)` or `(excluded, not scanned)`.
  A directory with a great many entries arrives in parts; while more are coming the title
  reads `8,192 items · reading…`.

The same `…` marks unfinished directories in the search results.

The footer shows what the scan has done and what is left: the size found per second (in the
metric on screen), directories and files seen, and the number of directories still to be read in
the whole tree. A status message shares the line with this progress and does not hide it.

### Shelling out

Press `b` to suspend the TUI and run `$SHELL` interactively in the directory you are viewing
(as ncdu does) — find it here, fix it there, `exit` to come straight back into icdu. The scan keeps running in the
background while you're away, up to a limit on what it queues for the display, and carries on
when you return.

By default sizes are **on-disk usage** (`st_blocks × 512`); press `a` for apparent size.
Symlinks are never followed (counted as the link itself), matching ncdu's default.

### Searching

Press `/` to ask: *where in this tree is anything called X?* Type part of a name. Case is
ignored for every letter (`übung` finds `Übung.pdf`), and an accented letter is found whether
the name stores it as one character or, as macOS does, as a letter with a combining mark.

100 ms after your last keystroke every entry in the tree is searched, and the matches are
listed with their path and size, largest first, with the matched part highlighted.
`↑`/`↓` and `PgUp`/`PgDn` move (letters all go to the query), `Enter` jumps to the directory
containing the selected match with it highlighted, and `Esc` closes the popup without moving.
To edit the query, `Backspace` deletes a character, `Ctrl-w` the last word and `Ctrl-u` the
whole line. Pasted text goes into the query as it is, without its line breaks.

`Tab` switches what is searched: the whole tree (the default), or only what lies below the
directory you are viewing. Limited to a directory, paths are shown relative to it. The title
always names the scope, for example `12 matches under node_modules/`. This is the way to narrow
down a huge directory: go into it, press `/`, then `Tab`.

The search runs in slices between frames, so the UI stays responsive even on trees with
millions of entries; the title shows the match count and progress. While a scan or rescan is
running, new matches appear as they are discovered, and entries you delete or rescan away
drop out of the list. A very broad query lists its 10,000 largest matches and says how many
there are in all: `123,456 matches in the whole tree, showing the largest 10,000`.

### Largest files

Press `T` to see the largest regular files under the directory you're viewing, flattened across
its whole subtree — "what exactly is eating this directory?" without descending level by level.
Sizes follow the active metric (`a` toggles disk usage vs. apparent before opening the popup).
`j`/`k` move, `Enter` jumps to the selected file's parent directory with the file highlighted,
`q` closes. The popup holds the top 100 files as of when you opened it.

### Item counts

Sizes show where the bytes are; the item count shows where the *files* are. Press `c` (or start
with `--show-itemcount`, as in ncdu) to add a column with the number of entries below each
directory, counted recursively: the number `find DIR -mindepth 1 | wc -l` prints. It finds the
directories that are full of many small files, such as `node_modules`, caches or mail spools,
which are slow to copy, back up or delete even when they are small.

Like sizes, the counts are live: they grow while a scan or rescan runs and shrink when you
delete. Hard-link duplicates (`H`) and excluded entries (`<`) are listed, so they count as
items. An excluded directory is not read, so it counts as one entry and its own cell stays
blank, like the cells of files, symlinks and special files. Counts below 100,000 are exact;
larger ones are compact and rounded down (`123k`, `4.1M`). Imported dumps (`-f`) get the same
counts, derived on load. The info popup (`i`) shows the count as `total items`.

### Rescanning in place

Press `r` to rescan the directory you are viewing from disk without restarting (as in ncdu; at
the top level this reads the whole tree again). Its entries are replaced as the new scan streams
back in, the footer shows a spinner until it completes, and the totals of surrounding directories
stay correct throughout. Navigation keeps steering the rescan, so the directory you're viewing
fills in first.

The same guards as delete apply: the directory must have finished scanning, only one rescan runs
at a time, and while a deletion is running, neither the directory being deleted nor any directory
around or above it can be rescanned. Excluded directories (`--exclude`, `-x`, the mtime filters)
are not rescanned either: they were left out on purpose. And the path must still be the
directory that was scanned: if it has been replaced since, by a symlink or by another directory,
`r` refuses instead of listing something else in its place.

Hard-linked files are counted once across a rescan as well, whichever side of the rescanned
directory their links are on.

### Entry details

Press `i` on the selected entry to see its path, type, total and own sizes, the number of
entries for directories (direct, and recursive as `total items`), and flags (hard links,
exclusions, read errors) — plus, statted lazily from disk at open time, its permissions,
numeric owner, modification time, inode, and link count. Nothing extra is stored per node, so
the tree stays lean; if the path is gone (deleted mid-session) the popup says the on-disk
details are unavailable. For an imported dump (`-f`) nothing is read from disk at all.

### Read errors

An entry the scan could not read is marked `!`: a directory it was not allowed to list or
could not list to the end, or an entry it could not examine. The reason, in the words of the
operating system ("Permission denied"), is shown in the info popup and in the footer when `x`
jumps there. What a failed listing did return is kept. Entries that simply disappear while the
scan runs (temporary files, processes in `/proc`) are not errors and are left out.

### Deleting

Press `d` to delete the selected file or directory; a confirmation dialog appears first.

**Only what the scan listed is deleted.** icdu does not hand the directory to a recursive
remove, which would take whatever is on disk at that moment. It walks the entries it scanned,
checks each one against what the scan recorded (same type, device and inode), unlinks the files
and removes each directory once it is empty. So:

- A file that **appeared after the scan** stays, and so do the directories that hold it.
- A directory the scan **left out** (`--exclude`, `-x`, the mtime filters) or could not read is
  never emptied. `d` on such a directory is refused; inside a larger deletion it is left in place.
- An entry that has **changed identity** since the scan is not touched: a directory that became a
  symlink, a directory that now has another filesystem mounted on it, a file that was replaced.

When something is left behind, the footer says what and why, and the directory is read again so
the list shows what is really there.

Further guards:

- **Read-only** (`-r`) disables deletion entirely, and so does browsing an imported dump (`-f`).
- A directory **cannot be deleted until its whole subtree has finished scanning**. Because the
  scan is live, a directory's size and contents may still be growing; icdu refuses to delete one
  until every descendant has been read, so you never delete something whose true extent isn't yet
  known. Files are always deletable immediately.

The removal itself runs on a **background thread**, so deleting a huge directory never freezes
the UI: a spinner marks the entry (`[| deleting…]`) and the footer shows progress, while you keep
browsing the rest of the tree normally. Only the subtree being deleted is locked (you can't enter
it), and one deletion runs at a time.

Quitting (`q` / `Ctrl-C`) **while a deletion is running** does not abort it silently: the first
press warns, and a second press force-quits — which interrupts the removal wherever it is and
leaves the rest on disk. Any other key disarms the prompt.

### Hard links, exclusions, filesystems

- **Hard links** (`nlink > 1`) are detected by `(device, inode)`. Each inode is counted **once**
  (the first link the scan delivers); further links are shown with their size, marked `H`, and
  add nothing to any total — so a tree's grand total isn't inflated by hard links. When you
  delete the link that was counted, the file is still on disk through its other links, so the
  size moves to one of them instead of dropping out of the totals. The delete dialog says when
  the selection contains hard-linked files: their space is only freed once the last link is
  gone.
- `--exclude PATTERN` matches an entry's **basename** with shell globbing (`*`, `?`, `[...]`).
  Excluded entries stay visible (marked `<`) but are not descended into and don't count.
- `-x` keeps the scan on the root's filesystem; mount points on other devices are shown but not
  traversed.
- `--older-than DUR` / `--newer-than DUR` exclude entries by modification time (durations:
  `s`/`m`/`h`/`d`/`w`/`y`, bare numbers = days). Filtered entries stay visible (marked
  `<mtime filter>`) but are not descended into and don't count — e.g. `icdu ~ --newer-than 1y`
  surfaces the data you haven't touched in a year.
- **Names that are not valid UTF-8** are shown with the replacement character (`�`) in place of
  the bytes that cannot be displayed. Everything that touches the entry (delete, open, the
  shell, rescan) uses its real bytes, and so does `-o`.

### ncdu JSON compatibility

`-o` writes the ncdu export format (`[1, 2, {meta}, <root>]`, per-item `asize`/`dsize`, `hlnkc`,
`notreg`, `excluded`, `read_error`), readable by ncdu and other tools. `-f` reads the same format
(including dumps produced by ncdu itself), re-deriving hard-link dedup so totals match a live scan.
Names are read and written byte for byte, as ncdu does, so a name that is not valid UTF-8
survives an import and a re-export unchanged.

A dump is parsed while it is read: memory use is that of the resulting tree, whatever the size
of the file, and directories can be nested to any depth. Fields icdu does not know are skipped.
A file that is not a dump (truncated, wrong types, anything after the end) is refused with the
byte position of the problem.

A dump describes the machine and the moment it was taken on, not the local filesystem as it is
now. While browsing one, delete, the shell, open and rescan are therefore disabled, and the
info popup reads nothing from disk, as in ncdu. So `ssh host ncdu -o- / | icdu -f -` can never
touch a local file that happens to have the same path. Independently of that, `-f` refuses a
dump whose entry names aren't single path components (empty, `.`, `..`, or containing `/` or
NUL), or whose root path contains a `..` component.

Press `E` in the UI to write the currently browsed tree — including deletions made this session —
to `icdu-dump.json` in the working directory (`icdu-dump-2.json` and so on if one already
exists), in the same format as `-o`: handy for snapshotting a long scan without redoing it.
A large dump is written in the background of the UI, with its progress in the footer; you can
keep browsing, but deleting and rescanning wait until it is done.
While the scan is still running, `E` asks first, because the dump will be partial. Directories
that have not been read yet are then written with the read-error flag, so icdu and ncdu show
them as incomplete (`!`) instead of as empty.

## Portable, dependency-free binaries

`icdu`'s core — scan, sort, delete, JSON import/export — makes **no external process calls**: no
`du`, no coreutils, nothing; it's pure Rust `std`. Two features deliberately start another
program: `o` hands the selected entry to the platform's default handler, and `b` opens your
shell. The binaries are self-contained:

- **Linux** builds are **fully static musl** binaries: libc is baked in, there are no `.so`
  dependencies and no dynamic loader. Copy to any x86-64 or ARM64 Linux box — Alpine, Ubuntu,
  ancient CentOS, a distroless container — and it just runs.
- **macOS** builds link only `libSystem`/`libiconv`, which are part of every macOS install
  (Apple does not support fully static binaries; this is the portability ceiling on macOS).

Build all three with the bundled script:

```sh
./build-portable.sh        # outputs dist/icdu-portable-{linux-x86_64,linux-aarch64,macos-<arch>}
```

Prebuilt binaries for every release (Linux x86_64/ARM64 static musl, macOS
Apple Silicon/Intel, plus SHA-256 checksums) are attached to the
[GitHub Releases](https://github.com/anhofmann/icdu/releases) page.

The Linux targets are cross-compiled straight from macOS using the toolchain's bundled
`rust-lld` — **no Docker, no zig, no external linker required**.

### Manual builds

```sh
# Native (this machine)
cargo build --release

# Linux static, cross-compiled from anywhere:
LLD=$(find "$(rustc --print sysroot)" -name rust-lld | head -1)
rustup target add x86_64-unknown-linux-musl
RUSTFLAGS="-C linker=$LLD -C linker-flavor=ld.lld -C link-self-contained=yes" \
  cargo build --release --target x86_64-unknown-linux-musl
```

## Design

| Module | Responsibility |
|--------|----------------|
| `scan.rs` | Parallel streaming scanner with a priority frontier (focused subtree first). Entries arrive in batches, a huge directory in several; excludes, one-filesystem. |
| `model.rs` | The tree owned by the UI thread. Lean `Node` (`CompactString` names, derived own-size); grafts batches, propagates *counted* sizes up every ancestor, counts each hard-linked inode once, buffers out-of-order batches, tracks per-subtree "unscanned dirs" for the delete gate, and takes deleted subtrees out. |
| `delete.rs` | Removal from disk, driven by the tree: a copy of the scanned subtree is walked on a worker thread, each entry checked against the scan (type, device, inode) before it is unlinked. Nothing the scan did not list is removed. |
| `glob.rs` | `fnmatch`-style globbing for `--exclude`. |
| `matcher.rs` | Matching a search query against names: case-insensitive for all letters, and across the two Unicode spellings of accented Latin letters. |
| `dump.rs` | ncdu-compatible JSON import/export, both streaming: a hand-written writer, and a pull parser for exactly this format. |
| `app.rs` | App state + key handling, modals (help/confirm), background delete and open. Selection tracks node *identity*, so the highlight doesn't jump as live updates reorder the list. |
| `ui.rs` | ratatui rendering: size bars, percentages, type indicators, popups. |
| `main.rs` | Args, modes (scan / import / export), terminal lifecycle, event loop. |

## Testing

```sh
cargo test
```

The test builds a known directory tree, runs the real scanner to completion, and asserts the
root aggregates every file's bytes for both the apparent and on-disk metrics, with no batch left
buffered — verifying the concurrency and size-propagation logic.

## Status

Implemented: live/progressive parallel scan, SSD-tuned threading, **navigation-prioritized
scanning**, sort, apparent/disk toggle, delete (background, with the still-scanning safety gate,
`-r`, and a guarded Ctrl-C), hard-link dedup, `--exclude` / `-X`, `-x` one-file-system, ncdu JSON
import/export, open (`o`, via the platform's default handler), help dialog, a memory-lean node
representation, and static portable binaries.

Possible future work: in-place refresh (`r`) of a subtree, the ncdu "shared/unique" size
breakdown column, and a file-info panel. Further memory wins are available if needed (`u32`
node indices, splitting file/dir storage, a per-tree device table).

## Origin

This project started as a fork of [rcdu](https://github.com/mayankasthana/rcdu) by Mayank Asthana
and has been developed independently since version 0.4.0.
