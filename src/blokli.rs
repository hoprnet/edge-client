use hopr_chain_connector::{
    BlockchainConnectorConfig, HoprBlockchainBasicConnector,
    blokli_client::{BlokliClient, BlokliClientConfig},
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
use hopr_lib::api::chain::ChainWriteSafeOperations;

lazy_static::lazy_static! {
    pub static ref DEFAULT_BLOKLI_URL: Url = "https://blokli.staging.hoprnet.link".parse().unwrap();
}

pub const SAFE_RETRIEVAL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

pub fn new_blokli_client(url: Option<Url>) -> BlokliClient {
    BlokliClient::new(
        url.unwrap_or(DEFAULT_BLOKLI_URL.clone()),
        BlokliClientConfig {
            timeout: std::time::Duration::from_secs(120),
            stream_reconnect_timeout: std::time::Duration::from_secs(30),
        },
    )
}

#[derive(Copy, Clone, Debug)]
pub struct TicketStats {
    pub ticket_price: Balance<WxHOPR>,
    pub winning_probability: f64,
}

pub struct SafelessInteractor {
    connector: Arc<HoprBlockchainBasicConnector<BlokliClient>>,
    chain_key: ChainKeypair,
}

impl SafelessInteractor {
    pub async fn new(
        blokli_provider: Option<Url>,
        chain_key: &ChainKeypair,
    ) -> anyhow::Result<Self> {
        let blokli_client = new_blokli_client(blokli_provider);

        let connector = create_trustful_safeless_hopr_blokli_connector(
            chain_key,
            BlockchainConnectorConfig {
                tx_confirm_timeout: std::time::Duration::from_secs(90),
                connection_timeout: std::time::Duration::from_secs(120),
            },
            blokli_client,
        )
        .await?;

        Ok(Self {
            connector: Arc::new(connector),
            chain_key: chain_key.clone(),
        })
    }

    async fn execute<F, T>(&self, f: F) -> anyhow::Result<T>
    where
        F: Fn(Arc<HoprBlockchainBasicConnector<BlokliClient>>) -> T,
    {
        Ok(f(self.connector.clone()))
    }

    #[tracing::instrument(skip(self), ret)]
    pub async fn retrieve_safe(&self) -> anyhow::Result<Option<SafeModuleDeploymentResult>> {
        let me = self.chain_key.public().to_address();
        let res = self.connector.safe_info(SafeSelector::Owner(me)).await?;
        match res {
        Some(safe_info) => {
            Ok(Some(SafeModuleDeploymentResult {
                safe_address: safe_info.address,
                module_address: safe_info.module,
            }))
        }
        None => {
            Ok(None)
        }
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
                .await_safe_deployment(SafeSelector::Owner(me), SAFE_RETRIEVAL_TIMEOUT)
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
