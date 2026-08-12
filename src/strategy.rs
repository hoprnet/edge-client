use std::collections::HashSet;

use bytesize::ByteSize;
use hopr_lib::api::node::PacketTransport;
use hopr_lib::api::types::internal::routing::RoutingOptions;
use hopr_lib::api::types::primitive::prelude::{
    Address, HoprBalance, UnitaryFloatOps as _, XDaiBalance,
};
pub use hopr_strategy::channel_lifecycle::{
    CapacitySizingMode, ChannelLifecycleConfig, EligibilityConfig, FundingConfig, PopulationConfig,
    ResolvedFunding, SelectorProfile,
};

/// Paid downstream relay hops a ticket's face value covers.
///
/// Fixed at the protocol's maximum path length rather than the single hop an edge node
/// typically routes over: the node issuing a ticket cannot know how long a path the packet
/// will take, so a face value assuming fewer hops cannot pay for a longer one.
///
/// `hopr-strategy` sizes stakes from this same constant. Reading it from the protocol here
/// rather than restating a number keeps [`compute_capacity`] from reporting more payable
/// tickets than a stake actually covers.
const ASSUMED_HOPS: u32 = RoutingOptions::MAX_INTERMEDIATE_HOPS as u32;

/// Safe capacity required before a channel opens, as a multiple of the initial capacity.
///
/// The strategy's own default is a fixed volume that does not track a requested one, so a
/// larger request would otherwise clear the gate on a Safe that cannot then top the
/// channel up. Twice covers the channel and its first top-up.
///
/// Applied as a floor *alongside* that default, not in place of it: the gate is the larger
/// of the two, so a small request cannot lower it below what the strategy would have
/// required on its own.
const MIN_SAFE_MULTIPLE: u64 = 2;

/// Confidence level the channel stake is sized for.
///
/// [`CapacitySizingMode::Deterministic`] sizes to the *mean* drain, leaving the lower
/// threshold near one ticket face value. Payouts are lumpy (winning tickets are
/// `Binomial(N, win_prob)`), so a channel resting there starves — it cannot issue the
/// next ticket, and relaying stalls until a top-up confirms. 99% adds `k·σ`
/// (`k = Φ⁻¹(0.99) ≈ 2.33`) above the mean.
const SIZING_SUCCESS_PROBABILITY: f64 = 0.99;

/// Range `hopr_strategy` accepts for a `Probabilistic` success probability, which it
/// silently clamps to. Restated here only so the assertion below can reject an
/// out-of-range [`SIZING_SUCCESS_PROBABILITY`] at build time.
const SIZING_PROBABILITY_BOUNDS: (f64, f64) = (0.5001, 0.99999);

// Out of range would otherwise be clamped at runtime, sizing channels for a confidence
// nobody asked for. Fail the build instead.
const _: () = assert!(
    SIZING_SUCCESS_PROBABILITY >= SIZING_PROBABILITY_BOUNDS.0
        && SIZING_SUCCESS_PROBABILITY <= SIZING_PROBABILITY_BOUNDS.1,
    "SIZING_SUCCESS_PROBABILITY must lie within the range hopr-strategy accepts"
);

/// The mode every capacity in [`compute_funding_config`] resolves through.
///
/// Deliberately *not* exposed on [`IncentiveConfiguration`]. The reactor and the balance
/// recommendations must agree on the mode, and a caller able to override it on the
/// returned [`FundingConfig`] could set one without the other.
const SIZING_MODE: CapacitySizingMode = CapacitySizingMode::Probabilistic {
    success_probability: SIZING_SUCCESS_PROBABILITY,
};

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

    /// Data volume a single channel should carry before it needs a top-up.
    ///
    /// The only sizing input a caller supplies. It becomes the strategy's initial
    /// capacity as given; top-up and lower threshold keep the strategy's own defaults.
    ///
    /// # Funding is all-or-nothing
    ///
    /// Raising this also raises the balance below which the node refuses to operate. The
    /// safe gate is the larger of [`MIN_SAFE_MULTIPLE`] × this volume and the strategy's
    /// own default, so a small request does not lower it. Under `stop_when_unfunded`, a
    /// Safe below that gate opens **zero** channels, not smaller ones and not fewer — read
    /// the figure off [`minimum_balance_recommendation`] rather than deriving it here.
    ///
    /// Default: `None` — the strategy's own initial capacity.
    #[default(None)]
    pub channel_capacity: Option<ByteSize>,
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

/// Supplies the packet payload size to the strategy's funding resolution.
///
/// [`FundingConfig::resolve`] is generic over the transport purely to read this value;
/// forwarding [`packet_payload_size`] keeps capacities and balances on one number.
struct EdgePacketTransport;

