# Changelog

What changed from one release to the next, for people who use icdu. The layout follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Version numbers are read as in
[Semantic Versioning](https://semver.org/); before 1.0 a new minor version may change how
things work.

Up to version 0.4.1 the program was called **rcdu**. Versions 0.1.0 to 0.3.0 were released by
the [original project](https://github.com/mayankasthana/rcdu). The numbers in brackets at the
end of older commit subjects are pull requests of that project, or of this one before its
history was separated; they do not match the pull requests of this repository.

## [Unreleased]

### Added

- **`S` stops a running scan or rescan** and keeps what it has read. Directories it has not
  read stay marked as unfinished and cannot be deleted; `r` reads one of them. The question
  of `--confirm-quit` offers it too (#43).
- **The search finds paths and patterns** (#50). A query with a `/` is looked for in the
  path: `src/main` finds `…/src/main.rs`, `src/` the directories called `src`, and `/src`
  only the one at the top. A query with `*` or `?` is a pattern the whole name has to match:
  `*.iso` finds the names that end in `.iso`.

### Changed

- **The footer shows how many entries the scan reads per second** (`files/s`), over the last
  30 seconds, instead of the size found per second. The size said nothing about the speed of
  the scan: one big file made it look fast. The new figure drops when the scan slows down,
  e.g. on reaching a slow mount.
- While a scan runs, the number of directories left to read was on screen twice, in the
  header and in the footer. The footer has it only where it is another number: the whole
  tree's, while the header counts what is below the directory on screen.

### Fixed

- **Closing the terminal left icdu running** (0.5.0): the process stayed, kept one processor
  core busy, and did not react to `SIGTERM` or `SIGHUP`, so that only `kill -9` ended it. It
  ends now when its terminal goes away, with or without a hang-up signal.
- **`SIGTERM` and `SIGHUP` end icdu whatever it is doing**, within about a second: also while
  a shell (`b`) or the delete command has the terminal, where it used to wait for that command
  to finish, and while the terminal takes no output.
- `SIGINT` and `SIGQUIT` from outside ended icdu with the terminal still in raw mode on the
  alternate screen. It gives the terminal back first now, as for `SIGTERM`.
- `Ctrl-C` typed in a command that icdu started (`NCDU_SHELL`, the delete command) ended
  icdu along with the command. It is the command's alone now.
- `SIGTSTP` did nothing while a shell or the delete command had the terminal. It stops icdu.
- `Ctrl-Z` froze the terminal where icdu ran without a shell above it (`ssh -t host icdu /`,
  `xterm -e icdu`): icdu stopped, and nothing was there to continue it. Stopping is left to
  the system now, which does not stop a program in that position, and icdu says that it was
  not suspended. A `SIGTSTP` sent from outside stops icdu as it is, like `SIGSTOP`; the
  screen is set up again when it is continued.
- Quitting left the cursor hidden, for the shell to switch on again.
- `Ctrl-L`, and coming back after `Ctrl-Z`, ended icdu with an error on a terminal that does
  not answer when asked for the cursor position. icdu no longer asks.
- Started without a terminal, icdu aborted with a panic message. It says that it has no
  terminal and exits with status 1, and it says so before an import or a scan (`-0`, `-1`)
  that nobody would get to see.
- Output that nobody reads any more (`icdu -o- / | head`, `icdu --help | head -1`) ended in
  "export failed: Broken pipe", or in a panic. icdu ends quietly there, as other programs do.
- The install script refused to install when the GitHub CLI on the machine was logged in but
  too old to have `gh attestation` (the one in Debian 12, for example). It installs now and
  says that it could not check who built the archive.

## [0.5.0] - 2026-11-17

The first release under the name icdu, with everything since rcdu 0.4.1.

### Changed, and needing attention when upgrading

- **The program is called icdu now** ("interactive ncdu"): binary, crate, repository, release
  assets, and the environment variables of the install script (`ICDU_VERSION`,
  `ICDU_INSTALL_DIR`). Exports from the browser are named `icdu-dump.json`.
- **The self-update command is gone.** Upgrade by running the install script again. icdu
  never uses the network.
- **Keys follow ncdu.** `Esc` no longer quits. Export moved from `e` to `E`, the largest
  files from `t` to `T`, jump to top from `g` to `Home`. `r` and `b` act on the directory on
  screen. `s` sorts by size and `n` by name (`s` used to switch between the two); `n` is
  still "no" in a question.
- **One search instead of filter and search.** `/` searches the whole tree; `Tab` limits it
  to the directory on screen. The old `f` key says where it went.
- **`--exclude` patterns see the path**, as in ncdu: a pattern with `/` in it has an effect
  now, and a wildcard can match across directories. A file given to `-X` has no comment lines
  any more, and its lines are not trimmed.
- **Names sort in natural order** (`file2` before `file10`). `--disable-natsort` gives the
  old order.
- **Rows start with ncdu's flag**, and directory names with `/`. Special files are flagged
  `@` instead of `*`.
- Dumps written by icdu leave out what ncdu leaves out (sizes of zero, the device where it
  is the parent's, the inode except for hard links). Readers of the format are not affected.
- Building from source needs Rust 1.88 or newer.
- **Releases are archives** named `icdu-<version>-<target>.tar.gz`, with the licence, the README
  and the changelog next to the binary, and GitHub keeps an attestation of where each was
  built. The bare binaries `…-portable-<os>-<arch>` are no longer published.

### Added

- **ncdu's command line.** Bundled short flags, values attached or separate, `--option=value`,
  `--`, and exit status 1 for a mistake. New flags, all with the counterpart that undoes them:
  `-0` `-1` `-2` `-q` (what to show while scanning), `-e` (owner, group, mode and time, in
  the tree and in dumps), `-L` (count symlinked files), `--exclude-caches`, `--exclude-kernfs`,
  `--sort`, `--group-directories-first`, `--show-hidden`, `--show-graph`, `--show-percent`,
  `--show-mtime`, `--graph-style`, `--si`, `--color`, `--disable-delete`, `--disable-shell`,
  `--disable-refresh`, `-rr`, `--confirm-quit`, `--confirm-delete`, `--delete-command`,
  `--disk-usage`, `--cross-file-system`, `--ignore-config`.
- **Config files**, in ncdu's format. ncdu's own files are read, so an existing setup
  applies, and then `/etc/icdu.conf` and `~/.config/icdu/config`.
- **ncdu's keys**: `n` `C` `M` sort by name, item count and time, and the key of the active
  column reverses the order; `t` puts directories first; `e` hides dot files and excluded
  entries; `g` chooses bar and percentage; `m` shows modification times.
- **Search**: a broad search lists its largest matches and counts all of them; letters match
  whatever their case or Unicode spelling; `]` and `[` step through the matches of the last
  search, and `/` brings it back.
- **While the scan runs**: the footer says how many directories are left; sizes that are still
  growing are marked; a delete or rescan that has to wait says for how much, and that part is
  read first; a directory with very many entries shows up in parts.
- **The shell** (`b`) uses `NCDU_SHELL`, then `SHELL`, then `/bin/sh`, and sets `NCDU_LEVEL`.
- **The install script** takes a version with or without `v`, says what it replaced, names a
  tool it is missing, installs the ARM build in an x86 shell on Apple Silicon, and verifies the
  attestation of the archive when the GitHub CLI is there.
- **A manual page**, `icdu.1`, laid out like ncdu's. It is in the release archives.
- **Debian packages** (`.deb`) for amd64 and arm64 with every release.
- `Ctrl-Z` suspends icdu, and `Ctrl-L` redraws the screen.
- The info popup shows the reason of a read error, and the number of hard links.

### Fixed

- **Deleting could remove what the scan had never listed**: files created after the scan, the
  content of excluded or unreadable directories, other filesystems mounted below. `d` now
  removes exactly the entries the scan listed, each only if it is still the one that was
  scanned.
- Browsing an imported dump allowed delete, shell, open and rescan on its paths, which belong
  to another machine or another time. All four are off there.
- Names that are not valid UTF-8 were acted on under a different name. They are kept as the
  bytes they are, on disk and in dumps.
- A directory being rescanned or deleted could be acted on a second time, and a rescan could
  count sizes twice or let a delete through too early.
- A hard-linked file dropped out of every total when the link that was counted was deleted.
- Entries that vanished during the scan were reported as read errors.
- Sizes could overflow and wrap around.
- Letters with `Ctrl` or `Alt` acted like the plain letters (`Ctrl-E` exported).
- `kill` and a closed terminal window left the terminal in raw mode.
- Dumps: a directory nested more than about 125 levels deep could not be imported; `dev` was
  not inherited from the parent; exclusion reasons and link counts were lost; an export during
  a scan looked complete.
- Directories with a million entries made every key press take seconds. Rows are sorted once
  and only the visible ones are drawn. The largest-files popup, the next-error jump, exports
  from the browser and quitting no longer stall on large trees either.
- The help popup was cut off on terminals shorter than 28 rows.

### Security

- See the first three entries under "Fixed".
- ratatui 0.30 replaces versions of `lru` and `paste` that have advisories against them
  (RUSTSEC-2026-0002, RUSTSEC-2026-0253, RUSTSEC-2024-0436).

## [0.4.1] - 2026-11-02

### Security

- An imported dump could name entries `..` or with a `/` in them, and actions then left the
  scanned directory. Such names are refused.
- The export from the browser followed a symlink at the place it wrote to.

### Changed

- `rcdu update` and the install script fetch releases from this repository.
- The actions of the workflows are pinned to commits.

## [0.4.0] - 2026-11-02

The first release of this repository.

### Added

- `f`: search the whole tree by name.
- `c`: a column with the number of items below each directory.

## [0.3.0] - 2026-09-06

### Added

- `--older-than` and `--newer-than`: leave out entries by modification time.
- `b`: a shell in the selected directory.
- `x`: jump to the next read error.
- `e`: export the tree as ncdu JSON from the browser.
- `i`: details of the selected entry.
- The footer shows how fast the scan is going.
- `PgUp`, `PgDn`, `Ctrl-D` and `Ctrl-U` page through the list.

## [0.2.0] - 2026-09-05

### Added

- `r`: rescan the selected directory in place.
- `t`: the largest files under the current directory.
- `/`: filter the list by name.
- `o`: open the selected entry with the system's handler.
- `rcdu update`: self-update from the releases.
- An install script.

## [0.1.0] - 2026-09-05

The first release of rcdu: a disk usage explorer that can be browsed while it scans, with a
parallel scanner, delete with a safety gate, hard-link handling, `--exclude`, `-x`, and import
and export of ncdu's JSON format.

[Unreleased]: https://github.com/anhofmann/icdu/compare/v0.5.0...HEAD
[0.5.0]: https://github.com/anhofmann/icdu/compare/v0.4.1...v0.5.0
[0.4.1]: https://github.com/anhofmann/icdu/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/anhofmann/icdu/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/anhofmann/icdu/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/anhofmann/icdu/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/anhofmann/icdu/releases/tag/v0.1.0
