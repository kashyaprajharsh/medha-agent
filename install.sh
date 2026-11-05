#!/bin/sh
# MEDHA installer -- Linux, macOS, WSL2.
#
#   curl -fsSL https://raw.githubusercontent.com/kashyaprajharsh/medha-agent/main/install.sh | sh
#
# Installs the release build. Set MEDHA_INSTALL_DIR to override the destination.
# To pin both installer and binary, fetch the same immutable release tag:
#   version=v0.1.0
#   curl -fsSL "https://raw.githubusercontent.com/kashyaprajharsh/medha-agent/$version/install.sh" \
#     | MEDHA_VERSION="$version" sh
set -eu

# Everything runs from main, called on the last line, so a download cut off part way runs nothing.
main() {
REPO="${MEDHA_REPO:-kashyaprajharsh/medha-agent}"
VERSION="${MEDHA_VERSION:-latest}"
INSTALL_DIR="${MEDHA_INSTALL_DIR:-}"
# Releases are downloaded from this mirror first and from GitHub when it cannot serve them.
# A fork named by MEDHA_REPO has no mirror and uses its own GitHub releases alone.
if [ -n "${MEDHA_REPO:-}" ]; then
  MIRROR=""
else
  MIRROR="${MEDHA_MIRROR:-https://download.manojavamlabs.com}"
fi
GITHUB="https://github.com/$REPO/releases/download"

# Use color only on an interactive terminal unless NO_COLOR is set.
if [ -t 1 ] && [ -z "${NO_COLOR:-}" ] && [ "${TERM:-dumb}" != "dumb" ]; then
  c_reset=$(printf '\033[0m'); c_dim=$(printf '\033[2m'); c_bold=$(printf '\033[1m')
  c_accent=$(printf '\033[36m'); c_ok=$(printf '\033[32m'); c_err=$(printf '\033[31m')
  g_step='→'; g_ok='✓'; g_err='✗'
else
  c_reset=''; c_dim=''; c_bold=''; c_accent=''; c_ok=''; c_err=''
  g_step='>'; g_ok='-'; g_err='x'
fi

say()  { printf '%s\n' "$*"; }
step() { printf '  %s%s%s %s\n' "$c_accent" "$g_step" "$c_reset" "$*"; }
ok()   { printf '  %s%s%s %s\n' "$c_ok" "$g_ok" "$c_reset" "$*"; }
note() { printf '  %s%s%s\n' "$c_dim" "$*" "$c_reset"; }
die()  { printf '  %s%s error:%s %s\n' "$c_err" "$g_err" "$c_reset" "$*" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || die "'$1' is required but not installed"; }

printf '\n  %s%smedha%s %s· verification-first agent harness%s\n' \
  "$c_bold" "$c_accent" "$c_reset" "$c_dim" "$c_reset"
printf '  %s────────────────────────────────────────%s\n\n' "$c_dim" "$c_reset"

need uname
need tar

# Bound connection attempts and retry transient release-download failures. The mirror is asked once:
# when it fails, GitHub is the retry.
if command -v curl >/dev/null 2>&1; then
  NET_OPTS="--connect-timeout 5 --retry 3 --proto =https --proto-redir =https"
  ONCE_OPTS="--connect-timeout 5 --proto =https --proto-redir =https"
  fetch() { curl -fsSL $NET_OPTS "$1"; }
  fetch_once() { curl -fsSL $ONCE_OPTS "$1"; }
  # Return 0 for success, 2 for HTTP 404, and 1 for other failures.
  fetch_optional_to() {
    status="$(curl -sSL $NET_OPTS -o "$2" -w '%{http_code}' "$1")" || return 1
    case "$status" in
      2??) return 0 ;;
      404) rm -f "$2"; return 2 ;;
      *) rm -f "$2"; return 1 ;;
    esac
  }
elif command -v wget >/dev/null 2>&1; then
  NET_OPTS="--connect-timeout=5 --tries=3 --https-only"
  ONCE_OPTS="--connect-timeout=5 --tries=1 --https-only"
  fetch() { wget $NET_OPTS -qO- "$1"; }
  fetch_once() { wget $ONCE_OPTS -qO- "$1"; }
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

