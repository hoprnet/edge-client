//! Executor-starvation profiling harness for identifying tokio executor yield issues.
//!
//! # Background
//!
//! Production applications using `transfer_session` / `copy_duplex` with a fast data
//! source can hold a tokio worker thread indefinitely:
//!
//! - `copy_duplex` → `poll_copy` loops without returning `Poll::Pending`
//! - `AsyncWriteSink::poll_write` → `CrossfireSink::poll_ready` **always** returns
//!   `Poll::Ready` (capacity = 200,000 slots)
//! - The tight write loop monopolises one tokio worker thread
//! - SURB balancer and ack-processing tasks are starved → SURB replenishment stalls
//!   → session blocks on echo return → measured throughput collapses 10×
//!
//! # Requirements
//!
//! Two requirements must hold together or tokio-console sees nothing:
//!
//! 1. **`RUSTFLAGS="--cfg tokio_unstable"`** — enables tokio's internal task
//!    instrumentation at compile time.
//!
//! 2. **`--profile tracer`** — the `tracer` profile inherits `release` (optimised,
//!    no stack overflow) but sets `debug-assertions = true`, which silences the
//!    `tracing/release_max_level_debug` static filter.  Without this flag the filter
//!    sets `STATIC_MAX_LEVEL = DEBUG`, and every `trace!` callsite — including
//!    tokio's task spans — is compiled to a no-op.  The runtime directive
//!    `tokio=trace` cannot resurrect callsites removed at compile time.
//!
//! 3. **`--features prof`** — pulls in `console-subscriber` and `tracing-chrome`.
//!
//! # Running
//!
//! Use the provided script — it handles env vars, build, and result collection:
//!
//! ```text
//! ./scripts/profile-executor-yield.sh
//! ```
//!
//! Or manually:
//!
//! ```text
//! export HOPRD_LOCALCLUSTER_BIN=/path/to/hoprd-localcluster
//! export HOPRD_BIN=/path/to/hoprd
//! export HOPRD_CHAIN_IMAGE='europe-west3-docker.pkg.dev/hoprassociation/docker-images/bloklid-anvil:0.10.5-pr.349@sha256:2e6747d9d6c97255474e243b5088d131f01bb67b5d8f17dbac6bb8aafdf1d7b6'
//! export HOPRD_CONTAINER_RUNTIME=container   # macOS Apple runtime
//! export EDGLI_TRACE_DIR=./profiling-results
//! export RUST_LOG=info,edgli=debug,tokio=trace,runtime=trace
//!
//! RUSTFLAGS="--cfg tokio_unstable" \
//! cargo nextest run \
//!   --test edgli_profiling \
//!   --profile tracer --features prof \
//!   --run-ignored ignored-only --no-capture --test-threads 1
//! ```
//!
//! # Output
//!
//! Each test writes a Chrome trace JSON file to `$EDGLI_TRACE_DIR/`:
//! - `edgli-trace-paced.json`    — baseline (paced 16-packet batches, free task interleaving)
//! - `edgli-trace-continuous.json` — starvation case (single write_all, writer holds thread)
//!
//! Load the files at <https://ui.perfetto.dev>.
//!
//! # What to look for
//!
//! In `edgli-trace-continuous.json`:
//! - **Session write task**: one single long-running poll spanning the whole `write_all`
//! - **SURB balancer task**: long gaps between wakeups (starved — can't run while writer holds thread)
//! - **Ack-processing task**: same pattern
//!
//! In `edgli-trace-paced.json`:
//! - All tasks interleave freely during the 100 ms inter-batch windows
//! - SURB balancer wakes up regularly between batches

mod common;

// ─── Subscriber setup ────────────────────────────────────────────────────────

/// Initialise tracing with both tokio-console (live TUI) and tracing-chrome
/// (persistent JSON file).  Returns the chrome flush guard — keep it alive
/// for the duration of the test.
///
/// The chrome trace is written to `$EDGLI_TRACE_DIR/<filename>` (or `./<filename>`
/// if the env var is not set).
///
/// Uses `try_init()` so it is safe if multiple tests share a process (e.g.
/// with the standard `cargo test` runner).  Subsequent calls are no-ops — the
/// first test's subscriber stays active.
#[cfg(feature = "prof")]
fn init_subscriber(filename: &str) -> tracing_chrome::FlushGuard {
    use tracing_subscriber::prelude::*;

    let dir = std::env::var("EDGLI_TRACE_DIR").unwrap_or_else(|_| ".".to_string());
    let path = format!("{dir}/{filename}");

    let (chrome_layer, guard) = tracing_chrome::ChromeLayerBuilder::new()
        .file(&path)
        .include_args(true)
        .build();

    // console_subscriber::spawn() starts the gRPC server on the current tokio
    // runtime and returns a Layer — no separate tokio::spawn needed.
    tracing_subscriber::registry()
        .with(console_subscriber::spawn())
        .with(chrome_layer)
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .try_init()
        .ok(); // swallow "already set" if multiple tests run in the same process

    eprintln!("Chrome trace → {path}");
    guard
}

// ─── Tests ───────────────────────────────────────────────────────────────────

/// Paced-pump baseline with tokio-console + Chrome trace active.
///
/// Writes `edgli-trace-paced.json`.  Observe the trace in Perfetto to see what
/// "healthy" task interleaving looks like during the 100 ms inter-batch gaps.
/// Compare against `edgli_profiling_continuous_pump`.
#[cfg(feature = "prof")]
#[ignore]
#[tokio::test(flavor = "multi_thread")]
async fn edgli_profiling_paced_pump_baseline() -> anyhow::Result<()> {
    let _guard = init_subscriber("edgli-trace-paced.json");
    common::run_one_megabyte_session_test(common::Network::Local).await
}

/// Continuous-pump starvation case with tokio-console + Chrome trace active.
///
/// Writes `edgli-trace-continuous.json`.  A single `write_all(1 MiB)` without
/// any inter-batch sleep replicates production `transfer_session`/`copy_duplex`
/// behaviour.  In the trace you should observe:
///
/// - Writer task: one very long poll (entire `write_all` without yielding)
/// - SURB balancer: long idle gaps (starved while writer holds the thread)
/// - Echo throughput significantly lower than the paced baseline
#[cfg(feature = "prof")]
#[ignore]
#[tokio::test(flavor = "multi_thread")]
async fn edgli_profiling_continuous_pump() -> anyhow::Result<()> {
    let _guard = init_subscriber("edgli-trace-continuous.json");
    common::run_throughput_comparison_test(common::Network::Local).await
}

/// Rotsee variant — same as `edgli_profiling_continuous_pump` against the
/// public Rotsee testnet.  Requires the `EDGLI_ROTSEE_*` env vars.
/// Writes `edgli-trace-rotsee.json`.
#[cfg(feature = "prof")]
#[ignore]
#[tokio::test(flavor = "multi_thread")]
async fn edgli_profiling_continuous_pump_rotsee() -> anyhow::Result<()> {
    let _guard = init_subscriber("edgli-trace-rotsee.json");
    common::run_throughput_comparison_test(common::Network::Rotsee).await
}
