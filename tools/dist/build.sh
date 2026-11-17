#!/bin/sh
# Build the release binary for one target. This is the one recipe: CI and the release workflow
# call it, and it is what to run by hand.
#
#   tools/dist/build.sh TARGET
#
#   x86_64-unknown-linux-musl, aarch64-unknown-linux-musl
#       A fully static binary. Builds on any host: it is linked with the rust-lld that
#       comes with the Rust toolchain, so no cross compiler and no C library are needed.
#   aarch64-apple-darwin, x86_64-apple-darwin
#       Builds on macOS only.
#
# The last line it prints (the only one on standard output) is the path of the binary.
set -eu

[ $# -eq 1 ] || { sed -n '2,13p' "$0" | sed 's/^# \{0,1\}//'; exit 2; }
target=$1
cd "$(dirname "$0")/../.."

case "$target" in
  x86_64-unknown-linux-musl | aarch64-unknown-linux-musl)
    host=$(rustc -vV | sed -n 's/^host: //p')
    lld="$(rustc --print sysroot)/lib/rustlib/$host/bin/rust-lld"
    [ -x "$lld" ] || { echo "build.sh: no rust-lld at $lld" >&2; exit 1; }
    rustup target add "$target"
    RUSTFLAGS="-C linker=$lld -C linker-flavor=ld.lld -C link-self-contained=yes" \
      cargo build --release --locked --target "$target"
    ;;
  aarch64-apple-darwin | x86_64-apple-darwin)
    # Never let a build made elsewhere carry the name of a macOS target.
    [ "$(uname -s)" = Darwin ] || { echo "build.sh: $target can only be built on macOS" >&2; exit 1; }
    rustup target add "$target"
    cargo build --release --locked --target "$target"
    ;;
  *)
    echo "build.sh: no release build for '$target'" >&2
    exit 2
    ;;
esac

# Where cargo put it: `target/`, unless CARGO_TARGET_DIR or a config file says otherwise.
dir=$(cargo metadata --format-version 1 --no-deps --locked |
  sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')
echo "$dir/$target/release/icdu"
