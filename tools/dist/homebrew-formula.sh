#!/bin/sh
# Print the Homebrew formula for a release: Formula/icdu.rb of the tap.
#
#   tools/dist/homebrew-formula.sh VERSION SHA256SUMS [BASE_URL]
#
# VERSION is the release (0.5.0). SHA256SUMS is its sha256sums.txt, which has to list the
# four archives. BASE_URL is where the archives are downloaded from; the default is the
# release's download address. (CI passes a file:// address, to try the formula on archives
# it has just built.)
#
# The formula installs the prebuilt binary for the machine, with the manual page.
set -eu

[ $# -eq 2 ] || [ $# -eq 3 ] || { sed -n '2,11p' "$0" | sed 's/^# \{0,1\}//'; exit 2; }
version=$1 sums=$2
base=${3:-https://github.com/anhofmann/icdu/releases/download/v$version}

sum() {
  found=$(awk -v file="icdu-$version-$1.tar.gz" '$2 == file { print $1 }' "$sums")
  [ -n "$found" ] || { echo "homebrew-formula.sh: $sums does not list icdu-$version-$1.tar.gz" >&2; exit 1; }
  printf '%s' "$found"
}
# All four, before anything is printed.
mac_arm=$(sum aarch64-apple-darwin)
mac_intel=$(sum x86_64-apple-darwin)
linux_arm=$(sum aarch64-unknown-linux-musl)
linux_intel=$(sum x86_64-unknown-linux-musl)

cat <<FORMULA
class Icdu < Formula
  desc "Disk usage explorer that can be browsed and searched while it scans"
  homepage "https://github.com/anhofmann/icdu"
  version "$version"
  license "MIT"

  on_macos do
    on_arm do
      url "$base/icdu-$version-aarch64-apple-darwin.tar.gz"
      sha256 "$mac_arm"
    end
    on_intel do
      url "$base/icdu-$version-x86_64-apple-darwin.tar.gz"
      sha256 "$mac_intel"
    end
  end

  on_linux do
    on_arm do
      url "$base/icdu-$version-aarch64-unknown-linux-musl.tar.gz"
      sha256 "$linux_arm"
    end
    on_intel do
      url "$base/icdu-$version-x86_64-unknown-linux-musl.tar.gz"
      sha256 "$linux_intel"
    end
  end

  def install
    bin.install "icdu"
    man1.install "icdu.1"
  end

  test do
    assert_equal "icdu #{version}", shell_output("#{bin}/icdu --version").strip
    (testpath/"dir/sub").mkpath
    (testpath/"dir/sub/file").write "hello"
    dump = shell_output("#{bin}/icdu --ignore-config -o- #{testpath}/dir")
    assert_match(/\A\[1,2,\{"progname":"icdu"/, dump)
    assert_match "\"name\":\"file\"", dump
  end
end
FORMULA
