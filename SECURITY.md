# Security

icdu deletes files when asked to, and it reads dump files that may come from other machines
and other people. A mistake in either can cost data. Reports about such mistakes are welcome,
and they are handled before anything else.

## What to report here

- icdu removing, overwriting or changing something the user did not confirm, or something
  other than what was on screen.
- A dump file (`-f`) that makes icdu touch the local filesystem, run something, crash, hang
  or use unbounded memory.
- A file or directory name, or the content of a directory, that makes icdu do any of that
  during a scan.
- The install script installing something other than the release it names.
- Anything that lets one user of a machine affect another through icdu.

A wrong size, a display problem or a missing ncdu feature is an ordinary bug: please open a
normal issue.

## How to report

Privately, through GitHub: **[Report a vulnerability](https://github.com/anhofmann/icdu/security/advisories/new)**
(the "Security" tab of the repository). Only the maintainer sees such a report. Please do not
open a public issue for something that could cost other users data before there is a fix.

Useful in a report: the version (`icdu --version`), the operating system and filesystem, the
command line, and the smallest directory tree or dump file that shows the problem.

You will get an answer on the report itself. Once a fix is released, the report is published
with credit to you, unless you prefer otherwise.

## Which versions get fixes

The latest release. There are no maintenance branches for older ones.

## What is done to keep it safe

- Deleting removes only what the scan listed, and only if each entry is still the one that was
  scanned (type, device, inode). See "Deleting" in the README.
- An imported dump is never taken for the local filesystem: delete, shell, open and rescan are
  off, and its names are checked.
- icdu itself never uses the network.
- Release binaries are built by a public workflow from a tagged commit, with pinned actions.
- The dependencies are checked against the RustSec advisory database every day.
