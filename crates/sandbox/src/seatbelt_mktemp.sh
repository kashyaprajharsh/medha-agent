#!/bin/sh
# macOS mktemp ignores TMPDIR; an empty -p restores it. Explicit templates pass through.
expect_prefix=
for arg in "$@"; do
  if [ -n "$expect_prefix" ]; then
    expect_prefix=
    continue
  fi
  case $arg in
    -p* | --tmpdir*) exec /usr/bin/mktemp "$@" ;;
    -t | -[dqu]*t) expect_prefix=1 ;;
    -*) ;;
    *) exec /usr/bin/mktemp "$@" ;;
  esac
done
exec /usr/bin/mktemp -p '' "$@"
