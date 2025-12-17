use hopr_chain_connector::{
    BasicPayloadGenerator, ContractAddresses, HoprBlockchainConnector, PayloadGenerator,
    TempDbBackend,
    blokli_client::{BlokliClient, BlokliClientConfig, BlokliQueryClient},
    errors::ConnectorError,
};
use hopr_lib::{Address, IntoEndian, Keypair, exports::types::chain::prelude::SignableTransaction};
use url::Url;

pub use hopr_chain_connector as connector;
pub use hopr_lib::ChainKeypair;

pub const DEFAULT_BLOKLI_URL: &str = "https://blokli.staging.hoprnet.link";

pub type HoprBlockchainSafelessConnector<C> = HoprBlockchainConnector<
    C,
    TempDbBackend,
    BasicPayloadGenerator,
    <BasicPayloadGenerator as PayloadGenerator>::TxRequest,
>;

pub async fn with_safeless_blokli_connector<F, T>(
    chain_key: &ChainKeypair,
    blokli_provider: Url,
    f: F,
) -> anyhow::Result<T>
where
    F: Fn(HoprBlockchainSafelessConnector<BlokliClient>) -> T,
{
    let blokli_client = BlokliClient::new(
        blokli_provider.as_ref().parse()?,
        BlokliClientConfig {
            timeout: std::time::Duration::from_secs(5),
            ..Default::default()
        },
    );

    let info = blokli_client.query_chain_info().await?;
    let contract_addrs = serde_json::from_str(&info.contract_addresses.0).map_err(|e| {
        ConnectorError::TypeConversion(format!("contract addresses not a valid JSON: {e}"))
    })?;

    let payload_gen = BasicPayloadGenerator::new(chain_key.public().to_address(), contract_addrs);

    let connector = HoprBlockchainConnector::new(
        chain_key.clone(),
        Default::default(),
        blokli_client,
        TempDbBackend::new()?,
        payload_gen,
    );

    Ok(f(connector))
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

pub async fn safe_creation_payload_generator(
    chain_key: &ChainKeypair,
    connector: &HoprBlockchainSafelessConnector<BlokliClient>,
    inputs: SafeModuleDeploymentInputs,
) -> anyhow::Result<Vec<u8>> {
    let info = connector.client().query_chain_info().await?;
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
        .sign_and_encode_to_eip2718(nonce.try_into()?, chain_id, None, chain_key)
        .await?;

    Ok(Vec::from(signed_payload))
}
