use std::collections::HashSet;
#[cfg(feature = "pix-secp256k1")]
use std::time::Duration;

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

/// Default mode every capacity in [`compute_funding_config`] resolves through when
/// [`IncentiveConfiguration::sizing_mode`] is left unset.
const SIZING_MODE: CapacitySizingMode = CapacitySizingMode::Probabilistic {
    success_probability: SIZING_SUCCESS_PROBABILITY,
};

#[cfg(any(feature = "blokli", feature = "runtime-tokio"))]
use hopr_lib::api::chain::{AccountSelector, ChainReadAccountOperations, ChainValues};

/// Subset of strategies relevant to an edge node.
///
/// `#[non_exhaustive]` because [`Pix`](Self::Pix) is feature-gated: a downstream `match` would
/// otherwise be exhaustive under one feature set and not under another, which makes turning
/// `pix-secp256k1` on a breaking change for every consumer rather than an additive one.
#[non_exhaustive]
pub enum EdgeStrategyKind {
    /// Boxed because [`ChannelLifecycleConfig`] is some 560 bytes against `Pix`'s few dozen, and
    /// every element of a `MultiStrategyConfig` would otherwise be sized for the largest. `hoprd`
    /// boxes the same variant of its own strategy enum for the same reason.
    ChannelLifecycle(Box<ChannelLifecycleConfig>),
    /// Pays the Exit for the traffic it delivers. See [`PixEntryConfig`].
    #[cfg(feature = "pix-secp256k1")]
    Pix(PixEntryConfig),
}

/// Entry-side PIX settlement knobs.
///
/// Only the fields an Entry reaches. The recovery store, the withdrawal batching window, the
/// sweep's gas top-up and its retry budget are Exit-side and stay at their upstream defaults —
/// which is safe rather than merely tidy. `HoprTransport::run_inner` gives an Entry node a PIX
/// toolbox with the share generator and a *dummy* reconstructor, and never wires the Exit
/// acknowledgement processing, so `PixEvent::PrivateKeyRecovered` — the only event that consults
/// any of those fields — cannot fire on this node. There is nothing here for an operator to get
/// wrong because there is nothing here that runs.
///
/// Defaults are read from the upstream types rather than restated, so a change to what PIX charges
/// arrives here instead of being silently overridden by a stale copy.
// The two cross-references below are code spans rather than intra-doc links on purpose:
// `Edgli` and `blokli` need the `blokli` feature, which `pix-secp256k1` does not imply, so a
// `--features pix-secp256k1` doc build would reject them as broken links.
#[cfg(feature = "pix-secp256k1")]
#[derive(Debug, Clone, PartialEq)]
pub struct PixEntryConfig {
    /// wxHOPR charged per byte of the agreed per-SSA quota. One deposit is
    /// `price_per_byte × quota_per_ssa`, where the quota is
    /// `polys_per_ssa × (shares_per_poly + surplus_shares) × PACKET_PAYLOAD_SIZE`.
    ///
    /// # Deposits are paid from the node's own account, not the Safe
    ///
    /// The deposit is a direct `HoprToken.transfer` signed by the node key: it is the one call the
    /// Safe payload generator does not route through the Safe module. So the wxHOPR comes off the
    /// node's own address, while `IncentiveOperations::deploy_safe` sweeps that balance *into* the
    /// Safe during onboarding.
    ///
    /// An operator running PIX therefore has to leave a wxHOPR float on the node address, sized
    /// against `price_per_byte × quota_per_ssa × expected SSA cycles`. A node that runs dry stops
    /// depositing and the Exit closes the Session on its deposit deadline — with nothing logged as
    /// an error at this end. `Edgli::describe_current_capacity_allocations` reports the remaining
    /// float as its `node` allocation.
    ///
    /// Upstream default: 1 wxHOPR, which prices a default-dimensioned SSA far past any sane
    /// ceiling. Set this against your own dimensions.
    pub price_per_byte: HoprBalance,

    /// Ceiling on a single SSA deposit.
    ///
    /// A computed deposit above this is refused outright rather than trimmed, which starves the
    /// Session until the Exit's deposit deadline closes it. It is a guard against a
    /// mis-dimensioned quota, not a budget.
    ///
    /// Upstream default: 100 wxHOPR.
    pub max_ssa_allocation: HoprBalance,

    /// Debounce window before a batch of pending deposits is flushed.
    ///
    /// Resets on each new event, so a burst is paid in one batched call. Upstream default: 500 ms.
    pub deposit_buffer_period: Duration,

    /// How long the pool keeps polling a stealth address for the deposit to land.
    ///
    /// Also sets the poll cadence, at a tenth of this. That has to stay comfortably below the peer
    /// Exit's `max_deposit_wait + max_ssa_delivery_time`, or only the single immediate balance
    /// check happens before the Exit gives up on the Session.
    ///
    /// Upstream default: 60 s.
    pub max_deposit_tracking_time: Duration,

    /// Attempts *in addition to* the first for a deposit transfer.
    ///
    /// Retrying is safe because every attempt re-reads the destination balance and sends nothing
    /// if it already holds the amount. Upstream default: 3.
    pub max_deposit_retries: usize,
}

