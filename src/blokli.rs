use hopr_chain_connector::{
    BlockchainConnectorConfig, HoprBlockchainBasicConnector,
    blokli_client::{
        BlokliClient, BlokliClientConfig, BlokliQueryClient, BlokliSubscriptionClient,
        BlokliTransactionClient,
    },
    create_trustful_safeless_hopr_blokli_connector,
};
use hopr_lib::{
    Address, Balance, HoprBalance, Keypair, WxHOPR, XDaiBalance,
    api::chain::{ChainReadSafeOperations, SafeSelector},
};
use std::sync::Arc;
use url::Url;

pub use hopr_chain_connector as connector;
pub use hopr_lib::ChainKeypair;
use hopr_lib::api::chain::{ChainWriteAccountOperations, ChainWriteSafeOperations};

lazy_static::lazy_static! {
    pub static ref DEFAULT_BLOKLI_URL: Url = "https://blokli.jura.gnosisvpn.io".parse().unwrap();
}

pub fn new_blokli_client(url: Option<Url>) -> BlokliClient {
    BlokliClient::new(
        url.unwrap_or(DEFAULT_BLOKLI_URL.clone()),
        BlokliClientConfig {
            timeout: std::time::Duration::from_secs(3),
            // This is actually maximum delay, it starts at 2s with backoff until 30s
            stream_reconnect_timeout: std::time::Duration::from_secs(30),
            ..Default::default()
        },
    )
}

#[derive(Copy, Clone, Debug)]
pub struct TicketStats {
    pub ticket_price: Balance<WxHOPR>,
    pub winning_probability: f64,
}

pub struct SafelessInteractor<C = BlokliClient> {
    connector: Arc<HoprBlockchainBasicConnector<C>>,
    chain_key: ChainKeypair,
}

impl SafelessInteractor<BlokliClient> {
    pub async fn new(
        blokli_provider: Option<Url>,
        chain_key: &ChainKeypair,
        connector_config: Option<BlockchainConnectorConfig>,
    ) -> anyhow::Result<Self> {
        Self::new_with_client(
            new_blokli_client(blokli_provider),
            chain_key,
            connector_config,
        )
        .await
    }
}

impl<C> SafelessInteractor<C>
where
    C: BlokliSubscriptionClient
        + BlokliQueryClient
        + BlokliTransactionClient
        + Send
        + Sync
        + 'static,
{
    pub async fn new_with_client(
        client: C,
        chain_key: &ChainKeypair,
        connector_config: Option<BlockchainConnectorConfig>,
    ) -> anyhow::Result<Self> {
        let cfg = connector_config.unwrap_or_default();
        let mut connector =
            create_trustful_safeless_hopr_blokli_connector(chain_key, cfg, client).await?;
        connector.connect().await?;

        Ok(Self {
            connector: Arc::new(connector),
            chain_key: chain_key.clone(),
        })
    }

    async fn execute<F, T>(&self, f: F) -> anyhow::Result<T>
    where
        F: Fn(Arc<HoprBlockchainBasicConnector<C>>) -> T,
    {
        Ok(f(self.connector.clone()))
    }

    #[tracing::instrument(skip(self), ret)]
    pub async fn retrieve_safe(&self) -> anyhow::Result<Option<SafeModuleDeploymentResult>> {
        let me = self.chain_key.public().to_address();
        let res = self.connector.safe_info(SafeSelector::Owner(me)).await?;
        match res {
            Some(safe_info) => Ok(Some(SafeModuleDeploymentResult {
                safe_address: safe_info.address,
                module_address: safe_info.module,
            })),
            None => Ok(None),
        }
    }

    #[tracing::instrument(skip(self), ret)]
    pub async fn deploy_safe(
        &self,
        token_amount: HoprBalance,
    ) -> anyhow::Result<SafeModuleDeploymentResult> {
        if let Some(safe_info) = self.retrieve_safe().await? {
            tracing::debug!(?safe_info, "safe already deployed");
            return Ok(safe_info);
        }

        let connector = self.connector.clone();

        let me = self.chain_key.public().to_address();
        let subscription_handle = tokio::spawn(async move {
            tracing::debug!("subscribing to safe deployment event");
            connector
                .await_safe_deployment(SafeSelector::Owner(me), std::time::Duration::from_mins(2))
                .await
        });

        let tx_hash = self.connector.deploy_safe(token_amount).await?.await?;
        tracing::debug!(%tx_hash, "safe deployment transaction submitted");

        let safe = subscription_handle
            .await
            .map_err(|e| anyhow::anyhow!("safe deployment subscription task failed: {e}"))??;

        Ok(SafeModuleDeploymentResult {
            safe_address: safe.address,
            module_address: safe.module,
        })
    }

    #[tracing::instrument(skip(self), ret)]
    pub async fn withdraw_wxhopr(
        &self,
        safe_address: Address,
        amount: HoprBalance,
    ) -> anyhow::Result<()> {
        self.connector
            .withdraw(amount, &safe_address)
            .await?
            .await?;
        Ok(())
    }

    pub async fn ticket_stats(&self) -> anyhow::Result<TicketStats> {
        Ok(TicketStats {
            ticket_price: hopr_lib::api::chain::ChainValues::minimum_ticket_price(&self.connector)
                .await
                .map_err(anyhow::Error::from)?,
            winning_probability:
                hopr_lib::api::chain::ChainValues::minimum_incoming_ticket_win_prob(&self.connector)
                    .await
                    .map_err(anyhow::Error::from)?
                    .as_f64(),
        })
    }

    pub async fn balances(&self) -> anyhow::Result<(HoprBalance, XDaiBalance)> {
        let me = self.chain_key.public().to_address();
        self.execute(move |connector| async move {
            Ok((
                hopr_lib::api::chain::ChainValues::balance(&connector, me)
                    .await
                    .map_err(anyhow::Error::from)?,
                hopr_lib::api::chain::ChainValues::balance(&connector, me)
                    .await
                    .map_err(anyhow::Error::from)?,
            ))
        })
        .await?
        .await
    }
}

