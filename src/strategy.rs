pub use hopr_lib::HoprBalance;
pub use hopr_strategy::{
    Strategy, auto_funding::AutoFundingStrategyConfig,
    channel_finalizer::ClosureFinalizerStrategyConfig, strategy::MultiStrategyConfig,
};

/// Returns the configuration of a default edge-client relevant [`Strategy`] configuration
/// that can be used to initialize the telemetry reactor.
pub fn default_edge_client_telemetry_reactor_cfg(
    min_channel_balance: HoprBalance,
    top_up_amount: HoprBalance,
) -> MultiStrategyConfig {
    MultiStrategyConfig {
        on_fail_continue: true,
        allow_recursive: false,
        execution_interval: std::time::Duration::from_secs(60),
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
