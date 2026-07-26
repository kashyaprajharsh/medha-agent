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

if command -v curl >/dev/null 2>&1; then
  fetch() { curl -fsSL "$1"; }
  fetch_to() { curl -fsSL "$1" -o "$2"; }
elif command -v wget >/dev/null 2>&1; then
  fetch() { wget -qO- "$1"; }
  fetch_to() { wget -qO "$2" "$1"; }
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

# Verify the checksum when the release publishes one.
if fetch_to "$URL.sha256" "$tmp/$ASSET.sha256" 2>/dev/null; then
  if command -v shasum >/dev/null 2>&1; then
    expected="$(cut -d' ' -f1 < "$tmp/$ASSET.sha256")"
    actual="$(shasum -a 256 "$tmp/$ASSET" | cut -d' ' -f1)"
    [ "$expected" = "$actual" ] || die "checksum mismatch -- refusing to install"
    say "Checksum verified."
  fi
fi

tar xzf "$tmp/$ASSET" -C "$tmp"
binary="$(find "$tmp" -type f -name medha -perm -u+x 2>/dev/null | head -n1)"
[ -n "$binary" ] || die "the archive did not contain a medha binary"

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
