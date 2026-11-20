//! Holds the PIX pool's relayer calls until the embedder's route to the relayer is up.

use std::sync::Arc;

use hopr_lib::api::chain::{
    BatchOutcomes, DepositNotification, DepositPool, PixAddressId, PoolTransferSource,
};
use hopr_lib::api::types::crypto::prelude::Keypair;
use hopr_lib::api::types::primitive::prelude::{Address, HoprBalance};
use hopr_strategy::pix::pools::curvy::PoolKeypair;
use tokio::sync::watch;

type DepositAddress = <PoolKeypair as Keypair>::Public;

/// Holds and releases the relayer calls of the PIX pool; starts released.
#[derive(Clone)]
pub struct PixDepositHold(Arc<watch::Sender<bool>>);

impl PixDepositHold {
    /// Relayer calls made from now on wait until [`Self::release`].
    pub fn hold(&self) {
        let was_released = self.0.send_replace(false);
        if was_released {
            tracing::info!(held = true, "pix deposits held");
        }
    }

    /// Lets held and future deposit calls through.
    pub fn release(&self) {
        let was_released = self.0.send_replace(true);
        if !was_released {
            tracing::info!(held = false, "pix deposits released");
        }
    }
}

/// A deposit pool whose relayer calls wait while its [`PixDepositHold`] holds them; local and Blokli calls pass through.
pub struct HeldDepositPool<P> {
    inner: P,
    released: watch::Receiver<bool>,
}

/// Wraps `inner` in a released hold.
pub fn hold_deposits<P>(inner: P) -> (HeldDepositPool<P>, PixDepositHold) {
    let (tx, rx) = watch::channel(true);
    let pool = HeldDepositPool {
        inner,
        released: rx,
    };
    (pool, PixDepositHold(Arc::new(tx)))
}

impl<P> HeldDepositPool<P> {
    async fn wait_for_release(&self) {
        // A dropped hold can never release, so it stops holding.
        let _ = self.released.clone().wait_for(|released| *released).await;
    }
}

