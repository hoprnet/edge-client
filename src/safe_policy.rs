//! Shares the Safe between strategies by shaping what each one sees of the chain.
//!
//! Each strategy assumes the whole Safe is its own. The channel-lifecycle strategy runs on a
//! [`PolicyNode`], which hides a [`SafeReserve`] from it; the PIX strategy runs on the plain node,
//! so its pool refill has first claim on that part.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use futures::FutureExt;
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use hopr_lib::api::chain::{
    AccountEntry, AccountSelector, AnnouncementError, ChainEvent, ChainEvents, ChainInfo,
    ChainKeyOperations, ChainReadAccountOperations, ChainReadChannelOperations,
    ChainReadSafeOperations, ChainReadServiceOperations, ChainReadTicketOperations, ChainReceipt,
    ChainValues, ChainWriteAccountOperations, ChainWriteChannelOperations,
    ChainWriteSafeOperations, ChainWriteServiceOperations, ChainWriteTicketOperations,
    ChannelEntry, ChannelId, ChannelSelector, DeployedSafe, DomainSeparators, HoprChainApi,
    Multiaddr, RedeemableTicket, RedemptionStats, SafeRegistrationError, SafeSelector,
    ServiceEntry, ServiceMetadata, ServiceRegistryConfig, ServiceSelector, ServiceType,
    ServiceTypeConfig, StateSyncOptions, SubmissionFailure, TicketRedeemError, VerifiedTicket,
    WinningProbability,
};
use hopr_lib::api::node::{
    ActionableEvent, ActionableEventDiscriminant, ActionableEventSource, ComponentStatus,
    ComponentStatusReporter, EventWaitResult, HasChainApi, HasGraphView, HasNetworkView,
    NodeOnchainIdentity, PacketTransport,
};
use hopr_lib::api::types::chain::prelude::Erc777Payload;
use hopr_lib::api::types::crypto::prelude::{ChainKeypair, OffchainKeypair, OffchainPublicKey};
use hopr_lib::api::types::primitive::prelude::{Address, Balance, Currency, HoprBalance, WxHOPR};
use tokio::sync::watch;

use crate::reclaim::LifecycleMode;

/// The Safe wxHOPR held back from the channel-lifecycle strategy for the next PIX pool refill.
///
/// Clones share one value. Zero by default, which leaves the strategy the whole Safe.
#[derive(Clone, Debug, Default)]
pub struct SafeReserve(Arc<Mutex<HoprBalance>>);

impl SafeReserve {
    pub fn get(&self) -> HoprBalance {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Returns the previous value.
    #[cfg_attr(
        not(feature = "pix-curvy"),
        allow(dead_code, reason = "only a Curvy PIX pool claims a reserve")
    )]
    pub(crate) fn set(&self, reserved: HoprBalance) -> HoprBalance {
        std::mem::replace(
            &mut self.0.lock().unwrap_or_else(PoisonError::into_inner),
            reserved,
        )
    }
}

/// An open or top-up refused because it would spend the Safe's reserve.
#[derive(Debug, thiserror::Error)]
#[error("{SAFE_RESERVED}")]
pub(crate) struct SafeReserved;

const SAFE_RESERVED: &str = "the Safe's wxHOPR is reserved for the PIX pool refill";

impl From<SafeReserved> for hopr_chain_connector::errors::ConnectorError {
    fn from(_: SafeReserved) -> Self {
        Self::InvalidState(SAFE_RESERVED)
    }
}

/// What the next refill of a pool holding `pool` claims of the Safe: nothing at `full`, what a
/// refill leaves in the pool, and the whole top-up once the pool is down to its refill floor.
#[cfg(feature = "pix-curvy")]
pub(crate) fn reserve_for(
    pool: HoprBalance,
    full: HoprBalance,
    policy: &crate::strategy::PixFundingPolicy,
) -> HoprBalance {
    let top_up = policy.top_up.amount();
    let full = full.amount().min(top_up);
    let pool = pool.amount();
    if pool >= full {
        return HoprBalance::zero();
    }
    let span = full.saturating_sub(policy.min_available.amount());
    if span.is_zero() {
        return policy.top_up;
    }
    HoprBalance::from((top_up.saturating_mul(full - pool) / span).min(top_up))
}

/// What a refill between two pool reads left in the pool, net of the shield fee; a refill is a
/// rise by over half a top-up.
#[cfg(feature = "pix-curvy")]
fn refilled_to(
    before: HoprBalance,
    after: HoprBalance,
    policy: &crate::strategy::PixFundingPolicy,
) -> Option<HoprBalance> {
    let top_up = policy.top_up.amount();
    let rise = after.amount().checked_sub(before.amount())?;
    (rise > top_up / 2).then(|| HoprBalance::from(after.amount().min(top_up)))
}

#[cfg(feature = "pix-curvy")]
const RESERVE_REFRESH: Duration = Duration::from_secs(15);

#[cfg(feature = "pix-curvy")]
fn pool_is_low(pool: HoprBalance, policy: &crate::strategy::PixFundingPolicy) -> bool {
    pool.amount() < (policy.top_up.amount() / 2).max(policy.min_available.amount())
}

