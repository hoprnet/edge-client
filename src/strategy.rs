use hopr_lib::api::types::primitive::prelude::{HoprBalance, UnitaryFloatOps as _};
pub use hopr_strategy::channel_lifecycle::{ChannelLifecycleConfig, FundingConfig};

/// Subset of strategies relevant to an edge node.
pub enum EdgeStrategyKind {
    ChannelLifecycle(ChannelLifecycleConfig),
}

/// Strategy configuration for an edge node reactor.
pub struct MultiStrategyConfig {
    /// Ordered list of strategies to run concurrently.
    pub strategies: Vec<EdgeStrategyKind>,
}

/// Desired channel capacity for computing the initial stake.
#[derive(Debug, Clone, Copy)]
pub struct ChannelSizing {
    /// Expected number of mixnet messages this channel should forward before exhaustion.
    /// The stake is sized as the expected drain: `desired_message_count × win_prob × ticket_price`.
    /// Default: 1,000,000.
    pub desired_message_count: u64,
}

impl Default for ChannelSizing {
    fn default() -> Self {
        Self {
            desired_message_count: 1_000_000,
        }
    }
}

/// Compute [`FundingConfig`] from chain-derived values and a desired message budget.
///
/// Sizes the initial channel stake as the expected drain over
/// `sizing.desired_message_count` messages:
/// `desired_message_count × win_prob × ticket_price`.
///
/// Derived fields keep the strategy's semantic invariants
/// (`lower < initial`, `topup + lower ≈ initial`):
/// - `topup_balance`            = 75% of `initial_balance`
/// - `lower_balance_threshold`  = 25% of `initial_balance`
/// - `min_safe_balance_required` = `initial_balance`
pub fn compute_funding_config(
    ticket_price: HoprBalance,
    win_prob: f64,
    sizing: &ChannelSizing,
) -> anyhow::Result<FundingConfig> {
    anyhow::ensure!(win_prob > 0.0, "win_prob must be positive; got {win_prob}");
    let initial = (ticket_price * sizing.desired_message_count).mul_f64(win_prob)?;
    let topup = initial.mul_f64(0.75)?;
    let lower = initial.mul_f64(0.25)?;
    Ok(FundingConfig {
        initial_balance: initial,
        topup_balance: topup,
        lower_balance_threshold: lower,
        min_safe_balance_required: initial,
        stop_when_unfunded: true,
    })
}

/// Returns the default [`MultiStrategyConfig`] for an edge client telemetry reactor.
///
/// Fetches the minimum ticket price and winning probability from the chain and
/// sizes the [`ChannelLifecycleStrategy`] funding to cover
/// `sizing.desired_message_count` messages per channel.
/// See [`compute_funding_config`] for the sizing formula.
#[cfg(feature = "runtime-tokio")]
pub async fn default_edge_client_telemetry_reactor_cfg(
    node: &crate::client::Edgli,
    sizing: ChannelSizing,
) -> anyhow::Result<MultiStrategyConfig> {
    use hopr_lib::api::{chain::ChainValues as _, node::HasChainApi as _};
    let chain = node.chain_api();
    let ticket_price = chain.minimum_ticket_price().await?;
    let win_prob = chain.minimum_incoming_ticket_win_prob().await?.as_f64();
    let cfg = ChannelLifecycleConfig {
        funding: compute_funding_config(ticket_price, win_prob, &sizing)?,
        ..Default::default()
    };
    Ok(MultiStrategyConfig {
        strategies: vec![EdgeStrategyKind::ChannelLifecycle(cfg)],
    })
}

#[cfg(test)]
mod tests {
    use hopr_lib::api::types::primitive::prelude::HoprBalance;

    use super::*;

    #[test]
    fn compute_funding_config_win_prob_one() {
        // With win_prob=1.0, every ticket pays out → initial = ticket_price × msg_count
        let cfg = compute_funding_config(
            HoprBalance::new_base(1),
            1.0,
            &ChannelSizing {
                desired_message_count: 1,
            },
        )
        .unwrap();
        assert_eq!(cfg.initial_balance, HoprBalance::new_base(1));
        assert_eq!(cfg.min_safe_balance_required, HoprBalance::new_base(1));
        assert!(cfg.lower_balance_threshold < cfg.initial_balance);
        assert!(cfg.topup_balance > cfg.lower_balance_threshold);
        assert!(cfg.topup_balance < cfg.initial_balance);
        assert!(cfg.stop_when_unfunded);
    }

    #[test]
    fn compute_funding_config_proportional_fields() {
        // Verify lower < initial, min_safe == initial, topup ∈ (lower, initial)
        let cfg = compute_funding_config(
            HoprBalance::new_base(10),
            1.0,
            &ChannelSizing {
                desired_message_count: 100,
            },
        )
        .unwrap();
        assert_eq!(cfg.min_safe_balance_required, cfg.initial_balance);
        assert!(cfg.lower_balance_threshold < cfg.initial_balance);
        assert!(cfg.topup_balance < cfg.initial_balance);
        assert!(cfg.topup_balance > cfg.lower_balance_threshold);
    }

    #[test]
    fn compute_funding_config_rejects_zero_win_prob() {
        assert!(
            compute_funding_config(HoprBalance::new_base(1), 0.0, &ChannelSizing::default())
                .is_err()
        );
    }

    #[test]
    fn compute_funding_config_default_sizing_has_one_strategy_shape() {
        // Smoke-test: can build a MultiStrategyConfig from compute_funding_config output
        let funding = compute_funding_config(
            HoprBalance::new_base(1),
            1.0,
            &ChannelSizing {
                desired_message_count: 1,
            },
        )
        .unwrap();
        let lifecycle_cfg = ChannelLifecycleConfig {
            funding,
            ..Default::default()
        };
        let cfg = MultiStrategyConfig {
            strategies: vec![EdgeStrategyKind::ChannelLifecycle(lifecycle_cfg)],
        };
        assert_eq!(cfg.strategies.len(), 1);
        assert!(matches!(
            cfg.strategies[0],
            EdgeStrategyKind::ChannelLifecycle(_)
        ));
    }
}
