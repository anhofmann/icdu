---
name: Bug report
about: Something icdu does wrong
labels: bug
---

**What happened, and what did you expect?**

**How to see it**

The command line, and the keys pressed. If it depends on the directory, the smallest tree
that shows it (or a dump made with `icdu -o dump.json DIR`, if it holds nothing private).

**Where**

- `icdu --version`:
- Operating system and version:
- Filesystem (`df -T DIR` on Linux, `mount` on macOS):
- How icdu was installed (install script, cargo, a package):

If icdu removed or changed something it should not have, or a dump file made it misbehave,
please do not write it here: see SECURITY.md for the private way to report it.