#[async_trait::async_trait]
impl<P> DepositPool<PoolKeypair> for HeldDepositPool<P>
where
    P: DepositPool<PoolKeypair> + Send + Sync,
{
    type Error = P::Error;
    type Receipt = P::Receipt;
    type PoolDepositData = P::PoolDepositData;

    async fn generate_deposit_data(
        &self,
        id: &PixAddressId,
    ) -> Result<Self::PoolDepositData, Self::Error> {
        self.inner.generate_deposit_data(id).await
    }

    fn accepts_funding(&self) -> bool {
        self.inner.accepts_funding()
    }

    // Funding asks the relayer when it recovers an unfinished aggregation.
    async fn fund(&self, amount: HoprBalance) -> Result<(), Self::Error> {
        self.wait_for_release().await;
        self.inner.fund(amount).await
    }

    async fn available_balance(&self) -> Result<HoprBalance, Self::Error> {
        self.inner.available_balance().await
    }

    async fn deposit_funds_to(
        &self,
        id: &PixAddressId,
        dst: &DepositAddress,
        amount: HoprBalance,
        additional_data: Self::PoolDepositData,
    ) -> Result<Self::Receipt, Self::Error> {
        self.wait_for_release().await;
        self.inner
            .deposit_funds_to(id, dst, amount, additional_data)
            .await
    }

    async fn deposit_funds_to_multiple(
        &self,
        deposits: &[(
            PixAddressId,
            DepositAddress,
            HoprBalance,
            Self::PoolDepositData,
        )],
    ) -> Result<BatchOutcomes<Self::Receipt, Self::Error>, Self::Error> {
        self.wait_for_release().await;
        self.inner.deposit_funds_to_multiple(deposits).await
    }

    fn notify_deposit(
        &self,
        id: PixAddressId,
        dst: DepositAddress,
        min_amount: HoprBalance,
    ) -> Result<DepositNotification<'static, DepositAddress, Self::Error>, Self::Error> {
        self.inner.notify_deposit(id, dst, min_amount)
    }

    async fn withdraw_deposit(
        &self,
        id: &PixAddressId,
        key: &PoolKeypair,
        dst: Address,
        amount: Option<HoprBalance>,
    ) -> Result<Self::Receipt, Self::Error> {
        self.wait_for_release().await;
        self.inner.withdraw_deposit(id, key, dst, amount).await
    }

    async fn withdraw_multiple_deposits(
        &self,
        keys: &[(PixAddressId, PoolKeypair)],
        dst: Address,
    ) -> Result<BatchOutcomes<Self::Receipt, Self::Error>, Self::Error> {
        self.wait_for_release().await;
        self.inner.withdraw_multiple_deposits(keys, dst).await
    }

    async fn pool_transfer(
        &self,
        src: PoolTransferSource<'_, PoolKeypair>,
        dst_id: &PixAddressId,
        dst: DepositAddress,
        additional_dst_data: Self::PoolDepositData,
        amount: Option<HoprBalance>,
    ) -> Result<Self::Receipt, Self::Error> {
        self.wait_for_release().await;
        self.inner
            .pool_transfer(src, dst_id, dst, additional_dst_data, amount)
            .await
    }

    async fn pool_transfer_multiple(
        &self,
        keys: &[(PixAddressId, PoolKeypair)],
        dst_id: &PixAddressId,
        dst: DepositAddress,
        additional_dst_data: Self::PoolDepositData,
    ) -> Result<BatchOutcomes<Self::Receipt, Self::Error>, Self::Error> {
        self.wait_for_release().await;
        self.inner
            .pool_transfer_multiple(keys, dst_id, dst, additional_dst_data)
            .await
    }
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;
    use std::num::NonZeroU32;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use hopr_lib::api::chain::PixDepositData;
    use hopr_lib::api::types::crypto::prelude::SimplePseudonym;
    use hopr_lib::api::types::crypto_random::Randomizable;

    use super::*;

    #[derive(Default)]
    struct CountingPool {
        relayer_calls: AtomicUsize,
    }

    impl CountingPool {
        fn count(&self) -> Result<(), Infallible> {
            self.relayer_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl DepositPool<PoolKeypair> for CountingPool {
        type Error = Infallible;
        type Receipt = ();
        type PoolDepositData = PixDepositData;

        async fn generate_deposit_data(
            &self,
            id: &PixAddressId,
        ) -> Result<PixDepositData, Infallible> {
            Ok(deposit_data(*id))
        }

        async fn fund(&self, _amount: HoprBalance) -> Result<(), Infallible> {
            self.count()
        }

        async fn available_balance(&self) -> Result<HoprBalance, Infallible> {
            Ok(HoprBalance::zero())
        }

        async fn deposit_funds_to(
            &self,
            _id: &PixAddressId,
            _dst: &DepositAddress,
            _amount: HoprBalance,
            _data: PixDepositData,
        ) -> Result<(), Infallible> {
            self.count()
        }

        fn notify_deposit(
            &self,
            _id: PixAddressId,
            _dst: DepositAddress,
            _min_amount: HoprBalance,
        ) -> Result<DepositNotification<'static, DepositAddress, Infallible>, Infallible> {
            unimplemented!()
        }

        async fn withdraw_deposit(
            &self,
            _id: &PixAddressId,
            _key: &PoolKeypair,
            _dst: Address,
            _amount: Option<HoprBalance>,
        ) -> Result<(), Infallible> {
            self.count()
        }

        async fn pool_transfer(
            &self,
            _src: PoolTransferSource<'_, PoolKeypair>,
            _dst_id: &PixAddressId,
            _dst: DepositAddress,
            _data: PixDepositData,
            _amount: Option<HoprBalance>,
        ) -> Result<(), Infallible> {
            self.count()
        }
    }

    type Pool = Arc<HeldDepositPool<Arc<CountingPool>>>;

    #[derive(Clone, Copy, Debug)]
    enum RelayerCall {
        Fund,
        Deposit,
        DepositMultiple,
        Withdraw,
        WithdrawMultiple,
        Transfer,
        TransferMultiple,
    }

    fn deposit_data(id: PixAddressId) -> PixDepositData {
        PixDepositData {
            id,
            data: Box::new([]),
        }
    }

    async fn call(pool: Pool, call: RelayerCall) {
        let id = PixAddressId::new(&SimplePseudonym::random(), NonZeroU32::MIN);
        let key = PoolKeypair::random();
        let dst = *PoolKeypair::random().public();
        let zero = HoprBalance::zero();
        let _ = match call {
            RelayerCall::Fund => pool.fund(zero).await,
            RelayerCall::Deposit => pool
                .deposit_funds_to(&id, &dst, zero, deposit_data(id))
                .await
                .map(drop),
            RelayerCall::DepositMultiple => pool
                .deposit_funds_to_multiple(&[(id, dst, zero, deposit_data(id))])
                .await
                .map(drop),
            RelayerCall::Withdraw => pool
                .withdraw_deposit(&id, &key, Address::default(), None)
                .await
                .map(drop),
            RelayerCall::WithdrawMultiple => pool
                .withdraw_multiple_deposits(&[(id, key)], Address::default())
                .await
                .map(drop),
            RelayerCall::Transfer => pool
                .pool_transfer(
                    PoolTransferSource::Funding,
                    &id,
                    dst,
                    deposit_data(id),
                    None,
                )
                .await
                .map(drop),
            RelayerCall::TransferMultiple => pool
                .pool_transfer_multiple(&[(id, key)], &id, dst, deposit_data(id))
                .await
                .map(drop),
        };
    }

    #[tokio::test]
    async fn released_hold_passes_deposits_through() {
        let inner = Arc::new(CountingPool::default());
        let (pool, _hold) = hold_deposits(Arc::clone(&inner));

        call(Arc::new(pool), RelayerCall::Deposit).await;

        assert_eq!(inner.relayer_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn held_relayer_calls_wait_for_release() {
        let relayer_calls = [
            RelayerCall::Fund,
            RelayerCall::Deposit,
            RelayerCall::DepositMultiple,
            RelayerCall::Withdraw,
            RelayerCall::WithdrawMultiple,
            RelayerCall::Transfer,
            RelayerCall::TransferMultiple,
        ];
        for relayer_call in relayer_calls {
            let inner = Arc::new(CountingPool::default());
            let (pool, hold) = hold_deposits(Arc::clone(&inner));
            hold.hold();

            let pending = tokio::spawn(call(Arc::new(pool), relayer_call));
            tokio::time::sleep(Duration::from_millis(50)).await;
            let calls_while_held = inner.relayer_calls.load(Ordering::SeqCst);
            assert_eq!(calls_while_held, 0, "{relayer_call:?} ran while held");

            hold.release();
            pending.await.expect("relayer call task");
            let calls_after_release = inner.relayer_calls.load(Ordering::SeqCst);
            assert_eq!(calls_after_release, 1, "{relayer_call:?} after release");
        }
    }

    #[tokio::test]
    async fn held_pool_passes_local_calls_through() {
        let (pool, hold) = hold_deposits(Arc::new(CountingPool::default()));
        hold.hold();
        let id = PixAddressId::new(&SimplePseudonym::random(), NonZeroU32::MIN);

        assert!(pool.accepts_funding());
        assert_eq!(pool.available_balance().await, Ok(HoprBalance::zero()));
        let data = pool.generate_deposit_data(&id).await.expect("deposit data");
        assert_eq!(data.id, id);
    }

    #[tokio::test]
    async fn dropped_hold_stops_holding() {
        let inner = Arc::new(CountingPool::default());
        let (pool, hold) = hold_deposits(Arc::clone(&inner));
        hold.hold();
        drop(hold);

        call(Arc::new(pool), RelayerCall::Deposit).await;

        assert_eq!(inner.relayer_calls.load(Ordering::SeqCst), 1);
    }
}
