use std::sync::Arc;

use hopr_chain_connector::{
    BlockchainConnectorConfig, HoprBlockchainBasicConnector,
    blokli_client::{BlokliClient, BlokliClientConfig},
    create_trustful_safeless_hopr_blokli_connector,
};
use hopr_lib::{
    api::{
        chain::{
            ChainReadSafeOperations, ChainValues, ChainWriteAccountOperations, ChainWriteSafeOperations, SafeSelector,
        },
        types::{
            internal::prelude::WinningProbability,
            primitive::prelude::{Address, Balance, HoprBalance, WxHOPR, XDaiBalance},
        },
    },
    builder::Keypair,
};
use url::Url;

pub use hopr_chain_connector as connector;
pub use hopr_lib::builder::ChainKeypair;

lazy_static::lazy_static! {
    pub static ref DEFAULT_BLOKLI_URL: Url = "https://blokli.jura.gnosisvpn.io".parse().unwrap();
}

pub fn new_blokli_client(url: Option<Url>) -> BlokliClient {
    BlokliClient::new(
        url.unwrap_or(DEFAULT_BLOKLI_URL.clone()),
        BlokliClientConfig {
            timeout: std::time::Duration::from_secs(3),
            // This is actually maximum delay; starts at 2 s with backoff until 30 s.
            stream_reconnect_timeout: std::time::Duration::from_secs(30),
            ..Default::default()
        },
    )
}

/// On-chain ticket pricing parameters.
#[derive(Copy, Clone, Debug)]
pub struct TicketStats {
    pub ticket_price: Balance<WxHOPR>,
    /// Minimum winning probability enforced by the network.
    ///
    /// Call `.as_f64()` (via [`hopr_lib::UnitaryFloatOps`]) to convert to f64.
    pub winning_probability: WinningProbability,
}

/// Trait facade for blockchain operations that do not require an active Safe.
///
/// Consumers should program against this trait rather than using
/// [`SafelessInteractor`] directly, which makes unit testing possible without
/// a live blockchain connection.
#[async_trait::async_trait]
pub trait SafeOperations: Send + Sync {
    /// Look up an existing Safe/module deployment for this key-pair.
    async fn retrieve_safe(&self) -> anyhow::Result<Option<SafeModuleDeploymentResult>>;

    /// Deploy a new Safe and module, funding it with `token_amount` WxHOPR.
    async fn deploy_safe(&self, token_amount: HoprBalance) -> anyhow::Result<SafeModuleDeploymentResult>;

    /// Fetch current on-chain ticket pricing parameters.
    async fn ticket_stats(&self) -> anyhow::Result<TicketStats>;

    /// Fetch the WxHOPR and xDAI balances for this key-pair.
    async fn balances(&self) -> anyhow::Result<(HoprBalance, XDaiBalance)>;
}

/// Blockchain interactor that operates without a HOPR Safe module.
///
/// Used for on-boarding flows that need to query balances or deploy a Safe
/// before a full [`crate::Edgli`] node is started.
pub struct SafelessInteractor {
    connector: Arc<HoprBlockchainBasicConnector<BlokliClient>>,
    chain_key: ChainKeypair,
}

impl SafelessInteractor {
    pub async fn new(
        blokli_provider: Option<Url>,
        chain_key: &ChainKeypair,
        connector_config: Option<BlockchainConnectorConfig>,
    ) -> anyhow::Result<Self> {
        let blokli_client = new_blokli_client(blokli_provider);

        let cfg = connector_config.unwrap_or_default();
        let connector =
            create_trustful_safeless_hopr_blokli_connector(chain_key, cfg, blokli_client).await?;

        Ok(Self {
            connector: Arc::new(connector),
            chain_key: chain_key.clone(),
        })
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
            ticket_price: ChainValues::minimum_ticket_price(&self.connector)
                .await
                .map_err(anyhow::Error::from)?,
            winning_probability: ChainValues::minimum_incoming_ticket_win_prob(&self.connector)
                .await
                .map_err(anyhow::Error::from)?,
        })
    }

    pub async fn balances(&self) -> anyhow::Result<(HoprBalance, XDaiBalance)> {
        let me = self.chain_key.public().to_address();
        let hopr: HoprBalance = ChainValues::balance(&self.connector, me)
            .await
            .map_err(anyhow::Error::from)?;
        let xdai: XDaiBalance = ChainValues::balance(&self.connector, me)
            .await
            .map_err(anyhow::Error::from)?;
        Ok((hopr, xdai))
    }
}

#[async_trait::async_trait]
impl SafeOperations for SafelessInteractor {
    async fn retrieve_safe(&self) -> anyhow::Result<Option<SafeModuleDeploymentResult>> {
        SafelessInteractor::retrieve_safe(self).await
    }

    async fn deploy_safe(&self, token_amount: HoprBalance) -> anyhow::Result<SafeModuleDeploymentResult> {
        SafelessInteractor::deploy_safe(self, token_amount).await
    }

    async fn ticket_stats(&self) -> anyhow::Result<TicketStats> {
        SafelessInteractor::ticket_stats(self).await
    }

    async fn balances(&self) -> anyhow::Result<(HoprBalance, XDaiBalance)> {
        SafelessInteractor::balances(self).await
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

    #[test]
    fn default_blokli_url_is_correct() {
        assert_eq!(
            DEFAULT_BLOKLI_URL.as_str(),
            "https://blokli.jura.gnosisvpn.io/"
        );
    }

    #[test]
    fn new_blokli_client_uses_default_url_when_none() {
        // Constructing the client must not panic and should use the default URL.
        let client = new_blokli_client(None);
        // BlokliClient doesn't expose a getter for the URL, but construction
        // succeeding without a panic is the relevant invariant here.
        let _ = client;
    }

    #[test]
    fn new_blokli_client_accepts_custom_url() {
        let url: Url = "https://custom.blokli.example.com".parse().unwrap();
        let client = new_blokli_client(Some(url));
        let _ = client;
    }

    #[test]
    fn ticket_stats_fields_accessible() {
        // TicketStats is a plain struct; verify it can be constructed and fields read.
        let stats = TicketStats {
            ticket_price: Balance::<WxHOPR>::zero(),
            winning_probability: WinningProbability::default(),
        };
        let _ = stats.ticket_price;
        let _ = stats.winning_probability;
    }

    #[test]
    fn winning_probability_as_f64_in_range() {
        let prob = WinningProbability::default();
        let f = prob.as_f64();
        assert!((0.0..=1.0).contains(&f));
    }

    #[test]
    fn safe_module_deployment_result_is_clone() {
        let r = SafeModuleDeploymentResult {
            safe_address: Address::default(),
            module_address: Address::default(),
        };
        let _ = r.clone();
    }
}
