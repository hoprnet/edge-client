use std::collections::HashSet;

use bytesize::ByteSize;
use hopr_lib::api::types::primitive::prelude::{
    Address, HoprBalance, UnitaryFloatOps as _, XDaiBalance,
};
pub use hopr_strategy::channel_lifecycle::{
    CapacitySizingMode, ChannelLifecycleConfig, EligibilityConfig, FundingConfig, PopulationConfig,
    SelectorProfile,
};

/// Paid downstream relay hops assumed when sizing a channel's stake. Edge nodes
/// route over a single hop by default, so a channel's tickets pay one relay.
const ASSUMED_HOPS: u32 = 1;

/// Ticket face values (`ticket_price × ASSUMED_HOPS / win_prob`) a new channel's
/// stake must cover. A ticket cannot be issued for more than the channel balance.
const INITIAL_FACE_VALUES: u64 = 5;

/// Face values a top-up adds back. Keeps `lower < topup < initial`.
const TOPUP_FACE_VALUES: u64 = 4;

/// Face values the balance may decay to before a top-up triggers. Above one so
/// tickets keep being issued while the top-up confirms.
const LOWER_THRESHOLD_FACE_VALUES: u64 = 2;

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

/// Packet payload size the strategy divides a data capacity by. Same value as
/// `PacketTransport::packet_payload_size()`, reachable from this ungated module.
fn packet_payload_size() -> u64 {
    hopr_lib::exports::transport::PACKET_PAYLOAD_SIZE as u64
}

/// Data capacity whose resolved stake covers `face_values` ticket face values.
///
/// An exact packet multiple, so the strategy's `ceil` reproduces the packet count.
/// Errors rather than clamping when a tiny `win_prob` overflows the byte count:
/// clamped capacities compare equal, breaking `lower < topup < initial`.
fn capacity_for_face_values(face_values: u64, win_prob: f64) -> anyhow::Result<ByteSize> {
    anyhow::ensure!(
        win_prob.is_finite() && win_prob > 0.0 && win_prob <= 1.0,
        "win_prob must be in (0, 1]; got {win_prob}"
    );
    let packets = (face_values as f64 / win_prob).ceil();
    anyhow::ensure!(
        packets <= u64::MAX as f64,
        "win_prob {win_prob} needs more than u64::MAX packets for {face_values} face values"
    );
    let bytes = (packets as u64)
        .checked_mul(packet_payload_size())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "capacity for {face_values} face values at win_prob {win_prob} overflows u64 bytes"
            )
        })?;
    Ok(ByteSize::b(bytes))
}

/// wxHOPR the strategy locks for a channel funded to `capacity`.
///
/// Mirrors the strategy's crate-private `capacity_to_balance` for
/// [`CapacitySizingMode::Deterministic`]; an upper bound for the other modes.
fn capacity_stake(capacity: ByteSize, ticket_price: HoprBalance, hops: u32) -> HoprBalance {
    let packets = capacity.as_u64().div_ceil(packet_payload_size());
    ticket_price * packets * hops as u64
}

/// Capacity-based [`FundingConfig`] for the given winning probability.
///
/// Each capacity is sized so the stake the strategy resolves it to covers a fixed
/// number of ticket face values; below one face value no ticket can be issued.
/// [`CapacitySizingMode::Deterministic`] is the expected-drain model here, and the
/// only mode this crate can mirror exactly for balance recommendations.
pub fn compute_funding_config(win_prob: f64) -> anyhow::Result<FundingConfig> {
    let initial_capacity = capacity_for_face_values(INITIAL_FACE_VALUES, win_prob)?;
    Ok(FundingConfig {
        initial_capacity,
        topup_capacity: capacity_for_face_values(TOPUP_FACE_VALUES, win_prob)?,
        lower_capacity_threshold: capacity_for_face_values(LOWER_THRESHOLD_FACE_VALUES, win_prob)?,
        min_safe_capacity_required: initial_capacity,
        stop_when_unfunded: true,
        sizing_mode: CapacitySizingMode::Deterministic,
    })
}

