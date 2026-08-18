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
// Re-exported so consumers constructing a `BlokliEndpoint` do not need their own
// `url` dependency, which would have to match this crate's version to unify.
#[cfg(feature = "blokli")]
pub use url::Url;
// `AccountEntry::get_multiaddrs`, reachable via the chain API, exposes this type.
pub use multiaddr;

/// Returns a [`PathPlannerConfig`] optimised for low-latency path selection.
///
/// Uses a much shorter latency half-life (`20 ms`) than the default (`100 ms`) so
/// paths with lower observed round-trip times receive a stronger preference
/// during candidate scoring and pruning, and prunes candidates down to the two
/// lowest-latency paths to reduce per-packet relay rotation (and with it the
/// latency variance that causes frame-reassembly reordering).  The
/// `min_ack_rate` controls the minimum message-acknowledgement rate an edge
/// must exhibit before it is eligible for path inclusion.
///
/// Pass the result as the `path_planner` configuration when constructing an
/// edge client to activate latency-optimised routing. The concrete client is
/// available when both the `runtime-tokio` and `blokli` features are enabled.
pub fn latency_path_planner_config(min_ack_rate: f64) -> PathPlannerConfig {
    PathPlannerConfig {
        min_ack_rate,
        latency_halflife: std::time::Duration::from_millis(20),
        min_paths_anonymity_floor: 2,
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

pub use strategy::{BalanceRecommendation, Capacity, CapacityAllocator, StartupCosts};

#[cfg(feature = "blokli")]
pub use strategy::minimum_balance_recommendation;

#[cfg(feature = "telemetry")]
pub use hopr_lib::collect_hopr_metrics;
