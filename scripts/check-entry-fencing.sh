#!/usr/bin/env bash
#
# Verifies that the edge client (a HOPR Entry) links none of the relay or exit code.
#
# The fence lives in `hopr-lib`, `hopr-transport`, `hopr-protocol-hopr` and `hopr-ticket-manager`
# (hoprnet PR #8644, hopr-impls PR #131). This script asserts, from the resolved dependency graph of
# this crate, that an entry build keeps it:
#   - `hopr-lib` has the `entry` feature and neither `relay` nor `exit`,
#   - `hopr-protocol-hopr` is built without `relaying` (no forwarding path, no ticket validation),
#   - `hopr-ticket-manager` is built without `manager` (no incoming queues, no `ahash`/`hashbrown`),
#   - the `hopr-transport` role features `relay` and `exit` are off.
# Every absence check has a control: the same query for a feature that must be on, so a typo in a
# feature name cannot pass as a fence.
#
# Usage: scripts/check-entry-fencing.sh [cargo args...]   e.g. --no-default-features -F runtime-tokio,blokli
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo_root"
export CARGO_TERM_COLOR=never

cargo_args=("$@")
failures=0

# Features enabled on crate `$1` in the resolved graph, one per line.
enabled_features() {
  cargo tree "${cargo_args[@]}" -e features -i "$1" --prefix none 2>/dev/null \
    | sed -n 's/^.*feature "\(.*\)".*$/\1/p' | sort -u
}

has_feature() { # crate feature
  enabled_features "$1" | grep -qx "$2"
}

expect_on() {
  if has_feature "$1" "$2"; then echo "ok:   $1 has feature $2"; else echo "FAIL: $1 lacks feature $2 (control)"; failures=$((failures + 1)); fi
}

expect_off() {
  if has_feature "$1" "$2"; then echo "FAIL: $1 has feature $2"; failures=$((failures + 1)); else echo "ok:   $1 has no feature $2"; fi
}

# Controls: features that an entry must have.
expect_on hopr-lib entry
expect_on hopr-lib session-entry
expect_on hopr-transport entry
expect_on hopr-ticket-manager redb

# The fence.
expect_off hopr-lib relay
expect_off hopr-lib exit
expect_off hopr-lib session-exit
expect_off hopr-transport relay
expect_off hopr-transport exit
expect_off hopr-transport session-exit
expect_off hopr-protocol-hopr relaying
expect_off hopr-ticket-manager manager

# `ahash` and `hashbrown` enter `hopr-ticket-manager` only through `manager`; check the edge itself.
if cargo tree "${cargo_args[@]}" -e normal --prefix none -p hopr-ticket-manager --depth 1 2>/dev/null | grep -qE '^(ahash|hashbrown) '; then
  echo "FAIL: hopr-ticket-manager depends on ahash or hashbrown"
  failures=$((failures + 1))
else
  echo "ok:   hopr-ticket-manager has no ahash or hashbrown dependency"
fi

if [ "$failures" -ne 0 ]; then
  echo "$failures fence check(s) failed"
  exit 1
fi
echo "entry fence holds"
