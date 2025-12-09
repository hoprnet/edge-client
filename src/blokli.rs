use hopr_chain_connector::{
    BasicPayloadGenerator, ContractAddresses, HoprBlockchainConnector, PayloadGenerator, TempDbBackend, blokli_client::{BlokliClient, BlokliClientConfig, BlokliQueryClient}, errors::ConnectorError
};
use hopr_lib::{Address, Keypair};
use url::Url;

pub use hopr_chain_connector as connector;
pub use hopr_lib::ChainKeypair;

pub const DEFAULT_BLOKLI_URL: &str = "https://blokli.stage.hoprnet.link";

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

pub async fn safe_creation_payload_generator(connector: &HoprBlockchainSafelessConnector<BlokliClient>, inputs: SafeModuleDeploymentInputs) -> anyhow::Result<Vec<u8>> {
    let info = connector.client().query_chain_info().await?;
    let contract_addrs: ContractAddresses = serde_json::from_str(&info.contract_addresses.0).map_err(|e| {
        ConnectorError::TypeConversion(format!("contract addresses not a valid JSON: {e}"))
    })?;

    // let safe_address = hopli_lib::payloads::edge_node_predict_safe_address(contract_addrs.node_stake_factory, contract_addrs.channels, inputs.nonce.into(), inputs.admins.into())?;
    // let data = hopli_lib::payloads::edge_node_deploy_safe_module_with_targets_and_nodes_payload();

    todo!("finish by returning the constructed payload")
}