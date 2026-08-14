//! Discovery of `gvpn:exit` nodes registered in the on-chain `HoprServiceRegistry`.
//!
//! Initial reads go through [`hopr_chain_connector::HoprBlockchainReader`], which does not require
//! the full connector to be initialized. Live updates use the domain event stream of the connected
//! chain connector, keeping Blokli wire types behind `hopr-chain-connector`.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::{Duration, SystemTime};

use futures::{Stream, StreamExt};
use hopr_chain_connector::HoprBlockchainReader;
use hopr_lib::api::chain::{
    ChainEvent, ChainEvents, ChainReadServiceOperations, ServiceEntry, ServiceSelector,
};
use hopr_lib::api::types::internal::prelude::ServiceType;
use hopr_lib::api::types::primitive::prelude::Address;
use serde::Deserialize;

use crate::endpoint::BlokliEndpoint;

/// `gvpn:exit` metadata, versioned per `design-service-registry-v3.md` §3.2 (in-band schema
/// discriminator; each service type documents its own encoding).
#[derive(Deserialize)]
struct ExitNodeMetadataV1 {
    schema_version: u32,
    /// Where the exit node's gvpn-server process listens on its private overlay: the HTTP
    /// registration API and the bridge-mode session forwarding target.
    gnosis_vpn_server: SocketAddr,
    /// Where the exit node's WireGuard server listens on its private overlay. Required, not
    /// `Option`: registering under `gvpn:exit` requires running both an exit node and an exit
    /// server, so a well-formed entry always has one.
    wireguard_server: SocketAddr,
    /// Free-form labels the operator publishes (e.g. location), mirroring gnosis_vpn-client's
    /// existing config `meta` tags.
    #[serde(flatten)]
    meta: HashMap<String, String>,
}

#[derive(Debug, thiserror::Error)]
enum MetadataDecodeError {
    #[error("malformed gvpn:exit metadata: {0}")]
    Json(#[from] serde_json::Error),
    #[error("unsupported gvpn:exit metadata schema version {0}")]
    UnsupportedVersion(u32),
}

fn parse_exit_node_metadata(bytes: &[u8]) -> Result<ExitNodeMetadataV1, MetadataDecodeError> {
    let metadata: ExitNodeMetadataV1 = serde_json::from_slice(bytes)?;
    if metadata.schema_version != 1 {
        return Err(MetadataDecodeError::UnsupportedVersion(
            metadata.schema_version,
        ));
    }
    Ok(metadata)
}

/// A `gvpn:exit` node, decoded from its on-chain registry entry.
#[derive(Clone, Debug, PartialEq)]
pub struct ExitNodeInfo {
    /// On-chain address of the exit node.
    pub node: Address,
    /// Safe that performed the last write to this entry.
    pub safe: Address,
    /// The exit node's gvpn-server endpoint (HTTP registration API / bridge-mode target).
    pub gnosis_vpn_server: SocketAddr,
    /// The exit node's WireGuard server endpoint.
    pub wireguard_server: SocketAddr,
    /// Free-form operator-published labels.
    pub meta: HashMap<String, String>,
    /// When the entry was registered.
    pub registered_at: SystemTime,
    /// When the entry was last updated; equal to `registered_at` until the first update.
    pub updated_at: SystemTime,
}

/// Decodes one registry entry's application-specific metadata.
fn decode(entry: ServiceEntry) -> Result<ExitNodeInfo, MetadataDecodeError> {
    let metadata = parse_exit_node_metadata(entry.metadata.as_ref())?;
    Ok(ExitNodeInfo {
        node: entry.node,
        safe: entry.safe,
        gnosis_vpn_server: metadata.gnosis_vpn_server,
        wireguard_server: metadata.wireguard_server,
        meta: metadata.meta,
        registered_at: entry.registered_at,
        updated_at: entry.updated_at,
    })
}

/// Fetches all currently registered, live `gvpn:exit` nodes.
///
/// "Live" means the node still has a Safe binding in the node-Safe registry — an entry whose
/// binding was lost is permanently listed but dead (`design-service-registry-v3.md` §9.5), and
/// [`ServiceSelector::with_live_only`] filters those out.
pub async fn list_exit_nodes(blokli_endpoint: BlokliEndpoint) -> anyhow::Result<Vec<ExitNodeInfo>> {
    list_exit_nodes_with_client(blokli_endpoint.build_client()).await
}

async fn list_exit_nodes_with_client<C>(client: C) -> anyhow::Result<Vec<ExitNodeInfo>>
where
    C: hopr_chain_connector::blokli_client::BlokliQueryClient + Send + Sync + 'static,
{
    let reader = HoprBlockchainReader::new(client);
    list_exit_nodes_with_reader(&reader).await
}

async fn list_exit_nodes_with_reader<R>(reader: &R) -> anyhow::Result<Vec<ExitNodeInfo>>
where
    R: ChainReadServiceOperations + Send + Sync,
{
    let selector = ServiceSelector::default()
        .with_service_type(ServiceType::GVPN_EXIT)
        .with_live_only(true);

    let entries: Vec<ServiceEntry> = reader.stream_services(selector)?.collect().await;
    Ok(entries
        .into_iter()
        .filter_map(|entry| {
            let node = entry.node;
            match decode(entry) {
                Ok(entry) => Some(entry),
                Err(error) => {
                    tracing::warn!(%error, %node, "skipping exit node with malformed metadata");
                    None
                }
            }
        })
        .collect())
}

/// What changed about a `gvpn:exit` registry entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitNodeUpdateKind {
    Registered,
    Updated,
}