/// Reserves a whole top-up at once; the returned future then keeps `reserve` at [`reserve_for`]
/// the pool's balance and, while the pool is below half a top-up or its refill floor, sets `mode` to
/// [`next_mode`](crate::reclaim::next_mode) for what `safe_view` reports. After a reclaim the whole
/// top-up stays reserved until the pool is refilled, across a restart too while its stake is still
/// closing. Never completes.
#[cfg(feature = "pix-curvy")]
pub(crate) fn run_safe_policy<P, PFut, PErr, V, VFut, VErr>(
    pool_balance: P,
    safe_view: V,
    policy: crate::strategy::PixFundingPolicy,
    reserve: SafeReserve,
    mode: watch::Sender<LifecycleMode>,
) -> impl Future<Output = ()>
where
    P: Fn() -> PFut,
    PFut: Future<Output = Result<HoprBalance, PErr>>,
    PErr: std::fmt::Display,
    V: Fn() -> VFut,
    VFut: Future<Output = Result<crate::reclaim::SafeView, VErr>>,
    VErr: std::fmt::Display,
{
    use crate::reclaim::next_mode;

    // The channel strategy reads the Safe as soon as it starts, before the pool has been read.
    reserve.set(policy.top_up);
    async move {
        let mut ticks = tokio::time::interval(RESERVE_REFRESH);
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // Channels closed for a whole top-up: all of it stays reserved until the refill.
        let mut reclaimed = false;
        // A restart forgets `reclaimed`: stake closing under a low pool is taken as a reclaim's.
        let mut recovering = true;
        // The fee is unknown until a shield is seen, so a pool counts as full at a whole top-up.
        let mut full = policy.top_up;
        let mut last_pool: Option<HoprBalance> = None;
        loop {
            ticks.tick().await;
            let pool = match pool_balance().await {
                Ok(pool) => pool,
                Err(error) => {
                    tracing::debug!(%error, "PIX pool balance unreadable; Safe policy unchanged");
                    continue;
                }
            };
            if let Some(refilled) = last_pool
                .replace(pool)
                .and_then(|last| refilled_to(last, pool, &policy))
            {
                full = refilled;
            }
            let low = pool_is_low(pool, &policy);
            reclaimed &= low;
            recovering &= low;
            let view = if low {
                safe_view()
                    .await
                    .inspect_err(|error| {
                        tracing::debug!(%error, "Safe or channels unreadable; channel mode unchanged");
                    })
                    .ok()
            } else {
                None
            };
            if recovering && let Some(view) = &view {
                reclaimed = !view.closing.is_zero();
                recovering = false;
            }
            let reserved = if reclaimed || recovering {
                policy.top_up
            } else {
                reserve_for(pool, full, &policy)
            };
            if reserve.set(reserved) != reserved {
                tracing::debug!(%pool, %reserved, "Safe reserve for the PIX pool updated");
            }

            let current = *mode.borrow();
            let next = match &view {
                Some(view) => next_mode(current, reserved, policy.top_up, view),
                None if low => current,
                None => LifecycleMode::Normal,
            };
            if next == current {
                continue;
            }
            if next == LifecycleMode::Normal {
                tracing::info!(%pool, ?current, "PIX refill covered; channels back to normal");
            } else {
                // A refill between the two reads debits the Safe that was read, not the pool that was.
                match pool_balance().await {
                    Ok(pool) if pool_is_low(pool, &policy) => {}
                    Ok(pool) => {
                        tracing::debug!(%pool, "PIX pool refilled meanwhile; no channel closed");
                        continue;
                    }
                    Err(error) => {
                        tracing::debug!(%error, "PIX pool balance unreadable; channel mode unchanged");
                        continue;
                    }
                }
                reclaimed = true;
                reserve.set(policy.top_up);
                if let Some(view) = &view {
                    tracing::info!(
                        %pool, %reserved, safe = %view.safe, closing = %view.closing,
                        open = view.open.len(), ?current, ?next,
                        "channel mode changed for the PIX pool refill"
                    );
                }
            }
            mode.send_replace(next);
        }
    }
}

/// What `safe` and the outgoing channels of `chain`'s node hold.
#[cfg(feature = "pix-curvy")]
pub(crate) async fn read_safe_view<C>(
    chain: &C,
    safe: Address,
) -> Result<crate::reclaim::SafeView, C::ChainError>
where
    C: HoprChainApi + Sync,
{
    use futures::StreamExt;
    use hopr_lib::api::chain::ChannelStatusDiscriminants;
    use hopr_lib::api::types::internal::channels::ChannelStatus;

    let balance: HoprBalance = chain.balance(safe).await?;
    let selector = ChannelSelector::default()
        .with_source(*chain.me())
        .with_allowed_states(&[
            ChannelStatusDiscriminants::Open,
            ChannelStatusDiscriminants::PendingToClose,
        ]);
    let channels: Vec<ChannelEntry> = chain.stream_channels(selector)?.collect().await;

    let mut view = crate::reclaim::SafeView {
        safe: balance,
        ..Default::default()
    };
    for channel in channels {
        match channel.status {
            ChannelStatus::Open => view.open.push(channel.balance),
            _ => view.closing += channel.balance,
        }
    }
    Ok(view)
}

#[derive(Clone, Copy, Debug, Default)]
struct Pending {
    writes: usize,
    spend: HoprBalance,
}

/// The channel writes sent through a [`PolicyChain`] and not yet confirmed or failed.
#[derive(Clone, Debug)]
pub(crate) struct ChannelWrites {
    pending: Arc<watch::Sender<Pending>>,
    admitting: Arc<tokio::sync::Mutex<()>>,
}

impl Default for ChannelWrites {
    fn default() -> Self {
        Self {
            pending: Arc::new(watch::Sender::new(Pending::default())),
            admitting: Arc::default(),
        }
    }
}

impl ChannelWrites {
    fn begin(&self, spend: HoprBalance) -> ChannelWrite {
        self.pending.send_modify(|pending| {
            pending.writes += 1;
            pending.spend += spend;
        });
        ChannelWrite {
            pending: Arc::clone(&self.pending),
            spend,
        }
    }

    /// Resolves once nothing is awaiting confirmation: the chain then shows every write sent.
    pub(crate) async fn settled(&self) {
        let _ = self
            .pending
            .subscribe()
            .wait_for(|pending| pending.writes == 0)
            .await;
    }
}

/// Counts as pending until dropped.
struct ChannelWrite {
    pending: Arc<watch::Sender<Pending>>,
    spend: HoprBalance,
}

impl ChannelWrite {
    fn until<'a, T: Send + 'a>(self, confirmation: BoxFuture<'a, T>) -> BoxFuture<'a, T> {
        async move {
            let _pending = self;
            confirmation.await
        }
        .boxed()
    }
}

impl Drop for ChannelWrite {
    fn drop(&mut self) {
        self.pending.send_modify(|pending| {
            pending.writes -= 1;
            pending.spend -= self.spend;
        });
    }
}

/// A chain API that hides `reserve` of the Safe's wxHOPR and refuses to spend it; while `mode`
/// reclaims channels the Safe reads as empty and nothing is spent.
#[derive(Clone)]
pub(crate) struct PolicyChain<C> {
    inner: C,
    safe: Address,
    reserve: SafeReserve,
    mode: watch::Receiver<LifecycleMode>,
    writes: ChannelWrites,
}

impl<C> PolicyChain<C> {
    pub(crate) fn new(
        inner: C,
        safe: Address,
        reserve: SafeReserve,
        mode: watch::Receiver<LifecycleMode>,
    ) -> Self {
        Self {
            inner,
            safe,
            reserve,
            mode,
            writes: ChannelWrites::default(),
        }
    }

