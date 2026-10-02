#!/bin/sh
# Writes latest.json for a release: usage `describe-update.sh <folder of release files> <tag>`.
set -eu

cd "$1"
tag="$2"
base="https://github.com/${GITHUB_REPOSITORY:-kashyaprajharsh/medha-agent}/releases/download/$tag"
platforms='{}'

add() {
  [ -f "$2.sig" ] || { echo "no update signature for $2" >&2; exit 1; }
  platforms="$(printf '%s' "$platforms" | jq --arg key "$1" --arg url "$base/$2" --rawfile sig "$2.sig" \
    '.[$key] = { url: $url, signature: ($sig | rtrimstr("\n")) }')"
}

add darwin-aarch64 medha-desktop-aarch64-apple-darwin.app.tar.gz
add darwin-x86_64 medha-desktop-x86_64-apple-darwin.app.tar.gz
add linux-x86_64 medha-desktop-x86_64-unknown-linux-gnu.AppImage
add windows-x86_64 medha-desktop-x86_64-pc-windows-msvc.exe

jq -n --arg version "${tag#v}" --arg date "$(date -u +%Y-%m-%dT%H:%M:%SZ)" --argjson platforms "$platforms" \
  '{ version: $version, pub_date: $date, platforms: $platforms }' > latest.json
# Each signature now lives in latest.json; the loose copies would only clutter the release.
rm -f ./*.sig
