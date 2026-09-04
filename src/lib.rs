// The concrete client needs both an async runtime and a blockchain connector;
// `blokli` is currently the only supported connector.
#[cfg(all(feature = "runtime-tokio", feature = "blokli"))]
pub mod client;
pub mod errors;

#[cfg(feature = "blokli")]
pub mod blokli;

#[cfg(feature = "blokli")]
pub mod endpoint;

pub mod strategy;
pub mod traits;

pub use hopr_lib;

#[cfg(feature = "blokli")]
pub use blokli::*;
#[cfg(feature = "blokli")]
pub use endpoint::*;
#[cfg(feature = "blokli")]
pub use hopr_chain_connector::{BlockchainConnectorConfig, DEFAULT_REQUEST_TIMEOUT};
pub use hopr_lib::exports::transport::path::PathPlannerConfig;
// Re-exported so consumers can set per-session flow control (the `flow_control`
// field of `HoprSessionClientConfig`) without reaching into `hopr_lib` internals.
pub use hopr_lib::exports::transport::FlowControlConfig;
// PIX session surface for consumers, plus SessionCapability (also needed for Segmentation/retransmission) which was previously only reachable via hopr_lib::.
#[cfg(feature = "pix")]
pub use hopr_lib::{InvalidPixParams, LOCAL_PIX_SUITE, PixParams, SessionCapability};
// Entry-side share generator dimensions, so protocol.pix doesn't need the full exports::transport::config path.
#[cfg(feature = "pix")]
pub use hopr_lib::exports::transport::config::PixGlobalConfig;
#[cfg(feature = "pix")]
pub use strategy::{PixEntryConfig, PixEntryPool, PixEntryStrategy, pix_ssa_quota, quota_per_ssa};
// Re-exported so consumers constructing a `BlokliEndpoint` do not need their own
// `url` dependency, which would have to match this crate's version to unify.
#[cfg(feature = "blokli")]
pub use url::Url;
// `AccountEntry::get_multiaddrs`, reachable via the chain API, exposes this type.
pub use multiaddr;

/// Returns a [`PathPlannerConfig`] tuned for edge clients: latency-preferring, but not at the cost
/// of return-path relayer diversity.
///
/// The `100 ms` half-life keeps latency a *soft* weight — a ~100 ms one-hop return path stays in
/// contention rather than being penalised out of it, while faster relayers still draw
/// proportionally more of the return stream (relaxed from the former hard `20 ms` bias).
/// `min_paths_anonymity_floor` is `0`, which disables latency-based candidate pruning, so every
/// reachable, ack-passing relayer is retained (up to `max_cached_paths`) instead of being dropped
/// down to the two fastest. Retention is what the return-path degradation detector needs — with a
/// sibling relayer still present (and kept warm by the default `return_path_exploration` draws) it
/// can corroborate a dead relayer instead of mistaking a single-relayer collapse for a quiet peer,
/// the collapse that made a 2026-08-28 tunnel outage undetectable. `min_ack_rate` is the minimum
/// message-acknowledgement rate an edge must exhibit before it is eligible for path inclusion.
///
/// Pass the result as the `path_planner` configuration when constructing an edge client. The
/// concrete client is available when both the `runtime-tokio` and `blokli` features are enabled.
pub fn latency_path_planner_config(min_ack_rate: f64) -> PathPlannerConfig {
    PathPlannerConfig {
        min_ack_rate,
        // 100 ms is the current default; kept explicit to document the deliberate relaxation from
        // the former hard 20 ms bias. Floor 0 overrides the default 8 to disable pruning (keep every
        // relayer). Rationale in the doc comment above.
        latency_halflife: std::time::Duration::from_millis(100),
        min_paths_anonymity_floor: 0,
        ..PathPlannerConfig::default()
    }
}

#[cfg(all(feature = "runtime-tokio", feature = "blokli"))]
pub use client::*;
pub use traits::{EdgeNodeApi, NodeBalances};

// Re-export types that appear in EdgeNodeApi method signatures so consumers
// do not need to dig into hopr_lib internal module paths.
pub use hopr_lib::api::types::{
    internal::channels::ChannelEntry,
    primitive::prelude::{Balance, XDai},
};

pub use strategy::{BalanceRecommendation, Capacity, CapacityAllocations, StartupCosts};

#[cfg(feature = "blokli")]
pub use strategy::minimum_balance_recommendation;

#[cfg(feature = "telemetry")]
pub use hopr_lib::collect_hopr_metrics;
