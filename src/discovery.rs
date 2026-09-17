//! Discovery of `gvpn:exit` nodes registered in the on-chain `HoprServiceRegistry`.
//!
//! Initial reads need no connector ([`HoprBlockchainReader`]); live updates ride the connected
//! connector's event stream so Blokli wire types stay behind `hopr-chain-connector`.

use std::collections::HashMap;
use std::net::SocketAddr;
#[cfg(feature = "runtime-tokio")]
use std::time::Duration;
use std::time::SystemTime;

use futures::{Stream, StreamExt};
use hopr_chain_connector::HoprBlockchainReader;
use hopr_lib::api::chain::{
    ChainEvent, ChainEvents, ChainReadServiceOperations, ServiceEntry, ServiceSelector,
};
use hopr_lib::api::types::internal::prelude::ServiceType;
use hopr_lib::api::types::primitive::prelude::Address;
use serde::Deserialize;

use crate::endpoint::BlokliEndpoint;

/// Schema per `design-service-registry-v3.md` §3.2; unknown keys are tolerated so an additive
/// field cannot invalidate every entry for older clients.
#[derive(Deserialize)]
struct ExitNodeMetadataV1 {
    /// Overlay address of the gvpn-server HTTP API and bridge-mode forwarding target.
    gnosis_vpn_server: SocketAddr,
    /// Not `Option`: a `gvpn:exit` registration implies a running exit server.
    wireguard_server: SocketAddr,
    /// `Value`, not `String`, so one non-string label cannot reject the whole entry.
    #[serde(default)]
    meta: HashMap<String, serde_json::Value>,
}

/// A label never disqualifies a node, so non-string values become their JSON text.
fn stringify_meta(meta: HashMap<String, serde_json::Value>) -> HashMap<String, String> {
    meta.into_iter()
        .map(|(key, value)| match value {
            serde_json::Value::String(text) => (key, text),
            other => (key, other.to_string()),
        })
        .collect()
}

#[derive(Debug, thiserror::Error)]
enum MetadataDecodeError {
    #[error("malformed gvpn:exit metadata: {0}")]
    Json(#[from] serde_json::Error),
    #[error("gvpn:exit metadata has no numeric `version` field")]
    MissingVersion,
    #[error("unsupported gvpn:exit metadata schema version {0}")]
    UnsupportedVersion(u64),
}

/// `version` is checked before the payload so a future schema reads as unsupported, not corrupt.
fn parse_exit_node_metadata(bytes: &[u8]) -> Result<ExitNodeMetadataV1, MetadataDecodeError> {
    let value: serde_json::Value = serde_json::from_slice(bytes)?;
    let version = value
        .get("version")
        .and_then(serde_json::Value::as_u64)
        .ok_or(MetadataDecodeError::MissingVersion)?;
    if version != 1 {
        return Err(MetadataDecodeError::UnsupportedVersion(version));
    }
    Ok(serde_json::from_value(value)?)
}

/// A `gvpn:exit` node, decoded from its on-chain registry entry.
#[derive(Clone, Debug, PartialEq)]
pub struct ExitNodeInfo {
    pub node: Address,
    /// Safe that performed the last write to this entry.
    pub safe: Address,
    /// gvpn-server HTTP API and bridge-mode forwarding target.
    pub gnosis_vpn_server: SocketAddr,
    pub wireguard_server: SocketAddr,
    /// Free-form operator-published labels.
    pub meta: HashMap<String, String>,
    pub registered_at: SystemTime,
    /// Equal to `registered_at` until the first update.
    pub updated_at: SystemTime,
}

fn decode(entry: ServiceEntry) -> Result<ExitNodeInfo, MetadataDecodeError> {
    let metadata = parse_exit_node_metadata(entry.metadata.as_ref())?;
    Ok(ExitNodeInfo {
        node: entry.node,
        safe: entry.safe,
        gnosis_vpn_server: metadata.gnosis_vpn_server,
        wireguard_server: metadata.wireguard_server,
        meta: stringify_meta(metadata.meta),
        registered_at: entry.registered_at,
        updated_at: entry.updated_at,
    })
}

/// Fetches all registered `gvpn:exit` nodes that still have a Safe binding.
///
/// An entry whose binding was lost stays listed but is dead (§9.5), hence `with_live_only`.
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

