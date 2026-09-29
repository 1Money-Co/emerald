#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)"
REPO_ROOT="$(CDPATH='' cd -- "$SCRIPT_DIR/../.." && pwd)"
TARGET_SCRIPT="$REPO_ROOT/scripts/tests/rollback_binary_qualification.sh"
TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT

help_output="$(bash "$TARGET_SCRIPT" --help)"
for argument in --current-emerald-bin --n-minus-one-emerald-bin --custom-reth-bin; do
    case "$help_output" in
        *"$argument"*) ;;
        *)
            echo "expected --help to document $argument" >&2
            exit 1
            ;;
    esac
done

if bash "$TARGET_SCRIPT" >"$TMP_DIR/missing.out" 2>&1; then
    echo "expected missing binary arguments to fail" >&2
    exit 1
fi
grep -q -- "all three binary arguments are required" "$TMP_DIR/missing.out"

for binary in current previous reth; do
    printf '#!/usr/bin/env bash\nprintf "emerald-test 1.0.0\\n"\n' >"$TMP_DIR/$binary"
    chmod +x "$TMP_DIR/$binary"
done

if bash "$TARGET_SCRIPT" \
    --current-emerald-bin "$TMP_DIR/current" \
    --n-minus-one-emerald-bin "$TMP_DIR/previous" \
    --custom-reth-bin "$TMP_DIR/reth" >"$TMP_DIR/identical.out" 2>&1; then
    echo "expected identical Emerald versions to fail" >&2
    exit 1
fi
grep -q -- "Emerald binaries must report distinct versions" "$TMP_DIR/identical.out"
grep -q -- "before any testnet process was started" "$TMP_DIR/identical.out"

printf 'ok\n'
