pub use hopr_lib::api::types::primitive::prelude::HoprBalance;
pub use hopr_strategy::auto_funding::AutoFundingStrategyConfig;
pub use hopr_strategy::channel_finalizer::ClosureFinalizerStrategyConfig;

/// Subset of strategies relevant to an edge node.
pub enum EdgeStrategyKind {
    AutoFunding(AutoFundingStrategyConfig),
    ClosureFinalizer(ClosureFinalizerStrategyConfig),
}

/// Strategy configuration for an edge node reactor.
pub struct MultiStrategyConfig {
    /// Interval between periodic strategy ticks.
    pub execution_interval: std::time::Duration,
    /// Ordered list of strategies to run concurrently.
    pub strategies: Vec<EdgeStrategyKind>,
}

/// Returns the default [`MultiStrategyConfig`] for an edge client telemetry reactor.
///
/// Configures two strategies that run every 15 seconds:
/// 1. **AutoFunding** — tops up channels that fall below `min_channel_balance`
/// 2. **ClosureFinalizer** — force-closes channels that have been pending-close for >5 minutes
pub fn default_edge_client_telemetry_reactor_cfg(
    min_channel_balance: HoprBalance,
    top_up_amount: HoprBalance,
) -> MultiStrategyConfig {
    MultiStrategyConfig {
        execution_interval: std::time::Duration::from_secs(15),
        strategies: vec![
            EdgeStrategyKind::AutoFunding(AutoFundingStrategyConfig {
                min_stake_threshold: min_channel_balance,
                funding_amount: top_up_amount,
            }),
            EdgeStrategyKind::ClosureFinalizer(ClosureFinalizerStrategyConfig {
                max_closure_overdue: std::time::Duration::from_secs(300),
            }),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wxhopr(amount: u128) -> HoprBalance {
        // HoprBalance = Balance<WxHOPR>; construct from a known string representation.
        amount.to_string().parse().unwrap_or_default()
    }

    #[test]
    fn default_cfg_has_two_strategies() {
        let cfg = default_edge_client_telemetry_reactor_cfg(wxhopr(1), wxhopr(10));
        assert_eq!(cfg.strategies.len(), 2);
    }

    #[test]
    fn default_cfg_execution_interval_is_15s() {
        let cfg = default_edge_client_telemetry_reactor_cfg(wxhopr(1), wxhopr(10));
        assert_eq!(cfg.execution_interval, std::time::Duration::from_secs(15));
    }

    #[test]
    fn default_cfg_first_strategy_is_auto_funding() {
        let cfg = default_edge_client_telemetry_reactor_cfg(wxhopr(1), wxhopr(10));
        assert!(
            matches!(cfg.strategies[0], EdgeStrategyKind::AutoFunding(_)),
            "expected first strategy to be AutoFunding"
        );
    }

    #[test]
    fn default_cfg_second_strategy_is_closure_finalizer() {
        let cfg = default_edge_client_telemetry_reactor_cfg(wxhopr(1), wxhopr(10));
        assert!(
            matches!(cfg.strategies[1], EdgeStrategyKind::ClosureFinalizer(_)),
            "expected second strategy to be ClosureFinalizer"
        );
    }

    #[test]
    fn closure_finalizer_overdue_is_300s() {
        let cfg = default_edge_client_telemetry_reactor_cfg(wxhopr(1), wxhopr(10));
        if let EdgeStrategyKind::ClosureFinalizer(c) = &cfg.strategies[1] {
            assert_eq!(c.max_closure_overdue, std::time::Duration::from_secs(300));
        } else {
            panic!("expected ClosureFinalizer");
        }
    }
}
