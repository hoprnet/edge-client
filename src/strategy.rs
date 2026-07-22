use std::collections::HashSet;

use hopr_lib::api::types::primitive::prelude::{
    Address, HoprBalance, UnitaryFloatOps as _, XDaiBalance,
};
pub use hopr_strategy::channel_lifecycle::{
    ChannelLifecycleConfig, EligibilityConfig, FundingConfig, PopulationConfig, SelectorProfile,
};

#[cfg(feature = "runtime-tokio")]
use hopr_lib::api::{chain::ChainValues as _, node::HasChainApi as _};

/// Subset of strategies relevant to an edge node.
pub enum EdgeStrategyKind {
    ChannelLifecycle(ChannelLifecycleConfig),
}

/// Strategy configuration for an edge node reactor.
pub struct MultiStrategyConfig {
    /// Ordered list of strategies to run concurrently.
    pub strategies: Vec<EdgeStrategyKind>,
}

/// Top-level incentive parameters for the channel lifecycle strategy reactor.
///
/// Covers channel funding sizing, population topology, and optional address targeting.
/// Set `channel_allowlist` when using explicit-path routing to restrict channel opening
/// to the required relayer addresses; leave it `None` for quality-score-based peer selection.
#[derive(Debug, Clone, smart_default::SmartDefault)]
pub struct IncentiveConfiguration {
    /// Number of forwarded packets the initial channel stake is sized to cover.
    ///
    /// Sets `initial_balance = max(N × ticket_price, ticket_price / win_prob)`:
    /// the first term is the expected channel drain; the second is a minimum floor
    /// ensuring at least one ticket face value is available even at low message counts.
    ///
    /// The channel strategy tops up when balance falls below 25% of `initial_balance`,
    /// so the channel can forward more than this many packets over its lifetime.
    /// Default: 1,000,000.
    #[default = 1_000_000]
    pub desired_message_count: u64,

    /// Minimum number of open outgoing channels to maintain. Default: 5.
    #[default = 5]
    pub min_open_channels: usize,

    /// Target number of open outgoing channels to open towards. Default: 8.
    #[default = 8]
    pub target_open_channels: usize,

    /// When `Some`, channels are opened exclusively to these addresses; all other peers
    /// are skipped regardless of quality score. Use this for explicit-path routing to
    /// ensure channels exist to the required relayers. Default: `None` (open to any peer).
    #[default(None)]
    pub channel_allowlist: Option<HashSet<Address>>,
}

impl IncentiveConfiguration {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.target_open_channels >= self.min_open_channels,
            "target_open_channels ({}) must be >= min_open_channels ({})",
            self.target_open_channels,
            self.min_open_channels
        );
        Ok(())
    }
}

/// Compute [`FundingConfig`] from chain-derived values and a desired message budget.
///
/// **Semantics:** `ticket_price` is the *expected* per-hop value (= `face_value × win_prob`),
/// not the face value. Per-hop expected channel drain is therefore `ticket_price`,
/// regardless of `win_prob`. The ticket face value (amount locked per ticket)
/// = `ticket_price / win_prob`.
///
/// Sizes `initial_balance` as the larger of:
/// - expected drain over `sizing.desired_message_count` packets: `N × ticket_price`
/// - one ticket's face value (the channel cannot mint a single ticket otherwise):
///   `ticket_price / win_prob`
///
/// Derived fields keep the strategy's semantic invariants
/// (`lower < initial`, `topup + lower ≈ initial`):
/// - `topup_balance`            = 75% of `initial_balance`
/// - `lower_balance_threshold`  = 25% of `initial_balance`
/// - `min_safe_balance_required` = `initial_balance`
pub fn compute_funding_config(
    ticket_price: HoprBalance,
    win_prob: f64,
    sizing: &IncentiveConfiguration,
) -> anyhow::Result<FundingConfig> {
    anyhow::ensure!(
        win_prob.is_finite() && win_prob > 0.0 && win_prob <= 1.0,
        "win_prob must be in (0, 1]; got {win_prob}"
    );
    // face_value = price / win_prob: channel needs ≥1 face_value to issue any ticket.
    let face_value = ticket_price.div_f64(win_prob)?;
    // expected_drain = N × ticket_price (NOT × win_prob — ticket_price is already
    // the expected per-hop value: each ticket has face = ticket_price/win_prob
    // and pays out with probability win_prob, so expected per-ticket drain = ticket_price).
    let expected_drain = ticket_price * sizing.desired_message_count;
    let initial = expected_drain.max(face_value);
    anyhow::ensure!(
        initial > HoprBalance::new_base(0),
        "computed initial_balance is zero; ticket_price is zero"
    );
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

/// Minimum recommended wxHOPR and xDAI balance to open the target number of channels.
#[derive(Clone, Copy, Debug)]
pub struct BalanceRecommendation {
    /// Minimum wxHOPR needed to stake the missing channels.
    pub wxhopr: HoprBalance,
    /// Minimum xDAI for gas (fixed at [`hopr_lib::SUGGESTED_NATIVE_BALANCE`]).
    pub xdai: XDaiBalance,
}

/// Data-throughput capacity for a stake of wxHOPR at the current ticket price.
///
/// Key for the map returned by
/// [`crate::client::Edgli::describe_current_capacity_allocations`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CapacityAllocator {
    /// An open outgoing payment channel to the given peer.
    Peer(Address),
    /// The unallocated wxHOPR balance held in the user's Safe contract.
    Safe,
}

