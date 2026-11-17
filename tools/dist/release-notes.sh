#!/bin/sh
# Print the section of CHANGELOG.md for one version: the notes of that release.
#
#   tools/dist/release-notes.sh 0.5.0
#
# Fails if the changelog has no section `## [0.5.0]`, or an empty one. The release workflow
# asks before it builds anything, so a release cannot go out without its notes.
set -eu

[ $# -eq 1 ] || { sed -n '2,7p' "$0" | sed 's/^# \{0,1\}//'; exit 2; }
version=$1
cd "$(dirname "$0")/../.."

notes=$(awk -v want="## [$version]" '
  index($0, want) == 1 { found = 1; next }   # the heading itself is the title of the release
  found && /^## \[/    { exit }              # the next version
  found && /^\[[^]]*\]: / { exit }           # the link definitions at the end
  found                { print }
' CHANGELOG.md | sed -e '/./,$!d')

if [ -z "$(printf '%s' "$notes" | tr -d '[:space:]')" ]; then
  echo "release-notes.sh: CHANGELOG.md has nothing under '## [$version]'" >&2
  exit 1
fi
printf '%s\n' "$notes"
