//! Discovery of `gvpn:exit` nodes registered in the on-chain `HoprServiceRegistry`.
//!
//! Reads go through [`hopr_chain_connector::HoprBlockchainReader`], which needs only a Blokli
//! client (no chain key, no `connect()`), so a client that only wants to discover exit nodes does
//! not have to sync the whole channel graph first.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use blokli_client::api::{
    BlokliQueryClient, BlokliSubscriptionClient, ServiceSelector as BlokliServiceSelector,
    types::{ServiceUpdate, ServiceUpdateKind},
};
use futures::{Stream, StreamExt};
use hopr_chain_connector::HoprBlockchainReader;
use hopr_lib::api::chain::{ChainReadServiceOperations, ServiceEntry, ServiceSelector};
use hopr_lib::api::types::internal::prelude::ServiceType;
use hopr_lib::api::types::primitive::prelude::{Address, ToHex};
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

/// Decodes one registry entry, dropping it (with a log) if the metadata does not parse.
///
/// The registry is permissionless — garbage and squatted entries under `gvpn:exit` are expected,
/// not fatal, so a decode failure is skipped rather than propagated.
fn decode(entry: ServiceEntry) -> Option<ExitNodeInfo> {
    match parse_exit_node_metadata(entry.metadata.as_ref()) {
        Ok(m) => Some(ExitNodeInfo {
            node: entry.node,
            safe: entry.safe,
            gnosis_vpn_server: m.gnosis_vpn_server,
            wireguard_server: m.wireguard_server,
            meta: m.meta,
            registered_at: entry.registered_at,
            updated_at: entry.updated_at,
        }),
        Err(error) => {
            tracing::warn!(%error, node = %entry.node, "skipping exit node with malformed metadata");
            None
        }
    }
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
    C: BlokliQueryClient + Send + Sync + 'static,
{
    let reader = HoprBlockchainReader::new(client);
    let selector = ServiceSelector::default()
        .with_service_type(ServiceType::GVPN_EXIT)
        .with_live_only(true);

    let entries: Vec<ServiceEntry> = reader.stream_services(selector)?.collect().await;
    Ok(entries.into_iter().filter_map(decode).collect())
}

/// What changed about a `gvpn:exit` registry entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitNodeUpdateKind {
    Registered,
    Updated,
    Deregistered,
}

/// A live change to a `gvpn:exit` registry entry.
#[derive(Clone, Debug)]
pub struct ExitNodeUpdate {
    pub kind: ExitNodeUpdateKind,
    pub node: Address,
    /// `None` for [`ExitNodeUpdateKind::Deregistered`], where the entry no longer exists, or when
    /// the updated entry's metadata failed to decode.
    pub entry: Option<ExitNodeInfo>,
}

fn model_timestamp(seconds: i32) -> Option<SystemTime> {
    match u64::try_from(seconds) {
        Ok(seconds) => Some(UNIX_EPOCH + Duration::from_secs(seconds)),
        Err(_) => {
            tracing::warn!(
                seconds,
                "exit node update timestamp precedes the Unix epoch"
            );
            None
        }
    }
}

/// Decodes the raw Blokli model carried by a subscription update, dropping it (with a log) on any
/// malformed field. Mirrors [`decode`], but for the hex-encoded GraphQL model rather than the
/// already-parsed domain [`ServiceEntry`] `list_exit_nodes` works with.
fn decode_model_entry(model: blokli_client::api::types::ServiceEntry) -> Option<ExitNodeInfo> {
    let node = match Address::from_hex(&model.node) {
        Ok(node) => node,
        Err(error) => {
            tracing::warn!(%error, node = %model.node, "skipping exit node update with malformed node address");
            return None;
        }
    };

    let safe = match Address::from_hex(&model.safe) {
        Ok(safe) => safe,
        Err(error) => {
            tracing::warn!(%error, %node, "skipping exit node update with malformed safe address");
            return None;
        }
    };

    let metadata_bytes = match const_hex::decode(&model.metadata) {
        Ok(bytes) => bytes,
        Err(error) => {
            tracing::warn!(%error, %node, "skipping exit node update with malformed metadata encoding");
            return None;
        }
    };

    let metadata = match parse_exit_node_metadata(&metadata_bytes) {
        Ok(metadata) => metadata,
        Err(error) => {
            tracing::warn!(%error, %node, "skipping exit node update with malformed metadata");
            return None;
        }
    };

    let registered_at = model_timestamp(model.registered_at)?;
    let updated_at = model_timestamp(model.updated_at)?;

    Some(ExitNodeInfo {
        node,
        safe,
        gnosis_vpn_server: metadata.gnosis_vpn_server,
        wireguard_server: metadata.wireguard_server,
        meta: metadata.meta,
        registered_at,
        updated_at,
    })
}

