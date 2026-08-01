#!/usr/bin/env bash
# Run the executor-yield profiling tests and collect Chrome trace files.
#
# Starts one shared 3-node localcluster, runs both profiling tests against it
# (paced baseline + continuous pump), then stops the cluster.  This avoids
# the 3–4 min cluster startup cost being charged to each test individually.
#
# Usage:
#   ./scripts/profile-executor-yield.sh [--local-only] [--rotsee-only] [--all]
#
# Options:
#   --local-only     Run only the local-cluster tests (default)
#   --rotsee-only    Run only the Rotsee testnet test (requires EDGLI_ROTSEE_* vars)
#   --all            Run both local and Rotsee tests
#
# Configuration (all overridable via env):
#   HOPRD_RELEASE_DIR        no default — point it at your hoprnet/hoprd build dir
#   HOPRD_LOCALCLUSTER_BIN   default: $HOPRD_RELEASE_DIR/hoprd-localcluster
#   HOPRD_BIN                default: $HOPRD_RELEASE_DIR/hoprd
#   HOPRD_CHAIN_IMAGE        default: bloklid-anvil:latest from GCR
#   HOPRD_CONTAINER_RUNTIME  default: container  (macOS Apple native runtime)
#   EDGLI_TRACE_DIR          default: ./profiling-results
#   RUST_LOG                 default: info,edgli=debug,tokio=trace,runtime=trace
#
# Cluster startup timeout: 10 minutes (CLUSTER_START_TIMEOUT below).

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# ── Parse arguments ──────────────────────────────────────────────────────────
RUN_LOCAL=true
RUN_ROTSEE=false
for arg in "$@"; do
    case "$arg" in
    --local-only)
        RUN_LOCAL=true
        RUN_ROTSEE=false
        ;;
    --rotsee-only)
        RUN_LOCAL=false
        RUN_ROTSEE=true
        ;;
    --all)
        RUN_LOCAL=true
        RUN_ROTSEE=true
        ;;
    *)
        echo "Unknown option: $arg"
        exit 1
        ;;
    esac
done

# ── Configuration ────────────────────────────────────────────────────────────
# No default: set HOPRD_RELEASE_DIR (or HOPRD_LOCALCLUSTER_BIN / HOPRD_BIN) to your hoprd build dir.
HOPRD_RELEASE_DIR="${HOPRD_RELEASE_DIR:-}"
export HOPRD_LOCALCLUSTER_BIN="${HOPRD_LOCALCLUSTER_BIN:-$HOPRD_RELEASE_DIR/hoprd-localcluster}"
export HOPRD_BIN="${HOPRD_BIN:-$HOPRD_RELEASE_DIR/hoprd}"
export HOPRD_CHAIN_IMAGE="${HOPRD_CHAIN_IMAGE:-europe-west3-docker.pkg.dev/hoprassociation/docker-images/bloklid-anvil:latest}"
export HOPRD_CONTAINER_RUNTIME="${HOPRD_CONTAINER_RUNTIME:-container}"
export EDGLI_TRACE_DIR="${EDGLI_TRACE_DIR:-$REPO_ROOT/profiling-results}"
export RUST_LOG="${RUST_LOG:-info,edgli=debug,tokio=trace,runtime=trace}"
export RUSTFLAGS="--cfg tokio_unstable"

CLUSTER_START_TIMEOUT=600 # seconds to wait for cluster to reach "running"

# ── Validate binaries ────────────────────────────────────────────────────────
missing=()
[[ -x $HOPRD_LOCALCLUSTER_BIN ]] || missing+=("$HOPRD_LOCALCLUSTER_BIN")
[[ -x $HOPRD_BIN ]] || missing+=("$HOPRD_BIN")