    fn reclaiming(&self) -> bool {
        *self.mode.borrow() != LifecycleMode::Normal
    }
}

impl<C> PolicyChain<C>
where
    C: HoprChainApi + Send + Sync,
    C::ChainError: From<SafeReserved>,
{
    // One at a time, counting spends not yet confirmed: the Safe doesn't show those.
    async fn admit_spend(&self, amount: HoprBalance) -> Result<ChannelWrite, C::ChainError> {
        let _admitting = self.writes.admitting.lock().await;
        if self.reclaiming() {
            tracing::debug!(%amount, "{SAFE_RESERVED}: channels are being closed for it");
            return Err(SafeReserved.into());
        }
        if !self.reserve.get().is_zero() {
            let safe: HoprBalance = self.inner.balance(self.safe).await?;
            // Read after the Safe: both may have changed since the strategy read its balance.
            let reserved = self.reserve.get();
            let pending = self.writes.pending.borrow().spend;
            if safe < reserved + pending + amount {
                tracing::debug!(
                    %safe, %reserved, %pending, %amount,
                    "{SAFE_RESERVED}: channel spend refused"
                );
                return Err(SafeReserved.into());
            }
        }
        Ok(self.writes.begin(amount))
    }
}

impl<C: ComponentStatusReporter> ComponentStatusReporter for PolicyChain<C> {
    fn component_status(&self) -> ComponentStatus {
        self.inner.component_status()
    }
}

#[async_trait::async_trait]
impl<C> ChainValues for PolicyChain<C>
where
    C: HoprChainApi + Send + Sync,
{
    type Error = C::ChainError;

    async fn balance<Cur: Currency, A: Into<Address> + Send>(
        &self,
        address: A,
    ) -> Result<Balance<Cur>, Self::Error> {
        let address = address.into();
        let balance: Balance<Cur> = self.inner.balance(address).await?;
        if address == self.safe && Cur::is::<WxHOPR>() {
            if self.reclaiming() {
                return Ok(Balance::zero());
            }
            return Ok(balance - self.reserve.get().amount());
        }
        Ok(balance)
    }

    async fn domain_separators(&self) -> Result<DomainSeparators, Self::Error> {
        self.inner.domain_separators().await
    }

    async fn minimum_incoming_ticket_win_prob(&self) -> Result<WinningProbability, Self::Error> {
        self.inner.minimum_incoming_ticket_win_prob().await
    }

    async fn minimum_ticket_price(&self) -> Result<HoprBalance, Self::Error> {
        self.inner.minimum_ticket_price().await
    }

    async fn key_binding_fee(&self) -> Result<HoprBalance, Self::Error> {
        self.inner.key_binding_fee().await
    }

    async fn channel_closure_notice_period(&self) -> Result<Duration, Self::Error> {
        self.inner.channel_closure_notice_period().await
    }

    async fn chain_info(&self) -> Result<ChainInfo, Self::Error> {
        self.inner.chain_info().await
    }

    async fn redemption_stats<A: Into<Address> + Send>(
        &self,
        safe_addr: A,
    ) -> Result<RedemptionStats, Self::Error> {
        self.inner.redemption_stats(safe_addr).await
    }

    async fn typical_resolution_time(&self) -> Result<Duration, Self::Error> {
        self.inner.typical_resolution_time().await
    }
}

#[async_trait::async_trait]
impl<C> ChainWriteChannelOperations for PolicyChain<C>
where
    C: HoprChainApi + Send + Sync,
    C::ChainError: From<SafeReserved>,
{
    type Error = C::ChainError;

    async fn open_channel<'a>(
        &'a self,
        dst: &'a Address,
        amount: HoprBalance,
    ) -> Result<BoxFuture<'a, Result<ChainReceipt, Self::Error>>, Self::Error> {
        let write = self.admit_spend(amount).await?;
        Ok(write.until(self.inner.open_channel(dst, amount).await?))
    }

    async fn fund_channel<'a>(
        &'a self,
        channel_id: &'a ChannelId,
        amount: HoprBalance,
    ) -> Result<BoxFuture<'a, Result<ChainReceipt, Self::Error>>, Self::Error> {
        let write = self.admit_spend(amount).await?;
        Ok(write.until(self.inner.fund_channel(channel_id, amount).await?))
    }

    async fn close_channel<'a>(
        &'a self,
        channel_id: &'a ChannelId,
    ) -> Result<BoxFuture<'a, Result<ChainReceipt, Self::Error>>, Self::Error> {
        let write = self.writes.begin(HoprBalance::zero());
        Ok(write.until(self.inner.close_channel(channel_id).await?))
    }
}

impl<C> ChainReadChannelOperations for PolicyChain<C>
where
    C: HoprChainApi,
{
    type Error = C::ChainError;

    fn me(&self) -> &Address {
        self.inner.me()
    }

    fn channel_by_parties(
        &self,
        src: &Address,
        dst: &Address,
    ) -> Result<Option<ChannelEntry>, Self::Error> {
        self.inner.channel_by_parties(src, dst)
    }

    fn channel_by_id(&self, channel_id: &ChannelId) -> Result<Option<ChannelEntry>, Self::Error> {
        self.inner.channel_by_id(channel_id)
    }

    fn stream_channels<'a>(
        &'a self,
        selector: ChannelSelector,
    ) -> Result<BoxStream<'a, ChannelEntry>, Self::Error> {
        self.inner.stream_channels(selector)
    }
}

#[async_trait::async_trait]
impl<C> ChainReadSafeOperations for PolicyChain<C>
where
    C: HoprChainApi + Send + Sync,
{
    type Error = C::ChainError;

    async fn safe_allowance<Cur: Currency, A: Into<Address> + Send>(
        &self,
        safe_address: A,
    ) -> Result<Balance<Cur>, Self::Error> {
        self.inner.safe_allowance(safe_address).await
    }

    async fn safe_info(&self, selector: SafeSelector) -> Result<Option<DeployedSafe>, Self::Error> {
        self.inner.safe_info(selector).await
    }

    async fn await_safe_deployment(
        &self,
        selector: SafeSelector,
        timeout: Duration,
    ) -> Result<DeployedSafe, Self::Error> {
        self.inner.await_safe_deployment(selector, timeout).await
    }

    async fn predict_module_address(
        &self,
        nonce: u64,
        owner: &Address,
        safe_address: &Address,
    ) -> Result<Address, Self::Error> {
        self.inner
            .predict_module_address(nonce, owner, safe_address)
            .await
    }
}

