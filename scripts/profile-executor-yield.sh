#!/usr/bin/env bash
# Run the executor-yield profiling tests and collect Chrome trace files.
#
# Each test starts a 3-node localcluster, runs paced vs continuous session
# pumps, and writes a Chrome trace JSON to EDGLI_TRACE_DIR.  Load results
# at https://ui.perfetto.dev.
#
# Usage:
#   ./scripts/profile-executor-yield.sh [--local-only] [--rotsee-only]
#
# Options:
#   --local-only     Run only the local-cluster tests (default)
#   --rotsee-only    Run only the Rotsee testnet test (requires EDGLI_ROTSEE_* vars)
#   --all            Run both local and Rotsee tests
#
# Configuration (all overridable via env):
#   HOPRD_RELEASE_DIR        default: ~/Fun/hoprnet.org/hoprd/target/release
#   HOPRD_LOCALCLUSTER_BIN   default: $HOPRD_RELEASE_DIR/hoprd-localcluster
#   HOPRD_BIN                default: $HOPRD_RELEASE_DIR/hoprd
#   HOPRD_CHAIN_IMAGE        default: bloklid-anvil image from localcluster/docker-compose.yml
#   HOPRD_CONTAINER_RUNTIME  default: container  (macOS Apple runtime)
#   EDGLI_TRACE_DIR          default: ./profiling-results
#   RUST_LOG                 default: info,edgli=debug,tokio=trace,runtime=trace

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# ── Parse arguments ──────────────────────────────────────────────────────────
RUN_LOCAL=true
RUN_ROTSEE=false
for arg in "$@"; do
    case "$arg" in
        --local-only)  RUN_LOCAL=true;  RUN_ROTSEE=false ;;
        --rotsee-only) RUN_LOCAL=false; RUN_ROTSEE=true  ;;
        --all)         RUN_LOCAL=true;  RUN_ROTSEE=true  ;;
        *) echo "Unknown option: $arg"; exit 1 ;;
    esac
done

# ── Configuration ────────────────────────────────────────────────────────────
HOPRD_RELEASE_DIR="${HOPRD_RELEASE_DIR:-$HOME/Fun/hoprnet.org/hoprd/target/release}"
export HOPRD_LOCALCLUSTER_BIN="${HOPRD_LOCALCLUSTER_BIN:-$HOPRD_RELEASE_DIR/hoprd-localcluster}"
export HOPRD_BIN="${HOPRD_BIN:-$HOPRD_RELEASE_DIR/hoprd}"
export HOPRD_CHAIN_IMAGE="${HOPRD_CHAIN_IMAGE:-europe-west3-docker.pkg.dev/hoprassociation/docker-images/bloklid-anvil:0.10.5-pr.349@sha256:2e6747d9d6c97255474e243b5088d131f01bb67b5d8f17dbac6bb8aafdf1d7b6}"
export HOPRD_CONTAINER_RUNTIME="${HOPRD_CONTAINER_RUNTIME:-container}"
export EDGLI_TRACE_DIR="${EDGLI_TRACE_DIR:-$REPO_ROOT/profiling-results}"
export RUST_LOG="${RUST_LOG:-info,edgli=debug,tokio=trace,runtime=trace}"
export RUSTFLAGS="--cfg tokio_unstable"

# ── Validate binaries ────────────────────────────────────────────────────────
missing=()
[[ -x "$HOPRD_LOCALCLUSTER_BIN" ]] || missing+=("$HOPRD_LOCALCLUSTER_BIN")
[[ -x "$HOPRD_BIN" ]]              || missing+=("$HOPRD_BIN")

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

# ── Cleanup trap ─────────────────────────────────────────────────────────────
# The test manages cluster lifetime via ClusterHandle (Drop sends SIGINT).
# If the test process is force-killed (SIGKILL), orphaned hoprd processes may
# linger.  This trap warns and lists them.
cleanup() {
    local orphans
    orphans="$(pgrep -f "hoprd-localcluster\|hoprd --" 2>/dev/null || true)"
    if [[ -n "$orphans" ]]; then
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
echo "[1/2] Building profiling test binary..."
cargo build --test edgli_profiling --profile tracer --features prof

# ── Run tests ────────────────────────────────────────────────────────────────
echo ""
echo "[2/2] Running profiling tests..."

# Select which tests to run.
# Each test gets its own process (nextest), so console_subscriber::init() is safe.
run_tests=()
if [[ "$RUN_LOCAL" == "true" ]]; then
    run_tests+=(
        "edgli_profiling_paced_pump_baseline"
        "edgli_profiling_continuous_pump"
    )
fi
if [[ "$RUN_ROTSEE" == "true" ]]; then
    run_tests+=("edgli_profiling_continuous_pump_rotsee")
fi

for test in "${run_tests[@]}"; do
    echo ""
    echo "── $test ──"
    cargo nextest run \
        --test edgli_profiling \
        --profile tracer \
        --features prof \
        --run-ignored ignored-only \
        --no-capture \
        --test-threads 1 \
        -E "test(=$test)"
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
