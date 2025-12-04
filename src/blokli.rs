use hopr_chain_connector::{BasicPayloadGenerator, HoprBlockchainConnector, PayloadGenerator, TempDbBackend, blokli_client::{BlokliClient, BlokliClientConfig, BlokliQueryClient}, errors::ConnectorError};
use hopr_lib::Keypair;
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

pub async fn with_sefeless_blokli_connector<F, T>(chain_key: &ChainKeypair, blokli_provider: Url, f: F) -> anyhow::Result<T> 
where
    F: Fn(HoprBlockchainSafelessConnector<BlokliClient>) -> T
{
    let blokli_client = BlokliClient::new(
    blokli_provider.as_ref().parse()?,
    BlokliClientConfig {
        timeout: std::time::Duration::from_secs(5),
    });

    let info = blokli_client.query_chain_info().await?;
    let contract_addrs = serde_json::from_str(&info.contract_addresses.0)
        .map_err(|e| ConnectorError::TypeConversion(format!("contract addresses not a valid JSON: {e}")))?;

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