/// Why an exit node was removed from the usable destination set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitNodeRemovalReason {
    /// The service registry entry was removed on-chain.
    Deregistered,
    /// The registered entry exists but no longer contains valid `gvpn:exit` metadata.
    InvalidMetadata,
}

/// A live change to the usable `gvpn:exit` destination set.
#[derive(Clone, Debug, PartialEq)]
pub enum ExitNodeUpdate {
    /// A valid registration or metadata update that should be inserted into the destination set.
    Upsert {
        kind: ExitNodeUpdateKind,
        entry: ExitNodeInfo,
    },
    /// An entry that should be removed from the usable destination set.
    Remove {
        node: Address,
        reason: ExitNodeRemovalReason,
    },
}

fn decode_event(event: ChainEvent) -> Option<ExitNodeUpdate> {
    let (kind, entry) = match event {
        ChainEvent::ServiceRegistered(entry) if entry.service_type == ServiceType::GVPN_EXIT => {
            (ExitNodeUpdateKind::Registered, entry)
        }
        ChainEvent::ServiceUpdated(entry) if entry.service_type == ServiceType::GVPN_EXIT => {
            (ExitNodeUpdateKind::Updated, entry)
        }
        ChainEvent::ServiceDeregistered(service_type, node)
            if service_type == ServiceType::GVPN_EXIT =>
        {
            return Some(ExitNodeUpdate::Remove {
                node,
                reason: ExitNodeRemovalReason::Deregistered,
            });
        }
        _ => return None,
    };

    let node = entry.node;
    Some(match decode(entry) {
        Ok(entry) => ExitNodeUpdate::Upsert { kind, entry },
        Err(error) => {
            tracing::warn!(%error, %node, "removing exit node with malformed metadata");
            ExitNodeUpdate::Remove {
                node,
                reason: ExitNodeRemovalReason::InvalidMetadata,
            }
        }
    })
}

/// Subscribes to live `gvpn:exit` registrations, updates, and deregistrations.
///
/// Uses the already-connected chain connector's domain event stream, keeping Blokli wire types
/// and conversion details behind `hopr-chain-connector`. Safe-binding liveness is reconciled by
/// [`ExitNodeRegistry`] because a node losing its binding does not emit a service deregistration.
pub fn subscribe_exit_nodes<C>(
    chain: &C,
) -> Result<impl Stream<Item = ExitNodeUpdate> + Send + 'static, C::Error>
where
    C: ChainEvents,
{
    Ok(chain
        .subscribe()?
        .filter_map(|event| futures::future::ready(decode_event(event))))
}