/// Data-throughput capacity for a wxHOPR stake at the current ticket price.
#[derive(Clone, Copy, Debug)]
pub struct Capacity {
    /// wxHOPR stake — locked balance for channels, unallocated balance for the Safe.
    pub stake: HoprBalance,
    /// Expected long-run session-frame count: `floor(stake / ticket_price)`.
    ///
    /// `ticket_price` is the *expected* per-hop drain (= `face_value × win_prob`),
    /// so this figure is win_prob-independent — it reflects average lifetime throughput.
    pub expected_messages: u64,
    /// Worst-case floor: `floor(stake / face_value)` = `floor(stake × win_prob / ticket_price)`.
    ///
    /// Reflects the maximum number of tickets the channel can hold simultaneously
    /// before balance drops below one face value and ticket minting halts.
    /// Lower win_prob → smaller face value → fewer guaranteed concurrent tickets.
    pub min_guaranteed_messages: u64,
    /// Raw byte capacity: `expected_messages × SESSION_MTU`.
    pub byte_capacity: u64,
}

/// Compute the recommended wxHOPR and xDAI balances for `missing_channels` new channels.
pub(crate) fn compute_balance_recommendation(
    ticket_price: HoprBalance,
    win_prob: f64,
    cfg: &IncentiveConfiguration,
    missing_channels: usize,
) -> anyhow::Result<BalanceRecommendation> {
    if missing_channels == 0 {
        return Ok(BalanceRecommendation {
            wxhopr: HoprBalance::zero(),
            xdai: *hopr_lib::SUGGESTED_NATIVE_BALANCE,
        });
    }
    let stake_per_channel = compute_funding_config(ticket_price, win_prob, cfg)?.initial_balance;
    let wxhopr = stake_per_channel * (missing_channels as u64);
    Ok(BalanceRecommendation {
        wxhopr,
        xdai: *hopr_lib::SUGGESTED_NATIVE_BALANCE,
    })
}

/// Compute the data-throughput capacity for a given wxHOPR stake at the current ticket price.
///
/// `win_prob` must be in `(0, 1]`; the same validation guard as [`compute_funding_config`] applies.
pub(crate) fn compute_capacity(
    stake: HoprBalance,
    ticket_price: HoprBalance,
    win_prob: f64,
) -> anyhow::Result<Capacity> {
    anyhow::ensure!(
        win_prob.is_finite() && win_prob > 0.0 && win_prob <= 1.0,
        "win_prob must be in (0, 1]; got {win_prob}"
    );
    let expected_messages = if ticket_price == HoprBalance::zero() {
        0u64
    } else {
        // Clamp to u64::MAX before converting so astronomically large quotients
        // saturate rather than truncate via low_u64().
        let quotient = stake.amount() / ticket_price.amount();
        quotient.min(u64::MAX.into()).low_u64()
    };
    // face_value = ticket_price / win_prob — minimum balance locked per outstanding ticket.
    let face_value = ticket_price.div_f64(win_prob)?;
    let min_guaranteed_messages = if face_value == HoprBalance::zero() {
        0u64
    } else {
        let quotient = stake.amount() / face_value.amount();
        quotient.min(u64::MAX.into()).low_u64()
    };
    Ok(Capacity {
        stake,
        expected_messages,
        min_guaranteed_messages,
        byte_capacity: expected_messages.saturating_mul(hopr_lib::SESSION_MTU as u64),
    })
}

