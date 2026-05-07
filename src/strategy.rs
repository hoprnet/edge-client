pub use hopr_strategy::channel_lifecycle::ChannelLifecycleConfig;

/// Subset of strategies relevant to an edge node.
pub enum EdgeStrategyKind {
    ChannelLifecycle(ChannelLifecycleConfig),
}

/// Strategy configuration for an edge node reactor.
pub struct MultiStrategyConfig {
    /// Ordered list of strategies to run concurrently.
    pub strategies: Vec<EdgeStrategyKind>,
}

/// Returns the default [`MultiStrategyConfig`] for an edge client telemetry reactor.
///
/// Runs a single [`ChannelLifecycleStrategy`] with its built-in defaults, which
/// opens, funds, closes, and finalizes outgoing payment channels automatically.
pub fn default_edge_client_telemetry_reactor_cfg() -> MultiStrategyConfig {
    MultiStrategyConfig {
        strategies: vec![EdgeStrategyKind::ChannelLifecycle(
            ChannelLifecycleConfig::default(),
        )],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_cfg_has_one_strategy() {
        let cfg = default_edge_client_telemetry_reactor_cfg();
        assert_eq!(cfg.strategies.len(), 1);
    }

    #[test]
    fn default_cfg_strategy_is_channel_lifecycle() {
        let cfg = default_edge_client_telemetry_reactor_cfg();
        assert!(
            matches!(cfg.strategies[0], EdgeStrategyKind::ChannelLifecycle(_)),
            "expected strategy to be ChannelLifecycle"
        );
    }
}