#[cfg(feature = "pix-secp256k1")]
impl Default for PixEntryConfig {
    fn default() -> Self {
        let strategy = hopr_strategy::pix::strategy::PixStrategyConfig::default();
        let pool = hopr_strategy::pix::secp256k1::PoolConfig::default();
        Self {
            price_per_byte: strategy.price_per_byte,
            max_ssa_allocation: strategy.max_ssa_allocation,
            deposit_buffer_period: strategy.deposit_buffer_period,
            max_deposit_tracking_time: pool.max_deposit_tracking_time,
            max_deposit_retries: pool.max_deposit_retries,
        }
    }
}

#[cfg(feature = "pix-secp256k1")]
impl PixEntryConfig {
    /// The pool-agnostic half, with the Exit-side fields left at their upstream defaults.
    ///
    /// `pix_recovery_db_path` and `pix_recovery_password_env` must stay *both* unset: the strategy
    /// refuses to build if exactly one of them is given, and an Entry has no recovered key to
    /// persist either way.
    pub(crate) fn strategy_config(&self) -> hopr_strategy::pix::strategy::PixStrategyConfig {
        hopr_strategy::pix::strategy::PixStrategyConfig {
            price_per_byte: self.price_per_byte,
            max_ssa_allocation: self.max_ssa_allocation,
            deposit_buffer_period: self.deposit_buffer_period,
            ..Default::default()
        }
    }

    /// The pool half, with the sweep's gas top-up and retry budget left at their upstream defaults.
    pub(crate) fn pool_config(&self) -> hopr_strategy::pix::secp256k1::PoolConfig {
        hopr_strategy::pix::secp256k1::PoolConfig {
            max_deposit_tracking_time: self.max_deposit_tracking_time,
            max_deposit_retries: self.max_deposit_retries,
            ..Default::default()
        }
    }
}

/// This node's own PIX dimensions, as the [`PixParams`] a Session announces.
///
/// Read from the node's installed generator configuration (`protocol.pix`) and nothing else, which
/// is the only source that can be right. The Exit is told these dimensions at Session start, and
/// the node checks the announcement against the generator that will actually produce the shares —
/// a Session whose two disagree is refused, so a quota a *caller* states is a quota that can go
/// stale. There is nothing to state here.
///
/// Mirrors what `hopr-transport` does when it builds that generator, step for step, because the two
/// have to agree:
///
/// - the configuration is validated first, so one the node would reject at startup is rejected here
///   too rather than becoming a plausible-looking quota;
/// - the three dimensions are narrowed with a checked conversion rather than an `as` cast, so a
///   future widening of an upstream range surfaces as an error instead of a silently truncated but
///   valid-looking dimension;
/// - the surplus is read through `PixGlobalConfig::surplus_shares`, never off the `additional_shares`
///   field. That field is `Option` because serde cannot default to a function of a sibling, so
///   reading it directly is how an unset surplus silently becomes zero — no loss tolerance at all,
///   and a quota the Exit will not recognise;
/// - the suite comes from `HoprPixSpec`, so what is announced and what produces the shares are the
///   same build-time choice.
#[cfg(feature = "pix-secp256k1")]
pub fn pix_ssa_quota(cfg: &hopr_lib::config::HoprLibConfig) -> anyhow::Result<hopr_lib::PixParams> {
    use hopr_lib::exports::transport::HoprPixSpec;

    let pix = &cfg.protocol.pix;
    validator::Validate::validate(pix)
        .map_err(|error| anyhow::anyhow!("invalid PIX configuration: {error}"))?;

    fn narrow<T: TryFrom<usize>>(value: usize, field: &str) -> anyhow::Result<T> {
        T::try_from(value).map_err(|_| anyhow::anyhow!("PIX {field} out of range: {value}"))
    }

    hopr_lib::PixParams::try_new_for::<HoprPixSpec>(
        narrow(pix.num_ssa_parts, "num_ssa_parts")?,
        narrow(pix.ssa_part_size, "ssa_part_size")?,
        narrow(pix.surplus_shares(), "additional_shares")?,
    )
    .map_err(|error| anyhow::anyhow!("invalid PIX dimensions: {error}"))
}

