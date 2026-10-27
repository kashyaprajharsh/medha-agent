#!/bin/sh
# Mirrors one published release to R2: usage `mirror_r2.sh <folder of release files> <tag>`.
# Only attested files whose checksums match go up; a version, once uploaded, never changes.
set -eu

dir="$1"
tag="$2"
: "${R2_ENDPOINT:?}" "${R2_BUCKET:?}" "${AWS_ACCESS_KEY_ID:?}" "${AWS_SECRET_ACCESS_KEY:?}"
repo="${GITHUB_REPOSITORY:-kashyaprajharsh/medha-agent}"
public="${R2_PUBLIC:-https://download.manojavamlabs.com}"
export AWS_DEFAULT_REGION=auto

fail() { echo "mirror: $*" >&2; exit 1; }
s3() { aws --endpoint-url "$R2_ENDPOINT" "$@"; }
digest() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1; else shasum -a 256 "$1" | cut -d' ' -f1; fi
}

printf '%s\n' "$tag" | grep -Eq '^v[0-9]+\.[0-9]+\.[0-9]+([.-][0-9A-Za-z.-]+)?$' || fail "not a release tag: $tag"
cd "$dir"
[ -f latest.json ] || fail "no latest.json in $dir"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# Bytes that this repository's release workflow attested, and that match their published checksums.
for file in *; do
  gh attestation verify "$file" --repo "$repo" --signer-workflow "$repo/.github/workflows/release.yml" >/dev/null \
    || fail "$file is not attested by $repo's release workflow"
done
for sum in *.sha256; do
  file="${sum%.sha256}"
  [ -f "$file" ] || fail "$sum names a missing file"
  [ "$(cut -d' ' -f1 "$sum")" = "$(digest "$file")" ] || fail "$file does not match $sum"
done

# The mirror's own update manifest points at the mirror; each signature still binds the same bytes.
jq --arg from "https://github.com/$repo/releases/download/$tag/" --arg to "$public/$tag/" \
  '.platforms |= map_values(.url |= if startswith($from) then $to + .[($from | length):] else error("unexpected url \(.)") end)' \
  latest.json > "$work/latest.json"

kind() {
  case "$1" in
    *.dmg) echo application/x-apple-diskimage ;;
    *.exe) echo application/vnd.microsoft.portable-executable ;;
    *.deb) echo application/vnd.debian.binary-package ;;
    *.tar.gz) echo application/gzip ;;
    *.zip) echo application/zip ;;
    *.json) echo application/json ;;
    # Scripts and checksums are shown as text, never run by a browser.
    *.sh | *.ps1 | *.sha256) echo 'text/plain; charset=utf-8' ;;
    *) echo application/octet-stream ;;
  esac
}

# The checksum R2 holds for a key, empty when the key is absent; a key without one reads `None`.
# Called as `x="$(held key)" || exit 1`, so a read error stops the run instead of reading as absent.
held() {
  if out="$(s3 s3api head-object --bucket "$R2_BUCKET" --key "$1" --query 'Metadata.sha256' --output text 2>&1)"; then
    echo "$out"
  elif printf '%s' "$out" | grep -Eq '\(404\)|Not Found'; then
    echo ""
  else
    echo "mirror: reading $1: $out" >&2
    return 1
  fi
}

# Uploads once; a second run skips identical bytes and refuses anything else under the same version.
put() {
  path="$1"
  key="$2"
  sum="$(digest "$path")"
  have="$(held "$key")" || exit 1
  if [ "$have" = "$sum" ]; then
    echo "kept     $key"
  elif [ -n "$have" ]; then
    fail "$key already holds other bytes; a published version is never replaced"
  else
    s3 s3api put-object --bucket "$R2_BUCKET" --key "$key" --body "$path" --content-type "$(kind "$key")" \
      --cache-control "public, max-age=31536000, immutable" --metadata "sha256=$sum" >/dev/null
    echo "uploaded $key"
  fi
}

for file in *; do
  [ "$file" = latest.json ] && continue
  put "$file" "$tag/$file"
done
put "$work/latest.json" "$tag/latest.json"

# What users will download, read back through the public address before anything points at it.
for file in *; do
  [ "$file" = latest.json ] && continue
  curl -fsSL --connect-timeout 10 --retry 3 -o "$work/check" "$public/$tag/$file" || fail "$public/$tag/$file is not served"
  [ "$(digest "$work/check")" = "$(digest "$file")" ] || fail "$public/$tag/$file serves other bytes"
done

# The moving `latest/` names follow last, the manifest after every file it names.
latest() {
  sum="$(held "$tag/$1")" || exit 1
  [ -n "$sum" ] && [ "$sum" != None ] || fail "$tag/$1 is missing before latest/ moves"
  s3 s3api copy-object --bucket "$R2_BUCKET" --copy-source "$R2_BUCKET/$tag/$1" --key "latest/$1" \
    --metadata-directive REPLACE --content-type "$(kind "$1")" --cache-control "public, max-age=300" \
    --metadata "sha256=$sum" >/dev/null
  echo "latest   $1"
}
for file in *; do
  [ "$file" = latest.json ] || latest "$file"
done
latest latest.json
echo "mirrored $tag to $public"
