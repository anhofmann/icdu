# icdu

A disk usage explorer for the terminal that you can use for finding files **while it is still scanning**, and is a drop in replacement for [ncdu](https://dev.yorhel.nl/ncdu).

- **Browse while it scans.** The directory you enter is scanned next, so what you are looking at is
  finished first.
- **Search the tree by name.** `/` finds entries anywhere, largest first, as you
  type.
- **Compatible with ncdu.** Keys, command-line options, config files work, and JSON dumps are compatible. Differences are listed
  under [Differences from ncdu](#differences-from-ncdu).

<p align="center">
  <img src=".github/assets/demo.svg" alt="icdu while a scan is running: the list fills in, a search finds a directory in the tree, and Enter jumps to it" width="97%">
</p>

<p align="center"><sub>Recorded on a sample tree with <code>tools/demo/record.sh</code>. The tree is small, so
the scan was slowed down for the recording.</sub></p>

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/anhofmann/icdu/main/install.sh | sh
```

The script finds out what machine it is on (macOS on Apple Silicon or Intel, Linux on x86-64 or
ARM64), downloads the archive of the latest
[release](https://github.com/anhofmann/icdu/releases) for it, checks it against the release's
SHA-256 checksums, and installs the binary to `~/.local/bin`.

- `ICDU_INSTALL_DIR=...` installs somewhere else.
- `ICDU_VERSION=0.5.0` (or `v0.5.0`) installs that release instead of the latest.
- **To upgrade, run it again.** It says what it replaced. There is no built-in updater, and
  icdu itself never connects to the network.

  
Other ways:

- **Debian and Ubuntu.** Each release has `icdu_<version>_amd64.deb` and
  `icdu_<version>_arm64.deb`. Download the one for your machine and
  `sudo apt install ./icdu_<version>_amd64.deb`. It installs the binary and the manual page
  and depends on nothing. There is no apt repository yet, so an upgrade is the same step with
  the newer file; `sudo apt remove icdu` takes it off again.
- **By hand.** Download an archive from the releases page and unpack it.
- **From source**, with a Rust toolchain: `cargo install --git https://github.com/anhofmann/icdu`.

### Uninstalling

`rm ~/.local/bin/icdu` (or wherever `ICDU_INSTALL_DIR` pointed). Config files you may have written might be in
`~/.config/icdu/config` and `/etc/icdu.conf`.

## Usage

```
icdu [OPTIONS] [PATH]

ARGS:
    PATH                     directory to scan (default: current directory)

OPTIONS:
    -t, --threads N          scanner threads (default: 4 per core, 4 to 64)
    -a, --apparent-size, --disk-usage
                             show apparent sizes, or disk usage (default)
    -x, --one-file-system, --cross-file-system
                             stay on one filesystem, or cross (default)
        --exclude PATTERN    exclude entries matching a glob (repeatable)
    -X, --exclude-from FILE  read exclude patterns from FILE (one per line)
        --exclude-caches, --include-caches
                             leave out directories that have a CACHEDIR.TAG
        --exclude-kernfs, --include-kernfs
                             leave out Linux pseudo filesystems (/proc, /sys)
    -L, --follow-symlinks, --no-follow-symlinks
                             count a symlink to a file as that file
        --older-than DUR     exclude entries older than DUR (e.g. 30d, 2w, 1y)
        --newer-than DUR     exclude entries newer than DUR
    -r, --read-only          no deleting; given twice: no shell or export either
        --disable-delete, --enable-delete
                             turn the delete key off, or on (default)
        --disable-shell, --enable-shell
                             turn shell and opener off, or on (default)
        --disable-refresh, --enable-refresh
                             turn rescanning off, or on (default)
        --delete-command CMD run CMD on the path instead of deleting it
        --confirm-quit, --no-confirm-quit
                             ask before quitting, or not (default)
        --confirm-delete, --no-confirm-delete
                             ask before deleting (default), or not
        --si, --no-si        sizes in powers of 1000 (kB, MB), or of 1024
        --color SCHEME       off, dark or dark-bg (off if NO_COLOR is set)
        --show-hidden, --hide-hidden
                             list dot files and excluded entries, or not
        --show-graph, --hide-graph
                             show or hide the size bar (default: show)
        --show-percent, --hide-percent
                             show or hide the percentage (default: show)
        --graph-style STYLE  the bar: hash, half-block or eighth-block
        --sort COLUMN        disk-usage, name, apparent-size, itemcount, mtime
        --group-directories-first, --no-group-directories-first
                             list directories before files, or not (default)
        --disable-natsort, --enable-natsort
                             order names byte by byte: file10 before file2
        --show-itemcount, --hide-itemcount
                             show or hide the item count column (default: hide)
    -o, --output FILE        scan without UI, write ncdu JSON (- is stdout)
    -f, --file FILE          browse a tree from ncdu JSON (- is stdin)
    -0                       no feedback while scanning (default with -o -)
    -1                       one line of progress (default with -o FILE)
    -2                       browse while scanning (default)
    -q, --slow-ui-updates, --fast-ui-updates
                             update the screen every 2 s, or 10 times a second
    -e, --extended, --no-extended
                             record owner, group, mode and mtime of entries
        --show-mtime, --hide-mtime
                             show or hide the mtime column (needs -e)
        --ignore-config      do not read the config files
    -h, --help               show this help
    -V, --version            show version
```


Options you always want can go into a config file; see [Configuration](#configuration).

Examples:

```sh
icdu /                          # browse / live, on-disk usage
icdu ~ --exclude .cache --exclude node_modules
icdu / -x -o backup.json        # headless scan of the root filesystem only to backup.json
icdu -f backup.json             # browse a previous dump
```

### Configuration

Default options are read from config files, in ncdu's format: one option per line, written as
on the command line, with its value after it on the same line.

```
# Always record owner, mode and time
-e
# The value is the rest of the line, as it stands: no quotes
--exclude My Documents
--sort name
# A line with @ in front may fail: this file need not exist
@--exclude-from ~/.ncduexcludes
```

Empty lines and lines that start with `#` are ignored. 
Four files are read, each overriding the ones before it, and the command line overrides them
all:

1. `/etc/ncdu.conf` and
2. `$XDG_CONFIG_HOME/ncdu/config` (`~/.config/ncdu/config` if the variable is not set): ncdu's
   own files, so that a setup you have for ncdu applies to icdu as well.
3. `/etc/icdu.conf` and
4. `$XDG_CONFIG_HOME/icdu/config` (`~/.config/icdu/config`)

`--ignore-config` reads none of them.

### Keys

| Key | Action |
|-----|--------|
| `j` / `k`, `↓` / `↑` | move selection |
| `l` / `Enter` / `→` | enter directory |
| `h` / `<` / `Backspace` / `←` | go up |
| `Home` / `End`, `G` | jump to top / bottom |
| `PgUp` / `PgDn`, `Ctrl-d` / `Ctrl-u` | page up / down |
| `s` | sort by size (the one on screen); again reverses the order |
| `n` | sort by name; again reverses |
| `C` | sort by the number of items; again reverses |
| `M` | sort by modification time (needs `-e`); again reverses |
| `t` | directories first, or not |
| `/` | search by name, path (`a/b`) or pattern (`*.iso`), larger than a size (`>1G`); `Enter` jumps to the selected match, `Tab` limits the search to the current directory |
| `]` / `[` | go to the next / previous match of the last search |
| `a` | toggle apparent size ↔ on-disk usage |
| `e` | show or hide hidden entries: dot files, and what the scan excluded |
| `g` | bar and percentage: both, neither, the bar, the percentage |
| `c` | toggle the item count column (entries below each directory) |
| `m` | toggle the modification time column (needs `-e`) |
| `d` | delete selected entry (asks to confirm) |
| `T` | show the largest files under the current directory |
| `r` | rescan the current directory in place |
| `S` | stop the scan; what it has not read stays marked as unfinished, and `r` reads it |
| `i` | show details of the selected entry |
| `E` | export the tree to an ncdu-format JSON file |
| `x` | jump to the next read-error entry (`!` marker), wrapping around; the footer shows the reason |
| `b` | spawn a shell in the current directory |
| `o` | open selected entry (default app; directories in the file manager) |
| `?` | show the help dialog (it scrolls with `j` / `k` when the terminal is too small for it) |
| `q` | quit |
| `Esc` | close a popup (it never quits) |
| `Ctrl-l` | redraw the screen |
| `Ctrl-z` | suspend to the shell; `fg` brings the UI back |

Every row starts with a flag, the same ones as in ncdu, and the name of a directory starts
with `/`:

| Flag | Meaning |
|---|---|
| `!` | the directory or entry could not be read |
| `.` | something further down could not be read, so the size may be too small |
| `<` | excluded: by a pattern, as a cache directory, or by the mtime filters |
| `>` | a directory on another filesystem, not entered (`-x`) |
| `^` | a kernel filesystem, not entered (`--exclude-kernfs`) |
| `H` | a hard link whose file is counted at another of its names |
| `@` | neither a file nor a directory: a symlink, device, socket or fifo |
| `e` | an empty directory |
| `F` | a firmlink (only in dumps that ncdu wrote on macOS) |

### While the scan is running

Sizes are live, so the screen says which of them are final:

- A directory whose size is followed by `…` and shown dimmed has not been read completely
  below. Its size is still growing, and `d` and `r` are not available on it yet. If you
  press one of them anyway, icdu says how many directories are left in it, reads that
  directory before anything else, and tells you in the footer when it is ready.
- The header shows how many directories below the one you are viewing are still to be read.

The same `…` marks unfinished directories in the search results.

### Searching

`/` finds what you type in a name. With a `/` in it, the query is found in the path (`src/main`, `src/` for the directory, `/src` from the top); with `*` or `?` it is a pattern the whole name has to match (`*.iso`). A word `>1G` at the end keeps only what is larger (`*.iso >1G`, or `>1G` alone).
`↑`/`↓` and `PgUp`/`PgDn` move, `Enter` jumps to the directory
containing the selected match with it highlighted, and `Esc` closes the popup.
The search is kept when the popup closes. 
`Tab` switches what is searched: the whole tree (the default), or only what lies below the
directory you are viewing.
The UI stays responsive even on trees with
millions of entries.

### Deleting

Press `d` to delete the selected file or directory; a confirmation dialog appears first.

**Only what the scan listed is deleted.**  So:

- A file that **appeared after the scan** stays, and so do the directories that hold it.
- A directory the scan **left out** (`--exclude`, `-x`, the mtime filters) or could not read is
  never emptied. `d` on such a directory is refused; inside a larger deletion it is left in place.
- An entry that has **changed identity** since the scan is not touched: a directory that became a
  symlink, a directory that now has another filesystem mounted on it, a file that was replaced.

When something is left behind, the footer says what and why, and the directory is read again so
the list shows what is really there.

Further guards:

- **Read-only** (`-r`) disables deletion entirely, and so does browsing an imported dump (`-f`).
- A directory cannot be deleted until its whole subtree has finished scanning. 

The removal itself runs on a background thread, so deleting a huge directory shouldn't freeze the UI.

Quitting (`q` / `Ctrl-C`) while a deletion is running does not abort it silently: the first
press warns, and a second press force-quits — which interrupts the removal wherever it is and
leaves the rest on disk. Any other key disarms the prompt.

### Hard links, exclusions, filesystems

- **Hard links** (`nlink > 1`) are detected by `(device, inode)`. Each inode is counted **once**
  (the first link the scan delivers); further links are shown with their size, marked `H`, and
  add nothing to any total — so a tree's grand total isn't inflated by hard links. 
- **Symlinks** are counted as the links they are, and never followed. With `-L` a symlink to
  a file is counted as that file instead: the entry has the file's size, type and owner, and
  every such link counts on its own, as in ncdu (a file with two links to it is there three
  times). 
- `--exclude PATTERN` takes a shell pattern (`*`, `?`, `[...]` with ranges, `[!...]` and
  `[[:alpha:]]`, `\` before a character to take it literally) and matches it the way ncdu does:
  against an entry's whole path, and against every tail of that path that starts after a `/`.
  So `node_modules` excludes every entry of that name, `src/build` only a `build` inside a
  `src`, and `/var/cache` exactly that directory. A `*` or `?` also matches a `/`: `build/*`
  excludes everything directly inside any `build` and leaves the directory itself.
  Excluded entries stay visible (marked `<`) but are not descended into and don't count.
- `--exclude-caches` leaves out the directories that are tagged as caches: those with a
  `CACHEDIR.TAG` file that begins with the signature of the
  [Cache Directory Tagging Specification](https://bford.info/cachedir/), which cargo and many
  other build tools and package managers write. They stay visible (marked `<`) and are neither
  read nor counted. `--include-caches`, the default, turns it off again.
- `--exclude-kernfs` leaves out the Linux kernel's pseudo filesystems wherever they are
  mounted: `proc`, `sysfs`, `cgroup`, `cgroup2`, `debugfs`, `tracefs`, `devpts`, `securityfs`,
  `pstore`, `bpf`, `binfmt_misc` and `selinuxfs`, the same list as ncdu's. Their directories
  are shown (`<kernel filesystem>`) but not entered, so `icdu --exclude-kernfs /` is not
  thrown off by `/proc/kcore` or slowed down by `/sys`. Which devices those are is read from
  `/proc/self/mountinfo` when the scan starts. On macOS the option is accepted and does
  nothing. `--include-kernfs`, the default, turns it off again.
- `-X FILE` reads one pattern per line. As in ncdu a line is taken as it stands: spaces belong
  to the pattern and a `#` does not start a comment. Empty lines are skipped.
- `-x` keeps the scan on the root's filesystem; mount points on other devices are shown but not
  traversed.
- `--older-than DUR` / `--newer-than DUR` exclude entries by modification time (durations:
  `s`/`m`/`h`/`d`/`w`/`y`, bare numbers = days). Filtered entries stay visible (marked
  `<mtime filter>`) but are not descended into and don't count — e.g. `icdu ~ --newer-than 1y`
  surfaces the data you haven't touched in a year.
- **Names that are not valid UTF-8** are shown with the replacement character (`�`) in place of
  the bytes that cannot be displayed. Everything that touches the entry (delete, open, the
  shell, rescan) uses its real bytes, and so does `-o`.

## Differences from ncdu

icdu aims to do what ncdu does wherever ncdu does it. Checked against ncdu 1.18 on the same
trees, the dumps of the two agree entry for entry.

**What icdu adds**

- The browser while the scan is still running, with the directory on screen scanned first.
- The search: `/`, and `]` / `[` through its matches.
- `T` (largest files), `x` (next read error), `o` (open), `E` (export from the browser).
- `--older-than` / `--newer-than`.
- A check before every removal: an entry is only deleted if it is still the one that was
  scanned (same type, device and inode).

**What icdu does not have**

- ncdu 2's binary export and its compression (`-O`, `-c`, `--compress`, `--compress-level`,
  `--export-block-size`). The flags are refused by name, and such files cannot be read. 
- The column for sizes that directories share through hard links (`u`, `--shared-column`).
  The flag is accepted and ignored.

**What is different**

- Times are shown in UTC, not in local time.
- Bar and percentage are both shown by default; ncdu shows the bar. `--hide-percent` gives
  ncdu's look.
- An imported dump can never be acted on: delete, shell, open and rescan stay off, also with
  `--enable-delete`.
- `-rr` also turns off opening and the export key, since both touch the local filesystem.
- **Hard links.** icdu counts a file with several names at one of them, the first the scan
  comes across; the other names are listed with their size, flagged `H`, and count nowhere.
  ncdu (1.7 and later) counts such a file once in every directory that holds one of its
  names. The total of the scanned directory is the same in both. The size of a directory
  below it can differ where a file's names lie in different directories: ncdu's is larger
  for every directory that holds only names icdu flags `H`.
- A directory excluded by a pattern is written to a dump as a directory, and with its owner
  and time under `-e`. ncdu writes it as a plain entry without them, because it never looks
  at it. Both programs read both forms.
- In a config file, `--option=value` and a leading `@` work as in ncdu 2, also where
  ncdu 1 would refuse them.

## Limitations

- **Memory.** The whole tree is kept in memory: about 165 bytes per entry (331 MB for a tree
  of two million entries; names longer than 24 bytes add their length, `-e` adds 32 bytes).
- **What was deleted or rescanned stays in memory** until icdu exits. A session with many
  rescans of large directories grows.
- **Paths longer than the system's limit** (4096 bytes on Linux, 1024 on macOS): the entry at
  which the limit is passed is flagged `!`, and nothing below it is scanned or counted.
- **The search is by name.** There is no search by path, size or type.
- **Kernel filesystems** (`--exclude-kernfs`) are known from the mount table when the scan
  starts. One that is mounted later is not recognised until the next scan or rescan.

## Design

| Module | Responsibility |
|--------|----------------|
| `scan.rs` | Parallel streaming scanner with a priority frontier (focused subtree first). Entries arrive in batches, a huge directory in several; excludes, one-filesystem. Reading directories is mostly waiting for the storage, so many threads read at once (four per core). That pays off on SSD and NVMe; on a spinning disk fewer are better, which is what `-t` is for. |
| `model.rs` | The tree owned by the UI thread. Lean `Node` (`CompactString` names, derived own-size); grafts batches, propagates *counted* sizes up every ancestor, counts each hard-linked inode once, buffers out-of-order batches, tracks per-subtree "unscanned dirs" for the delete gate, and takes deleted subtrees out. |
| `delete.rs` | Removal from disk, driven by the tree: a copy of the scanned subtree is walked on a worker thread, each entry checked against the scan (type, device, inode) before it is unlinked. Nothing the scan did not list is removed. |
| `glob.rs` | The `--exclude` patterns: `fnmatch`-style, compiled once, matched against paths as ncdu does. |
| `matcher.rs` | Matching a search query against names: case-insensitive for all letters, and across the two Unicode spellings of accented Latin letters. |
| `dump.rs` | ncdu-compatible JSON import/export, both streaming: a hand-written writer, and a pull parser for exactly this format. |
| `app.rs` | App state + key handling, modals (help/confirm), background delete and open. Selection tracks node *identity*, so the highlight doesn't jump as live updates reorder the list. |
| `ui.rs` | ratatui rendering: size bars, percentages, type indicators, popups. |
| `main.rs` | Args, modes (scan / import / export), terminal lifecycle, event loop. |

## Contributing, changes, security

- [CONTRIBUTING.md](CONTRIBUTING.md): how to build, test and propose a change.
- [CHANGELOG.md](CHANGELOG.md): what changed in each release.
- [SECURITY.md](SECURITY.md): how to report a problem that could cost someone data. Please
  not in a public issue.

## Origin

This project started as a fork of [rcdu](https://github.com/mayankasthana/rcdu) by Mayank Asthana
and has been developed independently since version 0.4.0.