impl PacketTransport for EdgePacketTransport {
    fn packet_payload_size() -> usize {
        self::packet_payload_size() as usize
    }
}

/// The wxHOPR the strategy resolves `funding` to at the current ticket economics.
///
/// Delegates to [`FundingConfig::resolve`] rather than reproducing the
/// capacity-to-balance conversion: a local copy keeps compiling after the formula changes
/// upstream, then reports figures the strategy disagrees with — and since
/// `min_safe_balance_required` gates opening under `stop_when_unfunded`, reporting low
/// leaves a node unable to open any channel. Honours whichever [`CapacitySizingMode`]
/// `funding` carries, so it tracks [`SIZING_MODE`] without restating it.
fn resolve_funding(
    funding: &FundingConfig,
    ticket_price: HoprBalance,
    win_prob: f64,
) -> ResolvedFunding {
    funding.resolve::<EdgePacketTransport>(ticket_price, win_prob)
}

/// [`FundingConfig`] for a requested transfer volume.
///
/// `channel_capacity` is the volume one channel should carry before a top-up. It is the
/// only field set from it; the rest keep the strategy's own defaults, which now fill in
/// on partial input. `min_safe_capacity_required` is the exception — see
/// [`MIN_SAFE_MULTIPLE`].
///
/// All resolve through [`SIZING_MODE`]; [`resolve_funding`] converts them to wxHOPR.
pub fn compute_funding_config(channel_capacity: Option<ByteSize>) -> anyhow::Result<FundingConfig> {
    let defaults = FundingConfig::default();
    let initial_capacity = channel_capacity.unwrap_or(defaults.initial_capacity);
    let min_safe = initial_capacity
        .as_u64()
        .checked_mul(MIN_SAFE_MULTIPLE)
        .ok_or_else(|| anyhow::anyhow!("{initial_capacity} × {MIN_SAFE_MULTIPLE} overflows u64"))?;

    Ok(FundingConfig {
        initial_capacity,
        min_safe_capacity_required: ByteSize::b(min_safe).max(defaults.min_safe_capacity_required),
        stop_when_unfunded: true,
        sizing_mode: SIZING_MODE,
        ..defaults
    })
}

/// wxHOPR the Safe must hold to fund `missing_channels` new channels.
///
/// Raised to `min_safe_balance_required`, which `stop_when_unfunded` gates every open on:
/// a node funded to exactly `missing × initial` would sit at the threshold and never open
/// its first channel.
fn channel_stakes(
    ticket_price: HoprBalance,
    win_prob: f64,
    channel_capacity: Option<ByteSize>,
    missing_channels: usize,
) -> anyhow::Result<HoprBalance> {
    let funding = compute_funding_config(channel_capacity)?;
    let resolved = resolve_funding(&funding, ticket_price, win_prob);
    let total = resolved.initial_balance * (missing_channels as u64);
    Ok(total.max(resolved.min_safe_balance_required))
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
    /// Expected long-run session-frame count: `floor(stake / (ticket_price × ASSUMED_HOPS))`.
    ///
    /// `ticket_price × ASSUMED_HOPS` is the *expected* per-message drain
    /// (= `face_value × win_prob`), so this figure is win_prob-independent — it reflects
    /// average lifetime throughput.
    pub expected_messages: u64,
    /// Worst-case floor: `floor(stake / face_value)`, where
    /// `face_value = ticket_price × ASSUMED_HOPS / win_prob`.
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
    channel_capacity: Option<ByteSize>,
) -> anyhow::Result<BalanceRecommendation> {
    let stake = if missing_channels == 0 {
        HoprBalance::zero()
    } else {
        channel_stakes(ticket_price, win_prob, channel_capacity, missing_channels)?
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
    // Both counts are per *message*, and a message pays `ASSUMED_HOPS` relays: it issues one
    // aggregated ticket of face value `ticket_price × ASSUMED_HOPS / win_prob`, whose expected
    // cost is `ticket_price × ASSUMED_HOPS`. Measuring either against the bare ticket price
    // would report `ASSUMED_HOPS`× more messages than the stake can actually pay for — and the
    // strategy funds and gates on the same hop count, so the two must agree.
    let per_message_drain = ticket_price * ASSUMED_HOPS as u64;
    let expected_messages = if per_message_drain == HoprBalance::zero() {
        0u64
    } else {
        // Clamp to u64::MAX before converting so astronomically large quotients
        // saturate rather than truncate via low_u64().
        let quotient = stake.amount() / per_message_drain.amount();
        quotient.min(u64::MAX.into()).low_u64()
    };
    // face_value = ticket_price × ASSUMED_HOPS / win_prob — balance locked per outstanding ticket.
    let face_value = per_message_drain.div_f64(win_prob)?;
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
        cfg.channel_capacity,
    )
}