/// How often [`ExitNodeRegistry`] re-fetches the full registry to catch nodes that went orphaned
/// (lost their Safe binding) without emitting a `Deregistered` event — [`subscribe_exit_nodes`]
/// does not get that liveness cross-check, only [`list_exit_nodes`] does.
const RECONCILE_INTERVAL: Duration = Duration::from_secs(300);

fn to_map(nodes: Vec<ExitNodeInfo>) -> HashMap<Address, ExitNodeInfo> {
    nodes.into_iter().map(|node| (node.node, node)).collect()
}

fn apply_update(nodes: &mut HashMap<Address, ExitNodeInfo>, update: ExitNodeUpdate) {
    match update {
        ExitNodeUpdate::Upsert { entry, .. } => {
            nodes.insert(entry.node, entry);
        }
        ExitNodeUpdate::Remove { node, .. } => {
            nodes.remove(&node);
        }
    }
}

/// A live, continuously updated view of registered `gvpn:exit` nodes.
///
/// Owns the live subscription and periodic liveness reconciliation behind one handle. Construct
/// it with the initial result from [`list_exit_nodes`] once the full edge client's chain connector
/// is available. Dropping the handle stops the background task.
pub struct ExitNodeRegistry {
    nodes: tokio::sync::watch::Receiver<HashMap<Address, ExitNodeInfo>>,
    task: tokio::task::AbortHandle,
}

impl ExitNodeRegistry {
    /// The current set of registered, live exit nodes, keyed by node address.
    pub fn nodes(&self) -> HashMap<Address, ExitNodeInfo> {
        self.nodes.borrow().clone()
    }

    /// Waits for the next change to [`Self::nodes`].
    pub async fn changed(&mut self) -> anyhow::Result<()> {
        self.nodes.changed().await.map_err(anyhow::Error::from)
    }
}

