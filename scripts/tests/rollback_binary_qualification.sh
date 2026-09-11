#!/usr/bin/env bash

set -euo pipefail

usage() {
    cat <<'EOF'
Usage: rollback_binary_qualification.sh \
  --current-emerald-bin PATH \
  --n-minus-one-emerald-bin PATH \
  --custom-reth-bin PATH

Runs an opt-in four-node N -> N-1 -> N rollback qualification with real binaries.

Required arguments:
  --current-emerald-bin       Current Emerald binary used to create and re-upgrade the network
  --n-minus-one-emerald-bin   Previous Emerald binary used for rollback nodes
  --custom-reth-bin           Custom Reth binary used by every execution node

This network run qualifies real process and version transitions. Deterministic database tests,
not this script, pin the exact N-proposal/N-1-commit interleaving.
EOF
}

current_emerald_bin=""
n_minus_one_emerald_bin=""
custom_reth_bin=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --current-emerald-bin)
            [[ $# -ge 2 ]] || { usage >&2; exit 2; }
            current_emerald_bin="$2"
            shift 2
            ;;
        --n-minus-one-emerald-bin)
            [[ $# -ge 2 ]] || { usage >&2; exit 2; }
            n_minus_one_emerald_bin="$2"
            shift 2
            ;;
        --custom-reth-bin)
            [[ $# -ge 2 ]] || { usage >&2; exit 2; }
            custom_reth_bin="$2"
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "unknown argument: $1" >&2
            usage >&2
            exit 2
            ;;
    esac
done

if [[ -z "$current_emerald_bin" || -z "$n_minus_one_emerald_bin" || -z "$custom_reth_bin" ]]; then
    echo "all three binary arguments are required" >&2
    usage >&2
    exit 2
fi

absolute_executable() {
    local candidate="$1"
    local directory
    local filename
    directory="$(CDPATH='' cd -- "$(dirname -- "$candidate")" 2>/dev/null && pwd)" || {
        echo "binary directory does not exist: $candidate" >&2
        return 1
    }
    filename="$(basename -- "$candidate")"
    candidate="$directory/$filename"
    if [[ ! -x "$candidate" ]]; then
        echo "binary is not executable: $candidate" >&2
        return 1
    fi
    printf '%s\n' "$candidate"
}

current_emerald_bin="$(absolute_executable "$current_emerald_bin")"
n_minus_one_emerald_bin="$(absolute_executable "$n_minus_one_emerald_bin")"
custom_reth_bin="$(absolute_executable "$custom_reth_bin")"

current_version="$("$current_emerald_bin" --version 2>&1)"
n_minus_one_version="$("$n_minus_one_emerald_bin" --version 2>&1)"
if [[ "$current_version" == "$n_minus_one_version" ]]; then
    echo "Emerald binaries must report distinct versions before any testnet process was started" >&2
    exit 2
fi

emerald_utils_bin="$(dirname -- "$current_emerald_bin")/emerald-utils"
if [[ ! -x "$emerald_utils_bin" ]]; then
    echo "current Emerald binary must have an executable emerald-utils sibling: $emerald_utils_bin" >&2
    exit 2
fi
command -v cast >/dev/null 2>&1 || {
    echo "cast is required for RPC height polling" >&2
    exit 2
}

artifact_dir="$(mktemp -d "${TMPDIR:-/tmp}/emerald-rollback-qualification.XXXXXX")"
testnet_home="$artifact_dir/nodes"
work_dir="$artifact_dir/work"
qualification_log="$artifact_dir/qualification.log"
mkdir -p "$work_dir/assets"

script_dir="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)"
repo_root="$(CDPATH='' cd -- "$script_dir/../.." && pwd)"
cp "$repo_root/assets/jwtsecret" "$work_dir/assets/jwtsecret"

log_command() {
    printf 'command:' >>"$qualification_log"
    printf ' %q' "$@" >>"$qualification_log"
    printf '\n' >>"$qualification_log"
}

run() {
    log_command "$@"
    "$@" >>"$qualification_log" 2>&1
}

stop_testnet() {
    if [[ -d "$testnet_home" ]]; then
        log_command "$current_emerald_bin" --home "$testnet_home" testnet stop
        "$current_emerald_bin" --home "$testnet_home" testnet stop >>"$qualification_log" 2>&1 || true
    fi
}

finish() {
    local status=$?
    trap - EXIT INT TERM
    stop_testnet
    if [[ $status -ne 0 ]]; then
        echo "rollback qualification failed; artifacts preserved at $artifact_dir" >&2
    else
        echo "rollback qualification passed; artifacts preserved at $artifact_dir"
    fi
    exit "$status"
}
trap finish EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

rpc_height() {
    local node="$1"
    local port=$((8645 + node * 100))
    cast block-number --rpc-url "http://127.0.0.1:$port" 2>/dev/null
}

wait_for_height() {
    local node="$1"
    local minimum="$2"
    local attempts="${3:-120}"
    local height=0
    for ((attempt = 1; attempt <= attempts; attempt++)); do
        height="$(rpc_height "$node" || printf '0')"
        if [[ "$height" =~ ^[0-9]+$ ]] && (( height >= minimum )); then
            printf 'node %s reached height %s (required %s)\n' "$node" "$height" "$minimum" \
                >>"$qualification_log"
            return 0
        fi
        sleep 1
    done
    echo "node $node did not reach height $minimum; last height was $height" >&2
    return 1
}

assert_node_running() {
    local node="$1"
    local status_output
    log_command "$current_emerald_bin" --home "$testnet_home" testnet status
    status_output="$("$current_emerald_bin" --home "$testnet_home" testnet status 2>&1)"
    printf '%s\n' "$status_output" >>"$qualification_log"
    printf '%s\n' "$status_output" | awk -v node="$node" '
        $0 == "Node " node ":" { in_node = 1; next }
        in_node && /Emerald: Running/ { emerald = 1 }
        in_node && /Reth:    Running/ { reth = 1 }
        in_node && /^$/ { exit !(emerald && reth) }
        END { if (in_node) exit !(emerald && reth) }
    '
}

assert_no_rollback_failures() {
    local node="$1"
    local log_file="$testnet_home/$node/logs/emerald.log"
    if grep -Eqi \
        'panic|Failed to decode synced value|empty execution[- ]payload|execution payload.*empty' \
        "$log_file"; then
        echo "rollback failure signature found in node $node log" >&2
        return 1
    fi
    assert_node_running "$node"
}

printf 'current Emerald: %s\n' "$current_version" >>"$qualification_log"
printf 'N-1 Emerald: %s\n' "$n_minus_one_version" >>"$qualification_log"
printf 'custom Reth: %s\n' "$("$custom_reth_bin" --version 2>&1)" >>"$qualification_log"

cd "$work_dir"
run "$current_emerald_bin" --home "$testnet_home" testnet start \
    --nodes 4 \
    --emerald-bin "$current_emerald_bin" \
    --emerald-utils-bin "$emerald_utils_bin" \
    --custom-reth-bin "$custom_reth_bin"

for node in 0 1 2 3; do
    wait_for_height "$node" 20
    assert_node_running "$node"
done

run "$current_emerald_bin" --home "$testnet_home" testnet stop-node 0
rollback_target="$(rpc_height 1)"
run "$current_emerald_bin" --home "$testnet_home" testnet start-node 0 \
    --emerald-bin "$n_minus_one_emerald_bin" \
    --custom-reth-bin "$custom_reth_bin"
wait_for_height 0 "$rollback_target"
assert_no_rollback_failures 0
for node in 1 2 3; do
    assert_node_running "$node"
done

run "$current_emerald_bin" --home "$testnet_home" testnet stop-node 1
lag_start="$(rpc_height 2)"
wait_for_height 2 "$((lag_start + 10))"
second_target="$(rpc_height 2)"
run "$current_emerald_bin" --home "$testnet_home" testnet start-node 1 \
    --emerald-bin "$n_minus_one_emerald_bin" \
    --custom-reth-bin "$custom_reth_bin"
wait_for_height 1 "$second_target"
assert_no_rollback_failures 0
assert_no_rollback_failures 1
for node in 2 3; do
    assert_node_running "$node"
done

for node in 0 1; do
    run "$current_emerald_bin" --home "$testnet_home" testnet stop-node "$node"
    run "$current_emerald_bin" --home "$testnet_home" testnet start-node "$node" \
        --emerald-bin "$current_emerald_bin" \
        --custom-reth-bin "$custom_reth_bin"
done

convergence_target="$(rpc_height 2)"
for node in 0 1 2 3; do
    wait_for_height "$node" "$convergence_target"
    assert_node_running "$node"
done