    // Decoded while streaming so malformed entries never occupy memory.
    let mut skipped = 0usize;
    let nodes: Vec<ExitNodeInfo> = reader
        .stream_services(selector)?
        .filter_map(|entry| {
            let node = entry.node;
            let decoded = match decode(entry) {
                Ok(entry) => Some(entry),
                Err(error) => {
                    skipped += 1;
                    tracing::debug!(%error, %node, "skipping exit node with malformed metadata");
                    None
                }
            };
            futures::future::ready(decoded)
        })
        .collect()
        .await;
    // Ratio, not per entry: malformed entries are normal; only the ratio shows our decoder breaking.
    tracing::info!(
        accepted = nodes.len(),
        skipped,
        "fetched gvpn:exit registry entries"
    );
    Ok(nodes)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitNodeUpdateKind {
    Registered,
    Updated,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitNodeRemovalReason {
    Deregistered,
    /// The entry still exists on-chain but its metadata no longer decodes.
    InvalidMetadata,
}

/// A live change to the usable `gvpn:exit` destination set.
#[derive(Clone, Debug, PartialEq)]
pub enum ExitNodeUpdate {
    Upsert {
        kind: ExitNodeUpdateKind,
        entry: ExitNodeInfo,
    },
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
            tracing::debug!(%error, %node, "removing exit node with malformed metadata");
            ExitNodeUpdate::Remove {
                node,
                reason: ExitNodeRemovalReason::InvalidMetadata,
            }
        }
    })
}

/// Subscribes to live `gvpn:exit` registrations, updates, and deregistrations.
///
/// A lost Safe binding emits no deregistration, so liveness is left to [`ExitNodeRegistry`].
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

/// Full re-fetch cadence; only [`list_exit_nodes`] sees nodes that lost their Safe binding.
#[cfg(feature = "runtime-tokio")]
const RECONCILE_INTERVAL: Duration = Duration::from_secs(300);

#[cfg(feature = "runtime-tokio")]
fn to_map(nodes: Vec<ExitNodeInfo>) -> HashMap<Address, ExitNodeInfo> {
    nodes.into_iter().map(|node| (node.node, node)).collect()
}

#[cfg(feature = "runtime-tokio")]
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

/// Live view of registered `gvpn:exit` nodes; owns the subscription and reconciliation task.
#[cfg(feature = "runtime-tokio")]
#[must_use = "dropping the registry stops live exit-node discovery"]
pub struct ExitNodeRegistry {
    nodes: tokio::sync::watch::Receiver<HashMap<Address, ExitNodeInfo>>,
    task: tokio::task::AbortHandle,
}

#[cfg(feature = "runtime-tokio")]
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

