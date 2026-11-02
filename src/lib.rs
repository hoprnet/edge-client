// The concrete client needs both an async runtime and a blockchain connector;
// `blokli` is currently the only supported connector.
#[cfg(all(feature = "runtime-tokio", feature = "blokli"))]
pub mod client;
pub mod errors;

#[cfg(feature = "blokli")]
pub mod blokli;

#[cfg(feature = "blokli")]
pub mod discovery;

#[cfg(feature = "blokli")]
pub mod endpoint;

pub mod strategy;
pub mod traits;

pub use hopr_lib;

#[cfg(feature = "blokli")]
pub use blokli::*;
#[cfg(feature = "blokli")]
pub use discovery::*;
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

pub use strategy::{
    BalanceRecommendation, Capacity, CapacityAllocations, StartupCosts, StrategyState,
};

#[cfg(feature = "blokli")]
pub use strategy::minimum_balance_recommendation;

#[cfg(feature = "telemetry")]
pub use hopr_lib::collect_hopr_metrics;

/// Applies mixer timing overrides; any override forces the uniform engine, replacing a configured Poisson one.
/// Without overrides the operator's mixer config is returned untouched.
pub fn resolve_mixer_timings(
    mut mixer: hopr_lib::config::MixerConfig,
    min_delay_override: Option<std::time::Duration>,
    delay_range_override: Option<std::time::Duration>,
) -> hopr_lib::config::MixerConfig {
    use hopr_transport_mixer::config::{MixerType, UniformConfig};

    let has_override = min_delay_override.is_some() || delay_range_override.is_some();
    if !has_override {
        return mixer;
    }

    // These knobs only exist for the uniform engine, so an override selects it.
    let mut uniform = match mixer.mixer_type {
        MixerType::Uniform(operator_uniform) => operator_uniform,
        MixerType::Poisson(_) => UniformConfig::default(),
    };
    if let Some(min_delay) = min_delay_override {
        uniform.min_delay = min_delay;
    }
    if let Some(delay_range) = delay_range_override {
        uniform.delay_range = delay_range;
    }
    mixer.mixer_type = MixerType::Uniform(uniform);
    mixer
}

#[cfg(test)]
mod mixer_timing_tests {
    use std::time::Duration;

    use hopr_lib::config::MixerConfig;
    use hopr_transport_mixer::config::{MixerType, UniformConfig};

    use super::resolve_mixer_timings;

    fn uniform_mixer(min_delay_ms: u64, delay_range_ms: u64) -> MixerConfig {
        MixerConfig {
            mixer_type: MixerType::Uniform(UniformConfig {
                min_delay: Duration::from_millis(min_delay_ms),
                delay_range: Duration::from_millis(delay_range_ms),
            }),
            ..Default::default()
        }
    }

    #[test]
    fn honors_operator_config_without_overrides() {
        let operator_mixer = uniform_mixer(50, 200);
        let resolved = resolve_mixer_timings(operator_mixer, None, None);
        assert_eq!(resolved, operator_mixer);
    }

    #[test]
    fn applies_explicit_overrides() {
        let resolved = resolve_mixer_timings(
            uniform_mixer(50, 200),
            Some(Duration::from_millis(5)),
            Some(Duration::from_millis(10)),
        );
        assert_eq!(resolved, uniform_mixer(5, 10));
    }
}