if [[ ${#missing[@]} -gt 0 ]]; then
    echo "ERROR: missing or non-executable binaries:"
    for b in "${missing[@]}"; do echo "  $b"; done
    echo ""
    echo "Build them with (from hoprnet/hoprd):"
    echo "  cargo build --release -p hoprd -p hoprd-localcluster"
    echo ""
    echo "Or override:"
    echo "  HOPRD_RELEASE_DIR=/your/path ./scripts/profile-executor-yield.sh"
    exit 1
fi

# ── Validate required tools ──────────────────────────────────────────────────
missing_tools=()
for tool in jq cargo-nextest pgrep du; do
    command -v "$tool" >/dev/null 2>&1 || missing_tools+=("$tool")
done
if [[ ${#missing_tools[@]} -gt 0 ]]; then
    echo "ERROR: missing required tools: ${missing_tools[*]}"
    exit 1
fi

# ── Cleanup trap ─────────────────────────────────────────────────────────────
CLUSTER_PID=""
CLUSTER_DATA_DIR=""

# shellcheck disable=SC2329  # invoked indirectly via `trap cleanup EXIT` below
cleanup() {
    # Stop the managed cluster if we started one.
    if [[ -n $CLUSTER_PID ]] && kill -0 "$CLUSTER_PID" 2>/dev/null; then
        echo ""
        echo "Stopping localcluster (PID $CLUSTER_PID)..."
        kill -INT "$CLUSTER_PID" 2>/dev/null || true
        # Wait up to 30 s for graceful exit.
        local deadline=$(($(date +%s) + 30))
        while kill -0 "$CLUSTER_PID" 2>/dev/null && [[ $(date +%s) -lt $deadline ]]; do
            sleep 1
        done
        kill -KILL "$CLUSTER_PID" 2>/dev/null || true
    fi

    # Remove the temporary cluster data dir we created (node DBs, keys, logs).
    if [[ -n $CLUSTER_DATA_DIR && -d $CLUSTER_DATA_DIR ]]; then
        rm -rf "$CLUSTER_DATA_DIR"
    fi

    # Warn about any remaining hoprd orphans. `pgrep -f` uses ERE, so `|` is the
    # alternation operator (an escaped `\|` would match a literal pipe).
    local orphans
    orphans="$(pgrep -f "hoprd-localcluster|hoprd --" 2>/dev/null || true)"
    if [[ -n $orphans ]]; then
        echo ""
        echo "WARNING: possible orphaned hoprd processes (PIDs: $(echo "$orphans" | tr '\n' ' '))"
        echo "Kill manually if needed:"
        echo "  pkill -f 'hoprd-localcluster|hoprd --'"
    fi
}
trap cleanup EXIT

# ── Prepare output dir ───────────────────────────────────────────────────────
mkdir -p "$EDGLI_TRACE_DIR"

echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo " edgli executor-yield profiling"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo " hoprd-localcluster : $HOPRD_LOCALCLUSTER_BIN"
echo " hoprd              : $HOPRD_BIN"
echo " chain image        : $HOPRD_CHAIN_IMAGE"
echo " container runtime  : $HOPRD_CONTAINER_RUNTIME"
echo " trace output       : $EDGLI_TRACE_DIR"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"

# ── Build ────────────────────────────────────────────────────────────────────
cd "$REPO_ROOT"
echo ""
echo "[1/3] Building profiling test binary..."
cargo build --test edgli_profiling --profile tracer --features prof

# ── Start cluster (local tests only) ─────────────────────────────────────────
if [[ $RUN_LOCAL == "true" ]]; then
    CLUSTER_DATA_DIR="$(mktemp -d /tmp/edgli-prof-cluster.XXXXXX)"
    echo ""
    echo "[2/3] Starting 3-node localcluster in background..."
    echo "      data dir: $CLUSTER_DATA_DIR"

    "$HOPRD_LOCALCLUSTER_BIN" \
        --hoprd-bin "$HOPRD_BIN" \
        --size 3 \
        --extra-identities 1 \
        --data-dir "$CLUSTER_DATA_DIR" \
        --api-host "127.0.0.1" \
        --api-port-base 13000 \
        --p2p-port-base 19000 \
        --api-token "test-token-localcluster" \
        --chain-image "$HOPRD_CHAIN_IMAGE" \
        --container-runtime "$HOPRD_CONTAINER_RUNTIME" \
        &
    CLUSTER_PID=$!
    echo "      PID: $CLUSTER_PID"

    # Poll hoprd-localcluster status until "running" or timeout.
    echo ""
    echo "      Waiting for cluster to reach 'running' state (timeout: ${CLUSTER_START_TIMEOUT}s)..."
    deadline=$(($(date +%s) + CLUSTER_START_TIMEOUT))
    cluster_ready=false
    while [[ $(date +%s) -lt $deadline ]]; do
        if ! kill -0 "$CLUSTER_PID" 2>/dev/null; then
            echo "ERROR: localcluster process exited prematurely"
            exit 1
        fi
        state=$("$HOPRD_LOCALCLUSTER_BIN" status --data-dir "$CLUSTER_DATA_DIR" 2>/dev/null |
            jq -r '.state' 2>/dev/null || true)
        if [[ $state == "running" ]]; then
            cluster_ready=true
            break
        fi
        printf "      cluster state: %-20s\r" "${state:-waiting...}"
        sleep 5
    done

    if [[ $cluster_ready != "true" ]]; then
        echo ""
        echo "ERROR: cluster did not reach 'running' within ${CLUSTER_START_TIMEOUT}s"
        exit 1
    fi
    echo ""
    echo "      ✓ cluster is running"

    # Export data dir so tests use external (already-running) cluster mode.
    export HOPRD_CLUSTER_DATA_DIR="$CLUSTER_DATA_DIR"
fi

# ── Run tests ────────────────────────────────────────────────────────────────
echo ""
echo "[3/3] Running profiling tests..."

# Local tests share the already-running cluster via HOPRD_CLUSTER_DATA_DIR.
# Rotsee test manages its own external network via env vars.
run_tests=()
if [[ $RUN_LOCAL == "true" ]]; then
    run_tests+=(
        "edgli_profiling_paced_pump_baseline"
        "edgli_profiling_continuous_pump"
    )
fi
if [[ $RUN_ROTSEE == "true" ]]; then
    run_tests+=("edgli_profiling_continuous_pump_rotsee")
fi

# Don't let a single failing test abort the run before the results/trace-file
# report — that report is most useful precisely when a test failed.
tests_failed=0
for test in "${run_tests[@]}"; do
    echo ""
    echo "── $test ──"
    cargo nextest run \
        --test edgli_profiling \
        --cargo-profile tracer \
        --features prof \
        --run-ignored ignored-only \
        --no-capture \
        --test-threads 1 \
        -E "test(=$test)" || tests_failed=1
done

# ── Results ──────────────────────────────────────────────────────────────────
echo ""
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo " Results"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo ""

trace_files=()
while IFS= read -r -d '' f; do
    trace_files+=("$f")
done < <(find "$EDGLI_TRACE_DIR" -name "edgli-trace-*.json" -print0 2>/dev/null)

if [[ ${#trace_files[@]} -eq 0 ]]; then
    echo " No trace files found in $EDGLI_TRACE_DIR"
    echo " The tests may have timed out before writing traces."
else
    echo " Trace files:"
    for f in "${trace_files[@]}"; do
        size=$(du -h "$f" | cut -f1)
        echo "   $size  $f"
    done
    echo ""
    echo " Load at: https://ui.perfetto.dev"
    echo " (File → Open trace file, or drag-and-drop)"
fi
echo ""

# Propagate test failure so CI / callers see the correct exit status.
exit "$tests_failed"
