use std::sync::Arc;

use futures::channel::mpsc::channel;
use futures::future::AbortHandle;
use futures::{SinkExt, StreamExt};
use hopr_chain_connector::{
    BlockchainConnectorConfig, HoprBlockchainSafeConnector, blokli_client::BlokliClient,
    create_trustful_hopr_blokli_connector,
};
use hopr_ct_immediate::{ImmediateNeighborProber, ProberConfig};
use hopr_lib::api::chain::{ChainEvents, StateSyncOptions};
use hopr_lib::api::types::chain::chain_events::ChainEvent;
use hopr_lib::api::types::{
    crypto::prelude::OffchainPublicKey,
    primitive::prelude::Address,
};
use hopr_lib::builder::{ChainKeypair, HoprBuilder, Keypair, OffchainKeypair};
use hopr_lib::{Hopr, HoprKeys, config::HoprLibConfig};
use hopr_network_graph::{ChannelGraph, SharedChannelGraph};
use hopr_ticket_manager::{HoprTicketFactory, MemoryStore};
use hopr_transport_p2p::{HoprLibp2pNetworkBuilder, HoprNetwork, PeerDiscovery};
use strum::{AsRefStr, Display, EnumString};
use tracing::info;

use crate::errors::EdgliError;
use crate::new_blokli_client;

pub use hopr_chain_connector;

/// The concrete HOPR edge node type used by this client.
///
/// An edge node (entry/exit node) has no ticket management (`TMgr = ()`),
/// since it originates packets but does not relay or redeem tickets.
pub type HoprEdgeClient = Hopr<
    Arc<HoprBlockchainSafeConnector<BlokliClient>>,
    SharedChannelGraph,
    HoprNetwork,
    (), // Edge nodes have no ticket management
>;

/// Represents the initialization states of the Edgli client.
/// Each state corresponds to a step in the `new()` function.
///
/// `as_ref()` returns a snake_case machine-readable identifier;
/// `to_string()` returns a human-readable description for display.
#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumString, AsRefStr, Display)]
pub enum EdgliInitState {
    /// Validating the host configuration and network address settings
    #[strum(
        serialize = "validating_config",
        to_string = "Validating host configuration..."
    )]
    ValidatingConfig,

    /// Logging node public identifiers (packet key and blockchain address)
    #[strum(
        serialize = "identifying_node",
        to_string = "Identifying node public keys..."
    )]
    IdentifyingNode,

    /// Creating and connecting to the blockchain via the chain connector
    #[strum(
        serialize = "connecting_blockchain",
        to_string = "Establishing blockchain connection to read the chain events..."
    )]
    ConnectingBlockchain,

    /// Building the HOPR edge node instance via the type-state builder
    #[strum(
        serialize = "creating_node",
        to_string = "Creating HOPR edge node instance..."
    )]
    CreatingNode,

    /// Starting the node and its network protocols
    #[strum(
        serialize = "starting_node",
        to_string = "Starting node and network protocols..."
    )]
    StartingNode,

    /// Initialization completed successfully
    #[strum(serialize = "ready", to_string = "Initialization complete.")]
    Ready,
}

/// Spawns an abortable task that drives a user-supplied closure over the running node.
///
/// Returns an [`AbortHandle`] that stops the closure task when aborted.
/// The node itself remains alive as long as the `Edgli` the closure captured is kept alive.
pub async fn run_hopr_edge_node_with<F, T>(
    cfg: HoprLibConfig,
    hopr_keys: HoprKeys,
    blokli_url: Option<String>,
    blokli_config: Option<BlockchainConnectorConfig>,
    f: F,
    visitor: impl Fn(EdgliInitState) + Send + 'static,
) -> anyhow::Result<AbortHandle>
where
    F: Fn(Arc<HoprEdgeClient>) -> T,
    T: std::future::Future<Output = ()> + Send + 'static,
{
    let edgli = Edgli::new(cfg, hopr_keys, blokli_url, blokli_config, visitor).await?;
    let (proc, abort_handle) = futures::future::abortable(f(edgli.hopr));
    let _jh = tokio::spawn(proc);
    Ok(abort_handle)
}

/// The primary edge-client handle.
///
/// Wraps [`HoprEdgeClient`] and adds Blokli-specific functionality such as
/// the auto-funding/closure-finalizer reactor. Implements [`std::ops::Deref`]
/// to [`HoprEdgeClient`], so the full `hopr-lib` trait API is accessible
/// directly on `Edgli` instances.
#[derive(Clone)]
pub struct Edgli {
    hopr: Arc<HoprEdgeClient>,
    /// The node's packet-layer public key, stored at construction for peer-ID access.
    packet_public_key: OffchainPublicKey,
}

impl std::ops::Deref for Edgli {
    type Target = HoprEdgeClient;

    fn deref(&self) -> &Self::Target {
        &self.hopr
    }
}