#[async_trait::async_trait]
impl<C> ChainWriteSafeOperations for PolicyChain<C>
where
    C: HoprChainApi + Send + Sync,
{
    type Error = C::ChainError;

    async fn deploy_safe<'a>(
        &'a self,
        balance: HoprBalance,
    ) -> Result<BoxFuture<'a, Result<ChainReceipt, Self::Error>>, Self::Error> {
        self.inner.deploy_safe(balance).await
    }
}

#[async_trait::async_trait]
impl<C> ChainReadAccountOperations for PolicyChain<C>
where
    C: HoprChainApi + Send + Sync,
{
    type Error = C::ChainError;

    fn stream_accounts<'a>(
        &'a self,
        selector: AccountSelector,
    ) -> Result<BoxStream<'a, AccountEntry>, Self::Error> {
        self.inner.stream_accounts(selector)
    }

    async fn count_accounts(&self, selector: AccountSelector) -> Result<usize, Self::Error> {
        self.inner.count_accounts(selector).await
    }

    async fn await_key_binding(
        &self,
        offchain_key: &OffchainPublicKey,
        timeout: Duration,
    ) -> Result<AccountEntry, Self::Error> {
        self.inner.await_key_binding(offchain_key, timeout).await
    }
}

#[async_trait::async_trait]
impl<C> ChainWriteAccountOperations for PolicyChain<C>
where
    C: HoprChainApi + Send + Sync,
{
    type Error = C::ChainError;

    fn submission_failure(&self, error: &Self::Error) -> SubmissionFailure {
        self.inner.submission_failure(error)
    }

    async fn announce(
        &self,
        multiaddrs: &[Multiaddr],
        key: &OffchainKeypair,
    ) -> Result<BoxFuture<'life0, Result<ChainReceipt, Self::Error>>, AnnouncementError<Self::Error>>
    {
        self.inner.announce(multiaddrs, key).await
    }

    async fn withdraw<Cur: Currency + Send>(
        &self,
        balance: Balance<Cur>,
        recipient: &Address,
    ) -> Result<BoxFuture<'life0, Result<ChainReceipt, Self::Error>>, Self::Error> {
        self.inner.withdraw(balance, recipient).await
    }

    async fn send_erc777(
        &self,
        balance: HoprBalance,
        recipient: &Address,
        payload: &Erc777Payload,
    ) -> Result<BoxFuture<'life0, Result<ChainReceipt, Self::Error>>, Self::Error> {
        self.inner.send_erc777(balance, recipient, payload).await
    }

    async fn withdraw_from_signer<Cur: Currency + Send>(
        &self,
        signer: &ChainKeypair,
        balance: Balance<Cur>,
        recipient: &Address,
    ) -> Result<BoxFuture<'life0, Result<ChainReceipt, Self::Error>>, Self::Error> {
        self.inner
            .withdraw_from_signer(signer, balance, recipient)
            .await
    }

    async fn register_safe(
        &self,
        safe_address: &Address,
    ) -> Result<
        BoxFuture<'life0, Result<ChainReceipt, Self::Error>>,
        SafeRegistrationError<Self::Error>,
    > {
        self.inner.register_safe(safe_address).await
    }
}

#[async_trait::async_trait]
impl<C> ChainReadServiceOperations for PolicyChain<C>
where
    C: HoprChainApi + Send + Sync,
{
    type Error = C::ChainError;

    fn stream_services<'a>(
        &'a self,
        selector: ServiceSelector,
    ) -> Result<BoxStream<'a, ServiceEntry>, Self::Error> {
        self.inner.stream_services(selector)
    }

    async fn count_services(&self, selector: ServiceSelector) -> Result<usize, Self::Error> {
        self.inner.count_services(selector).await
    }

    async fn get_service_type_config(
        &self,
        service_type: ServiceType,
    ) -> Result<Option<ServiceTypeConfig>, Self::Error> {
        self.inner.get_service_type_config(service_type).await
    }

    async fn get_service_registry_config(&self) -> Result<ServiceRegistryConfig, Self::Error> {
        self.inner.get_service_registry_config().await
    }
}

#[async_trait::async_trait]
impl<C> ChainWriteServiceOperations for PolicyChain<C>
where
    C: HoprChainApi + Send + Sync,
{
    type Error = C::ChainError;

    async fn register_service<'a>(
        &'a self,
        service_type: ServiceType,
        metadata: ServiceMetadata,
    ) -> Result<BoxFuture<'a, Result<ChainReceipt, Self::Error>>, Self::Error> {
        self.inner.register_service(service_type, metadata).await
    }

    async fn update_service<'a>(
        &'a self,
        service_type: ServiceType,
        metadata: ServiceMetadata,
    ) -> Result<BoxFuture<'a, Result<ChainReceipt, Self::Error>>, Self::Error> {
        self.inner.update_service(service_type, metadata).await
    }

    async fn deregister_service<'a>(
        &'a self,
        service_type: ServiceType,
    ) -> Result<BoxFuture<'a, Result<ChainReceipt, Self::Error>>, Self::Error> {
        self.inner.deregister_service(service_type).await
    }
}

impl<C> ChainEvents for PolicyChain<C>
where
    C: HoprChainApi,
{
    type Error = C::ChainError;

    fn subscribe(
        &self,
    ) -> Result<impl futures::Stream<Item = ChainEvent> + Send + 'static, Self::Error> {
        self.inner.subscribe()
    }

    fn subscribe_with_state_sync<I: IntoIterator<Item = StateSyncOptions>>(
        &self,
        options: I,
    ) -> Result<impl futures::Stream<Item = ChainEvent> + Send + 'static, Self::Error> {
        self.inner.subscribe_with_state_sync(options)
    }
}

impl<C> ChainKeyOperations for PolicyChain<C>
where
    C: HoprChainApi,
{
    type Error = C::ChainError;
    type Mapper = <C as ChainKeyOperations>::Mapper;

    fn chain_key_to_packet_key(
        &self,
        chain: &Address,
    ) -> Result<Option<OffchainPublicKey>, Self::Error> {
        self.inner.chain_key_to_packet_key(chain)
    }

    fn packet_key_to_chain_key(
        &self,
        packet: &OffchainPublicKey,
    ) -> Result<Option<Address>, Self::Error> {
        self.inner.packet_key_to_chain_key(packet)
    }

    fn key_id_mapper_ref(&self) -> &Self::Mapper {
        self.inner.key_id_mapper_ref()
    }
}

