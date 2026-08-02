#!/bin/sh
# MEDHA installer -- Linux, macOS, WSL2.
#
#   curl -fsSL https://raw.githubusercontent.com/kashyaprajharsh/medha-agent/main/install.sh | sh
#
# Downloads the release build for this platform and puts `medha` on your PATH.
# Override the destination with MEDHA_INSTALL_DIR, or pin a build with
# MEDHA_VERSION=v0.1.0.
set -eu

REPO="${MEDHA_REPO:-kashyaprajharsh/medha-agent}"
VERSION="${MEDHA_VERSION:-latest}"
INSTALL_DIR="${MEDHA_INSTALL_DIR:-}"

say() { printf '%s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || die "'$1' is required but not installed"; }

need uname
need tar

# GitHub serves release assets from several anycast addresses, and one that is
# unreachable from the caller's network must not stall the install: bound each
# connect attempt so the next address is tried promptly, then retry transient
# failures. The asset download also reports progress, because a slow network
# must not be indistinguishable from a hung one.
if command -v curl >/dev/null 2>&1; then
  NET_OPTS="--connect-timeout 5 --retry 3 --proto =https --proto-redir =https"
  fetch() { curl -fsSL $NET_OPTS "$1"; }
  fetch_to() { curl -fSL $NET_OPTS --progress-bar "$1" -o "$2"; }
  # Return 0 = downloaded, 2 = precise HTTP 404, 1 = transport/other HTTP
  # failure. Only a real 404 means "this release did not publish a checksum."
  fetch_optional_to() {
    status="$(curl -sSL $NET_OPTS -o "$2" -w '%{http_code}' "$1")" || return 1
    case "$status" in
      2??) return 0 ;;
      404) rm -f "$2"; return 2 ;;
      *) rm -f "$2"; return 1 ;;
    esac
  }
elif command -v wget >/dev/null 2>&1; then
  NET_OPTS="--connect-timeout=5 --tries=3"
  fetch() { wget $NET_OPTS -qO- "$1"; }
  fetch_to() { wget $NET_OPTS -qO "$2" "$1"; }
  fetch_optional_to() {
    headers="$2.headers"
    if wget $NET_OPTS -S -qO "$2" "$1" 2>"$headers"; then
      rm -f "$headers"
      return 0
    fi
    if awk '$1 ~ /^HTTP\// { status = $2 } END { exit !(status == 404) }' "$headers"; then
      rm -f "$2" "$headers"
      return 2
    fi
    rm -f "$2" "$headers"
    return 1
  }
else
  die "either curl or wget is required"
fi

# ---- target detection -------------------------------------------------------

os="$(uname -s)"
arch="$(uname -m)"

case "$os" in
  Linux)  os_part="unknown-linux-gnu" ;;
  Darwin) os_part="apple-darwin" ;;
  MINGW*|MSYS*|CYGWIN*)
    die "Windows detected -- use the PowerShell installer instead:
  irm https://raw.githubusercontent.com/$REPO/main/install.ps1 | iex" ;;
  *) die "unsupported operating system: $os" ;;
esac

case "$arch" in
  x86_64|amd64)  arch_part="x86_64" ;;
  aarch64|arm64) arch_part="aarch64" ;;
  *) die "unsupported architecture: $arch" ;;
esac

TARGET="${arch_part}-${os_part}"

# ---- resolve the version ----------------------------------------------------

if [ "$VERSION" = "latest" ]; then
  say "Resolving the latest release..."
  VERSION="$(fetch "https://api.github.com/repos/$REPO/releases/latest" \
    | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' \
    | head -n1)"
  [ -n "$VERSION" ] || die "could not resolve the latest release of $REPO.
Set MEDHA_VERSION to a tag, or check that the repository has a published release."
fi

ASSET="medha-${TARGET}.tar.gz"
URL="https://github.com/$REPO/releases/download/$VERSION/$ASSET"

# ---- choose an install directory --------------------------------------------