/// wxHOPR a single new channel's initial stake locks.
///
/// Read off [`compute_funding_config`] so the recommendation cannot drift from
/// what the strategy locks.
fn initial_channel_stake(ticket_price: HoprBalance, win_prob: f64) -> anyhow::Result<HoprBalance> {
    let funding = compute_funding_config(win_prob)?;
    Ok(capacity_stake(
        funding.initial_capacity,
        ticket_price,
        ASSUMED_HOPS,
    ))
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
    /// before balance drops below one face value and ticket issuance halts.
    /// Lower win_prob → smaller face value → fewer guaranteed concurrent tickets.
    ///
    /// Independent of [`CapacitySizingMode`] — a protocol constraint, not sizing.
    pub min_guaranteed_messages: u64,
    /// Raw byte capacity: `expected_messages × SESSION_MTU`.
    ///
    /// Session payload bytes, unlike [`FundingConfig`]'s capacities, which the
    /// strategy divides by the larger packet payload.
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
    missing_channels: usize,
    costs: StartupCosts,
) -> anyhow::Result<BalanceRecommendation> {
    let stake = if missing_channels == 0 {
        HoprBalance::zero()
    } else {
        initial_channel_stake(ticket_price, win_prob)? * (missing_channels as u64)
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
        cfg.target_open_channels,
        costs,
    )
}

