use std::collections::HashSet;

use bytesize::ByteSize;
use hopr_lib::api::types::primitive::prelude::{
    Address, HoprBalance, UnitaryFloatOps as _, XDaiBalance,
};
pub use hopr_strategy::channel_lifecycle::{
    ChannelLifecycleConfig, EligibilityConfig, FundingConfig, PopulationConfig, SelectorProfile,
};

/// Paid downstream relay hops assumed when sizing a channel's stake. Edge nodes
/// route over a single hop by default, so a channel's tickets pay one relay.
const ASSUMED_HOPS: u32 = 1;

#[cfg(any(feature = "blokli", feature = "runtime-tokio"))]
use hopr_lib::api::chain::{AccountSelector, ChainReadAccountOperations, ChainValues};

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
    /// Sets `initial_capacity = N × SESSION_MTU` bytes of channel data capacity;
    /// the strategy resolves this to a wxHOPR stake at the live ticket price and
    /// winning probability when it opens the channel.
    ///
    /// The channel strategy tops up when capacity falls below 25% of
    /// `initial_capacity`, so the channel can forward more than this many packets
    /// over its lifetime. Default: 1,000,000.
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

/// Compute the capacity-based [`FundingConfig`] for a desired message budget.
///
/// The strategy now sizes channels by *data capacity*: `initial_capacity` bytes
/// are resolved to a wxHOPR stake at the live ticket economics when a channel is
/// opened. We map `desired_message_count` packets to `N × SESSION_MTU` bytes,
/// matching [`Capacity::byte_capacity`].
///
/// Derived fields keep the strategy's invariants (`lower < initial`):
/// - `topup_capacity`            = 75% of `initial_capacity`
/// - `lower_capacity_threshold`  = 25% of `initial_capacity`
/// - `min_safe_capacity_required` = `initial_capacity`
pub fn compute_funding_config(sizing: &IncentiveConfiguration) -> anyhow::Result<FundingConfig> {
    let initial_bytes = sizing
        .desired_message_count
        .saturating_mul(hopr_lib::SESSION_MTU as u64);
    anyhow::ensure!(
        initial_bytes > 0,
        "computed initial_capacity is zero; desired_message_count is zero"
    );
    Ok(FundingConfig {
        initial_capacity: ByteSize::b(initial_bytes),
        topup_capacity: ByteSize::b(initial_bytes / 4 * 3),
        lower_capacity_threshold: ByteSize::b(initial_bytes / 4),
        min_safe_capacity_required: ByteSize::b(initial_bytes),
        assumed_hops: ASSUMED_HOPS,
        stop_when_unfunded: true,
    })
}

/// wxHOPR a single new channel's initial stake locks, mirroring the strategy's
/// capacity→balance conversion of `initial_capacity`
/// (`ticket_price × packets × ASSUMED_HOPS / win_prob`).
///
/// **Semantics:** `ticket_price` is the *expected* per-hop value
/// (= `face_value × win_prob`), not the face value. The ticket face value
/// (amount locked per ticket) = `ticket_price / win_prob`, which is why the
/// stake divides by `win_prob`: the channel must hold at least one face value
/// to mint a ticket at all.
///
/// Uses `desired_message_count` directly as the packet count (capacity is
/// `N × SESSION_MTU`, and `SESSION_MTU < HoprPacket::PAYLOAD_SIZE`, so the
/// strategy's own `ceil(bytes / PAYLOAD_SIZE)` resolves to ≤ N packets). This
/// keeps the recommendation on the never-underfund side.
fn initial_channel_stake(
    desired_message_count: u64,
    ticket_price: HoprBalance,
    win_prob: f64,
) -> anyhow::Result<HoprBalance> {
    anyhow::ensure!(
        win_prob.is_finite() && win_prob > 0.0 && win_prob <= 1.0,
        "win_prob must be in (0, 1]; got {win_prob}"
    );
    let base = ticket_price * desired_message_count * ASSUMED_HOPS as u64;
    Ok(base.div_f64(win_prob)?)
}