impl<C> ChainReadTicketOperations for PolicyChain<C>
where
    C: HoprChainApi,
{
    type Error = C::ChainError;

    fn incoming_ticket_values(&self) -> Result<(WinningProbability, HoprBalance), Self::Error> {
        self.inner.incoming_ticket_values()
    }

    fn outgoing_ticket_values(
        &self,
        configured_wp: Option<WinningProbability>,
        configured_price: Option<HoprBalance>,
    ) -> Result<(WinningProbability, HoprBalance), Self::Error> {
        self.inner
            .outgoing_ticket_values(configured_wp, configured_price)
    }
}

#[async_trait::async_trait]
impl<C> ChainWriteTicketOperations for PolicyChain<C>
where
    C: HoprChainApi + Send + Sync,
{
    type Error = C::ChainError;

    async fn redeem_ticket<'a>(
        &'a self,
        ticket: RedeemableTicket,
    ) -> Result<
        BoxFuture<'a, Result<(VerifiedTicket, ChainReceipt), TicketRedeemError<Self::Error>>>,
        TicketRedeemError<Self::Error>,
    > {
        self.inner.redeem_ticket(ticket).await
    }
}

/// A node whose chain is a [`PolicyChain`] over the node's own; everything else is the node's.
pub(crate) struct PolicyNode<N: HasChainApi> {
    node: Arc<N>,
    chain: PolicyChain<N::ChainApi>,
}

impl<N: HasChainApi> PolicyNode<N> {
    pub(crate) fn new(
        node: Arc<N>,
        reserve: SafeReserve,
        mode: watch::Receiver<LifecycleMode>,
    ) -> Self {
        let chain = PolicyChain::new(
            node.chain_api().clone(),
            node.identity().safe_address,
            reserve,
            mode,
        );
        Self { node, chain }
    }

    pub(crate) fn channel_writes(&self) -> ChannelWrites {
        self.chain.writes.clone()
    }
}

impl<N> HasChainApi for PolicyNode<N>
where
    N: HasChainApi,
    <N::ChainApi as HoprChainApi>::ChainError: From<SafeReserved>,
{
    type ChainApi = PolicyChain<N::ChainApi>;
    type ChainError = N::ChainError;

    fn identity(&self) -> &NodeOnchainIdentity {
        self.node.identity()
    }

    fn chain_api(&self) -> &Self::ChainApi {
        &self.chain
    }

    fn status(&self) -> ComponentStatus {
        HasChainApi::status(&*self.node)
    }

    fn wait_for_on_chain_event<F>(
        &self,
        predicate: F,
        context: String,
        timeout: Duration,
    ) -> EventWaitResult<<Self::ChainApi as HoprChainApi>::ChainError, Self::ChainError>
    where
        F: Fn(&ChainEvent) -> bool + Send + Sync + 'static,
    {
        self.node
            .wait_for_on_chain_event(predicate, context, timeout)
    }
}

impl<N: HasChainApi + HasNetworkView> HasNetworkView for PolicyNode<N> {
    type NetworkView = N::NetworkView;

    fn network_view(&self) -> &Self::NetworkView {
        self.node.network_view()
    }

    fn status(&self) -> ComponentStatus {
        HasNetworkView::status(&*self.node)
    }
}

impl<N: HasChainApi + HasGraphView> HasGraphView for PolicyNode<N> {
    type Graph = N::Graph;

    fn graph(&self) -> &Self::Graph {
        self.node.graph()
    }

    fn status(&self) -> ComponentStatus {
        HasGraphView::status(&*self.node)
    }
}

impl<N: HasChainApi + ActionableEventSource> ActionableEventSource for PolicyNode<N> {
    fn subscribe_to_actionable_events(
        &self,
        filter: Option<&[ActionableEventDiscriminant]>,
    ) -> Result<BoxStream<'static, ActionableEvent>, String> {
        self.node.subscribe_to_actionable_events(filter)
    }
}