/// Returns the default [`MultiStrategyConfig`] for an edge client reactor.
///
/// Takes no chain reading: the capacities are the requested volume and the strategy's own
/// defaults, and the winning probability only enters when the strategy resolves them to
/// balances each tick — so this tracks a changing win prob rather than pinning it.
pub fn default_strategy_cfg(
    sizing: &IncentiveConfiguration,
) -> anyhow::Result<MultiStrategyConfig> {
    sizing.validate()?;
    let funding = compute_funding_config(sizing.channel_capacity)?;
    tracing::info!(
        requested_capacity = ?sizing.channel_capacity,
        initial_capacity = %funding.initial_capacity,
        topup_capacity = %funding.topup_capacity,
        // The threshold that decides when a top-up fires, and so the first value
        // to reach for when investigating a channel that stalled mid-relay.
        lower_capacity_threshold = %funding.lower_capacity_threshold,
        min_safe_capacity_required = %funding.min_safe_capacity_required,
        sizing_mode = ?funding.sizing_mode,
        "channel-lifecycle funding configured"
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

    #[test]
    fn funding_config_uses_the_requested_capacity_verbatim() {
        // The requested volume is the only field taken from the caller, and it is passed
        // through as-is: no rounding, no floor, no conversion.
        let requested = ByteSize::mb(100);
        let cfg = compute_funding_config(Some(requested)).unwrap();
        assert_eq!(cfg.initial_capacity, requested);
    }

    #[test]
    fn funding_config_falls_back_to_the_strategy_default_capacity() {
        let cfg = compute_funding_config(None).unwrap();
        assert_eq!(
            cfg.initial_capacity,
            FundingConfig::default().initial_capacity
        );
    }

    #[test]
    fn funding_config_defers_topup_and_lower_threshold_to_the_strategy() {
        // Everything except the initial capacity and the safe gate is the strategy's own
        // default, so this crate carries no second opinion on how a channel is funded.
        let defaults = FundingConfig::default();
        for requested in [None, Some(ByteSize::mb(50)), Some(ByteSize::gib(4))] {
            let cfg = compute_funding_config(requested).unwrap();
            assert_eq!(cfg.topup_capacity, defaults.topup_capacity, "{requested:?}");
            assert_eq!(
                cfg.lower_capacity_threshold, defaults.lower_capacity_threshold,
                "{requested:?}"
            );
        }
    }

    #[test]
    fn funding_config_min_safe_covers_at_least_two_channels() {
        // The strategy's default gate is a fixed volume that does not track the request,
        // so a larger request would otherwise clear it on a safe that cannot then top the
        // channel up.
        for requested in [None, Some(ByteSize::mb(1)), Some(ByteSize::gib(4))] {
            let cfg = compute_funding_config(requested).unwrap();
            assert!(
                cfg.min_safe_capacity_required.as_u64()
                    >= cfg.initial_capacity.as_u64() * MIN_SAFE_MULTIPLE,
                "{requested:?}: {} < 2 x {}",
                cfg.min_safe_capacity_required,
                cfg.initial_capacity
            );
        }
    }

    #[test]
    fn funding_config_rejects_a_capacity_that_overflows_the_safe_gate() {
        assert!(compute_funding_config(Some(ByteSize::b(u64::MAX))).is_err());
    }

    #[test]
    fn funding_config_uses_probabilistic_sizing() {
        // `resolve_funding` asks the strategy to resolve whichever mode is set here, so
        // this pins the mode itself rather than any restatement of its arithmetic.
        let cfg = compute_funding_config(None).unwrap();
        assert!(
            matches!(
                cfg.sizing_mode,
                CapacitySizingMode::Probabilistic { success_probability }
                    if success_probability == SIZING_SUCCESS_PROBABILITY
            ),
            "expected Probabilistic({SIZING_SUCCESS_PROBABILITY}), got {:?}",
            cfg.sizing_mode
        );
        assert!(cfg.stop_when_unfunded);
    }

    #[test]
    fn resolved_stake_carries_the_variance_buffer() {
        // Guards against the mode silently collapsing to the mean drain: below
        // win_prob = 1 the Probabilistic term must add k, sigma on top of N x hops x price.
        let price = HoprBalance::new_base(10);
        let cfg = compute_funding_config(None).unwrap();
        let packets = cfg
            .initial_capacity
            .as_u64()
            .div_ceil(packet_payload_size());
        let mean_drain = price * packets * ASSUMED_HOPS as u64;
        for p in [0.5, 0.1, 0.01, 0.001] {
            let stake = resolve_funding(&cfg, price, p).initial_balance;
            assert!(
                stake > mean_drain,
                "p={p}: stake {stake} should exceed mean drain {mean_drain}"
            );
        }
    }

    #[test]
    fn resolved_stake_at_win_prob_one_equals_mean_drain() {
        // At win_prob = 1 the variance vanishes, so Probabilistic collapses onto
        // Deterministic and the stake is exactly the mean drain.
        // A small capacity keeps the packet count well inside f64's exact-integer range;
        // at the default 1 GiB it is ~1M packets and the strategy's f64 pipeline lands a
        // few wei off, which says nothing about the mode collapsing.
        let price = HoprBalance::new_base(10);
        let cfg = compute_funding_config(Some(ByteSize::kb(10))).unwrap();
        let packets = cfg
            .initial_capacity
            .as_u64()
            .div_ceil(packet_payload_size());
        assert_eq!(
            resolve_funding(&cfg, price, 1.0).initial_balance,
            price * packets * ASSUMED_HOPS as u64
        );
    }

    #[test]
    fn balance_recommendation_matches_strategy_initial_stake() {
        // The recommendation must equal what the strategy locks, never less: `missing x
        // initial`, raised to the safe gate when a single channel would fall under it.
        for price in [HoprBalance::new_base(10), HoprBalance::from(100u32)] {
            for p in [1.0, 0.01, 0.001] {
                let cfg = compute_funding_config(None).unwrap();
                let resolved = resolve_funding(&cfg, price, p);

                let one =
                    compute_balance_recommendation(price, p, 1, no_startup_costs(), None).unwrap();
                assert_eq!(
                    one.channel_stakes, resolved.min_safe_balance_required,
                    "price={price}, p={p}"
                );

                let many =
                    compute_balance_recommendation(price, p, 64, no_startup_costs(), None).unwrap();
                assert_eq!(
                    many.channel_stakes,
                    resolved.initial_balance * 64u64,
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
            let cfg = compute_funding_config(None).unwrap();
            let min_safe = resolve_funding(&cfg, price, p).min_safe_balance_required;
            let rec =
                compute_balance_recommendation(price, p, 1, no_startup_costs(), None).unwrap();
            assert!(rec.channel_stakes >= min_safe, "p={p}");
        }
    }

    #[test]
    fn balance_recommendation_tracks_requested_capacity() {
        // Regression: the recommendation is derived from the same funding config the
        // reactor runs on. If it ignored `channel_capacity` a node funded to the reported
        // figure could never open a channel at the requested volume.
        let price = HoprBalance::new_base(10);
        let requested = Some(ByteSize::gib(4));
        for p in [1.0, 0.01, 0.001] {
            let cfg = compute_funding_config(requested).unwrap();
            let expected = resolve_funding(&cfg, price, p).min_safe_balance_required;
            let rec =
                compute_balance_recommendation(price, p, 1, no_startup_costs(), requested).unwrap();
            assert_eq!(rec.channel_stakes, expected, "p={p}");

            let default =
                compute_balance_recommendation(price, p, 1, no_startup_costs(), None).unwrap();
            assert!(
                rec.channel_stakes > default.channel_stakes,
                "p={p}: a 4 GiB request must cost more than the default capacity"
            );
        }
    }

    #[test]
    fn incentive_configuration_default_channel_capacity_is_none() {
        assert!(IncentiveConfiguration::default().channel_capacity.is_none());
    }

    #[test]
    fn compute_funding_config_default_sizing_has_one_strategy_shape() {
        // Smoke-test: can build a MultiStrategyConfig from compute_funding_config output
        let funding = compute_funding_config(None).unwrap();
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
            0,
            no_startup_costs(),
            None,
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
            8,
            StartupCosts {
                fee_to_start: fee,
                txs_to_start: 3,
            },
            None,
        )
        .unwrap();
        let per_channel = resolve_funding(
            &compute_funding_config(None).unwrap(),
            HoprBalance::new_base(10),
            1.0,
        )
        .initial_balance;
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
            None,
        )
        .unwrap();
        assert_eq!(rec.channel_stakes, HoprBalance::zero());
        assert_eq!(rec.fee_to_start, fee);
        assert_eq!(rec.txs_to_start, 2);
        assert_eq!(rec.total_wxhopr(), fee);
    }

    #[test]
    fn compute_balance_recommendation_scales_by_missing_channels() {
        // Eight channels clears the safe gate, so the total is 8 x the per-channel stake.
        let rec = compute_balance_recommendation(
            HoprBalance::new_base(10),
            1.0,
            8,
            no_startup_costs(),
            None,
        )
        .unwrap();
        let per_channel = resolve_funding(
            &compute_funding_config(None).unwrap(),
            HoprBalance::new_base(10),
            1.0,
        )
        .initial_balance;
        assert_eq!(rec.channel_stakes, per_channel * 8u64);
        assert_eq!(rec.fee_to_start, HoprBalance::zero());
        assert_eq!(rec.txs_to_start, 0);
        assert_eq!(rec.total_wxhopr(), per_channel * 8u64);
        assert_eq!(rec.xdai_fee_per_tx, *hopr_lib::SUGGESTED_NATIVE_BALANCE);
    }

    #[test]
    fn compute_balance_recommendation_rises_as_win_prob_falls() {
        // The capacity is fixed now, so the mean drain does not move with win_prob. What
        // still grows is the variance buffer (σ ∝ 1/√p) and the one-winning-ticket floor
        // (∝ 1/p), so a lower win_prob must cost strictly more.
        let full = compute_balance_recommendation(
            HoprBalance::new_base(10),
            1.0,
            1,
            no_startup_costs(),
            None,
        )
        .unwrap();
        let half = compute_balance_recommendation(
            HoprBalance::new_base(10),
            0.5,
            1,
            no_startup_costs(),
            None,
        )
        .unwrap();
        assert!(
            half.channel_stakes > full.channel_stakes,
            "expected a strictly larger stake at the lower win_prob, got {} vs {}",
            half.channel_stakes,
            full.channel_stakes
        );
    }

    #[test]
    fn compute_balance_recommendation_zero_target_yields_zero() {
        let rec = compute_balance_recommendation(
            HoprBalance::new_base(10),
            1.0,
            0,
            no_startup_costs(),
            None,
        )
        .unwrap();
        assert_eq!(rec.total_wxhopr(), HoprBalance::zero());
    }

    #[test]
    fn compute_capacity_basic_arithmetic() {
        // A message pays ASSUMED_HOPS (3) relays, so it drains 10 × 3 = 30 per message:
        // expected = 900 / 30 = 30, and at win_prob = 1 the face value is the same 30.
        let cap =
            compute_capacity(HoprBalance::new_base(900), HoprBalance::new_base(10), 1.0).unwrap();
        assert_eq!(cap.expected_messages, 30);
        assert_eq!(cap.min_guaranteed_messages, 30);
        assert_eq!(cap.byte_capacity, 30 * hopr_lib::SESSION_MTU as u64);
        assert_eq!(cap.stake, HoprBalance::new_base(900));
    }

    /// The hop factor must be the one the strategy funds with, or a stake reports more
    /// payable messages than it can cover and the funding-issue checks under-report.
    #[test]
    fn compute_capacity_counts_every_paid_hop() {
        let price = HoprBalance::new_base(10);
        let stake = price * 30u64;
        let cap = compute_capacity(stake, price, 1.0).unwrap();
        assert_eq!(
            cap.expected_messages,
            30 / ASSUMED_HOPS as u64,
            "a message must be charged for all {ASSUMED_HOPS} paid hops"
        );
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
        // win_prob=1.0 → face_value = per-message drain → min_guaranteed == expected
        // 300 / (10 × 3 hops) = 10
        let cap =
            compute_capacity(HoprBalance::new_base(300), HoprBalance::new_base(10), 1.0).unwrap();
        assert_eq!(cap.expected_messages, 10);
        assert_eq!(cap.min_guaranteed_messages, 10);
    }

    #[test]
    fn compute_capacity_win_prob_half_halves_guaranteed() {
        // stake=600, ticket_price=10, 3 hops, win_prob=0.5
        // expected   = 600 / (10 × 3) = 20
        // face_value = 30 / 0.5 = 60 → min_guaranteed = 600 / 60 = 10
        let cap =
            compute_capacity(HoprBalance::new_base(600), HoprBalance::new_base(10), 0.5).unwrap();
        assert_eq!(cap.expected_messages, 20);
        assert_eq!(cap.min_guaranteed_messages, 10);
    }

    #[test]
    fn compute_capacity_win_prob_small_floors_guaranteed_to_zero() {
        // stake=40, ticket_price=10, 3 hops, win_prob=0.5
        // face_value = 30 / 0.5 = 60 > stake → min_guaranteed = 0; expected = 40 / 30 = 1
        let cap =
            compute_capacity(HoprBalance::new_base(40), HoprBalance::new_base(10), 0.5).unwrap();
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
