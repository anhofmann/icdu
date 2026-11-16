#!/bin/sh
# icdu installer — downloads a portable binary from GitHub Releases.
#
#   curl -fsSL https://raw.githubusercontent.com/anhofmann/icdu/main/install.sh | sh
#
# Environment overrides:
#   ICDU_VERSION=v0.5.0    install a specific release instead of the latest
#   ICDU_INSTALL_DIR=...   install location (default: ~/.local/bin)
#
# Releases up to v0.4.1 were published under the project's former name, rcdu. Asked for one of
# those, the script installs it as `rcdu`, which is what that binary is.
set -eu

REPO="anhofmann/icdu"

log()  { printf '%s\n' "$*"; }
warn() { printf 'warning: %s\n' "$*" >&2; }
die()  { printf 'error: %s\n' "$*" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }

# --- platform -----------------------------------------------------------
OS=$(uname -s)
ARCH=$(uname -m)
case "$OS" in
  Darwin) os=macos ;;
  Linux)  os=linux ;;
  *) die "unsupported OS '$OS' — prebuilt binaries cover macOS and Linux (build from source with: cargo install --git https://github.com/$REPO)" ;;
esac
case "$ARCH" in
  x86_64 | amd64)  arch=x86_64 ;;
  arm64 | aarch64) arch=aarch64 ;;
  *) die "unsupported architecture '$ARCH' — prebuilt binaries cover x86_64 and aarch64" ;;
esac

NAME=icdu
ASSET="$NAME-portable-$os-$arch"
VERSION="${ICDU_VERSION:-latest}"
if [ "$VERSION" = "latest" ]; then
  BASE="https://github.com/$REPO/releases/latest/download"
else
  BASE="https://github.com/$REPO/releases/download/$VERSION"
fi

# --- download -----------------------------------------------------------
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT INT TERM

log "fetching $ASSET from $REPO $VERSION ..."
if ! curl -fsSL --retry 3 -o "$TMP/bin" "$BASE/$ASSET" 2>/dev/null; then
  # No such asset: either the release does not exist, or it predates the rename.
  NAME=rcdu
  ASSET="$NAME-portable-$os-$arch"
  curl -fsSL --retry 3 -o "$TMP/bin" "$BASE/$ASSET" ||
    die "download failed — does release $VERSION exist? (see https://github.com/$REPO/releases)"
  warn "that release predates the rename to icdu: installing it under its old name, $NAME"
fi
curl -fsSL --retry 3 -o "$TMP/sha256sums.txt" "$BASE/sha256sums.txt" ||
  die "could not download checksums for release $VERSION"

# --- verify -------------------------------------------------------------
if have sha256sum; then
  actual=$(sha256sum "$TMP/bin" | awk '{print $1}')
elif have shasum; then
  actual=$(shasum -a 256 "$TMP/bin" | awk '{print $1}')
elif have openssl; then
  actual=$(openssl dgst -sha256 "$TMP/bin" | awk '{print $NF}')
else
  die "no sha256 tool found (need one of: sha256sum, shasum, openssl)"
fi
expected=$(awk -v a="$ASSET" '$2 == a { print $1 }' "$TMP/sha256sums.txt")
[ -n "$expected" ] || die "$ASSET not listed in sha256sums.txt — refusing to install"
[ "$actual" = "$expected" ] ||
  die "checksum mismatch for $ASSET (expected $expected, got $actual) — aborting"

# --- install ------------------------------------------------------------
DEST="${ICDU_INSTALL_DIR:-$HOME/.local/bin}"
mkdir -p "$DEST" 2>/dev/null || die "cannot create $DEST (try sudo, or set ICDU_INSTALL_DIR)"
[ -w "$DEST" ] || die "$DEST is not writable (try sudo, or set ICDU_INSTALL_DIR)"
install -m 0755 "$TMP/bin" "$DEST/$NAME"

case ":$PATH:" in
  *":$DEST:"*) ;;
  *) warn "$DEST is not on your PATH. Add it:  export PATH=\"$DEST:\$PATH\"" ;;
esac

log "installed $("$DEST/$NAME" --version) to $DEST"
