#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)"
REPO_ROOT="$(CDPATH='' cd -- "$SCRIPT_DIR/../.." && pwd)"
TARGET_SCRIPT="$REPO_ROOT/scripts/spawn.bash"

help_output="$(bash "$TARGET_SCRIPT" --help)"

case "$help_output" in
  *"--progress-check-delay SECONDS"*) ;;
  *)
    echo "expected --help to document --progress-check-delay" >&2
    exit 1
    ;;
esac

if bash "$TARGET_SCRIPT" --nodes 1 --home /tmp --progress-check-delay 0 >/tmp/spawn-progress-check-delay.out 2>&1; then
  echo "expected zero progress-check delay to fail" >&2
  exit 1
fi

if ! grep -q -- "--progress-check-delay must be a positive integer" /tmp/spawn-progress-check-delay.out; then
  echo "expected validation error for invalid progress-check delay" >&2
  cat /tmp/spawn-progress-check-delay.out >&2
  exit 1
fi

printf 'ok\n'