#[cfg(feature = "runtime-tokio")]
impl Drop for ExitNodeRegistry {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[cfg(feature = "runtime-tokio")]
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
    // First tick fires at once: `initial` predates the subscription, so re-read to close the gap.
    reconcile.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

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

/// Starts maintaining a live exit-node registry, seeded with the result of [`list_exit_nodes`].
///
/// Spawns onto the current Tokio runtime, which needs the time driver (`#[tokio::main]` default).
#[cfg(feature = "runtime-tokio")]
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
            "version": 1,
            "gnosis_vpn_server": "172.30.0.1:8000",
            "wireguard_server": "172.30.0.1:51820",
            "meta": { "location": "Germany" },
        }))
        .unwrap()
    }

    /// Verbatim from piz-palu-dev: fixtures built here only prove the decoder agrees with itself.
    const REAL_ON_CHAIN_METADATA: &str = r#"{
    "version":1,
    "gnosis_vpn_server":"172.30.0.1:8000",
    "wireguard_server":"172.30.0.1:51820",
    "meta":{"location":"London","flag":"GB"}
  }"#;

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

    /// Shares no field layout with v1, so only the version check running first can reject it.
    #[tokio::test]
    async fn list_exit_nodes_skips_unsupported_schema_version() -> anyhow::Result<()> {
        let metadata = serde_json::to_vec(&serde_json::json!({
            "version": 2,
            "endpoints": { "bridge": "172.30.0.1:8000", "wg": "172.30.0.1:51820" },
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
    async fn list_exit_nodes_decodes_the_real_on_chain_blob() -> anyhow::Result<()> {
        let client = BlokliTestStateBuilder::default()
            .with_services([entry_with_metadata(
                NODE,
                REAL_ON_CHAIN_METADATA.as_bytes().to_vec(),
            )?])
            .with_deployed_safes([safe_with_nodes(&[NODE])])
            .build_static_client();

        let nodes = list_exit_nodes_with_client(client).await?;

        assert_eq!(1, nodes.len());
        assert_eq!(
            "172.30.0.1:8000".parse::<SocketAddr>()?,
            nodes[0].gnosis_vpn_server
        );
        assert_eq!(
            "172.30.0.1:51820".parse::<SocketAddr>()?,
            nodes[0].wireguard_server
        );
        assert_eq!(Some(&"London".to_string()), nodes[0].meta.get("location"));
        assert_eq!(Some(&"GB".to_string()), nodes[0].meta.get("flag"));

        Ok(())
    }

    #[tokio::test]
    async fn list_exit_nodes_accepts_an_entry_without_meta() -> anyhow::Result<()> {
        let metadata = serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "gnosis_vpn_server": "172.30.0.1:8000",
            "wireguard_server": "172.30.0.1:51820",
        }))?;
        let client = BlokliTestStateBuilder::default()
            .with_services([entry_with_metadata(NODE, metadata)?])
            .with_deployed_safes([safe_with_nodes(&[NODE])])
            .build_static_client();

        let nodes = list_exit_nodes_with_client(client).await?;

        assert_eq!(1, nodes.len());
        assert!(nodes[0].meta.is_empty());

        Ok(())
    }

    /// A label's JSON type says nothing about whether the node works, so every key is kept.
    #[tokio::test]
    async fn list_exit_nodes_keeps_non_string_meta_values_as_text() -> anyhow::Result<()> {
        let metadata = serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "gnosis_vpn_server": "172.30.0.1:8000",
            "wireguard_server": "172.30.0.1:51820",
            "meta": {
                "location": "London",
                "port": 51820,
                "beta": true,
                "coords": { "lat": 51.5 },
            },
        }))?;
        let client = BlokliTestStateBuilder::default()
            .with_services([entry_with_metadata(NODE, metadata)?])
            .with_deployed_safes([safe_with_nodes(&[NODE])])
            .build_static_client();

        let nodes = list_exit_nodes_with_client(client).await?;

        assert_eq!(1, nodes.len());
        let meta = &nodes[0].meta;
        assert_eq!(Some(&"London".to_string()), meta.get("location"));
        assert_eq!(Some(&"51820".to_string()), meta.get("port"));
        assert_eq!(Some(&"true".to_string()), meta.get("beta"));
        assert_eq!(Some(&r#"{"lat":51.5}"#.to_string()), meta.get("coords"));

        Ok(())
    }

    #[tokio::test]
    async fn list_exit_nodes_tolerates_unknown_top_level_keys() -> anyhow::Result<()> {
        let metadata = serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "gnosis_vpn_server": "172.30.0.1:8000",
            "wireguard_server": "172.30.0.1:51820",
            "meta": { "location": "Germany" },
            "future_additive_field": 42,
        }))?;
        let client = BlokliTestStateBuilder::default()
            .with_services([entry_with_metadata(NODE, metadata)?])
            .with_deployed_safes([safe_with_nodes(&[NODE])])
            .build_static_client();

        let nodes = list_exit_nodes_with_client(client).await?;

        assert_eq!(1, nodes.len());

        Ok(())
    }

    /// What the previously expected `schema_version` layout now looks like to this decoder.
    #[tokio::test]
    async fn list_exit_nodes_skips_metadata_without_a_version() -> anyhow::Result<()> {
        let metadata = serde_json::to_vec(&serde_json::json!({
            "schema_version": 1,
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
            "version": 1,
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

    #[cfg(feature = "runtime-tokio")]
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
