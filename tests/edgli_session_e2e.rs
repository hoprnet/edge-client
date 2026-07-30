//! End-to-end integration test: Edgli + hoprd localcluster.
//!
//! # What this test does
//!
//! 1. Spins up a 3-node HOPR cluster via `hoprd-localcluster`.
//! 2. Boots Edgli with a pre-funded extra identity created by the cluster.
//! 3. Verifies cluster state: full P2P peer visibility and full-mesh channels.
//! 4. Lets the channel-lifecycle strategy open Edgli's outgoing channels.
//! 5. Pumps 1 MiB of random data through a 0-hop session (sanity check, no relay).
//! 6. Pumps 1 MiB of random data through a 1-hop session (full relay path).
//! 7. Verifies SHA-256 integrity of both echo round-trips.
//!
//! # Modes of operation
//!
//! ## Managed mode (default): test owns cluster lifetime
//!
//! The test starts `hoprd-localcluster`, waits for readiness, runs the session
//! pumps, then tears the cluster down. Required env vars:
//!
//! | Variable                  | Required | Description                                       |
//! |---------------------------|----------|---------------------------------------------------|
//! | `HOPRD_LOCALCLUSTER_BIN`  | yes      | Path to `hoprd-localcluster` binary               |
//! | `HOPRD_BIN`               | yes      | Path to `hoprd` binary                            |
//! | `HOPRD_CHAIN_IMAGE`       | yes      | `bloklid-anvil` container image tag               |
//! | `HOPRD_CONTAINER_RUNTIME` | no       | Container runtime (default: `docker`)             |
//!
//! Both `hoprd-localcluster` and `hoprd` are built from the **`hoprnet/hoprd`** repo
//! (not `hoprnet/hoprnet`):
//!
//! ```text
//! # In the hoprnet/hoprd repo:
//! cargo build --release -p hoprd-localcluster -p hoprd
//!
//! export HOPRD_LOCALCLUSTER_BIN=/path/to/hoprd/target/release/hoprd-localcluster
//! export HOPRD_BIN=/path/to/hoprd/target/release/hoprd
//! export HOPRD_CHAIN_IMAGE='<bloklid-anvil image tag>'
//! # export HOPRD_CONTAINER_RUNTIME=container  # Apple native runtime
//!
//! # --release is required: HOPR's async future chains overflow the default debug stack
//! RUST_LOG=info,edgli=debug cargo test --test edgli_session_e2e --release -- --ignored --nocapture
//! ```
//!
//! ## External mode: attach to an already-running cluster
//!
//! Start the cluster manually in one terminal with a known `--data-dir`, then
//! point the test at it via `HOPRD_CLUSTER_DATA_DIR`:
//!
//! ```text
//! # Terminal 1 — start cluster
//! hoprd-localcluster --size 3 --extra-identities 1 \
//!   --api-port-base 13000 --p2p-port-base 19000 \
//!   --api-token test-token-localcluster \
//!   --data-dir /tmp/edgli-cluster \
//!   --chain-image '...' ...
//!
//! # Wait until status reports "running":
//! hoprd-localcluster status --data-dir /tmp/edgli-cluster
//! # ... wait for {"state":"running"} ...
//!
//! # Terminal 2 — run the test
//! export HOPRD_LOCALCLUSTER_BIN=/path/to/hoprd-localcluster
//! export HOPRD_CLUSTER_DATA_DIR=/tmp/edgli-cluster
//! RUST_LOG=info,edgli=debug cargo test --test edgli_session_e2e --release -- --ignored --nocapture
//! ```
//!
//! The test will NOT stop the external cluster when it finishes.

mod common;

/// End-to-end test: 3-node localcluster + Edgli + 1 MiB session pump.
///
/// Gated behind `#[ignore]` — see module-level docs for required setup.
#[ignore]
#[tokio::test(flavor = "multi_thread")]
async fn edgli_sends_one_megabyte_through_local_cluster() -> anyhow::Result<()> {
    common::run_one_megabyte_session_test(common::Network::Local).await
}