impl<N: HasChainApi + PacketTransport> PacketTransport for PolicyNode<N> {
    fn packet_payload_size() -> usize {
        N::packet_payload_size()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hopr_chain_connector::testing::{
        BlokliTestClient, BlokliTestStateBuilder, FullStateEmulator, TestChainConnector,
        TestConnectorError, create_test_blokli_connector,
    };
    use hopr_lib::api::types::crypto::prelude::Keypair;
    use hopr_lib::api::types::internal::prelude::{AccountType, generate_channel_id};
    use hopr_lib::api::types::primitive::prelude::{XDai, XDaiBalance};

    impl From<SafeReserved> for TestConnectorError {
        fn from(e: SafeReserved) -> Self {
            anyhow::Error::new(e).into()
        }
    }

    const SAFE: [u8; 20] = [0x5a; 20];
    const PEER: [u8; 20] = [0x22; 20];
    const PEER_SAFE: [u8; 20] = [0x2a; 20];
    const OTHER_PEER: [u8; 20] = [0x33; 20];
    const OTHER_PEER_SAFE: [u8; 20] = [0x3a; 20];
    const MODULE: [u8; 20] = [0x11; 20];

    type TestChain = PolicyChain<Arc<TestChainConnector<BlokliTestClient<FullStateEmulator>>>>;

    fn account(chain_addr: Address, safe: [u8; 20], key_id: u32) -> AccountEntry {
        AccountEntry {
            public_key: *OffchainKeypair::random().public(),
            chain_addr,
            entry_type: AccountType::NotAnnounced,
            safe_address: Some(safe.into()),
            key_id: key_id.into(),
        }
    }

    async fn chain_with_safe(
        safe_wxhopr: u64,
        reserved: u64,
    ) -> anyhow::Result<(TestChain, ChannelId)> {
        // A dropped sender leaves the mode at normal.
        let mode = watch::Sender::new(LifecycleMode::Normal).subscribe();
        chain_with_mode(safe_wxhopr, reserved, mode).await
    }

    /// The node's Safe and each peer's hold `safe_wxhopr`.
    async fn chain_with_mode(
        safe_wxhopr: u64,
        reserved: u64,
        mode: watch::Receiver<LifecycleMode>,
    ) -> anyhow::Result<(TestChain, ChannelId)> {
        let key = ChainKeypair::random();
        let me = key.public().to_address();
        let wxhopr = HoprBalance::new_base(safe_wxhopr);
        let xdai = XDaiBalance::new_base(10);
        let client = BlokliTestStateBuilder::default()
            .with_accounts([
                (account(me, SAFE, 1), wxhopr, xdai),
                (account(PEER.into(), PEER_SAFE, 2), wxhopr, xdai),
                (account(OTHER_PEER.into(), OTHER_PEER_SAFE, 3), wxhopr, xdai),
            ])
            .with_balances([(Address::from(SAFE), XDaiBalance::new_base(1))])
            .with_hopr_network_chain_info("anvil-localhost")
            .build_dynamic_client(MODULE.into());
        let connector = create_test_blokli_connector(&key, client, MODULE.into()).await?;
        let reserve = SafeReserve::default();
        reserve.set(HoprBalance::new_base(reserved));
        let chain = PolicyChain::new(Arc::new(connector), SAFE.into(), reserve, mode);
        Ok((chain, generate_channel_id(&me, &PEER.into())))
    }

    async fn safe_wxhopr(chain: &TestChain) -> anyhow::Result<HoprBalance> {
        Ok(chain.balance(Address::from(SAFE)).await?)
    }

    fn refused<T>(result: Result<T, TestConnectorError>) -> bool {
        result
            .err()
            .is_some_and(|e| e.to_string().contains(SAFE_RESERVED))
    }

    #[cfg(feature = "pix-curvy")]
    fn policy(min_available: u64, top_up: u64) -> crate::strategy::PixFundingPolicy {
        crate::strategy::PixFundingPolicy {
            min_available: HoprBalance::new_base(min_available),
            top_up: HoprBalance::new_base(top_up),
            fund_at_startup: false,
        }
    }

    #[cfg(feature = "pix-curvy")]
    #[test]
    fn a_pool_holding_a_whole_top_up_reserves_nothing() {
        let policy = policy(2, 35);
        assert_eq!(
            reserve_for(HoprBalance::new_base(35), policy.top_up, &policy),
            HoprBalance::zero()
        );
        assert_eq!(
            reserve_for(HoprBalance::new_base(40), policy.top_up, &policy),
            HoprBalance::zero()
        );
    }

    #[cfg(feature = "pix-curvy")]
    #[test]
    fn a_pool_at_its_refill_floor_reserves_a_whole_top_up() {
        let policy = policy(2, 35);
        assert_eq!(
            reserve_for(HoprBalance::new_base(2), policy.top_up, &policy),
            policy.top_up
        );
        assert_eq!(
            reserve_for(HoprBalance::zero(), policy.top_up, &policy),
            policy.top_up
        );
    }

    #[cfg(feature = "pix-curvy")]
    #[test]
    fn the_reserve_grows_as_the_pool_drains() {
        let policy = policy(2, 35);
        assert_eq!(
            reserve_for(
                HoprBalance::new_base(18) + HoprBalance::from(5u64 * 10u64.pow(17)),
                policy.top_up,
                &policy
            ),
            HoprBalance::new_base(17) + HoprBalance::from(5u64 * 10u64.pow(17)),
        );
        let reserves: Vec<_> = (0..=36)
            .rev()
            .map(|pool| reserve_for(HoprBalance::new_base(pool), policy.top_up, &policy))
            .collect();
        assert!(reserves.windows(2).all(|w| w[0] <= w[1]));
    }

    #[cfg(feature = "pix-curvy")]
    #[test]
    fn a_floor_at_the_top_up_reserves_it_whole_below_it() {
        let policy = policy(35, 35);
        assert_eq!(
            reserve_for(HoprBalance::new_base(34), policy.top_up, &policy),
            policy.top_up
        );
        assert_eq!(
            reserve_for(HoprBalance::new_base(35), policy.top_up, &policy),
            HoprBalance::zero()
        );
    }

    #[cfg(feature = "pix-curvy")]
    type Reads<T> = Arc<Mutex<std::vec::IntoIter<Result<T, &'static str>>>>;

    #[cfg(feature = "pix-curvy")]
    fn reads<T>(results: Vec<Result<T, &'static str>>) -> Reads<T> {
        Arc::new(Mutex::new(results.into_iter()))
    }

    #[cfg(feature = "pix-curvy")]
    fn next<T>(reads: &Reads<T>) -> std::future::Ready<Result<T, &'static str>> {
        std::future::ready(reads.lock().expect("reads").next().expect("a read"))
    }

    #[cfg(feature = "pix-curvy")]
    fn safe_view(safe: u64, open: &[u64], closing: u64) -> crate::reclaim::SafeView {
        crate::reclaim::SafeView {
            safe: HoprBalance::new_base(safe),
            open: open.iter().copied().map(HoprBalance::new_base).collect(),
            closing: HoprBalance::new_base(closing),
        }
    }

