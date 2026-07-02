use std::collections::HashSet;
use std::net::IpAddr;
use std::sync::Arc;

use futures::StreamExt;
use futures::future::{AbortHandle, abortable};
use hopr_chain_connector::{
    BlockchainConnectorConfig, create_blokli_client, create_trustful_hopr_blokli_connector,
};
use hopr_ct_full_network::ProberConfig as FullNetworkProberConfig;
use hopr_lib::api::{
    chain::{ChainReadSafeOperations, ChainValues as _, SafeSelector},
    node::{HasChainApi, IncentiveChannelOperations},
    types::{
        crypto::prelude::OffchainPublicKey,
        internal::channels::ChannelStatus,
        primitive::prelude::{Address, HoprBalance},
    },
};
use hopr_lib::builder::{ChainKeypair, HoprBuilder, Keypair, OffchainKeypair};
use hopr_lib::{HoprKeys, config::HoprLibConfig};
use hopr_network_graph::{ChannelGraph, SharedChannelGraph};
use hopr_ticket_manager::ticket_factory_from_chain;
use hopr_transport_p2p::{HoprLibp2pNetworkBuilder, HoprNetwork, PeerDiscovery};
use strum::{AsRefStr, Display, EnumString};
use tracing::info;

#[cfg(feature = "blokli")]
use crate::DEFAULT_BLOKLI_URL;
#[cfg(feature = "blokli")]
use hopr_chain_connector::HoprBlokliClientConfig;

use crate::errors::EdgliError;

#[cfg(feature = "blokli")]
fn build_blokli_client_config(
    blokli_url: Option<&str>,
    blokli_dns_override: Option<(IpAddr, Option<u16>)>,
) -> Result<HoprBlokliClientConfig, EdgliError> {
    let url = match blokli_url {
        Some(url) => url
            .parse()
            .map_err(|e| EdgliError::ConfigError(format!("invalid Blokli URL '{url}': {e}")))?,
        None => DEFAULT_BLOKLI_URL.clone(),
    };

    Ok(HoprBlokliClientConfig {
        url,
        dns_override: blokli_dns_override,
    })
}

/// The concrete HOPR edge node type used by this client.
pub type HoprEdgeClient = hopr_lib::Hopr<
    Arc<
        hopr_chain_connector::HoprBlockchainSafeConnector<
            hopr_chain_connector::blokli_client::BlokliClient,
        >,
    >,
    SharedChannelGraph,
    HoprNetwork,
    (),
>;