/// Decodes one subscription update, dropping it (with a log) on any malformed field.
fn decode_update(
    update: Result<ServiceUpdate, blokli_client::errors::BlokliClientError>,
) -> Option<ExitNodeUpdate> {
    let update = match update {
        Ok(update) => update,
        Err(error) => {
            tracing::warn!(%error, "gvpn:exit subscription error");
            return None;
        }
    };

    let node = match Address::from_hex(&update.node) {
        Ok(node) => node,
        Err(error) => {
            tracing::warn!(%error, node = %update.node, "skipping update with malformed node address");
            return None;
        }
    };

    match update.kind {
        ServiceUpdateKind::Deregistered => Some(ExitNodeUpdate {
            kind: ExitNodeUpdateKind::Deregistered,
            node,
            entry: None,
        }),
        ServiceUpdateKind::Registered | ServiceUpdateKind::Updated => {
            let kind = if update.kind == ServiceUpdateKind::Registered {
                ExitNodeUpdateKind::Registered
            } else {
                ExitNodeUpdateKind::Updated
            };
            Some(ExitNodeUpdate {
                kind,
                node,
                entry: update.entry.and_then(decode_model_entry),
            })
        }
    }
}

/// Subscribes to live `gvpn:exit` registrations, updates, and deregistrations.
///
/// Built directly on the raw Blokli subscription — [`ChainReadServiceOperations`] has no
/// subscribe method — so unlike [`list_exit_nodes`], updates here do not get the Safe-binding
/// liveness cross-check: a node that goes orphaned mid-stream is not flagged until the next
/// `list_exit_nodes` call. Acceptable for v1; reconcile periodically if that gap matters.
///
/// A failure to open the subscription itself (as opposed to a malformed update once open, which
/// is merely skipped) ends the returned stream after a single log line rather than surfacing an
/// error to the caller — the selector is always a valid, concrete `gvpn:exit` filter, so this
/// path is not expected to fail in practice.
pub fn subscribe_exit_nodes(
    blokli_endpoint: BlokliEndpoint,
) -> impl Stream<Item = ExitNodeUpdate> + Send {
    subscribe_exit_nodes_with_client(blokli_endpoint.build_client())
}

fn subscribe_exit_nodes_with_client<C>(client: C) -> impl Stream<Item = ExitNodeUpdate> + Send
where
    C: BlokliSubscriptionClient + Send + Sync + 'static,
{
    // `subscribe_services` ties its returned stream to `&client`'s lifetime; `stream!` lets this
    // function return an owned, 'static stream that keeps `client` alive for exactly as long as
    // the inner stream borrowed from it, without unsafe lifetime extension.
    async_stream::stream! {
        let selector = BlokliServiceSelector::ServiceType(ServiceType::GVPN_EXIT.as_encoded());
        let updates = match client.subscribe_services(selector) {
            Ok(updates) => updates,
            Err(error) => {
                tracing::error!(%error, "failed to open the gvpn:exit registry subscription");
                return;
            }
        };

        let mut updates = std::pin::pin!(updates);
        while let Some(update) = updates.next().await {
            if let Some(mapped) = decode_update(update) {
                yield mapped;
            }
        }
    }
}

