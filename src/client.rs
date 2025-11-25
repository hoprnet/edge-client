use std::{path::{Path}, str::FromStr, sync::Arc};

pub use hopr_utils_chain_connector;

use futures::future::{AbortHandle, abortable};
use hopr_lib::{
    DummyCoverTrafficType, Hopr, HoprKeys, ToHex,
    config::HoprLibConfig,
};
use hopr_utils_chain_connector::{blokli_client::BlokliClient,{HoprBlockchainSafeConnector, init_blokli_connector}};
use hopr_utils_db_node::{HoprNodeDb, init_db};
use tracing::info;

use crate::errors::EdgliError;

pub async fn run_hopr_edge_node_with<F, T>(
    cfg: HoprLibConfig,
    db_data_path: &Path,
    hopr_keys: HoprKeys,
    f: F,
) -> anyhow::Result<AbortHandle>
where
    F: Fn(Arc<Hopr<Arc<HoprBlockchainSafeConnector<BlokliClient>>, HoprNodeDb>>) -> T,
    T: std::future::Future<Output = ()> + Send + 'static,
{
    let hopr = run_hopr_edge_node(cfg, db_data_path, hopr_keys).await?;

    let (proc, abort_handle) = abortable(f(hopr));
    let _jh = tokio::spawn(proc);

    Ok(abort_handle)
}

pub async fn run_hopr_edge_node(
    cfg: HoprLibConfig,
    db_data_path: &Path,
    hopr_keys: HoprKeys,
) -> anyhow::Result<Arc<Hopr<Arc<HoprBlockchainSafeConnector<BlokliClient>>, HoprNodeDb>>> {
    if let hopr_lib::config::HostType::IPv4(address) = &cfg.host.address {
        let ipv4: std::net::Ipv4Addr = std::net::Ipv4Addr::from_str(address)
            .map_err(|e| EdgliError::ConfigError(e.to_string()))?;

        if ipv4.is_loopback() && !cfg.transport.announce_local_addresses {
            Err(hopr_lib::errors::HoprLibError::GeneralError(
                "Cannot announce a loopback address".into(),
            ))?;
        }
    }

    info!(
        packet_key = hopr_lib::Keypair::public(&hopr_keys.packet_key).to_peerid_str(),
        blockchain_address = hopr_lib::Keypair::public(&hopr_keys.chain_key)
            .to_address()
            .to_hex(),
        "Node public identifiers"
    );

    // TODO: stored tickets need to be emitted from the Hopr object (addressed in #7575)
    //
    // edge_clients do not store tickets, since they are originators only.
    let (node_db, _stored_tickets) = init_db(
        &hopr_keys.chain_key,
        db_data_path
            .to_str()
            .ok_or_else(|| EdgliError::ConfigError("Invalid database path".into()))?,
        true,
        false,
    )
    .await?;

    let chain_connector = Arc::new(
        init_blokli_connector(
            &hopr_keys.chain_key,
            None, // read the provider URL from the default env variable
            cfg.safe_module.module_address,
        )
        .await?,
    );

    // Create the node instance
    info!("Creating the HOPR edge node instance from hopr-lib");
    let node = Arc::new(
        hopr_lib::Hopr::new(
            cfg.clone(),
            chain_connector,
            node_db,
            &hopr_keys.packet_key,
            &hopr_keys.chain_key,
        )
        .await?,
    );

    node.run(None::<DummyCoverTrafficType>).await?;

    Ok(node)
}
