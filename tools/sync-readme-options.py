#!/usr/bin/env python3
"""Bring the list of options in README.md up to date with what `icdu --help` prints.

    tools/sync-readme-options.py [BINARY]

BINARY is the icdu to ask (default: target/release/icdu). Run it from the top of the
repository after adding or changing a flag in `args::FLAGS`. A test compares the two lists
(`args::tests::readme_lists_the_same_options`), so a README that was not brought up to date
fails `cargo test`.
"""
import subprocess
import sys

binary = sys.argv[1] if len(sys.argv) > 1 else "target/release/icdu"
help_text = subprocess.run([binary, "--help"], capture_output=True, text=True, check=True).stdout
options = help_text[help_text.index("OPTIONS:\n") : help_text.index("\nKEYS:")].rstrip("\n")

with open("README.md", encoding="utf-8") as f:
    readme = f.read()
start = readme.index("OPTIONS:\n")
end = readme.index("```", start)
with open("README.md", "w", encoding="utf-8") as f:
    f.write(readme[:start] + options + "\n" + readme[end:])
print(f"README.md: {options.count(chr(10))} lines of options")