/// Returns the minimum recommended wxHOPR and xDAI for this node to open
/// `cfg.target_open_channels` channels from scratch.
///
/// Queries ticket pricing from the safeless chain interactor so this can be
/// called before the full node is started (e.g. during onboarding).
#[cfg(feature = "blokli")]
pub async fn minimum_balance_recommendation(
    incentive_ops: &dyn crate::blokli::IncentiveOperations,
    cfg: &IncentiveConfiguration,
) -> anyhow::Result<BalanceRecommendation> {
    let stats = incentive_ops.ticket_stats().await?;
    let win_prob = stats.winning_probability.as_f64();
    compute_balance_recommendation(stats.ticket_price, win_prob, cfg, cfg.target_open_channels)
}

/// Returns the default [`MultiStrategyConfig`] for an edge client reactor.
///
/// Fetches the minimum ticket price and winning probability from the chain and
/// sizes the [`ChannelLifecycleStrategy`] funding to cover
/// `sizing.desired_message_count` messages per channel.
/// See [`compute_funding_config`] for the sizing formula.
#[cfg(feature = "runtime-tokio")]
pub async fn default_strategy_cfg(
    node: &crate::client::Edgli,
    sizing: &IncentiveConfiguration,
) -> anyhow::Result<MultiStrategyConfig> {
    sizing.validate()?;
    let chain = node.chain_api();
    let ticket_price = chain.minimum_ticket_price().await?;
    let win_prob = chain.minimum_incoming_ticket_win_prob().await?.as_f64();
    let cfg = ChannelLifecycleConfig {
        funding: compute_funding_config(ticket_price, win_prob, sizing)?,
        population: PopulationConfig {
            min_open_channels: sizing.min_open_channels,
            target_open_channels: sizing.target_open_channels,
            ..Default::default()
        },
        eligibility: EligibilityConfig {
            allowlist: sizing.channel_allowlist.clone(),
            ..Default::default()
        },
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
            &IncentiveConfiguration {
                desired_message_count: 1,
                ..Default::default()
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
            &IncentiveConfiguration {
                desired_message_count: 100,
                ..Default::default()
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
            compute_funding_config(
                HoprBalance::new_base(1),
                0.0,
                &IncentiveConfiguration::default()
            )
            .is_err()
        );
    }

    #[test]
    fn compute_funding_config_rejects_win_prob_above_one() {
        assert!(
            compute_funding_config(
                HoprBalance::new_base(1),
                1.1,
                &IncentiveConfiguration::default()
            )
            .is_err()
        );
        assert!(
            compute_funding_config(
                HoprBalance::new_base(1),
                f64::INFINITY,
                &IncentiveConfiguration::default()
            )
            .is_err()
        );
        assert!(
            compute_funding_config(
                HoprBalance::new_base(1),
                f64::NAN,
                &IncentiveConfiguration::default()
            )
            .is_err()
        );
    }

    #[test]
    fn compute_funding_config_rejects_zero_initial_balance() {
        assert!(
            compute_funding_config(
                HoprBalance::new_base(0),
                1.0,
                &IncentiveConfiguration {
                    desired_message_count: 0,
                    ..Default::default()
                }
            )
            .is_err()
        );
    }

    #[test]
    fn compute_funding_config_default_sizing_has_one_strategy_shape() {
        // Smoke-test: can build a MultiStrategyConfig from compute_funding_config output
        let funding = compute_funding_config(
            HoprBalance::new_base(1),
            1.0,
            &IncentiveConfiguration {
                desired_message_count: 1,
                ..Default::default()
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

    #[test]
    fn channel_sizing_defaults_match_population_config_defaults() {
        let sizing = IncentiveConfiguration::default();
        let population = PopulationConfig::default();
        assert_eq!(sizing.min_open_channels, population.min_open_channels);
        assert_eq!(sizing.target_open_channels, population.target_open_channels);
    }

    #[test]
    fn compute_balance_recommendation_zero_missing_returns_zero_wxhopr() {
        let rec = compute_balance_recommendation(
            HoprBalance::new_base(10),
            1.0,
            &IncentiveConfiguration::default(),
            0,
        )
        .unwrap();
        assert_eq!(rec.wxhopr, HoprBalance::zero());
        assert_eq!(rec.xdai, *hopr_lib::SUGGESTED_NATIVE_BALANCE);
    }

    #[test]
    fn compute_balance_recommendation_scales_by_missing_channels() {
        // win_prob=1.0, ticket_price=10, msg=1: stake = max(10, 10) = 10; 8 channels = 80
        let rec = compute_balance_recommendation(
            HoprBalance::new_base(10),
            1.0,
            &IncentiveConfiguration {
                desired_message_count: 1,
                ..Default::default()
            },
            8,
        )
        .unwrap();
        assert_eq!(rec.wxhopr, HoprBalance::new_base(10) * 8u64);
        assert_eq!(rec.xdai, *hopr_lib::SUGGESTED_NATIVE_BALANCE);
    }

    #[test]
    fn compute_balance_recommendation_zero_target_yields_zero() {
        let cfg = IncentiveConfiguration {
            target_open_channels: 0,
            min_open_channels: 0,
            ..Default::default()
        };
        let rec = compute_balance_recommendation(HoprBalance::new_base(10), 1.0, &cfg, 0).unwrap();
        assert_eq!(rec.wxhopr, HoprBalance::zero());
    }

    #[test]
    fn compute_capacity_basic_arithmetic() {
        let cap =
            compute_capacity(HoprBalance::new_base(1000), HoprBalance::new_base(10), 1.0).unwrap();
        assert_eq!(cap.expected_messages, 100);
        assert_eq!(cap.min_guaranteed_messages, 100);
        assert_eq!(cap.byte_capacity, 100 * hopr_lib::SESSION_MTU as u64);
        assert_eq!(cap.stake, HoprBalance::new_base(1000));
    }

    #[test]
    fn compute_capacity_balance_below_ticket_price() {
        let cap =
            compute_capacity(HoprBalance::new_base(5), HoprBalance::new_base(10), 1.0).unwrap();
        assert_eq!(cap.expected_messages, 0);
        assert_eq!(cap.min_guaranteed_messages, 0);
        assert_eq!(cap.byte_capacity, 0);
    }

    #[test]
    fn compute_capacity_zero_ticket_price() {
        let cap = compute_capacity(HoprBalance::new_base(100), HoprBalance::zero(), 1.0).unwrap();
        assert_eq!(cap.expected_messages, 0);
        assert_eq!(cap.min_guaranteed_messages, 0);
        assert_eq!(cap.byte_capacity, 0);
    }

    #[test]
    fn compute_capacity_win_prob_one_matches_expected() {
        // win_prob=1.0 → face_value = ticket_price → min_guaranteed == expected
        let cap =
            compute_capacity(HoprBalance::new_base(300), HoprBalance::new_base(10), 1.0).unwrap();
        assert_eq!(cap.expected_messages, 30);
        assert_eq!(cap.min_guaranteed_messages, 30);
    }

    #[test]
    fn compute_capacity_win_prob_half_halves_guaranteed() {
        // stake=100, ticket_price=10, win_prob=0.5
        // expected  = 100/10 = 10
        // face_value = 10/0.5 = 20 → min_guaranteed = 100/20 = 5
        let cap =
            compute_capacity(HoprBalance::new_base(100), HoprBalance::new_base(10), 0.5).unwrap();
        assert_eq!(cap.expected_messages, 10);
        assert_eq!(cap.min_guaranteed_messages, 5);
    }

    #[test]
    fn compute_capacity_win_prob_small_floors_guaranteed_to_zero() {
        // stake=10, ticket_price=10, win_prob=0.5
        // face_value = 10/0.5 = 20 > stake → min_guaranteed = 0; expected = 1
        let cap =
            compute_capacity(HoprBalance::new_base(10), HoprBalance::new_base(10), 0.5).unwrap();
        assert_eq!(cap.expected_messages, 1);
        assert_eq!(cap.min_guaranteed_messages, 0);
    }

    #[test]
    fn compute_capacity_rejects_invalid_win_prob() {
        let stake = HoprBalance::new_base(100);
        let price = HoprBalance::new_base(10);
        assert!(compute_capacity(stake, price, 0.0).is_err());
        assert!(compute_capacity(stake, price, 1.1).is_err());
        assert!(compute_capacity(stake, price, f64::NAN).is_err());
        assert!(compute_capacity(stake, price, f64::INFINITY).is_err());
    }

    #[test]
    fn incentive_configuration_default_allowlist_is_none() {
        assert!(
            IncentiveConfiguration::default()
                .channel_allowlist
                .is_none()
        );
    }

    #[test]
    fn eligibility_config_uses_channel_allowlist() {
        use std::collections::HashSet;
        let addr = Address::default();
        let allowlist = HashSet::from([addr]);
        let sizing = IncentiveConfiguration {
            channel_allowlist: Some(allowlist.clone()),
            ..Default::default()
        };
        let eligibility = EligibilityConfig {
            allowlist: sizing.channel_allowlist.clone(),
            ..Default::default()
        };
        assert_eq!(eligibility.allowlist, Some(allowlist));
    }
}