/// One-time costs still owed before this node can be fully up and running,
/// verified against on-chain state.
#[derive(Clone, Copy, Debug)]
pub struct StartupCosts {
    /// One-time fee still owed before the node can start (today the
    /// key-binding fee); zero once the key is bound on-chain.
    pub fee_to_start: HoprBalance,
    /// Number of on-chain transactions still needed before channel-funding
    /// transactions can begin: Safe + module deployment (when no Safe exists
    /// yet), then Safe registration and the key-binding announcement (when the
    /// key is not bound yet). Zero for a fully set-up node.
    pub txs_to_start: u64,
}

/// Everything still needed for this node to be fully up and running.
#[derive(Clone, Copy, Debug)]
pub struct BalanceRecommendation {
    /// wxHOPR needed to stake the missing channels.
    pub channel_stakes: HoprBalance,
    /// One-time fee still owed before the node can start (today the
    /// key-binding fee); zero once the key is bound on-chain.
    pub fee_to_start: HoprBalance,
    /// Number of on-chain transactions still needed before channel-funding
    /// transactions can begin; see [`StartupCosts::txs_to_start`].
    pub txs_to_start: u64,
    /// Maximum xDAI fee per transaction (gas)
    /// (fixed at [`hopr_lib::SUGGESTED_NATIVE_BALANCE`]).
    pub xdai_fee_per_tx: XDaiBalance,
}