/// Represents the initialization states of the Edgli client.
/// Each state corresponds to a step in the `new()` function.
///
/// Both `as_ref()` and `to_string()` return the human-readable description
/// (strum's `AsRefStr` mirrors `Display`). The snake_case identifier given
/// by `#[strum(serialize = "...")]` is only used by `FromStr` for parsing.
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
/// `Edgli` is kept alive for the entire duration of `f` so that background tasks
/// remain active until `f` completes or the returned [`AbortHandle`] is used to cancel it.
pub async fn run_hopr_edge_node_with<F, T>(
    cfg: HoprLibConfig,
    hopr_keys: HoprKeys,
    blokli_url: Option<String>,
    blokli_dns_override: Option<(IpAddr, Option<u16>)>,
    blokli_connector_config: Option<BlockchainConnectorConfig>,
    f: F,
    visitor: impl Fn(EdgliInitState) + Send + 'static,
) -> anyhow::Result<AbortHandle>
where
    F: Fn(Arc<HoprEdgeClient>) -> T + Send + 'static,
    T: std::future::Future<Output = ()> + Send + 'static,
{
    let edgli = Edgli::new(
        cfg,
        hopr_keys,
        blokli_url,
        blokli_dns_override,
        blokli_connector_config,
        visitor,
    )
    .await?;
    let hopr = edgli.as_hopr();
    // Keep `edgli` alive inside the spawned task so the node and all its
    // background processes remain active until `f` completes (or the abort fires).
    let (proc, abort_handle) = abortable(async move {
        let _edgli = edgli;
        f(hopr).await;
    });
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
    /// * `cfg` – full HOPR node configuration; set `cfg.protocol.path_planner`
    ///   before calling to control the routing strategy.  Use
    ///   [`crate::latency_path_planner_config`] to obtain a latency-optimised default.
    /// * `hopr_keys` – chain and packet keypairs
    /// * `blokli_url` – optional Blokli client URL; defaults to the production endpoint
    /// * `blokli_dns_override` – optional DNS override for the Blokli client
    /// * `blokli_connector_config` – optional connector config overrides
    /// * `visitor` – called at each [`EdgliInitState`] transition for progress reporting
    pub async fn new(
        cfg: HoprLibConfig,
        hopr_keys: HoprKeys,
        blokli_url: Option<String>,
        blokli_dns_override: Option<(IpAddr, Option<u16>)>,
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
                create_blokli_client(build_blokli_client_config(
                    blokli_url.as_deref(),
                    blokli_dns_override,
                )?),
                cfg.safe_module.module_address,
            )
            .await?;
            connector.connect().await?;
            Arc::new(connector)
        };

        visitor(EdgliInitState::CreatingNode);
        info!("Building HOPR edge node directly via HoprBuilder");

        let probe_cfg = FullNetworkProberConfig {
            interval: std::time::Duration::from_secs(3),
            shuffle_ttl: std::time::Duration::from_secs(3),
            ..Default::default()
        };
        probe_cfg.validate_against_probe_timeout(cfg.protocol.probe.timeout)?;

        let ticket_factory = ticket_factory_from_chain(&chain_connector)
            .await
            .map_err(|e| anyhow::anyhow!("failed to seed ticket factory: {e}"))?;

        let path_cfg = cfg.protocol.path_planner;
        let graph: SharedChannelGraph = Arc::new(ChannelGraph::with_edge_params(
            *packet_key.public(),
            path_cfg.edge_penalty,
            path_cfg.min_ack_rate,
        ));
        let graph_for_ct = graph.clone();
        let safe_address = cfg.safe_module.safe_address;
        let module_address = cfg.safe_module.module_address;

        visitor(EdgliInitState::StartingNode);
        let node = Arc::new(
            HoprBuilder
                .with_identity(chain_key, packet_key)
                .with_config(cfg)
                .with_safe_module(&safe_address, &module_address)
                .with_chain_api(move |_ctx| chain_connector)
                .with_graph(move |_ctx| graph)
                .with_network(move |ctx| {
                    Box::pin(async move {
                        let peer_discovery_rx = ctx.take_peer_discovery_rx().ok_or(
                            hopr_lib::errors::HoprLibError::BuilderError(
                                "peer_discovery_rx already taken",
                            ),
                        )?;
                        let multiaddresses = vec![
                            (&ctx.cfg.host)
                                .try_into()
                                .map_err(hopr_lib::errors::HoprLibError::TransportError)?,
                        ];
                        let nb = HoprLibp2pNetworkBuilder::new(
                            peer_discovery_rx
                                .map(|(peer_id, addrs)| PeerDiscovery::Announce(peer_id, addrs)),
                        );
                        nb.build(
                            &ctx.packet_key,
                            multiaddresses,
                            "/hopr/mix/1.1.0",
                            ctx.cfg.protocol.transport.prefer_local_addresses,
                        )
                        .await
                        .map_err(|e| hopr_lib::errors::HoprLibError::GeneralError(e.to_string()))
                    })
                })
                .with_cover_traffic(move |ctx| {
                    hopr_ct_full_network::FullNetworkDiscovery::new(
                        *ctx.packet_key.public(),
                        probe_cfg,
                        graph_for_ct,
                    )
                })
                .build_edge(ticket_factory)
                .await?,
        );

        visitor(EdgliInitState::Ready);
        Ok(Self {
            hopr: node,
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

    /// Returns the ideal recommended wxHOPR and xDAI for this node to reach
    /// `cfg.target_open_channels` open channels to *currently connected* peers.
    ///
    /// Unlike [`minimum_balance_recommendation`](crate::strategy::minimum_balance_recommendation),
    /// this method discounts channels already open to connected peers, so the
    /// recommendation reflects only the additional stake the strategy needs.
    /// Channels open to disconnected peers are not counted — the strategy will
    /// close and replace them.
    pub async fn ideal_balance_recommendation(
        &self,
        cfg: &super::strategy::IncentiveConfiguration,
    ) -> anyhow::Result<super::strategy::BalanceRecommendation> {
        let chain = self.chain_api();
        let ticket_price = chain.minimum_ticket_price().await?;
        let win_prob = chain.minimum_incoming_ticket_win_prob().await?.as_f64();

        let source = HasChainApi::identity(&*self.hopr).node_address;
        let all_channels = IncentiveChannelOperations::channels_from(&*self.hopr, source)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        let connected: HashSet<_> = crate::traits::EdgeNodeApi::connected_peer_addresses(self)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?
            .into_iter()
            .collect();

        let open_to_connected = all_channels
            .iter()
            .filter(|c| c.status == ChannelStatus::Open && connected.contains(&c.destination))
            .count();

        let missing = cfg.target_open_channels.saturating_sub(open_to_connected);
        super::strategy::compute_balance_recommendation(ticket_price, win_prob, cfg, missing)
    }

    /// Returns a map of data-throughput capacities keyed by [`super::strategy::CapacityAllocator`].
    ///
    /// Open outgoing channels are keyed by `CapacityAllocator::Peer(address)`; the
    /// unallocated Safe balance is keyed by `CapacityAllocator::Safe`.  Each
    /// [`super::strategy::Capacity`] holds the wxHOPR stake, the floor number
    /// of session frames it can fund at the current ticket price, and the
    /// corresponding raw byte capacity (`expected_messages × SESSION_MTU`).
    pub async fn describe_current_capacity_allocations(
        &self,
    ) -> anyhow::Result<
        std::collections::HashMap<super::strategy::CapacityAllocator, super::strategy::Capacity>,
    > {
        let chain = self.chain_api();
        let ticket_price = chain.minimum_ticket_price().await?;
        let win_prob = chain.minimum_incoming_ticket_win_prob().await?.as_f64();

        let node_address = HasChainApi::identity(&*self.hopr).node_address;
        let channels = IncentiveChannelOperations::channels_from(&*self.hopr, node_address)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        let safe_balance = match ChainReadSafeOperations::safe_info(
            &chain,
            SafeSelector::NodeAddress(node_address),
        )
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?
        {
            Some(safe) => chain
                .balance(safe.address)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?,
            None => HoprBalance::zero(),
        };

        let mut map = std::collections::HashMap::new();
        for c in channels
            .into_iter()
            .filter(|c| c.status == ChannelStatus::Open)
        {
            let capacity = super::strategy::compute_capacity(c.balance, ticket_price, win_prob)?;
            map.insert(
                super::strategy::CapacityAllocator::Peer(c.destination),
                capacity,
            );
        }
        map.insert(
            super::strategy::CapacityAllocator::Safe,
            super::strategy::compute_capacity(safe_balance, ticket_price, win_prob)?,
        );

        Ok(map)
    }

    /// Run a node with HOPR edge strategies integrated.
    ///
    /// The default reactor runs a single [`ChannelLifecycleStrategy`] which
    /// owns open / fund / close / finalize for outgoing payment channels.
    ///
    /// Returns an [`AbortHandle`] that stops the strategy reactor when aborted.
    #[cfg(feature = "blokli")]
    pub fn run_reactor_from_cfg(
        &self,
        cfg: super::strategy::MultiStrategyConfig,
    ) -> anyhow::Result<AbortHandle> {
        use super::strategy::EdgeStrategyKind;
        use hopr_strategy::{
            channel_lifecycle::ChannelLifecycleStrategy,
            strategy::{MultiStrategy, Strategy},
        };

        let node = self.hopr.clone();

        let strategies = cfg
            .strategies
            .into_iter()
            .map(|kind| -> Box<dyn Strategy + Send> {
                match kind {
                    EdgeStrategyKind::ChannelLifecycle(sub_cfg) => {
                        ChannelLifecycleStrategy::new(sub_cfg).build(Arc::clone(&node))
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
    use std::net::{IpAddr, Ipv4Addr};

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
        // Parse the exact snake_case serialize form strum would derive for it.
        assert!("initializing_database".parse::<EdgliInitState>().is_err());
        // Exhaustive match — the compiler enforces this if a new variant is added.
        fn _exhaustive(s: EdgliInitState) {
            match s {
                EdgliInitState::ValidatingConfig
                | EdgliInitState::IdentifyingNode
                | EdgliInitState::ConnectingBlockchain
                | EdgliInitState::CreatingNode
                | EdgliInitState::StartingNode
                | EdgliInitState::Ready => {}
            }
        }
    }

    #[cfg(feature = "blokli")]
    #[test]
    fn build_blokli_client_config_uses_default_url() {
        let config = build_blokli_client_config(None, None).unwrap();
        assert_eq!(config.url, *DEFAULT_BLOKLI_URL);
        assert_eq!(config.dns_override, None);
    }

    #[cfg(feature = "blokli")]
    #[test]
    fn build_blokli_client_config_keeps_dns_override() {
        let dns_override = Some((IpAddr::V4(Ipv4Addr::new(10, 1, 2, 1)), Some(3002)));
        let config =
            build_blokli_client_config(Some("https://blokli.example.com"), dns_override).unwrap();
        assert_eq!(config.url.as_str(), "https://blokli.example.com/");
        assert_eq!(config.dns_override, dns_override);
    }

    #[cfg(feature = "blokli")]
    #[test]
    fn build_blokli_client_config_rejects_invalid_url() {
        let error = build_blokli_client_config(Some("not a url"), None).unwrap_err();
        assert_eq!(
            error.to_string(),
            "configuration error: 'invalid Blokli URL 'not a url': relative URL without a base'"
        );
    }
}
