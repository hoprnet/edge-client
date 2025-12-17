use std::{path::Path, str::FromStr, sync::Arc};

pub use hopr_chain_connector;
pub type HoprEdgeClient = Hopr<Arc<HoprBlockchainSafeConnector<BlokliClient>>, HoprNodeDb>;

use futures::future::{AbortHandle, abortable};
use hopr_chain_connector::{
    blokli_client::BlokliClient,
    {HoprBlockchainSafeConnector, init_blokli_connector},
};
use hopr_db_node::{HoprNodeDb, init_hopr_node_db};
use hopr_lib::{
    Hopr, HoprBalance, HoprKeys, Keypair, ToHex,
    api::chain::{ChainEvents, HoprChainApi},
    config::HoprLibConfig,
};
use hopr_strategy::{
    Strategy, auto_funding::AutoFundingStrategyConfig,
    channel_finalizer::ClosureFinalizerStrategyConfig, strategy::MultiStrategyConfig,
};
use tracing::info;

use crate::errors::EdgliError;

#[derive(Debug, PartialEq, Eq, Hash)]
pub enum EdgeProcessType {
    Hopr,
    Strategy,
}

pub async fn run_hopr_edge_node_with<F, T>(
    cfg: HoprLibConfig,
    db_data_path: &Path,
    hopr_keys: HoprKeys,
    f: F,
) -> anyhow::Result<AbortHandle>
where
    F: Fn(Arc<HoprEdgeClient>) -> T,
    T: std::future::Future<Output = ()> + Send + 'static,
{
    let chain_connector = Arc::new(
        init_blokli_connector(
            &hopr_keys.chain_key,
            None, // read the provider URL from the default env variable for now
            cfg.safe_module.module_address,
        )
        .await?,
    );

    let hopr = run_hopr_edge_node(cfg, db_data_path, chain_connector, hopr_keys).await?;

    let (proc, abort_handle) = abortable(f(hopr));
    let _jh = tokio::spawn(proc);

    Ok(abort_handle)
}

/// Run a node with HOPR edge strategies integrated.
///
/// Edge strategies comprise:
/// 1. automatically funding the channels when out of funds
/// 2. automatically closing the channels in pending to close state
pub async fn run_hopr_edge_node_with_edge_strategies_and<F, T>(
    cfg: HoprLibConfig,
    db_data_path: &Path,
    hopr_keys: HoprKeys,
    top_up_amount: HoprBalance,
    min_channel_balance: HoprBalance,
    f: F,
) -> anyhow::Result<(
    Arc<HoprEdgeClient>,
    std::collections::HashMap<EdgeProcessType, AbortHandle>,
)>
where
    F: Fn(Arc<HoprEdgeClient>) -> T,
    T: std::future::Future<Output = ()> + Send + 'static,
{
    let mut processes = std::collections::HashMap::new();

    let chain_connector = Arc::new(
        init_blokli_connector(
            &hopr_keys.chain_key,
            None, // read the provider URL from the default env variable for now
            cfg.safe_module.module_address,
        )
        .await?,
    );

    let chain_events = chain_connector.subscribe()?;
    let my_address = hopr_keys.chain_key.public().to_address();
    let chain_connector_strategy = chain_connector.clone();

    let hopr = run_hopr_edge_node(cfg, db_data_path, chain_connector, hopr_keys).await?;

    let strategy_cfg = MultiStrategyConfig {
        on_fail_continue: true,
        allow_recursive: false,
        execution_interval: std::time::Duration::from_secs(60),
        strategies: vec![
            Strategy::AutoFunding(AutoFundingStrategyConfig {
                min_stake_threshold: min_channel_balance,
                funding_amount: top_up_amount,
            }),
            Strategy::ClosureFinalizer(ClosureFinalizerStrategyConfig {
                max_closure_overdue: std::time::Duration::from_secs(300),
            }),
        ],
    };

    let multi_strategy = Arc::new(hopr_strategy::strategy::MultiStrategy::new(
        strategy_cfg,
        chain_connector_strategy,
        hopr.redemption_requests()?,
    ));

    processes.insert(
        EdgeProcessType::Strategy,
        hopr_strategy::stream_events_to_strategy_with_tick(
            multi_strategy,
            chain_events,
            hopr.subscribe_winning_tickets(),
            std::time::Duration::from_secs(5),
            my_address,
        ),
    );

    let (proc, abort_handle) = abortable(f(hopr.clone()));
    let _jh = tokio::spawn(proc);

    processes.insert(EdgeProcessType::Hopr, abort_handle);

    Ok((hopr, processes))
}

pub async fn run_hopr_edge_node<Chain>(
    cfg: HoprLibConfig,
    db_data_path: &Path,
    chain_connector: Chain,
    hopr_keys: HoprKeys,
) -> anyhow::Result<Arc<Hopr<Chain, HoprNodeDb>>>
where
    Chain: HoprChainApi + Clone + Send + Sync + 'static,
{
    if let hopr_lib::config::HostType::IPv4(address) = &cfg.host.address {
        let ipv4: std::net::Ipv4Addr = std::net::Ipv4Addr::from_str(address)
            .map_err(|e| EdgliError::ConfigError(e.to_string()))?;

        if ipv4.is_loopback() && !cfg.protocol.transport.prefer_local_addresses {
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
    let node_db = init_hopr_node_db(
        db_data_path
            .to_str()
            .ok_or_else(|| EdgliError::ConfigError("Invalid database path".into()))?,
        true,
        false,
    )
    .await?;

    // Create the node instance
    info!("Creating the HOPR edge node instance from hopr-lib");
    let node = Arc::new(
        hopr_lib::Hopr::new(
            (&hopr_keys.chain_key, &hopr_keys.packet_key),
            chain_connector,
            node_db,
            cfg.clone(),
        )
        .await?,
    );

    node.run(hopr_ct_telemetry::ImmediateNeighborProber::new(
        Default::default(),
    ))
    .await?;

    Ok(node)
}