/// Bytes of Exit → Entry traffic a single SSA deposit buys.
///
/// `polys_per_ssa × emitted_shares_per_poly × PACKET_PAYLOAD_SIZE`, matching what the Exit computes
/// when it decides whether the offered quota is one it accepts. Multiply by
/// [`PixEntryConfig::price_per_byte`] for the wxHOPR a single deposit costs, and by the number of
/// SSA cycles a Session is expected to run for the float that Session needs.
///
/// Counts `emitted_shares_per_poly` — threshold *plus* surplus — rather than the threshold alone,
/// and reads it off [`PixParams`] rather than re-deriving it. A polynomial leaves the generator's
/// queue having emitted the surplus whether or not any share was lost, so it is service the Exit
/// performs in every case and is charged for on purchase rather than on claim. Sizing against the
/// threshold alone underpays by the surplus factor — at the shipped 1.25×, a fifth of all
/// Exit → Entry traffic.
#[cfg(feature = "pix-secp256k1")]
pub fn quota_per_ssa(params: &hopr_lib::PixParams) -> u64 {
    u64::from(params.polys_per_ssa())
        * u64::from(params.emitted_shares_per_poly())
        * hopr_lib::exports::transport::PACKET_PAYLOAD_SIZE as u64
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
    /// Becomes the strategy's initial capacity as given, honoured verbatim — no rounding,
    /// no floor.
    ///
    /// # Funding is all-or-nothing
    ///
    /// Raising this also raises the balance below which the node refuses to operate. Unless
    /// [`min_safe_capacity_required`](Self::min_safe_capacity_required) is set explicitly,
    /// the safe gate is the larger of [`MIN_SAFE_MULTIPLE`] × this volume and the strategy's
    /// own default, so a small request does not lower it. Under `stop_when_unfunded`, a
    /// Safe below that gate opens **zero** channels, not smaller ones and not fewer — read
    /// the figure off [`minimum_balance_recommendation`] rather than deriving it here.
    ///
    /// Default: `None` — the strategy's own initial capacity.
    #[default(None)]
    pub channel_capacity: Option<ByteSize>,

    /// Data volume added to a channel's stake on top-up. Default: `None` — the strategy's own.
    #[default(None)]
    pub topup_capacity: Option<ByteSize>,

    /// Channel balance (as data capacity) below which a top-up fires. Default: `None` — the strategy's own.
    #[default(None)]
    pub lower_capacity_threshold: Option<ByteSize>,

    /// Minimum safe balance (as data capacity) before opening/funding any channel. Set
    /// explicitly to opt out of the [`MIN_SAFE_MULTIPLE`] × [`channel_capacity`](Self::channel_capacity)
    /// floor. Default: `None` — the derived floor.
    #[default(None)]
    pub min_safe_capacity_required: Option<ByteSize>,

    /// How each capacity field above converts to a wxHOPR stake. Feeds both the reactor and
    /// the balance recommendation via [`compute_funding_config`], so they can't disagree.
    /// Default: `None` — [`SIZING_MODE`].
    #[default(None)]
    pub sizing_mode: Option<CapacitySizingMode>,
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

/// [`FundingConfig`] for the sizing fields on `cfg`: each is passed through verbatim, `None`
/// keeps the strategy's default, and an unset `min_safe_capacity_required` gets the
/// [`MIN_SAFE_MULTIPLE`] floor instead. [`resolve_funding`] converts the result to wxHOPR.
pub fn compute_funding_config(cfg: &IncentiveConfiguration) -> anyhow::Result<FundingConfig> {
    let defaults = FundingConfig::default();
    let initial_capacity = cfg.channel_capacity.unwrap_or(defaults.initial_capacity);

    let min_safe_capacity_required = match cfg.min_safe_capacity_required {
        Some(explicit) => explicit,
        None => {
            let floor = initial_capacity
                .as_u64()
                .checked_mul(MIN_SAFE_MULTIPLE)
                .ok_or_else(|| {
                    anyhow::anyhow!("{initial_capacity} × {MIN_SAFE_MULTIPLE} overflows u64")
                })?;
            ByteSize::b(floor).max(defaults.min_safe_capacity_required)
        }
    };

    Ok(FundingConfig {
        initial_capacity,
        topup_capacity: cfg.topup_capacity.unwrap_or(defaults.topup_capacity),
        lower_capacity_threshold: cfg
            .lower_capacity_threshold
            .unwrap_or(defaults.lower_capacity_threshold),
        min_safe_capacity_required,
        stop_when_unfunded: true,
        sizing_mode: cfg.sizing_mode.clone().unwrap_or(SIZING_MODE),
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
    sizing: &IncentiveConfiguration,
    missing_channels: usize,
) -> anyhow::Result<HoprBalance> {
    let funding = compute_funding_config(sizing)?;
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

/// Recommended total xDai amount to fund the node with for gas: 0.005 xDai.
///
/// A hardcoded funding target covering all setup transactions plus headroom for
/// the larger Safe + module deployment, distinct from
/// [`BalanceRecommendation::xdai_fee_per_tx`] (the per-transaction fee, which
/// depends on the chain's current gas price). The figure halves upstream's
/// `hopr_lib::SUGGESTED_NATIVE_BALANCE` (0.01 xDai), which suggests roughly
/// double what setup transactions need on Gnosis Chain.
pub fn suggested_xdai_fund_amount() -> XDaiBalance {
    XDaiBalance::from(5_000_000_000_000_000_u64) // 0.005 xDai in wei
}

/// Maximum xDai one transaction can cost at the given chain `max_fee_per_gas`
/// (wei per gas).
///
/// The gas limit is the one `hopr-chain-connector` signs every transaction with
/// (`GasEstimation::default().gas_limit`), so this is the EIP-1559 ceiling the
/// sender authorises — *not* expected spend, which is `gas_used × effective
/// price` and on Gnosis Chain is orders of magnitude lower. Saturates rather
/// than overflowing on an absurd reported gas price. Obtain `max_fee_per_gas`
/// from the chain via `crate::blokli::query_max_fee_per_gas`.
pub fn xdai_fee_per_tx(max_fee_per_gas: u128) -> XDaiBalance {
    let gas_limit = hopr_lib::api::types::chain::payload::GasEstimation::default().gas_limit;
    XDaiBalance::from(max_fee_per_gas.saturating_mul(gas_limit as u128))
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
    /// Maximum xDAI fee per transaction (gas): the gas limit the connector signs
    /// transactions with, times the chain's current `max_fee_per_gas`, so this
    /// tracks the chain and its congestion.
    ///
    /// A ceiling on what one transaction may cost, not expected spend — see
    /// [`xdai_fee_per_tx`]. Use [`xdai_fund_amount`](Self::xdai_fund_amount) for
    /// what to actually fund the node with.
    pub xdai_fee_per_tx: XDaiBalance,
    /// Total xDAI to fund the node with for gas
    /// (fixed at [`suggested_xdai_fund_amount`]).
    pub xdai_fund_amount: XDaiBalance,
}

impl BalanceRecommendation {
    /// Total wxHOPR to fund: channel stakes plus the fee to start.
    pub fn total_wxhopr(&self) -> HoprBalance {
        self.channel_stakes + self.fee_to_start
    }
}

/// Data-throughput capacities of every wxHOPR stake the node can draw on,
/// returned by [`crate::client::Edgli::describe_current_capacity_allocations`].
#[derive(Clone, Debug)]
pub struct CapacityAllocations {
    /// Open outgoing payment channels, keyed by destination peer.
    pub peer_allocations: std::collections::HashMap<Address, Capacity>,
    /// wxHOPR on the node EOA, not yet swept into the Safe.
    pub node: Capacity,
    /// The unallocated wxHOPR balance held in the user's Safe contract.
    pub safe: Capacity,
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
///
/// `max_fee_per_gas` is the chain's current EIP-1559 gas price in wei per gas,
/// from which the per-transaction xDai fee is derived; see [`xdai_fee_per_tx`].
pub(crate) fn compute_balance_recommendation(
    ticket_price: HoprBalance,
    win_prob: f64,
    missing_channels: usize,
    costs: StartupCosts,
    sizing: &IncentiveConfiguration,
    max_fee_per_gas: u128,
) -> anyhow::Result<BalanceRecommendation> {
    let stake = if missing_channels == 0 {
        HoprBalance::zero()
    } else {
        channel_stakes(ticket_price, win_prob, sizing, missing_channels)?
    };
    Ok(BalanceRecommendation {
        channel_stakes: stake,
        fee_to_start: costs.fee_to_start,
        txs_to_start: costs.txs_to_start,
        xdai_fee_per_tx: xdai_fee_per_tx(max_fee_per_gas),
        xdai_fund_amount: suggested_xdai_fund_amount(),
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
    let max_fee_per_gas = incentive_ops.max_fee_per_gas().await?;
    compute_balance_recommendation(
        stats.ticket_price,
        win_prob,
        cfg.target_open_channels,
        costs,
        cfg,
        max_fee_per_gas,
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
    let funding = compute_funding_config(sizing)?;
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
        strategies: vec![EdgeStrategyKind::ChannelLifecycle(Box::new(cfg))],
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

    /// Gnosis-typical gas price in wei per gas (2 Gwei).
    const TEST_MAX_FEE_PER_GAS: u128 = 2_000_000_000;

    /// Winning probabilities spanning the range the network may run at.
    const WIN_PROBS: [f64; 6] = [1.0, 0.5, 0.1, 0.01, 0.001, 0.0001];

    #[test]
    fn funding_config_uses_the_requested_capacity_verbatim() {
        // The requested volume is the only field taken from the caller, and it is passed
        // through as-is: no rounding, no floor, no conversion.
        let requested = ByteSize::mb(100);
        let cfg = compute_funding_config(&IncentiveConfiguration {
            channel_capacity: Some(requested),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(cfg.initial_capacity, requested);
    }

    #[test]
    fn funding_config_falls_back_to_the_strategy_default_capacity() {
        let cfg = compute_funding_config(&IncentiveConfiguration::default()).unwrap();
        assert_eq!(
            cfg.initial_capacity,
            FundingConfig::default().initial_capacity
        );
    }

    #[test]
    fn funding_config_defers_topup_and_lower_threshold_to_the_strategy_when_unset() {
        // Unset case only — see funding_config_honours_explicit_topup_and_lower_threshold_overrides for the override.
        let defaults = FundingConfig::default();
        for requested in [None, Some(ByteSize::mb(50)), Some(ByteSize::gib(4))] {
            let cfg = compute_funding_config(&IncentiveConfiguration {
                channel_capacity: requested,
                ..Default::default()
            })
            .unwrap();
            assert_eq!(cfg.topup_capacity, defaults.topup_capacity, "{requested:?}");
            assert_eq!(
                cfg.lower_capacity_threshold, defaults.lower_capacity_threshold,
                "{requested:?}"
            );
        }
    }

    #[test]
    fn funding_config_honours_explicit_topup_and_lower_threshold_overrides() {
        let topup = ByteSize::mib(384);
        let lower = ByteSize::mib(128);
        let cfg = compute_funding_config(&IncentiveConfiguration {
            topup_capacity: Some(topup),
            lower_capacity_threshold: Some(lower),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(cfg.topup_capacity, topup);
        assert_eq!(cfg.lower_capacity_threshold, lower);
    }

    #[test]
    fn funding_config_honours_an_explicit_min_safe_capacity_verbatim() {
        // Not raised to 2x initial_capacity like the derived floor — see
        // funding_config_min_safe_covers_at_least_two_channels for that case.
        let cfg = compute_funding_config(&IncentiveConfiguration {
            channel_capacity: Some(ByteSize::mib(640)),
            min_safe_capacity_required: Some(ByteSize::mib(640)),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(cfg.min_safe_capacity_required, ByteSize::mib(640));
    }

    #[test]
    fn funding_config_honours_an_explicit_sizing_mode() {
        // The default (None) case is pinned by funding_config_uses_probabilistic_sizing.
        let cfg = compute_funding_config(&IncentiveConfiguration {
            sizing_mode: Some(CapacitySizingMode::Deterministic),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(cfg.sizing_mode, CapacitySizingMode::Deterministic);
    }

    #[test]
    fn funding_config_explicit_deterministic_mode_drops_the_variance_buffer() {
        // Proves the override reaches resolve_funding, not just FundingConfig's stored enum.
        let price = HoprBalance::new_base(10);
        let probabilistic = compute_funding_config(&IncentiveConfiguration::default()).unwrap();
        let deterministic = compute_funding_config(&IncentiveConfiguration {
            sizing_mode: Some(CapacitySizingMode::Deterministic),
            ..Default::default()
        })
        .unwrap();
        for p in [0.5, 0.1, 0.01, 0.001] {
            let probabilistic_stake = resolve_funding(&probabilistic, price, p).initial_balance;
            let deterministic_stake = resolve_funding(&deterministic, price, p).initial_balance;
            assert!(
                deterministic_stake < probabilistic_stake,
                "p={p}: deterministic {deterministic_stake} should be strictly below \
                 probabilistic {probabilistic_stake}"
            );
        }
    }

    #[test]
    fn funding_config_min_safe_covers_at_least_two_channels() {
        // The strategy's default gate is a fixed volume that does not track the request,
        // so a larger request would otherwise clear it on a safe that cannot then top the
        // channel up.
        for requested in [None, Some(ByteSize::mb(1)), Some(ByteSize::gib(4))] {
            let cfg = compute_funding_config(&IncentiveConfiguration {
                channel_capacity: requested,
                ..Default::default()
            })
            .unwrap();
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
        assert!(
            compute_funding_config(&IncentiveConfiguration {
                channel_capacity: Some(ByteSize::b(u64::MAX)),
                ..Default::default()
            })
            .is_err()
        );
    }

    #[test]
    fn funding_config_explicit_min_safe_capacity_skips_the_overflow_prone_floor() {
        // Must bypass the overflow-prone MIN_SAFE_MULTIPLE multiply entirely, not dodge it by luck.
        let cfg = compute_funding_config(&IncentiveConfiguration {
            channel_capacity: Some(ByteSize::b(u64::MAX)),
            min_safe_capacity_required: Some(ByteSize::mib(1)),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(cfg.min_safe_capacity_required, ByteSize::mib(1));
    }

    #[test]
    fn funding_config_uses_probabilistic_sizing() {
        // `resolve_funding` asks the strategy to resolve whichever mode is set here, so
        // this pins the mode itself rather than any restatement of its arithmetic.
        let cfg = compute_funding_config(&IncentiveConfiguration::default()).unwrap();
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
        let cfg = compute_funding_config(&IncentiveConfiguration::default()).unwrap();
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
        let cfg = compute_funding_config(&IncentiveConfiguration {
            channel_capacity: Some(ByteSize::kb(10)),
            ..Default::default()
        })
        .unwrap();
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
                let cfg = compute_funding_config(&IncentiveConfiguration::default()).unwrap();
                let resolved = resolve_funding(&cfg, price, p);

                let one = compute_balance_recommendation(
                    price,
                    p,
                    1,
                    no_startup_costs(),
                    &IncentiveConfiguration::default(),
                    TEST_MAX_FEE_PER_GAS,
                )
                .unwrap();
                assert_eq!(
                    one.channel_stakes, resolved.min_safe_balance_required,
                    "price={price}, p={p}"
                );

                let many = compute_balance_recommendation(
                    price,
                    p,
                    64,
                    no_startup_costs(),
                    &IncentiveConfiguration::default(),
                    TEST_MAX_FEE_PER_GAS,
                )
                .unwrap();
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
            let cfg = compute_funding_config(&IncentiveConfiguration::default()).unwrap();
            let min_safe = resolve_funding(&cfg, price, p).min_safe_balance_required;
            let rec = compute_balance_recommendation(
                price,
                p,
                1,
                no_startup_costs(),
                &IncentiveConfiguration::default(),
                TEST_MAX_FEE_PER_GAS,
            )
            .unwrap();
            assert!(rec.channel_stakes >= min_safe, "p={p}");
        }
    }

    #[test]
    fn balance_recommendation_tracks_requested_capacity() {
        // Regression: the recommendation is derived from the same funding config the
        // reactor runs on. If it ignored `channel_capacity` a node funded to the reported
        // figure could never open a channel at the requested volume.
        let price = HoprBalance::new_base(10);
        let requested_cfg = IncentiveConfiguration {
            channel_capacity: Some(ByteSize::gib(4)),
            ..Default::default()
        };
        for p in [1.0, 0.01, 0.001] {
            let cfg = compute_funding_config(&requested_cfg).unwrap();
            let expected = resolve_funding(&cfg, price, p).min_safe_balance_required;
            let rec = compute_balance_recommendation(
                price,
                p,
                1,
                no_startup_costs(),
                &requested_cfg,
                TEST_MAX_FEE_PER_GAS,
            )
            .unwrap();
            assert_eq!(rec.channel_stakes, expected, "p={p}");

            let default = compute_balance_recommendation(
                price,
                p,
                1,
                no_startup_costs(),
                &IncentiveConfiguration::default(),
                TEST_MAX_FEE_PER_GAS,
            )
            .unwrap();
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
    fn incentive_configuration_default_sizing_overrides_are_none() {
        let cfg = IncentiveConfiguration::default();
        assert!(cfg.topup_capacity.is_none());
        assert!(cfg.lower_capacity_threshold.is_none());
        assert!(cfg.min_safe_capacity_required.is_none());
        assert!(cfg.sizing_mode.is_none());
    }

    #[test]
    fn compute_funding_config_default_sizing_has_one_strategy_shape() {
        // Smoke-test: can build a MultiStrategyConfig from compute_funding_config output
        let funding = compute_funding_config(&IncentiveConfiguration::default()).unwrap();
        let lifecycle_cfg = ChannelLifecycleConfig {
            funding,
            ..Default::default()
        };
        let cfg = MultiStrategyConfig {
            strategies: vec![EdgeStrategyKind::ChannelLifecycle(Box::new(lifecycle_cfg))],
        };
        assert_eq!(cfg.strategies.len(), 1);
        assert!(matches!(
            cfg.strategies[0],
            EdgeStrategyKind::ChannelLifecycle(_)
        ));
    }

    #[test]
    fn default_strategy_cfg_carries_every_sizing_override_through_to_the_reactor() {
        // default_strategy_cfg is what reactor callers use — prove it wires overrides through too.
        let sizing = IncentiveConfiguration {
            channel_capacity: Some(ByteSize::mib(640)),
            topup_capacity: Some(ByteSize::mib(384)),
            lower_capacity_threshold: Some(ByteSize::mib(128)),
            min_safe_capacity_required: Some(ByteSize::mib(640)),
            sizing_mode: Some(CapacitySizingMode::Deterministic),
            ..Default::default()
        };
        let cfg = default_strategy_cfg(&sizing).unwrap();
        // A `match` rather than a `let`-`else`: the wildcard arm only exists when the enum has a
        // second variant, and a `let`-`else` would have an unreachable `else` in the build where
        // it does not.
        let funding = match &cfg.strategies[0] {
            EdgeStrategyKind::ChannelLifecycle(lifecycle_cfg) => &lifecycle_cfg.funding,
            #[cfg(feature = "pix-secp256k1")]
            _ => panic!("default_strategy_cfg must yield a ChannelLifecycle strategy"),
        };
        assert_eq!(funding.initial_capacity, ByteSize::mib(640));
        assert_eq!(funding.topup_capacity, ByteSize::mib(384));
        assert_eq!(funding.lower_capacity_threshold, ByteSize::mib(128));
        assert_eq!(funding.min_safe_capacity_required, ByteSize::mib(640));
        assert_eq!(funding.sizing_mode, CapacitySizingMode::Deterministic);
    }

    #[test]
    fn channel_sizing_defaults_match_population_config_defaults() {
        let sizing = IncentiveConfiguration::default();
        let population = PopulationConfig::default();
        assert_eq!(sizing.min_open_channels, population.min_open_channels);
        assert_eq!(sizing.target_open_channels, population.target_open_channels);
    }

    #[test]
    fn suggested_xdai_fund_amount_is_0_005_xdai() {
        assert_eq!(suggested_xdai_fund_amount(), "0.005 xdai".parse().unwrap());
    }

    #[test]
    fn xdai_fee_per_tx_scales_with_gas_price() {
        let base = xdai_fee_per_tx(TEST_MAX_FEE_PER_GAS);
        assert_eq!(xdai_fee_per_tx(TEST_MAX_FEE_PER_GAS * 2), base * 2u64);
        assert_eq!(xdai_fee_per_tx(0), XDaiBalance::zero());
    }

    #[test]
    fn xdai_fee_per_tx_is_the_signed_gas_limit_times_price() {
        // Read the limit off GasEstimation rather than restating 10_000_000, so an
        // upstream change to what the connector signs with fails this loudly
        // instead of being masked by a hardcoded copy.
        let gas_limit = hopr_lib::api::types::chain::payload::GasEstimation::default().gas_limit;
        let expected = XDaiBalance::from(TEST_MAX_FEE_PER_GAS * gas_limit as u128);
        assert_eq!(xdai_fee_per_tx(TEST_MAX_FEE_PER_GAS), expected);
        // 10M gas at 2 Gwei, today's upstream default.
        assert_eq!(
            xdai_fee_per_tx(TEST_MAX_FEE_PER_GAS),
            "0.02 xdai".parse().unwrap()
        );
    }

    #[test]
    fn xdai_fee_per_tx_is_a_ceiling_not_expected_spend() {
        // Deliberately above the fund amount, and not the defect this field used
        // to have: it is the EIP-1559 maximum the sender authorises (the signed
        // gas limit x max_fee_per_gas), while xdai_fund_amount is what a node
        // actually needs. Actual spend is gas_used x effective price, far lower.
        // Previously the field held hopr_lib::SUGGESTED_NATIVE_BALANCE — a *total*
        // funding suggestion misapplied per transaction, arbitrary in either role.
        assert!(xdai_fee_per_tx(TEST_MAX_FEE_PER_GAS) > suggested_xdai_fund_amount());
    }

    #[test]
    fn xdai_fee_per_tx_saturates_on_absurd_gas_price() {
        // An absurd reported gas price must clamp, not overflow.
        let _ = xdai_fee_per_tx(u128::MAX);
    }

    #[test]
    fn compute_balance_recommendation_zero_missing_returns_zero_wxhopr() {
        let rec = compute_balance_recommendation(
            HoprBalance::new_base(10),
            1.0,
            0,
            no_startup_costs(),
            &IncentiveConfiguration::default(),
            TEST_MAX_FEE_PER_GAS,
        )
        .unwrap();
        assert_eq!(rec.total_wxhopr(), HoprBalance::zero());
        assert_eq!(rec.xdai_fee_per_tx, xdai_fee_per_tx(TEST_MAX_FEE_PER_GAS));
        assert_eq!(rec.xdai_fund_amount, suggested_xdai_fund_amount());
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
            &IncentiveConfiguration::default(),
            TEST_MAX_FEE_PER_GAS,
        )
        .unwrap();
        let per_channel = resolve_funding(
            &compute_funding_config(&IncentiveConfiguration::default()).unwrap(),
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
            &IncentiveConfiguration::default(),
            TEST_MAX_FEE_PER_GAS,
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
            &IncentiveConfiguration::default(),
            TEST_MAX_FEE_PER_GAS,
        )
        .unwrap();
        let per_channel = resolve_funding(
            &compute_funding_config(&IncentiveConfiguration::default()).unwrap(),
            HoprBalance::new_base(10),
            1.0,
        )
        .initial_balance;
        assert_eq!(rec.channel_stakes, per_channel * 8u64);
        assert_eq!(rec.fee_to_start, HoprBalance::zero());
        assert_eq!(rec.txs_to_start, 0);
        assert_eq!(rec.total_wxhopr(), per_channel * 8u64);
        assert_eq!(rec.xdai_fee_per_tx, xdai_fee_per_tx(TEST_MAX_FEE_PER_GAS));
        assert_eq!(rec.xdai_fund_amount, suggested_xdai_fund_amount());
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
            &IncentiveConfiguration::default(),
            TEST_MAX_FEE_PER_GAS,
        )
        .unwrap();
        let half = compute_balance_recommendation(
            HoprBalance::new_base(10),
            0.5,
            1,
            no_startup_costs(),
            &IncentiveConfiguration::default(),
            TEST_MAX_FEE_PER_GAS,
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
            &IncentiveConfiguration::default(),
            TEST_MAX_FEE_PER_GAS,
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
    fn compute_capacity_balance_below_one_message_drain() {
        // 5 is below the 10 × 3 a single message costs, so nothing is payable.
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

    /// Every default is taken from upstream rather than restated, so this asserts the wiring
    /// rather than five literals: a change to what PIX charges has to arrive here, and a
    /// hand-copied constant that stopped tracking it would show up as a failure.
    #[cfg(feature = "pix-secp256k1")]
    #[test]
    fn pix_entry_defaults_track_the_upstream_ones() {
        let strategy = hopr_strategy::pix::strategy::PixStrategyConfig::default();
        let pool = hopr_strategy::pix::secp256k1::PoolConfig::default();
        let cfg = PixEntryConfig::default();

        assert_eq!(cfg.price_per_byte, strategy.price_per_byte);
        assert_eq!(cfg.max_ssa_allocation, strategy.max_ssa_allocation);
        assert_eq!(cfg.deposit_buffer_period, strategy.deposit_buffer_period);
        assert_eq!(
            cfg.max_deposit_tracking_time,
            pool.max_deposit_tracking_time
        );
        assert_eq!(cfg.max_deposit_retries, pool.max_deposit_retries);
    }

    /// The debounce windows are the ones to watch: a bare `serde(default)` on them upstream once
    /// produced `Duration::default()` — 0 ns — rather than the 500 ms the type documents, which
    /// disables batching silently.
    #[cfg(feature = "pix-secp256k1")]
    #[test]
    fn pix_entry_overrides_reach_both_halves() {
        let cfg = PixEntryConfig {
            price_per_byte: "0.0001 wxHOPR".parse().unwrap(),
            max_ssa_allocation: "10 wxHOPR".parse().unwrap(),
            deposit_buffer_period: Duration::from_millis(250),
            max_deposit_tracking_time: Duration::from_secs(30),
            max_deposit_retries: 7,
        };

        let strategy = cfg.strategy_config();
        assert_eq!(strategy.price_per_byte, "0.0001 wxHOPR".parse().unwrap());
        assert_eq!(strategy.max_ssa_allocation, "10 wxHOPR".parse().unwrap());
        assert_eq!(strategy.deposit_buffer_period, Duration::from_millis(250));

        let pool = cfg.pool_config();
        assert_eq!(pool.max_deposit_tracking_time, Duration::from_secs(30));
        assert_eq!(pool.max_deposit_retries, 7);
    }

    /// The Exit-side fields an Entry never reaches must come out at their upstream defaults, and
    /// the recovery pair must come out *both* unset in particular: `PixStrategy::build_with_pool`
    /// refuses to build when exactly one of the two is given, so a half-filled pair would turn a
    /// field this node has no use for into a startup failure.
    #[cfg(feature = "pix-secp256k1")]
    #[test]
    fn pix_entry_leaves_the_exit_side_alone() {
        let strategy = PixEntryConfig::default().strategy_config();
        assert!(strategy.pix_recovery_db_path.is_none());
        assert!(strategy.pix_recovery_password_env.is_none());
        assert_eq!(
            strategy.withdrawal_buffer_period,
            hopr_strategy::pix::strategy::PixStrategyConfig::default().withdrawal_buffer_period
        );

        let pool = PixEntryConfig::default().pool_config();
        let upstream = hopr_strategy::pix::secp256k1::PoolConfig::default();
        assert_eq!(pool.gas_xdai_per_sweep, upstream.gas_xdai_per_sweep);
        assert_eq!(pool.max_sweep_retries, upstream.max_sweep_retries);
    }

    /// The quota is derived from the installed generator's three dimensions and this build's spec,
    /// so it is the same value the node will check the Session's announcement against.
    #[cfg(feature = "pix-secp256k1")]
    #[test]
    fn pix_ssa_quota_reads_the_nodes_own_dimensions() {
        use hopr_lib::exports::transport::{HoprPixSpec, PixSpec};

        let mut cfg = hopr_lib::config::HoprLibConfig::default();
        cfg.protocol.pix.num_ssa_parts = 64;
        cfg.protocol.pix.ssa_part_size = 16;
        cfg.protocol.pix.additional_shares = Some(4);

        let params = pix_ssa_quota(&cfg).unwrap();
        assert_eq!(params.polys_per_ssa(), 64);
        assert_eq!(params.shares_per_poly(), 16);
        assert_eq!(params.surplus_shares(), 4);
        // Not configurable — it is the build's spec, and announcing anything else would let the
        // Session proceed while emitting shares the Exit cannot read.
        assert_eq!(params.suite(), HoprPixSpec::PIX_SUITE);
        assert_eq!(params.suite(), hopr_lib::LOCAL_PIX_SUITE);
    }

    /// `additional_shares` is `Option` only because serde cannot default to a function of a
    /// sibling, so the unset case has to go through `surplus_shares()`. Reading the field directly
    /// would announce zero surplus — no loss tolerance at all, and a quota the Exit computes
    /// differently.
    #[cfg(feature = "pix-secp256k1")]
    #[test]
    fn pix_ssa_quota_derives_an_unset_surplus_rather_than_zeroing_it() {
        let mut cfg = hopr_lib::config::HoprLibConfig::default();
        cfg.protocol.pix.additional_shares = None;

        let params = pix_ssa_quota(&cfg).unwrap();
        assert!(
            params.surplus_shares() > 0,
            "an unset surplus must be derived from ssa_part_size, not read as zero"
        );
        assert_eq!(
            u64::from(params.surplus_shares()),
            cfg.protocol.pix.surplus_shares() as u64,
            "the derived surplus must be the one the generator will use"
        );
    }

    /// A configuration the node would refuse at startup has to be refused here too. Rejecting it
    /// rather than narrowing it is the point: `ssa_part_size` below the validator's minimum of 2
    /// still fits a `u8`, so a cast would produce a dimension that looks valid and is not.
    #[cfg(feature = "pix-secp256k1")]
    #[test]
    fn pix_ssa_quota_rejects_a_config_the_node_would_reject() {
        let mut cfg = hopr_lib::config::HoprLibConfig::default();
        cfg.protocol.pix.ssa_part_size = 1;
        assert!(pix_ssa_quota(&cfg).is_err());

        // Above `u8::MAX` and above the validator's own maximum: caught rather than truncated to
        // a plausible 44.
        let mut cfg = hopr_lib::config::HoprLibConfig::default();
        cfg.protocol.pix.ssa_part_size = 300;
        assert!(pix_ssa_quota(&cfg).is_err());
    }

    /// The surplus is inside the quota, because a cycle emits it whether or not a share is lost.
    /// Leaving it out is what let an Entry take a fifth of the Exit → Entry traffic unbilled.
    #[cfg(feature = "pix-secp256k1")]
    #[test]
    fn quota_per_ssa_prices_the_surplus() {
        let payload = hopr_lib::exports::transport::PACKET_PAYLOAD_SIZE as u64;

        let bare =
            hopr_lib::PixParams::try_new_for::<hopr_lib::exports::transport::HoprPixSpec>(8, 4, 0)
                .unwrap();
        let insured =
            hopr_lib::PixParams::try_new_for::<hopr_lib::exports::transport::HoprPixSpec>(8, 4, 2)
                .unwrap();

        assert_eq!(quota_per_ssa(&bare), 8 * 4 * payload);
        assert_eq!(quota_per_ssa(&insured), 8 * (4 + 2) * payload);
        assert!(
            quota_per_ssa(&insured) > quota_per_ssa(&bare),
            "a surplus the Exit delivers must be a surplus the Entry pays for"
        );
    }
}
