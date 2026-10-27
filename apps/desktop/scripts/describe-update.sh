#!/bin/sh
# Writes a release's update manifests: usage `describe-update.sh <folder of release files> <tag>`.
# latest.json links the download mirror, latest-github.json the same files on GitHub; apps read the
# second only when the mirror fails, and each signature binds the bytes wherever they are fetched.
set -eu

cd "$1"
tag="$2"
github="https://github.com/${GITHUB_REPOSITORY:-kashyaprajharsh/medha-agent}/releases/download/$tag"
mirror="${MIRROR_BASE:-https://download.manojavamlabs.com}/$tag"
date="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

describe() {
  base="$1"
  platforms='{}'
  for entry in darwin-aarch64:Medha-macOS-arm64.app.tar.gz darwin-x86_64:Medha-macOS-x64.app.tar.gz \
    linux-x86_64:Medha-Linux-x64.AppImage windows-x86_64:Medha-Windows-x64.exe; do
    key="${entry%%:*}"
    file="${entry#*:}"
    [ -f "$file.sig" ] || { echo "no update signature for $file" >&2; exit 1; }
    platforms="$(printf '%s' "$platforms" | jq --arg key "$key" --arg url "$base/$file" --rawfile sig "$file.sig" \
      '.[$key] = { url: $url, signature: ($sig | rtrimstr("\n")) }')"
  done
  jq -n --arg version "${tag#v}" --arg date "$date" --argjson platforms "$platforms" \
    '{ version: $version, pub_date: $date, platforms: $platforms }'
}

describe "$mirror" > latest.json
describe "$github" > latest-github.json
# Each signature now lives in both manifests; the loose copies would only clutter the release.
rm -f ./*.sig
