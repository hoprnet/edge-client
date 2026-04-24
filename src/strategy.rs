pub use hopr_lib::HoprBalance;
pub use hopr_strategy::{
    Strategy, auto_funding::AutoFundingStrategyConfig,
    channel_finalizer::ClosureFinalizerStrategyConfig, strategy::MultiStrategyConfig,
};

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
        on_fail_continue: true,
        allow_recursive: false,
        execution_interval: std::time::Duration::from_secs(15),
        strategies: vec![
            Strategy::AutoFunding(AutoFundingStrategyConfig {
                min_stake_threshold: min_channel_balance,
                funding_amount: top_up_amount,
            }),
            Strategy::ClosureFinalizer(ClosureFinalizerStrategyConfig {
                max_closure_overdue: std::time::Duration::from_secs(300),
            }),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wxhopr(amount: u128) -> HoprBalance {
        use hopr_lib::UnitaryFloatOps;
        // Use Balance::from_str or the available constructor.
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
    fn default_cfg_on_fail_continue_is_true() {
        let cfg = default_edge_client_telemetry_reactor_cfg(wxhopr(1), wxhopr(10));
        assert!(cfg.on_fail_continue);
    }

    #[test]
    fn default_cfg_not_recursive() {
        let cfg = default_edge_client_telemetry_reactor_cfg(wxhopr(1), wxhopr(10));
        assert!(!cfg.allow_recursive);
    }

    #[test]
    fn default_cfg_first_strategy_is_auto_funding() {
        let cfg = default_edge_client_telemetry_reactor_cfg(wxhopr(1), wxhopr(10));
        assert!(
            matches!(cfg.strategies[0], Strategy::AutoFunding(_)),
            "expected first strategy to be AutoFunding"
        );
    }

    #[test]
    fn default_cfg_second_strategy_is_closure_finalizer() {
        let cfg = default_edge_client_telemetry_reactor_cfg(wxhopr(1), wxhopr(10));
        assert!(
            matches!(cfg.strategies[1], Strategy::ClosureFinalizer(_)),
            "expected second strategy to be ClosureFinalizer"
        );
    }

    #[test]
    fn closure_finalizer_overdue_is_300s() {
        let cfg = default_edge_client_telemetry_reactor_cfg(wxhopr(1), wxhopr(10));
        if let Strategy::ClosureFinalizer(c) = &cfg.strategies[1] {
            assert_eq!(c.max_closure_overdue, std::time::Duration::from_secs(300));
        } else {
            panic!("expected ClosureFinalizer");
        }
    }
}
