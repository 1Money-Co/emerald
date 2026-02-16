#!/usr/bin/env bash
#
# test_access_control.sh - End-to-end test for ValidatorManager AccessControl
#
# Tests the full delegation workflow:
#   1. Generate a fresh account
#   2. Verify it cannot manage validators (no role)
#   3. Grant VALIDATOR_MANAGER_ROLE from the owner
#   4. Unregister a validator from the delegated account
#   5. Re-register the same validator
#   6. Revoke the role and verify access is removed
#
# Repeatable: script restores the validator set to its original state on success.
#
# Usage:
#   ./scripts/test_access_control.sh
#
# Environment Variables:
#   RPC_URL    RPC endpoint (default: http://127.0.0.1:8645)
#   OWNER_KEY  Owner private key (default: Hardhat account #0)

set -euo pipefail

# --- Configuration ---
RPC_URL="${RPC_URL:-http://127.0.0.1:8645}"
VM_ADDRESS="0x0000000000000000000000000000000000002000"
OWNER_KEY="${OWNER_KEY:-0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80}"
VALIDATOR_MANAGER_ROLE=$(cast keccak "VALIDATOR_MANAGER_ROLE")

# Validator #4 from the genesis set
TARGET_PUBKEY="0x0446c9850cde33214a34ed324ab6ed8c24a23b05a1d5188668da4c60654b00c9a1e8d5b04e6c4e82b68554670ed5ca6beeb0ff50a438d52fcc3797709570dc28d8"
TARGET_ADDR="0xcf50f11805a680143f7342701085e6fb918c1a83"
TARGET_POWER=100

# --- Colors ---
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

pass() { printf "${GREEN}[PASS]${NC} %s\n" "$*"; }
fail() { printf "${RED}[FAIL]${NC} %s\n" "$*" >&2; exit 1; }
step() { printf "\n${BLUE}==> Step %s${NC}\n" "$*"; }
info() { printf "    %s\n" "$*"; }

# Helper: send a tx and return success/failure without printing cast's full output
quiet_send() {
    cast send --rpc-url "$RPC_URL" "$@" > /dev/null 2>&1
}

# ---------------------------------------------------------------------------
step "0: Pre-flight checks"
# ---------------------------------------------------------------------------
OWNER_ADDR=$(cast call --rpc-url "$RPC_URL" "$VM_ADDRESS" "owner()(address)")
info "Contract owner: $OWNER_ADDR"

INITIAL_COUNT=$(cast call --rpc-url "$RPC_URL" "$VM_ADDRESS" "getValidatorCount()(uint256)")
info "Current validator count: $INITIAL_COUNT"

IS_VAL=$(cast call --rpc-url "$RPC_URL" "$VM_ADDRESS" "isValidator(address)(bool)" "$TARGET_ADDR")
info "Target validator registered: $IS_VAL"

# Ensure the target validator is registered before we begin
if [[ "$IS_VAL" == "false" ]]; then
    info "Registering target validator so the test starts from a known state..."
    cast send --rpc-url "$RPC_URL" --private-key "$OWNER_KEY" \
        "$VM_ADDRESS" "register(bytes,uint64)" "$TARGET_PUBKEY" "$TARGET_POWER" > /dev/null
    pass "Target validator pre-registered"
fi

# ---------------------------------------------------------------------------
step "1: Generate a fresh account"
# ---------------------------------------------------------------------------
NEW_PK=$(cast wallet new --json | jq -r '.[0].private_key')
NEW_ADDR=$(cast wallet address --private-key "$NEW_PK")
info "Address:     $NEW_ADDR"
info "Private key: $NEW_PK"

# ---------------------------------------------------------------------------
step "2: Fund the new account"
# ---------------------------------------------------------------------------
cast send --rpc-url "$RPC_URL" --private-key "$OWNER_KEY" \
    "$NEW_ADDR" --value 1ether > /dev/null
BALANCE=$(cast balance --rpc-url "$RPC_URL" "$NEW_ADDR" --ether)
info "Balance: $BALANCE ETH"
pass "Account funded"