#[derive(Clone, Debug)]
pub struct SafeModuleDeploymentResult {
    pub safe_address: Address,
    pub module_address: Address,
}

#[cfg(test)]
mod tests {
    use super::*;
    use hopr_chain_connector::{errors::ConnectorError, testing::BlokliTestStateBuilder};

    fn placeholder_module_addr() -> Address {
        [0x11u8; 20].into()
    }

    fn build_test_client(
        node: Address,
        node_wxhopr: HoprBalance,
        recipient: Address,
    ) -> hopr_chain_connector::testing::BlokliTestClient<
        hopr_chain_connector::testing::FullStateEmulator,
    > {
        BlokliTestStateBuilder::default()
            .with_balances([(node, node_wxhopr)])
            .with_balances([(node, XDaiBalance::new_base(10))])
            .with_balances([(recipient, HoprBalance::zero())])
            .with_balances([(recipient, XDaiBalance::zero())])
            .with_hopr_network_chain_info("rotsee")
            .build_dynamic_client(placeholder_module_addr())
    }

    #[tokio::test]
    async fn withdraw_wxhopr_succeeds_with_sufficient_balance() -> anyhow::Result<()> {
        let chain_key = ChainKeypair::random();
        let me = chain_key.public().to_address();
        let recipient: Address = [0x22u8; 20].into();

        let client = build_test_client(me, HoprBalance::new_base(1000), recipient);
        let interactor = SafelessInteractor::new_with_client(client, &chain_key, None).await?;

        interactor
            .withdraw_wxhopr(recipient, HoprBalance::new_base(10))
            .await?;

        Ok(())
    }

    #[tokio::test]
    async fn withdraw_wxhopr_fails_with_insufficient_balance() -> anyhow::Result<()> {
        let chain_key = ChainKeypair::random();
        let me = chain_key.public().to_address();
        let recipient: Address = [0x22u8; 20].into();

        let client = build_test_client(me, HoprBalance::new_base(1), recipient);
        let interactor = SafelessInteractor::new_with_client(client, &chain_key, None).await?;

        let err = interactor
            .withdraw_wxhopr(recipient, HoprBalance::new_base(10))
            .await
            .expect_err("expected withdrawal to fail with insufficient balance");
        let connector_err = err
            .downcast_ref::<ConnectorError>()
            .unwrap_or_else(|| panic!("expected ConnectorError, got: {err:#}"));
        // `as_transaction_rejection_error` returns `Some` only for tx-rejection variants
        // (Reverted / ValidationFailed). `InvalidState("not connected")` returns `None`,
        // so this single match also guards against a connect() regression.
        assert!(
            connector_err.as_transaction_rejection_error().is_some(),
            "expected tx-rejection error from chain emulator, got: {connector_err:?}"
        );

        Ok(())
    }

    #[tokio::test]
    async fn new_with_client_yields_connected_interactor() -> anyhow::Result<()> {
        let chain_key = ChainKeypair::random();
        let me = chain_key.public().to_address();
        let recipient: Address = [0x22u8; 20].into();

        let client = build_test_client(me, HoprBalance::new_base(100), recipient);
        let interactor = SafelessInteractor::new_with_client(client, &chain_key, None).await?;

        // Regression guard: withdraw is in ChainWriteAccountOperations, which requires the
        // connector to be connected. If new_with_client ever stops calling connect(), this
        // call will fail with InvalidState("connector is not connected").
        interactor
            .withdraw_wxhopr(recipient, HoprBalance::new_base(1))
            .await?;

        Ok(())
    }
}
