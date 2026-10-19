#!/usr/bin/env bash
set -euo pipefail

# Ubuntu restricts unprivileged namespaces by AppArmor profile. Permit only
# these ephemeral CI executables; keep the machine-wide restriction enabled.
if [[ ! -e /proc/sys/kernel/apparmor_restrict_unprivileged_userns ]]; then
  exit 0
fi
profile=$(mktemp)
trap 'rm -f "$profile"' EXIT
printf 'abi <abi/4.0>,\nprofile medha-ci "%s/target/debug/**" flags=(unconfined) { userns, }\nprofile medha-ci-desktop "%s/apps/desktop/src-tauri/target/debug/**" flags=(unconfined) { userns, }\n' \
  "${GITHUB_WORKSPACE:?}" "$GITHUB_WORKSPACE" > "$profile"
sudo apparmor_parser -r "$profile"
