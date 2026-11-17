#!/bin/sh
# Print the PKGBUILD of the AUR package icdu-bin for a release.
#
#   tools/dist/aur-pkgbuild.sh VERSION SHA256SUMS [BASE_URL]
#
# VERSION is the release (0.5.0). SHA256SUMS is its sha256sums.txt, which has to list the
# two Linux archives. BASE_URL is where the archives are downloaded from; the default is the
# release's download address. (CI passes a file:// address, to build the package from
# archives it has just made.)
#
# The package installs the prebuilt static binary, the manual page and the licence. Next to
# the PKGBUILD the AUR wants a .SRCINFO: `makepkg --printsrcinfo > .SRCINFO`.
set -eu

[ $# -eq 2 ] || [ $# -eq 3 ] || { sed -n '2,12p' "$0" | sed 's/^# \{0,1\}//'; exit 2; }
version=$1 sums=$2
# In the PKGBUILD the default address is written with its variables, as is usual there.
base=${3:-\$url/releases/download/v\$pkgver}

sum() {
  found=$(awk -v file="icdu-$version-$1.tar.gz" '$2 == file { print $1 }' "$sums")
  [ -n "$found" ] || { echo "aur-pkgbuild.sh: $sums does not list icdu-$version-$1.tar.gz" >&2; exit 1; }
  printf '%s' "$found"
}
x86_64=$(sum x86_64-unknown-linux-musl)
aarch64=$(sum aarch64-unknown-linux-musl)

cat <<PKGBUILD
# Maintainer: icdu maintainers <anhofmann at users dot noreply dot github dot com>
pkgname=icdu-bin
pkgver=$version
pkgrel=1
pkgdesc="Disk usage explorer that can be browsed and searched while it scans (ncdu-compatible)"
arch=('x86_64' 'aarch64')
url="https://github.com/anhofmann/icdu"
license=('MIT')
provides=('icdu')
conflicts=('icdu')
source_x86_64=("$base/icdu-\$pkgver-x86_64-unknown-linux-musl.tar.gz")
source_aarch64=("$base/icdu-\$pkgver-aarch64-unknown-linux-musl.tar.gz")
sha256sums_x86_64=('$x86_64')
sha256sums_aarch64=('$aarch64')

package() {
  cd "icdu-\$pkgver-\$CARCH-unknown-linux-musl"
  install -Dm755 icdu "\$pkgdir/usr/bin/icdu"
  install -Dm644 icdu.1 "\$pkgdir/usr/share/man/man1/icdu.1"
  install -Dm644 LICENSE "\$pkgdir/usr/share/licenses/\$pkgname/LICENSE"
}
PKGBUILD