impl Edgli {
    /// Constructs and starts an edge HOPR node.
    ///
    /// # Arguments
    /// * `cfg` – full HOPR node configuration
    /// * `hopr_keys` – chain and packet keypairs
    /// * `blokli_url` – optional Blokli endpoint URL; defaults to the production endpoint
    /// * `blokli_connector_config` – optional connector config overrides
    /// * `visitor` – called at each [`EdgliInitState`] transition for progress reporting
    pub async fn new(
        cfg: HoprLibConfig,
        hopr_keys: HoprKeys,
        blokli_url: Option<String>,
        blokli_connector_config: Option<BlockchainConnectorConfig>,
        visitor: impl Fn(EdgliInitState) + Send + 'static,
    ) -> anyhow::Result<Self> {
        visitor(EdgliInitState::ValidatingConfig);
        if let hopr_lib::config::HostType::IPv4(address) = &cfg.host.address {
            let ipv4: std::net::Ipv4Addr = address
                .parse()
                .map_err(|e| EdgliError::ConfigError(format!("{e}")))?;

            if ipv4.is_loopback() && !cfg.protocol.transport.prefer_local_addresses {
                Err(hopr_lib::errors::HoprLibError::GeneralError(
                    "Cannot announce a loopback address".into(),
                ))?;
            }
        }

        let chain_key: &ChainKeypair = &hopr_keys.chain_key;
        let packet_key: &OffchainKeypair = &hopr_keys.packet_key;
        let packet_public_key: OffchainPublicKey = *packet_key.public();

        visitor(EdgliInitState::IdentifyingNode);
        info!(
            packet_key = packet_key.public().to_peerid_str(),
            blockchain_address = %chain_key.public().to_address(),
            "Node public identifiers"
        );

        #[cfg(feature = "blokli")]
        let chain_connector = {
            let blokli_config = blokli_connector_config.unwrap_or_default();
            visitor(EdgliInitState::ConnectingBlockchain);
            let mut connector = create_trustful_hopr_blokli_connector(
                chain_key,
                blokli_config,
                new_blokli_client(blokli_url.map(|url| url.parse()).transpose()?),
                cfg.safe_module.module_address,
            )
            .await?;
            connector.connect().await?;
            Arc::new(connector)
        };

        // Wire chain → peer-discovery: announce events feed libp2p peer discovery.
        let (peer_discovery_tx, peer_discovery_rx) = channel(2048);
        {
            let chain_events = chain_connector
                .subscribe_with_state_sync([StateSyncOptions::PublicAccounts])
                .map_err(|e| anyhow::anyhow!("failed to subscribe to chain events: {e}"))?;
            let tx = peer_discovery_tx;
            tokio::spawn(async move {
                chain_events
                    .for_each(|event| {
                        let mut tx = tx.clone();
                        async move {
                            if let ChainEvent::Announcement(account) = event {
                                let peer_id: hopr_lib::api::PeerId = account.public_key.into();
                                if let Err(error) = tx
                                    .send(PeerDiscovery::Announce(
                                        peer_id,
                                        account.get_multiaddrs().to_vec(),
                                    ))
                                    .await
                                {
                                    tracing::error!(
                                        %peer_id, %error,
                                        "failed to forward peer discovery announcement"
                                    );
                                }
                            }
                        }
                    })
                    .await;
            });
        }

        // Build the network graph. Cloned reference shared with cover-traffic prober.
        let path_cfg = cfg.protocol.path_planner;
        let graph: SharedChannelGraph = Arc::new(ChannelGraph::with_edge_params(
            *packet_key.public(),
            path_cfg.edge_penalty,
            path_cfg.min_ack_rate,
        ));
        let graph_for_ct = graph.clone();

        // Ticket factory for outgoing multihop tickets. Edge nodes do not manage
        // incoming tickets (TMgr = ()), so we do not need a paired ticket manager here.
        // Wrap in Arc: build_edge requires TFact: Clone, and Arc<T> is Clone for any T.
        let ticket_factory = Arc::new(HoprTicketFactory::new(MemoryStore::default()));

        let safe_address = cfg.safe_module.safe_address;
        let module_address = cfg.safe_module.module_address;

        visitor(EdgliInitState::CreatingNode);
        info!("Building HOPR edge node via type-state builder");

        let chain_connector_for_builder = chain_connector.clone();

        visitor(EdgliInitState::StartingNode);
        let node = HoprBuilder::new()
            .with_identity(chain_key, packet_key)
            .with_config(cfg.clone())
            .with_safe_module(&safe_address, &module_address)
            .with_chain_api(move |_ctx| chain_connector_for_builder)
            .with_graph(move |_ctx| graph)
            .with_network(move |ctx| {
                Box::pin(async move {
                    let multiaddresses = vec![
                        (&ctx.cfg.host)
                            .try_into()
                            .expect("host config must be a valid multiaddress"),
                    ];
                    HoprLibp2pNetworkBuilder::new(peer_discovery_rx)
                        .build(
                            &ctx.packet_key,
                            multiaddresses,
                            "/hopr/mix/1.1.0",
                            ctx.cfg.protocol.transport.prefer_local_addresses,
                        )
                        .await
                        .expect("network must be constructible")
                })
            })
            .with_cover_traffic(move |_ctx| {
                ImmediateNeighborProber::new(ProberConfig::default(), graph_for_ct)
            })
            .build_edge(ticket_factory)
            .await?;

        visitor(EdgliInitState::Ready);
        Ok(Self {
            hopr: Arc::new(node),
            packet_public_key,
        })
    }

