#!/bin/sh
# icdu installer: downloads a release from GitHub and puts the binary on your disk.
#
#   curl -fsSL https://raw.githubusercontent.com/anhofmann/icdu/main/install.sh | sh
#
# Run it again to upgrade. What it reads from the environment:
#
#   ICDU_VERSION=0.5.0     the release to install instead of the latest (v0.5.0 works too)
#   ICDU_INSTALL_DIR=...   where to put the binary (default: ~/.local/bin)
#
# Releases up to 0.4.1 were published under the project's former name, rcdu. Asked for one of
# those, the script installs it as `rcdu`, which is what that binary is.
#
# For testing: ICDU_BASE_URL is where the release's files are fetched from, in place of the
# release's download address. It needs ICDU_VERSION.
set -eu

REPO="anhofmann/icdu"

log()  { printf '%s\n' "$*"; }
warn() { printf 'warning: %s\n' "$*" >&2; }
die()  { printf 'error: %s\n' "$*" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }

# --- what the script needs ----------------------------------------------
for tool in curl tar uname mktemp awk install; do
  have "$tool" || die "this script needs '$tool', which was not found"
done

# --- platform -----------------------------------------------------------
OS=$(uname -s)
ARCH=$(uname -m)
case "$OS" in
  Darwin) os=macos ;;
  Linux)  os=linux ;;
  *) die "unsupported OS '$OS': prebuilt binaries cover macOS and Linux (build from source with: cargo install --git https://github.com/$REPO)" ;;
esac
case "$ARCH" in
  x86_64 | amd64)  arch=x86_64 ;;
  arm64 | aarch64) arch=aarch64 ;;
  *) die "unsupported architecture '$ARCH': prebuilt binaries cover x86_64 and aarch64" ;;
esac
# A shell that runs under Rosetta on an Apple Silicon Mac says x86_64. The machine is arm64.
if [ "$os" = macos ] && [ "$arch" = x86_64 ] &&
  [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || echo 0)" = 1 ]; then
  arch=aarch64
fi
case "$os" in
  linux) target="$arch-unknown-linux-musl" ;;
  macos) target="$arch-apple-darwin" ;;
esac