/// How often [`ExitNodeRegistry`] re-fetches the full registry to catch nodes that went orphaned
/// (lost their Safe binding) without emitting a `Deregistered` event — [`subscribe_exit_nodes`]
/// does not get that liveness cross-check, only [`list_exit_nodes`] does.
const RECONCILE_INTERVAL: Duration = Duration::from_secs(300);

fn to_map(nodes: Vec<ExitNodeInfo>) -> HashMap<Address, ExitNodeInfo> {
    nodes.into_iter().map(|node| (node.node, node)).collect()
}

/// A live, continuously updated view of registered `gvpn:exit` nodes.
///
/// Owns the whole discovery lifecycle behind one handle — the initial fetch, the live
/// subscription, and periodic liveness reconciliation — so a caller never has to see a Blokli
/// selector or a raw registry update. Dropping the handle stops the background task.
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

async fn reconcile_exit_nodes<C>(
    client: C,
    tx: tokio::sync::watch::Sender<HashMap<Address, ExitNodeInfo>>,
) where
    C: BlokliQueryClient + BlokliSubscriptionClient + Clone + Send + Sync + 'static,
{
    // `None` once the live subscription ends (e.g. exhausted its own reconnect attempts); from
    // then on this task falls back to periodic reconciliation only, rather than busy-polling a
    // stream that will keep reporting `None`.
    let mut live_updates = Some(std::pin::pin!(subscribe_exit_nodes_with_client(
        client.clone()
    )));
    let mut reconcile = tokio::time::interval(RECONCILE_INTERVAL);
    reconcile.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    reconcile.tick().await; // the initial fetch already happened in `watch_exit_nodes`

    loop {
        tokio::select! {
            update = async { live_updates.as_mut()?.next().await }, if live_updates.is_some() => {
                match update {
                    Some(update) => {
                        tx.send_modify(|nodes| match update.entry {
                            Some(entry) => { nodes.insert(update.node, entry); }
                            None => { nodes.remove(&update.node); }
                        });
                    }
                    None => {
                        tracing::warn!("gvpn:exit subscription ended; falling back to periodic reconciliation only");
                        live_updates = None;
                    }
                }
            }
            _ = reconcile.tick() => {
                match list_exit_nodes_with_client(client.clone()).await {
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

/// Starts discovering registered `gvpn:exit` nodes, returning once the initial fetch completes.
///
/// This is the intended entry point for callers that just want a live destination list without
/// touching Blokli-specific types: it fetches the current registry, then keeps the result fresh
/// in the background (live subscription plus periodic reconciliation) for as long as the
/// returned [`ExitNodeRegistry`] stays alive.
pub async fn watch_exit_nodes(blokli_endpoint: BlokliEndpoint) -> anyhow::Result<ExitNodeRegistry> {
    watch_exit_nodes_with_client(blokli_endpoint.build_client()).await
}

async fn watch_exit_nodes_with_client<C>(client: C) -> anyhow::Result<ExitNodeRegistry>
where
    C: BlokliQueryClient + BlokliSubscriptionClient + Clone + Send + Sync + 'static,
{
    let initial = to_map(list_exit_nodes_with_client(client.clone()).await?);
    let (tx, rx) = tokio::sync::watch::channel(initial);
    let task = tokio::spawn(reconcile_exit_nodes(client, tx));

    Ok(ExitNodeRegistry {
        nodes: rx,
        task: task.abort_handle(),
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

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

    #[tokio::test]
    async fn watch_exit_nodes_returns_the_initial_registry() -> anyhow::Result<()> {
        let client = BlokliTestStateBuilder::default()
            .with_services([entry_with_metadata(NODE, valid_metadata())?])
            .with_deployed_safes([safe_with_nodes(&[NODE])])
            .build_static_client();

        let registry = watch_exit_nodes_with_client(client).await?;
        let nodes = registry.nodes();

        assert_eq!(1, nodes.len());
        assert_eq!(Address::from(NODE), nodes[&Address::from(NODE)].node);

        Ok(())
    }

    #[tokio::test]
    async fn watch_exit_nodes_dropping_the_registry_stops_the_background_task() -> anyhow::Result<()>
    {
        let client = BlokliTestStateBuilder::default().build_static_client();

        let registry = watch_exit_nodes_with_client(client).await?;
        let task = registry.task.clone();
        drop(registry);

        // `AbortHandle::abort` schedules cancellation; give the runtime a tick to apply it.
        tokio::task::yield_now().await;
        assert!(task.is_finished());

        Ok(())
    }

    fn model_entry_with_metadata(
        node: [u8; 20],
        metadata: Vec<u8>,
    ) -> blokli_client::api::types::ServiceEntry {
        blokli_client::api::types::ServiceEntry {
            service_type: "gvpn:exit".to_string(),
            node: const_hex::encode(node),
            safe: const_hex::encode(SAFE),
            metadata: const_hex::encode(metadata),
            registered_at: 1_700_000_000,
            updated_at: 1_700_000_000,
        }
    }

    #[test]
    fn decode_model_entry_decodes_a_well_formed_entry() {
        let info = decode_model_entry(model_entry_with_metadata(NODE, valid_metadata()))
            .expect("should decode");

        assert_eq!(Address::from(NODE), info.node);
        assert_eq!(Address::from(SAFE), info.safe);
        assert_eq!(
            "172.30.0.1:8000".parse::<SocketAddr>().unwrap(),
            info.gnosis_vpn_server
        );
        assert_eq!(
            "172.30.0.1:51820".parse::<SocketAddr>().unwrap(),
            info.wireguard_server
        );
    }

    #[test]
    fn decode_model_entry_skips_malformed_node_address() {
        let mut model = model_entry_with_metadata(NODE, valid_metadata());
        model.node = "not hex".to_string();

        assert!(decode_model_entry(model).is_none());
    }

    #[test]
    fn decode_model_entry_skips_malformed_metadata_encoding() {
        let mut model = model_entry_with_metadata(NODE, valid_metadata());
        model.metadata = "not hex".to_string();

        assert!(decode_model_entry(model).is_none());
    }

    #[test]
    fn decode_update_maps_registered_and_updated_kinds() {
        let registered = decode_update(Ok(ServiceUpdate {
            kind: ServiceUpdateKind::Registered,
            service_type: "gvpn:exit".to_string(),
            node: const_hex::encode(NODE),
            entry: Some(model_entry_with_metadata(NODE, valid_metadata())),
        }))
        .expect("should decode");
        assert_eq!(ExitNodeUpdateKind::Registered, registered.kind);
        assert_eq!(Address::from(NODE), registered.node);
        assert!(registered.entry.is_some());

        let updated = decode_update(Ok(ServiceUpdate {
            kind: ServiceUpdateKind::Updated,
            service_type: "gvpn:exit".to_string(),
            node: const_hex::encode(NODE),
            entry: Some(model_entry_with_metadata(NODE, valid_metadata())),
        }))
        .expect("should decode");
        assert_eq!(ExitNodeUpdateKind::Updated, updated.kind);
    }

    #[test]
    fn decode_update_maps_deregistered_with_no_entry() {
        let deregistered = decode_update(Ok(ServiceUpdate {
            kind: ServiceUpdateKind::Deregistered,
            service_type: "gvpn:exit".to_string(),
            node: const_hex::encode(NODE),
            entry: None,
        }))
        .expect("should decode");

        assert_eq!(ExitNodeUpdateKind::Deregistered, deregistered.kind);
        assert_eq!(Address::from(NODE), deregistered.node);
        assert!(deregistered.entry.is_none());
    }

    #[test]
    fn decode_update_skips_malformed_node_address() {
        let update = decode_update(Ok(ServiceUpdate {
            kind: ServiceUpdateKind::Deregistered,
            service_type: "gvpn:exit".to_string(),
            node: "not hex".to_string(),
            entry: None,
        }));

        assert!(update.is_none());
    }
}