    /// Returns the shared [`HoprEdgeClient`] handle.
    pub fn as_hopr(&self) -> Arc<HoprEdgeClient> {
        self.hopr.clone()
    }

    /// The node's on-chain address.
    ///
    /// Convenience wrapper replacing the removed `Hopr::me_onchain()` method.
    pub fn me_onchain(&self) -> Address {
        use hopr_lib::api::node::HasChainApi;
        self.hopr.identity().node_address
    }

    /// The node's off-chain peer ID as a string (libp2p representation).
    ///
    /// Derived from the packet key stored at construction time.
    pub fn me_peer_id(&self) -> String {
        self.packet_public_key.to_peerid_str()
    }

    /// Run a node with HOPR edge strategies integrated.
    ///
    /// Edge strategies comprise:
    /// 1. Automatically funding channels that fall below a stake threshold
    /// 2. Automatically closing channels stuck in pending-close state
    ///
    /// Returns an [`AbortHandle`] that stops the strategy reactor when aborted.
    #[cfg(feature = "blokli")]
    pub fn run_reactor_from_cfg(
        &self,
        cfg: super::strategy::MultiStrategyConfig,
    ) -> anyhow::Result<AbortHandle> {
        use hopr_strategy::{
            auto_funding::AutoFundingStrategy,
            channel_finalizer::ClosureFinalizerStrategy,
            strategy::{MultiStrategy, Strategy},
        };
        use super::strategy::EdgeStrategyKind;

        let interval = cfg.execution_interval;
        let node = self.hopr.clone();

        let strategies = cfg
            .strategies
            .into_iter()
            .map(|kind| -> Box<dyn Strategy + Send> {
                match kind {
                    EdgeStrategyKind::AutoFunding(sub_cfg) => {
                        AutoFundingStrategy::new(sub_cfg, interval).build(Arc::clone(&node))
                    }
                    EdgeStrategyKind::ClosureFinalizer(sub_cfg) => {
                        ClosureFinalizerStrategy::new(sub_cfg, interval).build(Arc::clone(&node))
                    }
                }
            })
            .collect();

        let mut multi_strategy = MultiStrategy::new(strategies);

        let (abortable, abort_handle) = futures::future::abortable(async move {
            if let Err(e) = multi_strategy.run().await {
                tracing::error!(%e, "edge strategy reactor failed");
            }
        });

        tokio::spawn(abortable);
        Ok(abort_handle)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_state_as_ref_matches_to_string() {
        // strum's AsRefStr intentionally returns the same value as to_string()
        // (see strum comment: "always enum.as_ref().to_string() == enum.to_string()")
        assert_eq!(
            EdgliInitState::ValidatingConfig.as_ref(),
            EdgliInitState::ValidatingConfig.to_string()
        );
        assert_eq!(
            EdgliInitState::Ready.as_ref(),
            EdgliInitState::Ready.to_string()
        );
    }

    #[test]
    fn init_state_strum_to_string() {
        assert_eq!(
            EdgliInitState::ValidatingConfig.to_string(),
            "Validating host configuration..."
        );
        assert_eq!(
            EdgliInitState::ConnectingBlockchain.to_string(),
            "Establishing blockchain connection to read the chain events..."
        );
        assert_eq!(
            EdgliInitState::StartingNode.to_string(),
            "Starting node and network protocols..."
        );
        assert_eq!(
            EdgliInitState::Ready.to_string(),
            "Initialization complete."
        );
    }

    #[test]
    fn init_state_all_variants_covered() {
        let all = [
            EdgliInitState::ValidatingConfig,
            EdgliInitState::IdentifyingNode,
            EdgliInitState::ConnectingBlockchain,
            EdgliInitState::CreatingNode,
            EdgliInitState::StartingNode,
            EdgliInitState::Ready,
        ];
        // Verify each variant has a non-empty display string
        for state in &all {
            assert!(!state.to_string().is_empty(), "{state:?} has empty display");
        }
    }

    #[test]
    fn no_initializing_database_state() {
        // Ensure the removed InitializingDatabase variant does not exist.
        // This test documents the intentional removal.
        let s = "InitializingDatabase";
        assert!(
            s.parse::<EdgliInitState>().is_err(),
            "InitializingDatabase variant must not exist in EdgliInitState"
        );
    }
}
