use std::{path::Path, str::FromStr, sync::Arc};

use futures::future::{AbortHandle, abortable};
use hopr_chain_connector::{
    BlockchainConnectorConfig, HoprBlockchainSafeConnector, blokli_client::BlokliClient,
    create_trustful_hopr_blokli_connector,
};
use hopr_db_node::{HoprNodeDb, init_hopr_node_db};
use hopr_lib::{Hopr, HoprKeys, ToHex, api::chain::ChainEvents, config::HoprLibConfig};
use strum::{AsRefStr, EnumString};
use tracing::info;

use crate::errors::EdgliError;
use crate::new_blokli_client;

pub use hopr_chain_connector;

pub type HoprEdgeClient = Hopr<Arc<HoprBlockchainSafeConnector<BlokliClient>>, HoprNodeDb>;

/// Represents the initialization states of the Edgli client.
/// Each state corresponds to a step in the `new()` function.
#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumString, AsRefStr)]
#[strum(serialize_all = "snake_case")]
pub enum EdgliInitState {
    /// Validating the host configuration and network address settings
    #[strum(to_string = "Validating host configuration...")]
    ValidatingConfig,

    /// Logging node public identifiers (packet key and blockchain address)
    #[strum(to_string = "Identifying node public keys...")]
    IdentifyingNode,

    /// Initializing the node database for persistent storage
    #[strum(to_string = "Initializing node database...")]
    InitializingDatabase,

    /// Creating and connecting to the blockchain via the chain connector
    #[strum(to_string = "Establishing blockchain connection to read the chain events...")]
    ConnectingBlockchain,

    /// Creating the HOPR edge node instance from hopr-lib
    #[strum(to_string = "Creating HOPR edge node instance...")]
    CreatingNode,

    /// Starting the node and its network protocols
    #[strum(to_string = "Starting node and network protocols...")]
    StartingNode,

    /// Initialization completed successfully
    #[strum(to_string = "Initialization complete.")]
    Ready,
}

pub async fn run_hopr_edge_node_with<F, T, V>(
    cfg: HoprLibConfig,
    db_data_path: &Path,
    hopr_keys: HoprKeys,
    blokli_url: Option<String>,
    f: F,
    visitor: Option<V>,
) -> anyhow::Result<AbortHandle>
where
    F: Fn(Arc<HoprEdgeClient>) -> T,
    T: std::future::Future<Output = ()> + Send + 'static,
    V: Fn(EdgliInitState) + Send + 'static,
{
    let edgli = Edgli::new(cfg, db_data_path, hopr_keys, blokli_url, visitor).await?;
    let (proc, abort_handle) = abortable(f(edgli.hopr));
    let _jh = tokio::spawn(proc);
    Ok(abort_handle)
}

#[derive(Clone)]
pub struct Edgli {
    hopr: Arc<HoprEdgeClient>,
    /// Serves only for a potential later registration of strategies that need
    /// direct access to the blockchain connector until a more significant refactor.
    #[cfg(feature = "blokli")]
    blokli_connector: Arc<HoprBlockchainSafeConnector<BlokliClient>>,
}

impl std::ops::Deref for Edgli {
    type Target = HoprEdgeClient;

    fn deref(&self) -> &Self::Target {
        &self.hopr
    }
}

impl Edgli {
    pub async fn new<V>(
        cfg: HoprLibConfig,
        db_data_path: &Path,
        hopr_keys: HoprKeys,
        blokli_url: Option<String>,
        visitor: Option<V>,
    ) -> anyhow::Result<Self>
    where
    V: Fn(EdgliInitState) + Send + 'static,
    {
        if let Some(ref v) = visitor {
            v(EdgliInitState::ValidatingConfig);
        }
        if let hopr_lib::config::HostType::IPv4(address) = &cfg.host.address {
            let ipv4: std::net::Ipv4Addr = std::net::Ipv4Addr::from_str(address)
                .map_err(|e| EdgliError::ConfigError(e.to_string()))?;

            if ipv4.is_loopback() && !cfg.protocol.transport.prefer_local_addresses {
                Err(hopr_lib::errors::HoprLibError::GeneralError(
                    "Cannot announce a loopback address".into(),
                ))?;
            }
        }

        if let Some(ref v) = visitor {
            v(EdgliInitState::IdentifyingNode);
        }
        info!(
            packet_key = hopr_lib::Keypair::public(&hopr_keys.packet_key).to_peerid_str(),
            blockchain_address = hopr_lib::Keypair::public(&hopr_keys.chain_key)
                .to_address()
                .to_hex(),
            "Node public identifiers"
        );

        // edge_clients do not store tickets, since they are originators only.
        if let Some(ref v) = visitor {
            v(EdgliInitState::InitializingDatabase);
        }
        let node_db = init_hopr_node_db(
            db_data_path
                .to_str()
                .ok_or_else(|| EdgliError::ConfigError("Invalid database path".into()))?,
            true,
            false,
        )
        .await?;

        #[cfg(feature = "blokli")]
        let chain_connector = {
            if let Some(ref v) = visitor {
                v(EdgliInitState::ConnectingBlockchain);
            }
            let mut connector = create_trustful_hopr_blokli_connector(
                &hopr_keys.chain_key,
                BlockchainConnectorConfig {
                    tx_confirm_timeout: std::time::Duration::from_secs(90),
                    connection_timeout: std::time::Duration::from_mins(2),
                },
                new_blokli_client(blokli_url.map(|url| url.parse()).transpose()?),
                cfg.safe_module.module_address,
            )
            .await?;
            connector.connect().await?;

            Arc::new(connector)
        };

        // Create the node instance
        if let Some(ref v) = visitor {
            v(EdgliInitState::CreatingNode);
        }
        info!("Creating the HOPR edge node instance from hopr-lib");
        let node = Arc::new(
            hopr_lib::Hopr::new(
                (&hopr_keys.chain_key, &hopr_keys.packet_key),
                #[cfg(feature = "blokli")]
                chain_connector.clone(),
                node_db,
                cfg.clone(),
            )
            .await?,
        );

        if let Some(ref v) = visitor {
            v(EdgliInitState::StartingNode);
        }
        node.run(hopr_ct_telemetry::ImmediateNeighborProber::new(
            Default::default(),
        ))
        .await?;

        if let Some(ref v) = visitor {
            v(EdgliInitState::Ready);
        }
        Ok(Self {
            hopr: node,
            #[cfg(feature = "blokli")]
            blokli_connector: chain_connector,
        })
    }

    pub fn as_hopr(&self) -> Arc<HoprEdgeClient> {
        self.hopr.clone()
    }

    /// Run a node with HOPR edge strategies integrated.
    ///
    /// Edge strategies comprise:
    /// 1. automatically funding the channels when out of funds
    /// 2. automatically closing the channels in pending to close state
    #[cfg(feature = "blokli")]
    pub fn run_reactor_from_cfg(
        &self,
        cfg: super::strategy::MultiStrategyConfig,
    ) -> anyhow::Result<AbortHandle> {
        let multi_strategy = Arc::new(hopr_strategy::strategy::MultiStrategy::new(
            cfg,
            self.blokli_connector.clone(),
            self.hopr.redemption_requests()?,
        ));

        Ok(hopr_strategy::stream_events_to_strategy_with_tick(
            multi_strategy,
            self.blokli_connector.subscribe()?,
            self.hopr.subscribe_winning_tickets(),
            std::time::Duration::from_secs(60),
            self.hopr.me_onchain(),
        ))
    }
}