# Whether a download that failed with exit code $1 may be tried on the next host: a network failure
# or an error answer may; a certificate that does not check out is someone in the way, not a host
# that is down, so it may not.
retryable() {
  if command -v curl >/dev/null 2>&1; then
    case "$1" in 5|6|7|18|22|28|52|55|56) return 0 ;; esac
  else
    case "$1" in 4|8) return 0 ;; esac
  fi
  return 1
}

# Downloads $1 to $2, with a progress bar when $3 is set: 0 when it arrived, 3 when this host could
# not serve it so the next host may (no connection, a timeout, a missing file, a refusal), 1 otherwise.
get() {
  rm -f "$2"
  opts="$NET_OPTS"
  case "$1" in "${MIRROR:-none}"/*) opts="$ONCE_OPTS" ;; esac
  if command -v curl >/dev/null 2>&1; then
    if [ -n "${3:-}" ]; then
      code="$(curl -SL $opts --progress-bar -o "$2" -w '%{http_code}' "$1")" && rc=0 || rc=$?
    else
      code="$(curl -sSL $opts -o "$2" -w '%{http_code}' "$1")" && rc=0 || rc=$?
    fi
    if [ "$rc" != 0 ]; then
      rm -f "$2"
      retryable "$rc" && return 3
      return 1
    fi
    case "$code" in
      2??) return 0 ;;
      403|429)
        # Cloudflare answers for the mirror when it blocks a network, not the file being missing.
        note "the download mirror refused this network ($code); trying GitHub"
        rm -f "$2"; return 3 ;;
      404|5??) rm -f "$2"; return 3 ;;
      *) rm -f "$2"; return 1 ;;
    esac
  fi
  wget $opts -qO "$2" "$1" && return 0
  rc=$?
  rm -f "$2"
  retryable "$rc" && return 3
  return 1
}

# A mirror that failed once is not asked again in this run.
mirror_failed() {
  [ -n "$MIRROR" ] && note "the download mirror could not serve this release; using GitHub"
  MIRROR=""
}

# Downloads release file $1 to $2 from the mirror, else GitHub. Called without a subshell, so a
# mirror that failed stays skipped for the rest of the run.
get_release() {
  for base in ${MIRROR:+"$MIRROR"} "$GITHUB"; do
    status=0
    get "$base/$VERSION/$1" "$2" "${3:-}" || status=$?
    case "$status" in
      0) return 0 ;;
      3) [ "$base" = "$GITHUB" ] || mirror_failed ;;
      *) return 1 ;;
    esac
  done
  return 1
}


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


if [ "$VERSION" = "latest" ]; then
  step "resolving latest release"
  VERSION=""
  if [ -n "$MIRROR" ]; then
    manifest="$(fetch_once "$MIRROR/latest/latest.json" 2>/dev/null)" && rc=0 || rc=$?
    if [ "$rc" = 0 ]; then
      VERSION="$(printf '%s\n' "$manifest" \
        | sed -n 's/.*"version"[[:space:]]*:[[:space:]]*"v\{0,1\}\([^"]*\)".*/v\1/p' | head -n1)"
    elif ! retryable "$rc"; then
      die "the download mirror's certificate or connection did not check out; refusing to continue"
    fi
    [ -n "$VERSION" ] || mirror_failed
  fi
  [ -n "$VERSION" ] || VERSION="$(fetch "https://api.github.com/repos/$REPO/releases/latest" \
    | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' \
    | head -n1)"
  [ -n "$VERSION" ] || die "could not resolve the latest release of $REPO.
Set MEDHA_VERSION to a tag, or check that the repository has a published release."
  ok "resolved $VERSION"
fi

# The version names a folder, so it may hold only what a release tag holds.
case "$VERSION" in
  ''|.*|*[!A-Za-z0-9._-]*) die "not a release tag: $VERSION" ;;
esac

ASSET="medha-${TARGET}.tar.gz"


if [ -z "$INSTALL_DIR" ]; then
  # Prefer a writable directory already on PATH.
  if [ -w "/usr/local/bin" ] 2>/dev/null; then
    INSTALL_DIR="/usr/local/bin"
  else
    INSTALL_DIR="$HOME/.local/bin"
  fi
fi
mkdir -p "$INSTALL_DIR" || die "cannot create $INSTALL_DIR"

