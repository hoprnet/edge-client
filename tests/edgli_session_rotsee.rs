//! End-to-end integration test: Edgli against the Rotsee public testnet.
//!
//! # What this test does
//!
//! 1. Reads a pre-funded Rotsee identity and network config from env vars.
//! 2. Boots Edgli with that identity.
//! 3. Lets the channel-lifecycle strategy open Edgli's outgoing channels.
//! 4. Pumps 1 MiB of random data through a 0-hop session (sanity check, no relay).
//! 5. Pumps 1 MiB of random data through a 1-hop session (full relay path).
//! 6. Verifies SHA-256 integrity of both echo round-trips.
//!
//! # Setup
//!
//! You need a HOPR identity file that is already funded and registered with a
//! Safe + HOPR module on Gnosis Chain (the chain Rotsee runs on).
//!
//! | Variable                      | Required | Description                           |
//! |-------------------------------|----------|---------------------------------------|
//! | `EDGLI_ROTSEE_BLOKLI_URL`     | yes      | Blokli endpoint for Rotsee            |
//! | `EDGLI_ROTSEE_IDENTITY_FILE`  | yes      | Path to funded keystore JSON          |
//! | `EDGLI_ROTSEE_IDENTITY_PASSWORD` | yes   | Keystore password                     |
//! | `EDGLI_ROTSEE_SAFE_ADDRESS`   | yes      | Safe contract address (0x…)           |
//! | `EDGLI_ROTSEE_MODULE_ADDRESS` | yes      | HOPR module contract address (0x…)    |
//!
//! ```text
//! export EDGLI_ROTSEE_BLOKLI_URL=https://blokli.rotsee.gnosisvpn.io
//! export EDGLI_ROTSEE_IDENTITY_FILE=/path/to/identity.json
//! export EDGLI_ROTSEE_IDENTITY_PASSWORD=my-password
//! export EDGLI_ROTSEE_SAFE_ADDRESS=0x...
//! export EDGLI_ROTSEE_MODULE_ADDRESS=0x...
//! # --release is required: HOPR's async future chains overflow the default debug stack
//! RUST_LOG=info,edgli=debug cargo test --test edgli_session_rotsee --release -- --ignored --nocapture
//! ```

mod common;

/// End-to-end test: Rotsee testnet + Edgli + 1 MiB session pump.
///
/// Gated behind `#[ignore]` — see module-level docs for required setup.
#[ignore]
#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn edgli_sends_one_megabyte_through_rotsee() -> anyhow::Result<()> {
    common::run_one_megabyte_session_test(common::Network::Rotsee).await
}