# ---------------------------------------------------------------------------
step "3: Verify new account CANNOT manage validators (no role)"
# ---------------------------------------------------------------------------
if quiet_send --private-key "$NEW_PK" "$VM_ADDRESS" \
    "unregister(address)" "$TARGET_ADDR"; then
    fail "unregister should have reverted (no VALIDATOR_MANAGER_ROLE)"
fi
pass "unregister correctly reverted for unauthorized account"

# ---------------------------------------------------------------------------
step "4: Grant VALIDATOR_MANAGER_ROLE to the new account"
# ---------------------------------------------------------------------------
cast send --rpc-url "$RPC_URL" --private-key "$OWNER_KEY" \
    "$VM_ADDRESS" "grantRole(bytes32,address)" \
    "$VALIDATOR_MANAGER_ROLE" "$NEW_ADDR" > /dev/null

HAS_ROLE=$(cast call --rpc-url "$RPC_URL" "$VM_ADDRESS" \
    "hasRole(bytes32,address)(bool)" "$VALIDATOR_MANAGER_ROLE" "$NEW_ADDR")

[[ "$HAS_ROLE" == "true" ]] || fail "hasRole returned false after grantRole"
pass "VALIDATOR_MANAGER_ROLE granted and confirmed"

# ---------------------------------------------------------------------------
step "5: Unregister validator from the delegated account"
# ---------------------------------------------------------------------------
cast send --rpc-url "$RPC_URL" --private-key "$NEW_PK" \
    "$VM_ADDRESS" "unregister(address)" "$TARGET_ADDR" > /dev/null

IS_VAL=$(cast call --rpc-url "$RPC_URL" "$VM_ADDRESS" "isValidator(address)(bool)" "$TARGET_ADDR")
[[ "$IS_VAL" == "false" ]] || fail "Validator still registered after unregister"
pass "Validator unregistered by delegated manager"

# ---------------------------------------------------------------------------
step "6: Re-register the same validator from the delegated account"
# ---------------------------------------------------------------------------
cast send --rpc-url "$RPC_URL" --private-key "$NEW_PK" \
    "$VM_ADDRESS" "register(bytes,uint64)" "$TARGET_PUBKEY" "$TARGET_POWER" > /dev/null

IS_VAL=$(cast call --rpc-url "$RPC_URL" "$VM_ADDRESS" "isValidator(address)(bool)" "$TARGET_ADDR")
[[ "$IS_VAL" == "true" ]] || fail "Validator not registered after register"
pass "Validator re-registered by delegated manager"

# ---------------------------------------------------------------------------
step "7: Revoke VALIDATOR_MANAGER_ROLE and verify access is removed"
# ---------------------------------------------------------------------------
cast send --rpc-url "$RPC_URL" --private-key "$OWNER_KEY" \
    "$VM_ADDRESS" "revokeRole(bytes32,address)" \
    "$VALIDATOR_MANAGER_ROLE" "$NEW_ADDR" > /dev/null

HAS_ROLE=$(cast call --rpc-url "$RPC_URL" "$VM_ADDRESS" \
    "hasRole(bytes32,address)(bool)" "$VALIDATOR_MANAGER_ROLE" "$NEW_ADDR")
[[ "$HAS_ROLE" == "false" ]] || fail "hasRole still true after revokeRole"
pass "VALIDATOR_MANAGER_ROLE revoked"

if quiet_send --private-key "$NEW_PK" "$VM_ADDRESS" \
    "unregister(address)" "$TARGET_ADDR"; then
    fail "unregister should have reverted after role was revoked"
fi
pass "unregister correctly reverted after role revocation"

# ---------------------------------------------------------------------------
step "8: Final state verification"
# ---------------------------------------------------------------------------
FINAL_COUNT=$(cast call --rpc-url "$RPC_URL" "$VM_ADDRESS" "getValidatorCount()(uint256)")
info "Validator count: $FINAL_COUNT (was: $INITIAL_COUNT)"

IS_VAL=$(cast call --rpc-url "$RPC_URL" "$VM_ADDRESS" "isValidator(address)(bool)" "$TARGET_ADDR")
[[ "$IS_VAL" == "true" ]] || fail "Target validator should still be registered"
pass "Validator set restored to original state"

printf "\n${GREEN}All access control tests passed.${NC}\n"
