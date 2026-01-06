use std::sync::Arc;

use hopr_chain_connector::{
    BasicPayloadGenerator, ContractAddresses, HoprBlockchainConnector, PayloadGenerator,
    TempDbBackend,
    blokli_client::{BlokliClient, BlokliClientConfig, BlokliQueryClient},
    errors::ConnectorError,
};
use hopr_lib::{
    Address, Balance, HoprBalance, IntoEndian, Keypair, WxHOPR, XDai, XDaiBalance,
    api::chain::{ChainReadSafeOperations, SafeSelector},
};
use hopr_chain_types::prelude::SignableTransaction;
use url::Url;

pub use hopr_chain_connector as connector;
pub use hopr_lib::ChainKeypair;

lazy_static::lazy_static! {
    pub static ref DEFAULT_BLOKLI_URL: Url = "https://blokli.staging.hoprnet.link".parse().unwrap();
}

pub type HoprBlockchainSafelessConnector<C> = HoprBlockchainConnector<
    C,
    TempDbBackend,
    BasicPayloadGenerator,
    <BasicPayloadGenerator as PayloadGenerator>::TxRequest,
>;

pub const SAFE_RETRIEVAL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

#[derive(Copy, Clone, Debug)]
pub struct TicketStats {
    pub ticket_price: Balance<WxHOPR>,
    pub winning_probability: f64,
}

pub struct SafelessInteractor {
    connector: Arc<HoprBlockchainSafelessConnector<BlokliClient>>,
    chain_key: ChainKeypair,
}

impl SafelessInteractor {
    pub async fn new(
        blokli_provider: Option<Url>,
        chain_key: &ChainKeypair,
    ) -> anyhow::Result<Self> {
        let blokli_client = BlokliClient::new(
            blokli_provider.unwrap_or_else(|| DEFAULT_BLOKLI_URL.clone()),
            BlokliClientConfig {
                timeout: std::time::Duration::from_secs(5),
                ..Default::default()
            },
        );

        let info = blokli_client.query_chain_info().await?;
        let contract_addrs = serde_json::from_str(&info.contract_addresses.0).map_err(|e| {
            ConnectorError::TypeConversion(format!("contract addresses not a valid JSON: {e}"))
        })?;

        let payload_gen =
            BasicPayloadGenerator::new(chain_key.public().to_address(), contract_addrs);

        let connector = HoprBlockchainConnector::new(
            chain_key.clone(),
            Default::default(),
            blokli_client,
            TempDbBackend::new()?,
            payload_gen,
        );

        Ok(Self {
            connector: Arc::new(connector),
            chain_key: chain_key.clone(),
        })
    }

    pub async fn execute<F, T>(&self, f: F) -> anyhow::Result<T>
    where
        F: Fn(Arc<HoprBlockchainSafelessConnector<BlokliClient>>) -> T,
    {
        Ok(f(self.connector.clone()))
    }

    pub async fn deploy_safe(
        &self,
        inputs: SafeModuleDeploymentInputs,
    ) -> anyhow::Result<SafeModuleDeploymentResult> {
        let me = self.chain_key.public().to_address();

        let signed_tx = self
            .create_safe_deployment_payload(inputs)
            .await
            .map_err(anyhow::Error::from)?;

        let transaction = connector::blokli_client::BlokliTransactionClient::submit_transaction(
            self.connector.client(),
            signed_tx.as_ref(),
        )
        .await;

        tracing::debug!(?transaction, "safe deployment transaction submitted");

        let safe = self
            .connector
            .await_safe_deployment(SafeSelector::Owner(me), SAFE_RETRIEVAL_TIMEOUT)
            .await
            .map_err(anyhow::Error::from)?;

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
            let balance_wxhopr = hopr_lib::api::chain::ChainValues::balance::<
                WxHOPR,
                hopr_lib::Address,
            >(&connector, me)
            .await
            .map_err(anyhow::Error::from)?;
            let balance_xdai =
                hopr_lib::api::chain::ChainValues::balance::<XDai, hopr_lib::Address>(
                    &connector, me,
                )
                .await
                .map_err(anyhow::Error::from)?;

            Ok::<_, anyhow::Error>((balance_wxhopr, balance_xdai))
        })
        .await?
        .await
    }

    async fn create_safe_deployment_payload(
        &self,
        inputs: SafeModuleDeploymentInputs,
    ) -> anyhow::Result<Vec<u8>> {
        let info = self.connector.client().query_chain_info().await?;
        let contract_addrs: ContractAddresses = serde_json::from_str(&info.contract_addresses.0)
            .map_err(|e| {
                ConnectorError::TypeConversion(format!("contract addresses not a valid JSON: {e}"))
            })?;

        let chain_id = info.chain_id as u64;
        let nonce: hopli_lib::exports::alloy::primitives::Uint<256, 4> =
            hopli_lib::exports::alloy::primitives::U256::from_be_bytes(inputs.nonce.to_be_bytes());
        let token_amount = hopli_lib::exports::alloy::primitives::U256::from_be_bytes(
            inputs.token_amount.to_be_bytes(),
        );

        let payload = hopli_lib::payloads::edge_node_deploy_safe_module_and_maybe_include_node(
            contract_addrs.node_stake_factory,
            contract_addrs.token,
            contract_addrs.channels,
            nonce,
            token_amount,
            inputs
                .admins
                .into_iter()
                .map(|v| hopli_lib::Address::from_slice(v.as_ref()))
                .collect(),
            true,
        )?;

        let signed_payload = payload
            .sign_and_encode_to_eip2718(nonce.try_into()?, chain_id, None, &self.chain_key)
            .await?;

        Ok(Vec::from(signed_payload))
    }
}

#[derive(Clone, Debug)]
pub struct SafeModuleDeploymentInputs {
    pub token_amount: hopr_lib::U256,
    pub nonce: hopr_lib::U256,
    pub admins: Vec<Address>,
}

#[derive(Clone, Debug)]
pub struct SafeModuleDeploymentResult {
    pub safe_address: Address,
    pub module_address: Address,
}
