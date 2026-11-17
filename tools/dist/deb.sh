#!/bin/sh
# Make a Debian package out of a release binary.
#
#   tools/dist/deb.sh TARGET BINARY OUT_DIR
#
# TARGET is x86_64-unknown-linux-musl (amd64) or aarch64-unknown-linux-musl (arm64). Writes
# OUT_DIR/icdu_VERSION_ARCH.deb, with the version from Cargo.toml, and prints its path.
#
# The package holds the binary, the manual page, a copyright file and a changelog. The
# binary is static, so the package depends on nothing. It is built with dpkg-deb alone.
set -eu

[ $# -eq 3 ] || { sed -n '2,10p' "$0" | sed 's/^# \{0,1\}//'; exit 2; }
target=$1 binary=$2 out=$3
top=$(cd "$(dirname "$0")/../.." && pwd)

case "$target" in
  x86_64-unknown-linux-musl)  arch=amd64 ;;
  aarch64-unknown-linux-musl) arch=arm64 ;;
  *) echo "deb.sh: no Debian package for '$target'" >&2; exit 2 ;;
esac
version=$(sed -n 's/^version = "\(.*\)"$/\1/p' "$top/Cargo.toml" | head -n 1)
[ -n "$version" ] || { echo "deb.sh: no version in Cargo.toml" >&2; exit 1; }
# The same package from the same commit, whenever it is built.
epoch=${SOURCE_DATE_EPOCH:-$(git -C "$top" log -1 --format=%ct)}
export SOURCE_DATE_EPOCH="$epoch"

stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT INT TERM
chmod 0755 "$stage"
doc="$stage/usr/share/doc/icdu"
mkdir -p "$stage/DEBIAN" "$stage/usr/bin" "$stage/usr/share/man/man1" "$doc"

install -m 0755 "$binary" "$stage/usr/bin/icdu"
gzip -9n < "$top/doc/icdu.1" > "$stage/usr/share/man/man1/icdu.1.gz"

cat > "$doc/copyright" <<COPYRIGHT
Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/
Upstream-Name: icdu
Source: https://github.com/anhofmann/icdu

Files: *
Copyright: $(sed -n 's/^Copyright (c) //p' "$top/LICENSE" | paste -sd ';' - | sed 's/;/\n           /g')
License: Expat
$(sed -e '1,/^Copyright/d' -e '/^Copyright/d' -e '/./,$!d' -e 's/^$/./' -e 's/^/ /' "$top/LICENSE")
COPYRIGHT

# What changed is in the project's changelog; this is the form Debian's tools expect.
{
  printf 'icdu (%s) unstable; urgency=medium\n\n' "$version"
  printf '  * Release %s. What changed:\n' "$version"
  printf '    https://github.com/anhofmann/icdu/blob/v%s/CHANGELOG.md\n\n' "$version"
  printf ' -- Ansgar Hofmann <mail@ahofmann.org>  %s\n' \
    "$(date -u -R -d "@$epoch" 2>/dev/null || date -u -R -r "$epoch")"
} | gzip -9n > "$doc/changelog.gz"

# The binary is static on purpose: one file that runs on any Linux. Debian's checker counts
# that as a mistake unless the package says that it is meant.
mkdir -p "$stage/usr/share/lintian/overrides"
cat > "$stage/usr/share/lintian/overrides/icdu" <<OVERRIDES
# A fully static binary is the point of this package: it has no dependencies.
icdu: statically-linked-binary [usr/bin/icdu]
# It is position independent as well, which makes it look like a library without any.
icdu: shared-library-lacks-prerequisites [usr/bin/icdu]
OVERRIDES

chmod 0644 "$stage/usr/share/man/man1/icdu.1.gz" "$doc/copyright" "$doc/changelog.gz" \
  "$stage/usr/share/lintian/overrides/icdu"
installed=$(du -sk "$stage/usr" | cut -f1)
cat > "$stage/DEBIAN/control" <<CONTROL
Package: icdu
Version: $version
Architecture: $arch
Maintainer: Ansgar Hofmann <mail@ahofmann.org>
Installed-Size: $installed
Section: utils
Priority: optional
Homepage: https://github.com/anhofmann/icdu
Description: disk usage explorer that can be browsed while it scans
 icdu shows where the space on a disk has gone. Its list can be walked
 through, sorted and searched while the scan is still running, and the
 directory on screen is scanned first.
 .
 It is compatible with ncdu: the same keys and options, ncdu's
 configuration files, and ncdu's JSON export format.
CONTROL

find "$stage" -exec touch -h -d "@$epoch" {} +
mkdir -p "$out"
deb="$out/icdu_${version}_$arch.deb"
dpkg-deb --root-owner-group -Zxz --build "$stage" "$deb" >&2
echo "$deb"
