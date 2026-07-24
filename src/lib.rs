#[cfg(feature = "runtime-tokio")]
pub mod client;
pub mod errors;
pub mod telemetry;
mod telemetry_common;

#[cfg(feature = "blokli")]
pub mod blokli;

pub mod strategy;
pub mod traits;

pub use hopr_lib;

#[cfg(feature = "blokli")]
pub use blokli::*;
pub use hopr_chain_connector::BlockchainConnectorConfig;
pub use hopr_lib::exports::transport::path::PathPlannerConfig;

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
/// Pass the result as the `path_planner` argument of [`Edgli::new`] or
/// [`run_hopr_edge_node_with`] to activate latency-optimised routing.
pub fn latency_path_planner_config(min_ack_rate: f64) -> PathPlannerConfig {
    PathPlannerConfig {
        min_ack_rate,
        latency_halflife: std::time::Duration::from_millis(20),
        min_paths_anonymity_floor: 2,
        ..PathPlannerConfig::default()
    }
}

#[cfg(feature = "runtime-tokio")]
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