# --- which release ------------------------------------------------------
# The version without its "v", whichever way it was given.
VERSION=${ICDU_VERSION:-latest}
VERSION=${VERSION#v}
if [ "$VERSION" = latest ]; then
  [ -z "${ICDU_BASE_URL:-}" ] || die "ICDU_BASE_URL needs ICDU_VERSION"
  # The address of the latest release redirects to that of its tag.
  latest=$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/$REPO/releases/latest") ||
    die "could not find the latest release (see https://github.com/$REPO/releases)"
  VERSION=${latest##*/tag/v}
  case "$VERSION" in
    "" | */* | *[!0-9A-Za-z.+-]*) die "could not tell the latest release from $latest" ;;
  esac
fi
BASE=${ICDU_BASE_URL:-https://github.com/$REPO/releases/download/v$VERSION}

# --- download -----------------------------------------------------------
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT INT TERM

fetch() { curl -fsSL --retry 3 -o "$TMP/$1" "$BASE/$1" 2>/dev/null; }

NAME=icdu
ASSET="icdu-$VERSION-$target.tar.gz"
log "fetching icdu $VERSION for $target ..."
if fetch "$ASSET"; then
  archive=yes
else
  # No archive of that name. Releases up to 0.4.1 are bare binaries, called rcdu.
  archive=no
  NAME=rcdu
  ASSET="rcdu-portable-$os-$arch"
  fetch "$ASSET" ||
    die "download failed: is there a release $VERSION? (see https://github.com/$REPO/releases)"
  warn "release $VERSION is from before the rename to icdu: installing it under its old name, rcdu"
fi
fetch sha256sums.txt || die "could not download the checksums of release $VERSION"

# --- check --------------------------------------------------------------
# The checksum shows that the download is intact. It comes from the same place, so it does
# not show who built the release; the attestation below does.
if have sha256sum; then
  actual=$(sha256sum "$TMP/$ASSET" | awk '{print $1}')
elif have shasum; then
  actual=$(shasum -a 256 "$TMP/$ASSET" | awk '{print $1}')
elif have openssl; then
  actual=$(openssl dgst -sha256 "$TMP/$ASSET" | awk '{print $NF}')
else
  die "no tool for SHA-256 found (one of sha256sum, shasum, openssl is needed)"
fi
expected=$(awk -v a="$ASSET" '$2 == a { print $1 }' "$TMP/sha256sums.txt")
[ -n "$expected" ] || die "$ASSET is not listed in sha256sums.txt: not installing"
[ "$actual" = "$expected" ] ||
  die "checksum mismatch for $ASSET (expected $expected, got $actual): not installing"

# Who built it: GitHub keeps a signed statement for every archive that it came out of this
# repository's release workflow. The GitHub CLI can check it, if it is there and logged in.
if [ "$archive" = yes ] && [ -z "${ICDU_BASE_URL:-}" ]; then
  if have gh && gh auth status >/dev/null 2>&1; then
    gh attestation verify "$TMP/$ASSET" --repo "$REPO" >/dev/null 2>&1 ||
      die "the attestation of $ASSET could not be verified: not installing (try: gh attestation verify FILE --repo $REPO)"
    log "verified: built by the release workflow of $REPO"
  else
    log "not verified who built it (that needs the GitHub CLI, logged in):"
    log "  gh attestation verify $ASSET --repo $REPO"
  fi
fi

# --- unpack -------------------------------------------------------------
if [ "$archive" = yes ]; then
  tar -xzf "$TMP/$ASSET" -C "$TMP"
  BIN="$TMP/icdu-$VERSION-$target/icdu"
  [ -f "$BIN" ] || die "$ASSET does not contain icdu-$VERSION-$target/icdu"
else
  BIN="$TMP/$ASSET"
fi

# --- install ------------------------------------------------------------
DEST="${ICDU_INSTALL_DIR:-$HOME/.local/bin}"
mkdir -p "$DEST" 2>/dev/null || die "cannot create $DEST (try sudo, or set ICDU_INSTALL_DIR)"
[ -w "$DEST" ] || die "$DEST is not writable (try sudo, or set ICDU_INSTALL_DIR)"

before=
if [ -x "$DEST/$NAME" ]; then
  before=$("$DEST/$NAME" --version 2>/dev/null | awk '{print $2; exit}') || before=
fi
install -m 0755 "$BIN" "$DEST/$NAME"
now=$("$DEST/$NAME" --version | awk '{print $2; exit}')

# Which of two versions is the newer one: their numbers, compared part by part.
newer() {
  printf '%s %s\n' "$1" "$2" | awk '{
    n = split($1, a, /[.+-]/); m = split($2, b, /[.+-]/)
    for (i = 1; i <= (n > m ? n : m); i++) {
      if (a[i] + 0 > b[i] + 0) { print "first"; exit }
      if (a[i] + 0 < b[i] + 0) { print "second"; exit }
    }
    print "same"
  }'
}
if [ -z "$before" ]; then
  log "installed $NAME $now to $DEST"
elif [ "$before" = "$now" ]; then
  log "$NAME $now was installed in $DEST already, and has been put there again"
elif [ "$(newer "$now" "$before")" = first ]; then
  log "upgraded $NAME from $before to $now in $DEST"
else
  log "replaced $NAME $before by the older $now in $DEST"
fi

# The program used to be called rcdu. A binary of that name next to the new one is the old
# version, which nothing will ever update.
if [ "$NAME" = icdu ] && [ -e "$DEST/rcdu" ]; then
  log "an rcdu from before the rename is still in $DEST. To remove it:"
  log "  rm $DEST/rcdu"
fi

case ":$PATH:" in
  *":$DEST:"*) ;;
  *) warn "$DEST is not on your PATH. Add it:  export PATH=\"$DEST:\$PATH\"" ;;
esac
