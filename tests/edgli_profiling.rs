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
//! # tokio-console setup
//!
//! Two requirements must hold together or tokio-console sees nothing:
//!
//! 1. **`RUSTFLAGS="--cfg tokio_unstable"`** — enables tokio's internal task
//!    instrumentation at compile time.
//!
//! 2. **`--profile tracer`** — the `tracer` profile inherits `release` (optimised,
//!    no stack overflow) but sets `debug-assertions = true`, which silences the
//!    `tracing/release_max_level_debug` static filter. Without this flag the filter
//!    sets `STATIC_MAX_LEVEL = DEBUG`, and every `trace!` callsite — including
//!    tokio's task spans — is compiled to a no-op. The subscriber directive
//!    `tokio=trace` added at runtime cannot resurrect callsites that were removed
//!    at compile time.
//!
//! 3. **`--features prof`** — wires `console_subscriber` into the tracing stack.
//!    The test below calls `console_subscriber::init()` directly; the regular
//!    `init_logger()` path in `main.rs` does the same for the binary.
//!
//! # How to run
//!
//! ```text
//! # Terminal 1 — start the localcluster (external mode example)
//! hoprd-localcluster --size 3 --extra-identities 1 \
//!   --api-port-base 13000 --p2p-port-base 19000 \
//!   --api-token test-token-localcluster \
//!   --data-dir /tmp/edgli-cluster \
//!   --chain-image '<bloklid-anvil image>' ...
//!
//! # Wait for {"state":"running"}:
//! hoprd-localcluster status --data-dir /tmp/edgli-cluster
//!
//! # Terminal 2 — run profiling tests
//! export HOPRD_LOCALCLUSTER_BIN=/path/to/hoprd-localcluster
//! export HOPRD_CLUSTER_DATA_DIR=/tmp/edgli-cluster
//!
//! RUSTFLAGS="--cfg tokio_unstable" \
//! RUST_LOG=info,edgli=debug \
//! cargo test --test edgli_profiling \
//!   --profile tracer --features prof \
//!   -- --ignored --nocapture
//!
//! # Terminal 3 — attach tokio-console (install once with: cargo install tokio-console)
//! tokio-console
//! ```
//!
//! # What to look for
//!
//! With executor starvation present:
//! - **Session write task**: very short poll time (channel always ready → fast) but the
//!   `poll_copy` loop NEVER returns `Poll::Pending`, so it holds the thread until all
//!   data is written; show as a single long-running poll of the parent copy task.
//! - **SURB balancer**: high "idle" time between wakeups — starved.
//! - **ack processing task**: same — starved.
//!
//! With a fix (e.g. `tokio::task::yield_now()` after each `poll_write` batch, or a
//! tighter channel bound that lets `poll_ready` return `Poll::Pending`):
//! - All tasks interleave normally.
//! - SURB replenishment keeps pace with data consumption.
//! - End-to-end throughput improves toward the in-process test baseline.

mod common;

// ─── Profiling tests ────────────────────────────────────────────────────────
//
// Gated on `#[cfg(feature = "prof")]` so they are never compiled into the
// default test binary — they require both `--features prof` and
// `RUSTFLAGS="--cfg tokio_unstable"`.

/// Profiling run of the standard 1 MiB session test with tokio-console active.
///
/// This uses the *paced* pump (16 packets per batch, 100 ms sleep) as a
/// baseline.  Attach `tokio-console` while this runs and observe task
/// behaviour during the 100 ms idle windows between write batches — those
/// windows show the "healthy" state where all tasks run freely.
///
/// Compare against `edgli_profiling_continuous_pump` to see what changes
/// when the pacing is removed.
#[cfg(feature = "prof")]
#[ignore]
#[tokio::test(flavor = "multi_thread")]
async fn edgli_profiling_paced_pump_baseline() -> anyhow::Result<()> {
    // console_subscriber::init() sets the global tracing subscriber so
    // tokio-console can connect.  It must be called before any tokio tasks
    // are spawned (including those in Edgli::new).
    //
    // IMPORTANT: this call succeeds only when `STATIC_MAX_LEVEL >= TRACE`,
    // i.e. when built with `--profile tracer` (debug-assertions = true).
    // With `--profile release` the `tracing/release_max_level_debug` feature
    // compiles all `trace!` callsites to no-ops, and tokio-console sees
    // zero tasks — making the profiling run useless.
    console_subscriber::init();

    common::run_one_megabyte_session_test(common::Network::Local).await
}

/// Profiling run of the **continuous** (unthrottled) pump.
///
/// Unlike `pump_and_verify`, `pump_continuous` issues a single
/// `write_all(1 MiB)` without any inter-batch sleep.  This replicates how
/// production callers use `transfer_session` / `copy_duplex` with a fast
/// data source: the write loop never blocks because `CrossfireSink` (capacity
/// 200,000) almost never fills up, so `poll_copy` never returns `Poll::Pending`
/// and the tokio worker thread is held for the entire write.
///
/// With tokio-console attached you should observe:
/// - The session write task shows a single very long poll (the whole `write_all`)
///   instead of many short ones.
/// - The SURB balancer and ack processing tasks show long gaps between wakeups
///   (executor starvation) while the write is in progress.
/// - Effective echo throughput is noticeably lower than the paced baseline.
#[cfg(feature = "prof")]
#[ignore]
#[tokio::test(flavor = "multi_thread")]
async fn edgli_profiling_continuous_pump() -> anyhow::Result<()> {
    console_subscriber::init();

    common::run_throughput_comparison_test(common::Network::Local).await
}

/// Rotsee variant of the continuous-pump profiling test.
///
/// Same as `edgli_profiling_continuous_pump` but against the public Rotsee
/// testnet.  Requires the `EDGLI_ROTSEE_*` environment variables.
#[cfg(feature = "prof")]
#[ignore]
#[tokio::test(flavor = "multi_thread")]
async fn edgli_profiling_continuous_pump_rotsee() -> anyhow::Result<()> {
    console_subscriber::init();

    common::run_throughput_comparison_test(common::Network::Rotsee).await
}