impl BalanceRecommendation {
    /// Total wxHOPR to fund: channel stakes plus the fee to start.
    pub fn total_wxhopr(&self) -> HoprBalance {
        self.channel_stakes + self.fee_to_start
    }
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
///
/// `costs` are the one-time startup costs (fee and remaining transactions)
/// reported as their own fields next to the channel stakes; callers obtain
/// them from [`compute_costs_to_start`], which returns zeros for a fully
/// set-up node. On networks with a dust ticket price (e.g. rotsee: 100 wei
/// tickets, 0.01 wxHOPR fee) the fee dominates the recommendation, so omitting
/// it underfunds the node.
pub(crate) fn compute_balance_recommendation(
    ticket_price: HoprBalance,
    win_prob: f64,
    cfg: &IncentiveConfiguration,
    missing_channels: usize,
    costs: StartupCosts,
) -> anyhow::Result<BalanceRecommendation> {
    let stake = if missing_channels == 0 {
        HoprBalance::zero()
    } else {
        anyhow::ensure!(
            cfg.desired_message_count > 0,
            "desired_message_count is zero; cannot size a channel stake"
        );
        initial_channel_stake(cfg.desired_message_count, ticket_price, win_prob)?
            * (missing_channels as u64)
    };
    Ok(BalanceRecommendation {
        channel_stakes: stake,
        fee_to_start: costs.fee_to_start,
        txs_to_start: costs.txs_to_start,
        xdai_fee_per_tx: *hopr_lib::SUGGESTED_NATIVE_BALANCE,
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

/// Returns what this user still owes before being fully up and running,
/// verified against on-chain state: the one-time key-binding (announcement)
/// fee — the full fee when no account exists for `node_address` yet, zero once
/// the key is bound (the fee is never charged twice) — and the number of
/// on-chain transactions left before channel funding can begin (Safe + module
/// deployment, Safe registration, key-binding announcement).
///
/// `node_address` is `None` when the user has no address yet — nothing can be
/// bound, so everything is still owed. `safe_deployed` tells whether a Safe
/// already exists for this user; a running node always has one.
#[cfg(any(feature = "blokli", feature = "runtime-tokio"))]
pub(crate) async fn compute_costs_to_start<T>(
    chain: &T,
    node_address: Option<Address>,
    safe_deployed: bool,
) -> anyhow::Result<StartupCosts>
where
    T: ChainReadAccountOperations + ChainValues + Sync,
{
    let bound = match node_address {
        Some(addr) => {
            chain
                .count_accounts(AccountSelector::default().with_chain_key(addr))
                .await?
                > 0
        }
        None => false,
    };
    let fee_to_start = if bound {
        HoprBalance::zero()
    } else {
        chain.key_binding_fee().await?
    };
    // Startup submits: Safe + module deployment (unless one exists), then Safe
    // registration and the key-binding announcement (skipped once the key is
    // bound — a bound key implies a completed registration).
    let txs_to_start = u64::from(!safe_deployed) + if bound { 0 } else { 2 };
    Ok(StartupCosts {
        fee_to_start,
        txs_to_start,
    })
}

/// Returns the minimum recommended wxHOPR and xDAI for this node to open
/// `cfg.target_open_channels` channels from scratch.
///
/// Queries ticket pricing from the safeless chain interactor so this can be
/// called before the full node is started (e.g. during onboarding). The
/// one-time key-binding (announcement) fee is included on top of the channel
/// stakes only when the node's key is not yet bound on-chain.
#[cfg(feature = "blokli")]
pub async fn minimum_balance_recommendation(
    incentive_ops: &dyn crate::blokli::IncentiveOperations,
    cfg: &IncentiveConfiguration,
) -> anyhow::Result<BalanceRecommendation> {
    let stats = incentive_ops.ticket_stats().await?;
    let win_prob = stats.winning_probability.as_f64();
    let costs = incentive_ops.compute_costs_to_start().await?;
    compute_balance_recommendation(
        stats.ticket_price,
        win_prob,
        cfg,
        cfg.target_open_channels,
        costs,
    )
}

/// Returns the default [`MultiStrategyConfig`] for an edge client reactor.
///
/// Sizes the channel-lifecycle funding to cover `sizing.desired_message_count`
/// messages of data capacity per channel; the strategy resolves that capacity
/// to a wxHOPR stake at the live ticket economics. See [`compute_funding_config`].
#[cfg(feature = "runtime-tokio")]
pub async fn default_strategy_cfg(
    _node: &crate::client::Edgli,
    sizing: &IncentiveConfiguration,
) -> anyhow::Result<MultiStrategyConfig> {
    sizing.validate()?;
    let cfg = ChannelLifecycleConfig {
        funding: compute_funding_config(sizing)?,
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

    /// Fully set-up node: no fee owed, no startup transactions left.
    fn no_startup_costs() -> StartupCosts {
        StartupCosts {
            fee_to_start: HoprBalance::zero(),
            txs_to_start: 0,
        }
    }

    #[test]
    fn compute_funding_config_capacity_from_message_count() {
        // initial_capacity = desired_message_count × SESSION_MTU bytes
        let cfg = compute_funding_config(&IncentiveConfiguration {
            desired_message_count: 1,
            ..Default::default()
        })
        .unwrap();
        let mtu = hopr_lib::SESSION_MTU as u64;
        assert_eq!(cfg.initial_capacity.as_u64(), mtu);
        assert_eq!(cfg.min_safe_capacity_required.as_u64(), mtu);
        assert!(cfg.lower_capacity_threshold < cfg.initial_capacity);
        assert!(cfg.topup_capacity > cfg.lower_capacity_threshold);
        assert!(cfg.topup_capacity < cfg.initial_capacity);
        assert_eq!(cfg.assumed_hops, ASSUMED_HOPS);
        assert!(cfg.stop_when_unfunded);
    }

    #[test]
    fn compute_funding_config_proportional_fields() {
        // Verify lower < initial, min_safe == initial, topup ∈ (lower, initial)
        let cfg = compute_funding_config(&IncentiveConfiguration {
            desired_message_count: 100,
            ..Default::default()
        })
        .unwrap();
        assert_eq!(cfg.min_safe_capacity_required, cfg.initial_capacity);
        assert!(cfg.lower_capacity_threshold < cfg.initial_capacity);
        assert!(cfg.topup_capacity < cfg.initial_capacity);
        assert!(cfg.topup_capacity > cfg.lower_capacity_threshold);
    }

    #[test]
    fn compute_funding_config_rejects_zero_message_count() {
        assert!(
            compute_funding_config(&IncentiveConfiguration {
                desired_message_count: 0,
                ..Default::default()
            })
            .is_err()
        );
    }

    #[test]
    fn compute_funding_config_default_sizing_has_one_strategy_shape() {
        // Smoke-test: can build a MultiStrategyConfig from compute_funding_config output
        let funding = compute_funding_config(&IncentiveConfiguration {
            desired_message_count: 1,
            ..Default::default()
        })
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
            no_startup_costs(),
        )
        .unwrap();
        assert_eq!(rec.total_wxhopr(), HoprBalance::zero());
        assert_eq!(rec.xdai_fee_per_tx, *hopr_lib::SUGGESTED_NATIVE_BALANCE);
    }

    #[test]
    fn compute_balance_recommendation_includes_startup_costs() {
        // rotsee-like: the fee dwarfs the channel stakes and must be reported
        // both as its own field and in the total
        let fee = HoprBalance::new_base(3);
        let rec = compute_balance_recommendation(
            HoprBalance::new_base(10),
            1.0,
            &IncentiveConfiguration {
                desired_message_count: 1,
                ..Default::default()
            },
            8,
            StartupCosts {
                fee_to_start: fee,
                txs_to_start: 3,
            },
        )
        .unwrap();
        assert_eq!(rec.channel_stakes, HoprBalance::new_base(10) * 8u64);
        assert_eq!(rec.fee_to_start, fee);
        assert_eq!(rec.txs_to_start, 3);
        assert_eq!(rec.total_wxhopr(), HoprBalance::new_base(10) * 8u64 + fee);
    }

    #[test]
    fn compute_balance_recommendation_zero_missing_still_includes_fee() {
        // No channels to fund, key not yet bound: recommendation is exactly the
        // fee — and a zero ticket price must not error on this path.
        let fee = HoprBalance::new_base(1);
        let rec = compute_balance_recommendation(
            HoprBalance::zero(),
            1.0,
            &IncentiveConfiguration::default(),
            0,
            StartupCosts {
                fee_to_start: fee,
                txs_to_start: 2,
            },
        )
        .unwrap();
        assert_eq!(rec.channel_stakes, HoprBalance::zero());
        assert_eq!(rec.fee_to_start, fee);
        assert_eq!(rec.txs_to_start, 2);
        assert_eq!(rec.total_wxhopr(), fee);
    }

    #[test]
    fn compute_balance_recommendation_scales_by_missing_channels() {
        // win_prob=1.0, ticket_price=10, msg=1, hops=1: stake = 10; 8 channels = 80
        let rec = compute_balance_recommendation(
            HoprBalance::new_base(10),
            1.0,
            &IncentiveConfiguration {
                desired_message_count: 1,
                ..Default::default()
            },
            8,
            no_startup_costs(),
        )
        .unwrap();
        assert_eq!(rec.channel_stakes, HoprBalance::new_base(10) * 8u64);
        assert_eq!(rec.fee_to_start, HoprBalance::zero());
        assert_eq!(rec.txs_to_start, 0);
        assert_eq!(rec.total_wxhopr(), HoprBalance::new_base(10) * 8u64);
        assert_eq!(rec.xdai_fee_per_tx, *hopr_lib::SUGGESTED_NATIVE_BALANCE);
    }

    #[test]
    fn compute_balance_recommendation_zero_target_yields_zero() {
        let cfg = IncentiveConfiguration {
            target_open_channels: 0,
            min_open_channels: 0,
            ..Default::default()
        };
        let rec = compute_balance_recommendation(
            HoprBalance::new_base(10),
            1.0,
            &cfg,
            0,
            no_startup_costs(),
        )
        .unwrap();
        assert_eq!(rec.total_wxhopr(), HoprBalance::zero());
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