impl Drop for ExitNodeRegistry {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn reconcile_exit_nodes<C, S>(
    chain: C,
    updates: S,
    tx: tokio::sync::watch::Sender<HashMap<Address, ExitNodeInfo>>,
) where
    C: ChainReadServiceOperations + Clone + Send + Sync + 'static,
    S: Stream<Item = ExitNodeUpdate> + Send + 'static,
{
    let mut live_updates = Some(std::pin::pin!(updates));
    let mut reconcile = tokio::time::interval(RECONCILE_INTERVAL);
    reconcile.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    reconcile.tick().await; // the caller already supplied the initial fetch

    loop {
        tokio::select! {
            update = async { live_updates.as_mut()?.next().await }, if live_updates.is_some() => {
                match update {
                    Some(update) => {
                        tx.send_modify(|nodes| apply_update(nodes, update));
                    }
                    None => {
                        tracing::warn!("gvpn:exit subscription ended; falling back to periodic reconciliation only");
                        live_updates = None;
                    }
                }
            }
            _ = reconcile.tick() => {
                match list_exit_nodes_with_reader(&chain).await {
                    Ok(nodes) => {
                        let nodes = to_map(nodes);
                        tx.send_if_modified(|current| {
                            let changed = *current != nodes;
                            *current = nodes;
                            changed
                        });
                    }
                    Err(error) => tracing::warn!(%error, "periodic gvpn:exit reconciliation failed"),
                }
            }
        }

        if tx.is_closed() {
            break;
        }
    }
}

/// Starts maintaining a live exit-node registry through an already-connected chain connector.
///
/// `initial` should be fetched with [`list_exit_nodes`] before or during edge-client startup. The
/// connected connector supplies domain-level service events and is also used for periodic
/// Safe-binding reconciliation.
pub fn watch_exit_nodes<C>(
    initial: Vec<ExitNodeInfo>,
    chain: C,
) -> Result<ExitNodeRegistry, <C as ChainEvents>::Error>
where
    C: ChainEvents + ChainReadServiceOperations + Clone + Send + Sync + 'static,
{
    let updates = subscribe_exit_nodes(&chain)?;
    let (tx, rx) = tokio::sync::watch::channel(to_map(initial));
    let task = tokio::spawn(reconcile_exit_nodes(chain, updates, tx));

    Ok(ExitNodeRegistry {
        nodes: rx,
        task: task.abort_handle(),
    })
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use hopr_chain_connector::testing::BlokliTestStateBuilder;
    use hopr_lib::api::chain::{DeployedSafe, ServiceMetadata};

    use super::*;

    const NODE: [u8; 20] = [0x11; 20];
    const OTHER_NODE: [u8; 20] = [0x22; 20];
    const SAFE: [u8; 20] = [0x33; 20];

    fn valid_metadata() -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "schema_version": 1,
            "gnosis_vpn_server": "172.30.0.1:8000",
            "wireguard_server": "172.30.0.1:51820",
            "location": "Germany",
        }))
        .unwrap()
    }

    fn entry_with_metadata(node: [u8; 20], metadata: Vec<u8>) -> anyhow::Result<ServiceEntry> {
        let registered_at = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        Ok(ServiceEntry::new(
            ServiceType::GVPN_EXIT,
            node.into(),
            SAFE.into(),
            ServiceMetadata::try_from(metadata)?,
            registered_at,
            registered_at,
        )?)
    }

    fn safe_with_nodes(nodes: &[[u8; 20]]) -> DeployedSafe {
        DeployedSafe {
            address: SAFE.into(),
            owners: vec![[0x44; 20].into()],
            module: [0x66; 20].into(),
            registered_nodes: nodes.iter().map(|node| Address::from(*node)).collect(),
            deployer: [0x44; 20].into(),
        }
    }

    #[tokio::test]
    async fn list_exit_nodes_decodes_a_well_formed_entry() -> anyhow::Result<()> {
        let client = BlokliTestStateBuilder::default()
            .with_services([entry_with_metadata(NODE, valid_metadata())?])
            .with_deployed_safes([safe_with_nodes(&[NODE])])
            .build_static_client();

        let nodes = list_exit_nodes_with_client(client).await?;

        assert_eq!(1, nodes.len());
        assert_eq!(Address::from(NODE), nodes[0].node);
        assert_eq!(
            "172.30.0.1:8000".parse::<SocketAddr>()?,
            nodes[0].gnosis_vpn_server
        );
        assert_eq!(
            "172.30.0.1:51820".parse::<SocketAddr>()?,
            nodes[0].wireguard_server
        );
        assert_eq!(Some(&"Germany".to_string()), nodes[0].meta.get("location"));

        Ok(())
    }

    #[tokio::test]
    async fn list_exit_nodes_skips_malformed_metadata() -> anyhow::Result<()> {
        let client = BlokliTestStateBuilder::default()
            .with_services([
                entry_with_metadata(NODE, b"not json".to_vec())?,
                entry_with_metadata(OTHER_NODE, valid_metadata())?,
            ])
            .with_deployed_safes([safe_with_nodes(&[NODE, OTHER_NODE])])
            .build_static_client();

        let nodes = list_exit_nodes_with_client(client).await?;

        assert_eq!(1, nodes.len());
        assert_eq!(Address::from(OTHER_NODE), nodes[0].node);

        Ok(())
    }

    #[tokio::test]
    async fn list_exit_nodes_skips_unsupported_schema_version() -> anyhow::Result<()> {
        let metadata = serde_json::to_vec(&serde_json::json!({
            "schema_version": 2,
            "gnosis_vpn_server": "172.30.0.1:8000",
            "wireguard_server": "172.30.0.1:51820",
        }))?;
        let client = BlokliTestStateBuilder::default()
            .with_services([entry_with_metadata(NODE, metadata)?])
            .with_deployed_safes([safe_with_nodes(&[NODE])])
            .build_static_client();

        let nodes = list_exit_nodes_with_client(client).await?;

        assert!(nodes.is_empty());

        Ok(())
    }

    #[tokio::test]
    async fn list_exit_nodes_skips_missing_wireguard_server() -> anyhow::Result<()> {
        let metadata = serde_json::to_vec(&serde_json::json!({
            "schema_version": 1,
            "gnosis_vpn_server": "172.30.0.1:8000",
        }))?;
        let client = BlokliTestStateBuilder::default()
            .with_services([entry_with_metadata(NODE, metadata)?])
            .with_deployed_safes([safe_with_nodes(&[NODE])])
            .build_static_client();

        let nodes = list_exit_nodes_with_client(client).await?;

        assert!(nodes.is_empty());

        Ok(())
    }

    #[tokio::test]
    async fn list_exit_nodes_drops_entries_without_a_live_safe_binding() -> anyhow::Result<()> {
        let client = BlokliTestStateBuilder::default()
            .with_services([
                entry_with_metadata(NODE, valid_metadata())?,
                entry_with_metadata(OTHER_NODE, valid_metadata())?,
            ])
            .with_deployed_safes([safe_with_nodes(&[NODE])])
            .build_static_client();

        let nodes = list_exit_nodes_with_client(client).await?;

        assert_eq!(1, nodes.len());
        assert_eq!(Address::from(NODE), nodes[0].node);

        Ok(())
    }

    #[test]
    fn service_events_map_to_domain_updates() -> anyhow::Result<()> {
        let registered = decode_event(ChainEvent::ServiceRegistered(entry_with_metadata(
            NODE,
            valid_metadata(),
        )?))
        .expect("gvpn registration should be mapped");
        assert!(matches!(
            registered,
            ExitNodeUpdate::Upsert {
                kind: ExitNodeUpdateKind::Registered,
                ref entry,
            } if entry.node == Address::from(NODE)
        ));

        let updated = decode_event(ChainEvent::ServiceUpdated(entry_with_metadata(
            NODE,
            valid_metadata(),
        )?))
        .expect("gvpn update should be mapped");
        assert!(matches!(
            updated,
            ExitNodeUpdate::Upsert {
                kind: ExitNodeUpdateKind::Updated,
                ..
            }
        ));

        let deregistered = decode_event(ChainEvent::ServiceDeregistered(
            ServiceType::GVPN_EXIT,
            NODE.into(),
        ))
        .expect("gvpn deregistration should be mapped");
        assert_eq!(
            ExitNodeUpdate::Remove {
                node: NODE.into(),
                reason: ExitNodeRemovalReason::Deregistered,
            },
            deregistered
        );
        Ok(())
    }

    #[test]
    fn invalid_metadata_is_an_explicit_invalid_metadata_removal() -> anyhow::Result<()> {
        let update = decode_event(ChainEvent::ServiceUpdated(entry_with_metadata(
            NODE,
            b"not json".to_vec(),
        )?))
        .expect("gvpn update should be mapped");

        assert_eq!(
            ExitNodeUpdate::Remove {
                node: NODE.into(),
                reason: ExitNodeRemovalReason::InvalidMetadata,
            },
            update
        );
        Ok(())
    }

    #[test]
    fn service_events_for_other_types_are_ignored() -> anyhow::Result<()> {
        let registered_at = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let entry = ServiceEntry::new(
            "other".parse()?,
            NODE.into(),
            SAFE.into(),
            ServiceMetadata::try_from(valid_metadata())?,
            registered_at,
            registered_at,
        )?;

        assert!(decode_event(ChainEvent::ServiceRegistered(entry)).is_none());
        assert!(
            decode_event(ChainEvent::ServiceDeregistered(
                "other".parse()?,
                NODE.into()
            ))
            .is_none()
        );
        Ok(())
    }

    #[test]
    fn applying_invalid_metadata_removes_without_claiming_deregistration() -> anyhow::Result<()> {
        let info = decode(entry_with_metadata(NODE, valid_metadata())?)?;
        let mut nodes = to_map(vec![info]);

        apply_update(
            &mut nodes,
            ExitNodeUpdate::Remove {
                node: NODE.into(),
                reason: ExitNodeRemovalReason::InvalidMetadata,
            },
        );

        assert!(!nodes.contains_key(&Address::from(NODE)));
        Ok(())
    }
}