    /// Runs the policy over `pools`, with `view` for every Safe read; returns the reserve and mode.
    #[cfg(feature = "pix-curvy")]
    fn run_policy(
        pools: Vec<Result<HoprBalance, &'static str>>,
        view: crate::reclaim::SafeView,
    ) -> (
        SafeReserve,
        tokio::sync::watch::Receiver<crate::reclaim::LifecycleMode>,
        tokio::task::JoinHandle<()>,
    ) {
        let reserve = SafeReserve::default();
        let (mode, modes) = tokio::sync::watch::channel(crate::reclaim::LifecycleMode::Normal);
        let pools = reads(pools);
        let policy = run_safe_policy(
            move || next(&pools),
            move || std::future::ready(Ok::<_, &'static str>(view.clone())),
            policy(2, 35),
            reserve.clone(),
            mode,
        );
        assert_eq!(
            reserve.get(),
            HoprBalance::new_base(35),
            "a whole top-up before the first read"
        );
        (reserve, modes, tokio::spawn(policy))
    }

    #[cfg(feature = "pix-curvy")]
    #[tokio::test(start_paused = true)]
    async fn a_whole_top_up_is_reserved_until_the_pool_is_read() {
        let (reserve, _, policy) =
            run_policy(vec![Ok(HoprBalance::new_base(35))], safe_view(0, &[], 0));
        tokio::time::sleep(Duration::from_millis(1)).await;
        assert_eq!(reserve.get(), HoprBalance::zero());
        policy.abort();
    }

    #[cfg(feature = "pix-curvy")]
    #[tokio::test(start_paused = true)]
    async fn the_reserve_follows_the_pool_and_survives_a_failed_read() {
        let (reserve, _, policy) = run_policy(
            vec![
                Ok(HoprBalance::new_base(2)),
                Err("down"),
                Ok(HoprBalance::new_base(35)),
            ],
            safe_view(100, &[], 0),
        );
        tokio::time::sleep(RESERVE_REFRESH / 2).await;
        assert_eq!(reserve.get(), HoprBalance::new_base(35));
        tokio::time::sleep(RESERVE_REFRESH).await;
        assert_eq!(
            reserve.get(),
            HoprBalance::new_base(35),
            "a failed read keeps the last value"
        );
        tokio::time::sleep(RESERVE_REFRESH).await;
        assert_eq!(reserve.get(), HoprBalance::zero());
        policy.abort();
    }

    #[cfg(feature = "pix-curvy")]
    #[tokio::test(start_paused = true)]
    async fn a_low_pool_and_a_short_safe_reclaim_channels_until_the_refill() {
        use crate::reclaim::LifecycleMode;

        let (reserve, modes, policy) = run_policy(
            vec![
                Ok(HoprBalance::new_base(10)),
                // Read again before any channel is closed.
                Ok(HoprBalance::new_base(10)),
                Ok(HoprBalance::new_base(10)),
                Ok(HoprBalance::new_base(35)),
            ],
            safe_view(5, &[10, 20, 30], 0),
        );
        tokio::time::sleep(RESERVE_REFRESH / 2).await;
        assert_eq!(*modes.borrow(), LifecycleMode::Reclaim { keep: 1 });
        assert_eq!(
            reserve.get(),
            HoprBalance::new_base(35),
            "the stake being freed is all for the refill"
        );
        tokio::time::sleep(RESERVE_REFRESH).await;
        assert_eq!(*modes.borrow(), LifecycleMode::Reclaim { keep: 1 });
        assert_eq!(reserve.get(), HoprBalance::new_base(35));
        tokio::time::sleep(RESERVE_REFRESH).await;
        assert_eq!(*modes.borrow(), LifecycleMode::Normal);
        assert_eq!(reserve.get(), HoprBalance::zero());
        policy.abort();
    }

    #[cfg(feature = "pix-curvy")]
    #[tokio::test(start_paused = true)]
    async fn a_safe_covering_the_reserve_closes_no_channel() {
        let (_, modes, policy) = run_policy(
            vec![Ok(HoprBalance::new_base(10))],
            safe_view(30, &[10, 20, 30], 0),
        );
        tokio::time::sleep(RESERVE_REFRESH / 2).await;
        assert_eq!(*modes.borrow(), crate::reclaim::LifecycleMode::Normal);
        policy.abort();
    }

    #[cfg(feature = "pix-curvy")]
    #[tokio::test(start_paused = true)]
    async fn a_refill_landing_between_the_reads_closes_no_channel() {
        // The Safe is read already debited by a refill the first pool read predates.
        let (_, modes, policy) = run_policy(
            vec![Ok(HoprBalance::new_base(1)), Ok(HoprBalance::new_base(36))],
            safe_view(5, &[10, 20, 30], 0),
        );
        tokio::time::sleep(RESERVE_REFRESH / 2).await;
        assert_eq!(*modes.borrow(), crate::reclaim::LifecycleMode::Normal);
        policy.abort();
    }

    #[cfg(feature = "pix-curvy")]
    #[tokio::test(start_paused = true)]
    async fn stake_closing_under_a_low_pool_at_startup_keeps_a_whole_top_up_reserved() {
        let (reserve, modes, policy) = run_policy(
            vec![Ok(HoprBalance::new_base(10)), Ok(HoprBalance::new_base(10))],
            safe_view(5, &[20], 30),
        );
        tokio::time::sleep(RESERVE_REFRESH / 2).await;
        assert_eq!(reserve.get(), HoprBalance::new_base(35));
        assert_eq!(*modes.borrow(), crate::reclaim::LifecycleMode::Normal);
        tokio::time::sleep(RESERVE_REFRESH).await;
        assert_eq!(reserve.get(), HoprBalance::new_base(35), "until the refill");
        policy.abort();
    }

    #[cfg(feature = "pix-curvy")]
    #[tokio::test(start_paused = true)]
    async fn a_refill_short_of_the_top_up_by_its_fee_still_frees_the_safe() {
        let (reserve, _, policy) = run_policy(
            vec![
                Ok(HoprBalance::zero()),
                Ok(HoprBalance::new_base(34)),
                Ok(HoprBalance::new_base(18)),
                Ok(HoprBalance::new_base(2)),
            ],
            safe_view(100, &[], 0),
        );
        tokio::time::sleep(RESERVE_REFRESH / 2).await;
        assert_eq!(reserve.get(), HoprBalance::new_base(35));
        tokio::time::sleep(RESERVE_REFRESH).await;
        assert_eq!(reserve.get(), HoprBalance::zero(), "34 is a full pool");
        tokio::time::sleep(RESERVE_REFRESH).await;
        assert_eq!(
            reserve.get(),
            HoprBalance::new_base(17) + HoprBalance::from(5u64 * 10u64.pow(17)),
            "halfway from 34 to the floor"
        );
        tokio::time::sleep(RESERVE_REFRESH).await;
        assert_eq!(reserve.get(), HoprBalance::new_base(35), "the gross top-up");
        policy.abort();
    }

    #[cfg(feature = "pix-curvy")]
    #[tokio::test]
    async fn the_safe_view_counts_open_and_closing_outgoing_channels() -> anyhow::Result<()> {
        use hopr_lib::api::types::internal::channels::ChannelStatus;

        let key = ChainKeypair::random();
        let me = key.public().to_address();
        let channel = |src: Address, dst: Address, balance: u32, status| {
            ChannelEntry::builder()
                .between(src, dst)
                .amount(balance)
                .ticket_index(0)
                .status(status)
                .epoch(0)
                .build()
        };
        let closing =
            ChannelStatus::PendingToClose(std::time::SystemTime::now() + Duration::from_secs(600));
        let client = BlokliTestStateBuilder::default()
            .with_accounts([
                (
                    account(me, SAFE, 1),
                    HoprBalance::new_base(40),
                    XDaiBalance::new_base(1),
                ),
                (
                    account(PEER.into(), PEER_SAFE, 2),
                    HoprBalance::zero(),
                    XDaiBalance::new_base(1),
                ),
                (
                    account(OTHER_PEER.into(), OTHER_PEER_SAFE, 3),
                    HoprBalance::zero(),
                    XDaiBalance::new_base(1),
                ),
            ])
            .with_channels([
                channel(me, PEER.into(), 7, ChannelStatus::Open)?,
                channel(me, OTHER_PEER.into(), 3, closing)?,
                channel(PEER.into(), me, 11, ChannelStatus::Open)?,
            ])
            .with_hopr_network_chain_info("anvil-localhost")
            .build_dynamic_client(MODULE.into());
        let connector = create_test_blokli_connector(&key, client, MODULE.into()).await?;

        let view = read_safe_view(&connector, SAFE.into()).await?;
        assert_eq!(view.safe, HoprBalance::new_base(40));
        assert_eq!(view.open, vec![HoprBalance::from(7u32)]);
        assert_eq!(view.closing, HoprBalance::from(3u32));
        Ok(())
    }

    #[tokio::test]
    async fn the_safe_reads_without_the_reserve() -> anyhow::Result<()> {
        let (chain, _) = chain_with_safe(100, 30).await?;
        assert_eq!(safe_wxhopr(&chain).await?, HoprBalance::new_base(70));
        Ok(())
    }

    #[tokio::test]
    async fn a_reserve_above_the_safe_reads_as_an_empty_safe() -> anyhow::Result<()> {
        let (chain, _) = chain_with_safe(100, 150).await?;
        assert_eq!(safe_wxhopr(&chain).await?, HoprBalance::zero());
        Ok(())
    }

    #[tokio::test]
    async fn other_balances_read_unchanged() -> anyhow::Result<()> {
        let (chain, _) = chain_with_safe(100, 30).await?;
        let peer_safe: HoprBalance = chain.balance(Address::from(PEER_SAFE)).await?;
        let safe_xdai: Balance<XDai> = chain.balance(Address::from(SAFE)).await?;
        assert_eq!(peer_safe, HoprBalance::new_base(100));
        assert_eq!(safe_xdai, XDaiBalance::new_base(1));
        Ok(())
    }

    #[tokio::test]
    async fn an_open_into_the_reserve_is_refused() -> anyhow::Result<()> {
        let (chain, _) = chain_with_safe(100, 30).await?;
        let peer = Address::from(PEER);
        assert!(refused(
            chain.open_channel(&peer, HoprBalance::new_base(71)).await
        ));
        assert_eq!(safe_wxhopr(&chain).await?, HoprBalance::new_base(70));
        Ok(())
    }

    #[tokio::test]
    async fn opens_and_top_ups_spend_the_safe_down_to_the_reserve() -> anyhow::Result<()> {
        let (chain, channel) = chain_with_safe(100, 30).await?;
        chain
            .open_channel(&PEER.into(), HoprBalance::new_base(40))
            .await?
            .await?;
        chain
            .fund_channel(&channel, HoprBalance::new_base(30))
            .await?
            .await?;
        assert_eq!(safe_wxhopr(&chain).await?, HoprBalance::zero());

        assert!(refused(
            chain.fund_channel(&channel, HoprBalance::new_base(1)).await
        ));
        Ok(())
    }

    #[tokio::test]
    async fn overlapping_spends_share_what_is_above_the_reserve() -> anyhow::Result<()> {
        let (chain, channel) = chain_with_safe(100, 35).await?;
        let (peer, other) = (Address::from(PEER), Address::from(OTHER_PEER));
        let amount = HoprBalance::new_base(40);

        let (first, second) = futures::join!(
            chain.open_channel(&peer, amount),
            chain.open_channel(&other, amount),
        );
        let second_refused = refused(second);
        assert!(second_refused, "100 − 35 covers one open of 40, not two");
        first?.await?;
        assert_eq!(safe_wxhopr(&chain).await?, HoprBalance::new_base(25));

        // Confirmed, the first open no longer counts twice.
        chain
            .fund_channel(&channel, HoprBalance::new_base(25))
            .await?
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn a_spend_awaiting_confirmation_counts_without_a_reserve_too() -> anyhow::Result<()> {
        let (chain, _) = chain_with_safe(100, 0).await?;
        let (peer, other) = (Address::from(PEER), Address::from(OTHER_PEER));
        let unconfirmed = chain.open_channel(&peer, HoprBalance::new_base(80)).await?;

        chain.reserve.set(HoprBalance::new_base(35));
        assert!(refused(
            chain.open_channel(&other, HoprBalance::new_base(1)).await
        ));
        unconfirmed.await?;
        Ok(())
    }

    #[tokio::test]
    async fn nothing_is_spent_while_channels_are_reclaimed() -> anyhow::Result<()> {
        let mode = watch::Sender::new(LifecycleMode::Normal);
        let (chain, channel) = chain_with_mode(100, 0, mode.subscribe()).await?;
        chain
            .open_channel(&PEER.into(), HoprBalance::new_base(10))
            .await?
            .await?;

        mode.send_replace(LifecycleMode::Reclaim { keep: 1 });
        assert_eq!(safe_wxhopr(&chain).await?, HoprBalance::zero());
        assert!(refused(
            chain.fund_channel(&channel, HoprBalance::new_base(1)).await
        ));
        let other = Address::from(OTHER_PEER);
        assert!(refused(
            chain.open_channel(&other, HoprBalance::new_base(1)).await
        ));
        chain.close_channel(&channel).await?.await?;

        mode.send_replace(LifecycleMode::Normal);
        assert_eq!(safe_wxhopr(&chain).await?, HoprBalance::new_base(90));
        Ok(())
    }

    #[tokio::test]
    async fn writes_are_settled_once_confirmed() -> anyhow::Result<()> {
        let (chain, channel) = chain_with_safe(100, 0).await?;
        chain
            .open_channel(&PEER.into(), HoprBalance::new_base(10))
            .await?
            .await?;
        assert!(chain.writes.settled().now_or_never().is_some());

        let unconfirmed = chain.close_channel(&channel).await?;
        assert!(chain.writes.settled().now_or_never().is_none());
        unconfirmed.await?;
        assert!(chain.writes.settled().now_or_never().is_some());
        Ok(())
    }

    #[tokio::test]
    async fn closing_is_never_refused() -> anyhow::Result<()> {
        let (chain, channel) = chain_with_safe(100, 30).await?;
        chain
            .open_channel(&PEER.into(), HoprBalance::new_base(70))
            .await?
            .await?;
        chain.reserve.set(HoprBalance::new_base(500));
        chain.close_channel(&channel).await?.await?;
        Ok(())
    }

    #[tokio::test]
    async fn no_reserve_spends_the_whole_safe() -> anyhow::Result<()> {
        let (chain, _) = chain_with_safe(100, 0).await?;
        chain
            .open_channel(&PEER.into(), HoprBalance::new_base(100))
            .await?
            .await?;
        assert_eq!(safe_wxhopr(&chain).await?, HoprBalance::zero());
        Ok(())
    }
}