/// Returns the default [`MultiStrategyConfig`] for an edge client reactor.
///
/// Reads the winning probability from the chain to size the funding config; see
/// [`compute_funding_config`]. Read once, so later changes are not tracked.
#[cfg(feature = "runtime-tokio")]
pub async fn default_strategy_cfg(
    node: &crate::client::Edgli,
    sizing: &IncentiveConfiguration,
) -> anyhow::Result<MultiStrategyConfig> {
    use hopr_lib::api::node::HasChainApi as _;

    sizing.validate()?;
    let win_prob = node
        .chain_api()
        .minimum_incoming_ticket_win_prob()
        .await?
        .as_f64();
    let funding = compute_funding_config(win_prob)?;
    tracing::info!(
        win_prob,
        initial_capacity = %funding.initial_capacity,
        topup_capacity = %funding.topup_capacity,
        "channel-lifecycle funding sized from live winning probability"
    );
    let cfg = ChannelLifecycleConfig {
        funding,
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

    /// Winning probabilities spanning the range the network may run at.
    const WIN_PROBS: [f64; 6] = [1.0, 0.5, 0.1, 0.01, 0.001, 0.0001];

    /// One ticket's face value, computed as the ticket factory does.
    fn face_value(ticket_price: HoprBalance, win_prob: f64) -> HoprBalance {
        (ticket_price * ASSUMED_HOPS as u64)
            .div_f64(win_prob)
            .unwrap()
    }

    /// Whether `stake` covers `count` ticket face values.
    ///
    /// The 1e-6 slack absorbs `div_f64` rounding a few parts in 1e13 upward; it is
    /// far below one face value, so an under-funded stake still fails.
    fn covers_face_values(
        stake: HoprBalance,
        ticket_price: HoprBalance,
        win_prob: f64,
        count: u64,
    ) -> bool {
        let required = face_value(ticket_price, win_prob) * count;
        stake * 1_000_000u64 >= required * 999_999u64
    }

    #[test]
    fn compute_funding_config_capacity_is_a_whole_number_of_packets() {
        let cfg = compute_funding_config(0.001).unwrap();
        let payload = packet_payload_size();
        assert_eq!(
            cfg.initial_capacity.as_u64(),
            (INITIAL_FACE_VALUES as f64 / 0.001).ceil() as u64 * payload
        );
        assert_eq!(cfg.min_safe_capacity_required, cfg.initial_capacity);
        assert!(cfg.stop_when_unfunded);
    }

    #[test]
    fn compute_funding_config_proportional_fields() {
        // Verify lower < topup < initial and min_safe == initial at every win_prob;
        // the per-field ceil could otherwise invert the ordering.
        for p in WIN_PROBS {
            let cfg = compute_funding_config(p).unwrap();
            assert_eq!(
                cfg.min_safe_capacity_required, cfg.initial_capacity,
                "p={p}"
            );
            assert!(cfg.lower_capacity_threshold < cfg.topup_capacity, "p={p}");
            assert!(cfg.topup_capacity < cfg.initial_capacity, "p={p}");
        }
    }

    #[test]
    fn compute_funding_config_lower_threshold_stays_above_one_face_value() {
        // The top-up threshold must stay above one face value, otherwise the channel
        // could fall below the balance needed to issue a ticket before a top-up fires.
        let price = HoprBalance::new_base(10);
        for p in WIN_PROBS {
            let cfg = compute_funding_config(p).unwrap();
            let stake = capacity_stake(cfg.lower_capacity_threshold, price, ASSUMED_HOPS);
            assert!(
                covers_face_values(stake, price, p, 1),
                "p={p}, stake={stake}"
            );
        }
    }

    #[test]
    fn compute_funding_config_uses_deterministic_sizing() {
        // `capacity_stake` mirrors `capacity_to_balance` for this mode only; a change
        // here silently breaks the balance recommendations.
        let cfg = compute_funding_config(1.0).unwrap();
        assert!(
            matches!(cfg.sizing_mode, CapacitySizingMode::Deterministic),
            "expected Deterministic, got {:?}",
            cfg.sizing_mode
        );
    }

    #[test]
    fn funding_config_initial_stake_covers_face_values() {
        // Regression: under the pre-#17 face-value model a 5-packet capacity locked
        // 5 face values; the stake must still cover them at every win_prob.
        let price = HoprBalance::new_base(10);
        for p in WIN_PROBS {
            let cfg = compute_funding_config(p).unwrap();
            let stake = capacity_stake(cfg.initial_capacity, price, ASSUMED_HOPS);
            assert!(
                covers_face_values(stake, price, p, INITIAL_FACE_VALUES),
                "p={p}, stake={stake}"
            );
        }
    }

    #[test]
    fn funding_config_topup_stake_covers_face_values() {
        let price = HoprBalance::new_base(10);
        for p in WIN_PROBS {
            let cfg = compute_funding_config(p).unwrap();
            let stake = capacity_stake(cfg.topup_capacity, price, ASSUMED_HOPS);
            assert!(
                covers_face_values(stake, price, p, TOPUP_FACE_VALUES),
                "p={p}, stake={stake}"
            );
        }
    }

    #[test]
    fn funding_config_capacities_are_exact_packet_multiples() {
        // The strategy divides each capacity by the packet payload and rounds up;
        // exact multiples keep `capacity_stake` an exact mirror of that conversion.
        let payload = packet_payload_size();
        for p in WIN_PROBS {
            let cfg = compute_funding_config(p).unwrap();
            for cap in [
                cfg.initial_capacity,
                cfg.topup_capacity,
                cfg.lower_capacity_threshold,
                cfg.min_safe_capacity_required,
            ] {
                assert_eq!(cap.as_u64() % payload, 0, "p={p}, cap={cap}");
            }
        }
    }

    #[test]
    fn funding_config_win_prob_one_matches_face_value_counts() {
        // Pins the pre-0.22 economics: 5 / 4 / 2 packets at win_prob = 1.
        let cfg = compute_funding_config(1.0).unwrap();
        let payload = packet_payload_size();
        assert_eq!(cfg.initial_capacity.as_u64(), INITIAL_FACE_VALUES * payload);
        assert_eq!(cfg.topup_capacity.as_u64(), TOPUP_FACE_VALUES * payload);
        assert_eq!(
            cfg.lower_capacity_threshold.as_u64(),
            LOWER_THRESHOLD_FACE_VALUES * payload
        );
    }

    #[test]
    fn compute_funding_config_rejects_invalid_win_prob() {
        assert!(compute_funding_config(0.0).is_err());
        assert!(compute_funding_config(1.1).is_err());
        assert!(compute_funding_config(f64::NAN).is_err());
        assert!(compute_funding_config(f64::INFINITY).is_err());
    }

    #[test]
    fn compute_funding_config_rejects_capacity_overflow() {
        // `WinningProbability` encodes into 7 bytes and `as_f64` yields `k × 2⁻⁵²`,
        // so 2⁻⁵² is the smallest value the chain can report. Its capacity exceeds
        // `u64::MAX` bytes and must be rejected rather than clamped — clamping makes
        // `lower < topup < initial` false and `min_safe` unfundable.
        assert!(compute_funding_config(2f64.powi(-52)).is_err());
        assert!(compute_funding_config(f64::MIN_POSITIVE).is_err());
    }

    #[test]
    fn compute_funding_config_smallest_usable_win_prob_keeps_ordering() {
        // One encoding step above the rejected value the config is still well formed.
        let cfg = compute_funding_config(2f64.powi(-51)).unwrap();
        assert!(cfg.lower_capacity_threshold < cfg.topup_capacity);
        assert!(cfg.topup_capacity < cfg.initial_capacity);
    }

    #[test]
    fn balance_recommendation_matches_strategy_initial_stake() {
        // The recommendation must equal what the strategy locks, never less.
        for price in [HoprBalance::new_base(10), HoprBalance::from(100u32)] {
            for p in [1.0, 0.01, 0.001] {
                let cfg = compute_funding_config(p).unwrap();
                let expected = capacity_stake(cfg.initial_capacity, price, ASSUMED_HOPS);
                let one = compute_balance_recommendation(price, p, 1, no_startup_costs()).unwrap();
                assert_eq!(one.channel_stakes, expected, "price={price}, p={p}");
                let eight =
                    compute_balance_recommendation(price, p, 8, no_startup_costs()).unwrap();
                assert_eq!(
                    eight.channel_stakes,
                    expected * 8u64,
                    "price={price}, p={p}"
                );
            }
        }
    }

    #[test]
    fn balance_recommendation_covers_min_safe_balance() {
        // `stop_when_unfunded` blocks every open below min_safe_capacity_required.
        let price = HoprBalance::new_base(10);
        for p in WIN_PROBS {
            let cfg = compute_funding_config(p).unwrap();
            let min_safe = capacity_stake(cfg.min_safe_capacity_required, price, ASSUMED_HOPS);
            let rec = compute_balance_recommendation(price, p, 1, no_startup_costs()).unwrap();
            assert!(rec.channel_stakes >= min_safe, "p={p}");
        }
    }

    #[test]
    fn compute_funding_config_default_sizing_has_one_strategy_shape() {
        // Smoke-test: can build a MultiStrategyConfig from compute_funding_config output
        let funding = compute_funding_config(1.0).unwrap();
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
        let rec =
            compute_balance_recommendation(HoprBalance::new_base(10), 1.0, 0, no_startup_costs())
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
            8,
            StartupCosts {
                fee_to_start: fee,
                txs_to_start: 3,
            },
        )
        .unwrap();
        // stake per channel = ticket_price × INITIAL_FACE_VALUES at win_prob = 1
        let per_channel = HoprBalance::new_base(10) * 5u64;
        assert_eq!(rec.channel_stakes, per_channel * 8u64);
        assert_eq!(rec.fee_to_start, fee);
        assert_eq!(rec.txs_to_start, 3);
        assert_eq!(rec.total_wxhopr(), per_channel * 8u64 + fee);
    }

    #[test]
    fn compute_balance_recommendation_zero_missing_still_includes_fee() {
        // No channels to fund, key not yet bound: recommendation is exactly the
        // fee — and a zero ticket price must not error on this path.
        let fee = HoprBalance::new_base(1);
        let rec = compute_balance_recommendation(
            HoprBalance::zero(),
            1.0,
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
        // win_prob=1.0, ticket_price=10, hops=1: stake = 10 × 5 face values; 8 channels
        let rec =
            compute_balance_recommendation(HoprBalance::new_base(10), 1.0, 8, no_startup_costs())
                .unwrap();
        let per_channel = HoprBalance::new_base(10) * 5u64;
        assert_eq!(rec.channel_stakes, per_channel * 8u64);
        assert_eq!(rec.fee_to_start, HoprBalance::zero());
        assert_eq!(rec.txs_to_start, 0);
        assert_eq!(rec.total_wxhopr(), per_channel * 8u64);
        assert_eq!(rec.xdai_fee_per_tx, *hopr_lib::SUGGESTED_NATIVE_BALANCE);
    }

    #[test]
    fn compute_balance_recommendation_halved_win_prob_doubles_stake() {
        // The derived capacity is ceil(INITIAL_FACE_VALUES / win_prob) packets, so
        // halving win_prob doubles the packet count and with it the stake.
        let full =
            compute_balance_recommendation(HoprBalance::new_base(10), 1.0, 1, no_startup_costs())
                .unwrap();
        let half =
            compute_balance_recommendation(HoprBalance::new_base(10), 0.5, 1, no_startup_costs())
                .unwrap();
        assert_eq!(half.channel_stakes, full.channel_stakes * 2u64);
    }

    #[test]
    fn compute_balance_recommendation_zero_target_yields_zero() {
        let rec =
            compute_balance_recommendation(HoprBalance::new_base(10), 1.0, 0, no_startup_costs())
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
