#!/usr/bin/env bash
# Cuts a release in one step: sets the version, refreshes the lockfiles, checks that every
# copy agrees, then commits and tags. Pushing the tag starts the release workflow.
#
#   scripts/release.sh 0.1.9
#
# The version is written in exactly two manifests: the workspace (every crate inherits it)
# and the desktop shell, which is its own workspace. Its bundle and npm package follow.
set -euo pipefail

version="${1:?usage: scripts/release.sh <version, for example 0.1.9>}"
if ! [[ $version =~ ^[0-9]+\.[0-9]+\.[0-9]+([-.][0-9A-Za-z.-]+)?$ ]]; then
  echo "not a semantic version: $version" >&2
  exit 1
fi
tag="v$version"

cd "$(git rev-parse --show-toplevel)"
desktop=apps/desktop
shell="$desktop/src-tauri/Cargo.toml"

[ "$(git branch --show-current)" = main ] || { echo "release from main" >&2; exit 1; }
git diff --quiet && git diff --cached --quiet || { echo "commit or stash your changes first" >&2; exit 1; }
if git rev-parse -q --verify "refs/tags/$tag" >/dev/null; then
  echo "$tag already exists" >&2
  exit 1
fi

# The first `version = "..."` in each manifest is the package version, never a dependency's.
set_manifest_version() {
  awk -v v="$version" '!done && /^version *= *"/ { sub(/"[^"]*"/, "\"" v "\""); done = 1 } { print }' "$1" >"$1.tmp"
  mv "$1.tmp" "$1"
}
set_manifest_version Cargo.toml
set_manifest_version "$shell"
(cd "$desktop" && npm version "$version" --no-git-tag-version --allow-same-version >/dev/null)

# Cargo rewrites only the lockfile entries whose version changed; every other dependency keeps
# its locked version. Anything beyond our own version lines undoes the release.
cargo metadata --format-version 1 >/dev/null
cargo metadata --format-version 1 --manifest-path "$shell" >/dev/null
unexpected="$(git diff -U0 -- Cargo.lock "$desktop/src-tauri/Cargo.lock" | grep -E '^[+-][^+-]' | grep -vE '^[+-]version = "' || true)"
if [ -n "$unexpected" ]; then
  git checkout -- .
  printf 'the lockfiles changed beyond our own versions; nothing was released:\n%s\n' "$unexpected" >&2
  exit 1
fi

read_version() { sed -n 's/^version *= *"\([^"]*\)"/\1/p' "$1" | head -1; }
for found in "$(read_version Cargo.toml)" "$(read_version "$shell")" "$(node -p "require('./$desktop/package.json').version")"; do
  [ "$found" = "$version" ] || { echo "a version still reads $found, not $version" >&2; exit 1; }
done
if node -e "process.exit('version' in require('./$desktop/src-tauri/tauri.conf.json') ? 0 : 1)"; then
  echo "$desktop/src-tauri/tauri.conf.json pins a version; remove it" >&2
  exit 1
fi

git commit --quiet -am "chore: release $version"
git tag -a "$tag" -m "Medha $version"
echo "Ready. Publish with: git push origin main && git push origin $tag"
