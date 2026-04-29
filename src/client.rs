use std::sync::Arc;

use futures::future::{AbortHandle, abortable};
use hopr_chain_connector::{BlockchainConnectorConfig, create_trustful_hopr_blokli_connector};
use hopr_lib::api::types::{crypto::prelude::OffchainPublicKey, primitive::prelude::Address};
use hopr_lib::builder::{ChainKeypair, Keypair, OffchainKeypair};
use hopr_lib::{HoprKeys, config::HoprLibConfig};
use hopr_reference::build_with_chain;
use strum::{AsRefStr, Display, EnumString};
use tracing::info;

use crate::errors::EdgliError;
use crate::new_blokli_client;

pub use hopr_chain_connector;

/// The concrete HOPR edge node type used by this client.
///
/// Edge nodes use the same [`hopr_reference::FullHopr`] type as full relay nodes —
/// both go through `build_full` with a [`hopr_reference::SharedTicketManager`]
/// so outgoing ticket indices and any unexpected incoming tickets are tracked
/// correctly. The session-server feature is not enabled, so no session server runs.
pub type HoprEdgeClient = hopr_reference::FullHopr;

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
    blokli_config: Option<BlockchainConnectorConfig>,
    f: F,
    visitor: impl Fn(EdgliInitState) + Send + 'static,
) -> anyhow::Result<AbortHandle>
where
    F: Fn(Arc<HoprEdgeClient>) -> T + Send + 'static,
    T: std::future::Future<Output = ()> + Send + 'static,
{
    let edgli = Edgli::new(cfg, hopr_keys, blokli_url, blokli_config, visitor).await?;
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

        visitor(EdgliInitState::CreatingNode);
        info!("Building HOPR edge node via hopr-reference");

        visitor(EdgliInitState::StartingNode);
        let node = build_with_chain(
            chain_key,
            packet_key,
            cfg,
            None, // use default FullNetworkDiscovery ProberConfig
            chain_connector,
        )
        .await?;

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
        use super::strategy::EdgeStrategyKind;
        use hopr_strategy::{
            auto_funding::AutoFundingStrategy,
            channel_finalizer::ClosureFinalizerStrategy,
            strategy::{MultiStrategy, Strategy},
        };

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
}