# Each version keeps its own folder, so an update never writes over a medha that is running.
# `.local/lib` is readable inside Medha's sandbox, so the command still works there.
# An update names the store its running copy lives in, so the new version lands beside it.
STORE="${MEDHA_STORE:-$HOME/.local/lib/medha}"
if [ -z "${MEDHA_STORE:-}" ] && [ "$INSTALL_DIR" = "/usr/local/bin" ] && mkdir -p /usr/local/lib/medha 2>/dev/null; then
  STORE="/usr/local/lib/medha"
fi
mkdir -p "$STORE/versions" || die "cannot create $STORE"

# One installer at a time; a lock left by one that crashed is taken over.
LOCK="$STORE/install.lock"
waited=0
until mkdir "$LOCK" 2>/dev/null; do
  holder="$(cat "$LOCK/pid" 2>/dev/null || true)"
  if [ -n "$holder" ] && ! ps -p "$holder" >/dev/null 2>&1; then
    rm -rf "$LOCK"
    continue
  fi
  [ "$waited" -lt 120 ] || die "another medha install is still running (lock: $LOCK)"
  sleep 1
  waited=$((waited + 1))
done
printf '%s\n' "$$" > "$LOCK/pid"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp" "$LOCK"' EXIT

step "downloading medha $VERSION  ($TARGET)"
get_release "$ASSET" "$tmp/$ASSET" progress || die "download failed: $ASSET of $VERSION
This platform may not have a published build for $VERSION."

# Every release publishes a checksum beside each archive; a missing or unreadable one fails closed.
# It is read from GitHub first, so a file from the mirror is checked against a second host.
URL="$GITHUB/$VERSION/$ASSET"
checksum_status=0
fetch_optional_to "$URL.sha256" "$tmp/$ASSET.sha256" || checksum_status=$?
if [ "$checksum_status" = 1 ] && [ -n "$MIRROR" ]; then
  note "GitHub could not be reached for the checksum; reading the mirror's"
  URL="$MIRROR/$VERSION/$ASSET"
  checksum_status=0
  fetch_optional_to "$URL.sha256" "$tmp/$ASSET.sha256" || checksum_status=$?
fi
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
    ok "checksum verified"
    ;;
  2) die "no checksum published at $URL.sha256 -- refusing an unverifiable install" ;;
  *) die "checksum download failed: $URL.sha256 -- refusing an unverifiable install" ;;
esac

# Validate one regular root entry before extraction to reject archive attacks.
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

# Holding the lock, any staging folder still here was left by an install that crashed.
rm -rf "$STORE/versions"/.staging.*
staging="$STORE/versions/.staging.$$"
mkdir "$staging" || die "could not prepare $staging"
cp "$binary" "$staging/medha" && chmod 755 "$staging/medha" \
  || die "could not stage the medha binary"
release="$STORE/versions/$VERSION"
# A running copy of the same version keeps its open file after the folder is replaced.
rm -rf "$release"
mv "$staging" "$release" || die "could not move the new version into $release"

link="$INSTALL_DIR/medha"
ln -s "$release/medha" "$link.new.$$" || die "could not create the medha link in $INSTALL_DIR"
mv -f "$link.new.$$" "$link" || { rm -f "$link.new.$$"; die "could not switch $link to $VERSION"; }

# Keep this version and the one before it; a running older one keeps its open file.
ls -1t "$STORE/versions" | grep -v '^\.' | grep -Fvx "$VERSION" | tail -n +2 \
  | while IFS= read -r old; do rm -rf "$STORE/versions/$old"; done

ok "installed  ${c_dim}${g_step}${c_reset}  $link"
printf '\n'

case ":$PATH:" in
  *":$INSTALL_DIR:"*)
    printf '  %smedha %s is ready.%s  run %smedha%s to begin.\n\n' \
      "$c_bold" "$VERSION" "$c_reset" "$c_accent" "$c_reset"
    ;;
  *)
    printf '  %smedha %s installed.%s\n' "$c_bold" "$VERSION" "$c_reset"
    note "$INSTALL_DIR is not on your PATH yet — add it:"
    printf '    %sexport PATH="%s:$PATH"%s\n\n' "$c_dim" "$INSTALL_DIR" "$c_reset"
    ;;
esac
}

main "$@"