if [ -z "$INSTALL_DIR" ]; then
  # Prefer a location already on PATH that we can write to without sudo.
  if [ -w "/usr/local/bin" ] 2>/dev/null; then
    INSTALL_DIR="/usr/local/bin"
  else
    INSTALL_DIR="$HOME/.local/bin"
  fi
fi
mkdir -p "$INSTALL_DIR" || die "cannot create $INSTALL_DIR"

# ---- download and install ---------------------------------------------------

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

say "Downloading medha $VERSION for ${TARGET}..."
fetch_to "$URL" "$tmp/$ASSET" || die "download failed: $URL
This platform may not have a published build for $VERSION."

# A precise 404 means an older release omitted a checksum and may continue.
# Every other fetch failure is uncertain and therefore fails closed.
checksum_status=0
fetch_optional_to "$URL.sha256" "$tmp/$ASSET.sha256" || checksum_status=$?
case "$checksum_status" in
  0)
    records="$(awk 'NF { count += 1 } END { print count + 0 }' "$tmp/$ASSET.sha256")"
    [ "$records" = "1" ] \
      || die "checksum file must contain exactly one non-empty record"
    expected="$(awk 'NF { print $1; exit }' "$tmp/$ASSET.sha256")"
    [ "${#expected}" = "64" ] \
      || die "checksum file did not contain one 64-hex SHA-256 digest"
    case "$expected" in
      *[!0-9A-Fa-f]*) die "checksum file contained a malformed SHA-256 digest" ;;
    esac
    expected="$(printf '%s' "$expected" | tr 'A-F' 'a-f')"

    if command -v sha256sum >/dev/null 2>&1; then
      actual="$(sha256sum "$tmp/$ASSET" | awk '{ print $1; exit }')"
    elif command -v shasum >/dev/null 2>&1; then
      actual="$(shasum -a 256 "$tmp/$ASSET" | awk '{ print $1; exit }')"
    else
      die "checksum verification requires sha256sum or shasum"
    fi
    actual="$(printf '%s' "$actual" | tr 'A-F' 'a-f')"
    [ "$expected" = "$actual" ] || die "checksum mismatch -- refusing to install"
    say "Checksum verified."
    ;;
  2) say "No checksum was published for this release; continuing without one." ;;
  *) die "checksum download failed: $URL.sha256 -- refusing an unverifiable install" ;;
esac

# Require one regular root entry with the release's exact layout. Validating
# before extraction rejects traversal, absolute paths, duplicates, links,
# devices, FIFOs, and "first executable named medha wins" ambiguity.
entries="$(tar tzf "$tmp/$ASSET")" \
  || die "could not inspect the downloaded archive"
[ "$entries" = "medha" ] \
  || die "archive layout is invalid; expected exactly one root regular file named medha"
detail="$(tar tvzf "$tmp/$ASSET")" \
  || die "could not inspect archive entry types"
case "$detail" in
  -*) ;;
  *) die "archive medha entry is not a regular file" ;;
esac

mkdir "$tmp/extracted" || die "could not prepare the extraction directory"
tar xzf "$tmp/$ASSET" -C "$tmp/extracted" medha \
  || die "could not extract the validated medha binary"
binary="$tmp/extracted/medha"
[ -f "$binary" ] && [ ! -L "$binary" ] \
  || die "the validated archive did not produce a regular medha binary"

install -m 755 "$binary" "$INSTALL_DIR/medha" 2>/dev/null \
  || { cp "$binary" "$INSTALL_DIR/medha" && chmod 755 "$INSTALL_DIR/medha"; }

say ""
say "medha $VERSION installed to $INSTALL_DIR/medha"

case ":$PATH:" in
  *":$INSTALL_DIR:"*) say "Run 'medha' to get started." ;;
  *)
    say ""
    say "$INSTALL_DIR is not on your PATH. Add it:"
    say "  export PATH=\"$INSTALL_DIR:\$PATH\""
    ;;
esac
