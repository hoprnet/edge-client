pub mod constants;
// pub mod contracts;
pub mod errors;

use hopr_utils_chain_connector::blokli_client::{BlokliClient, BlokliClientConfig};
use url::Url;
pub use hopr_utils_chain_connector::blokli_client;

const DEFAULT_BLOKLI_URL: &str = "https://blokli.prod.hoprnet.org";

pub fn with_blokli_client<F, T>(blokli_provider: Url, f: F) -> anyhow::Result<T> 
where 
    F: Fn(&BlokliClient) -> T
{
    let blokli_client = BlokliClient::new(
        blokli_provider.as_ref().parse()?,
        BlokliClientConfig {
            timeout: std::time::Duration::from_secs(5),
        });

    Ok(f(&blokli_client))
}