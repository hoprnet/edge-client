//! End-to-end integration test: Edgli + hoprd localcluster.
//!
//! # What this test does
//!
//! 1. Spins up a 3-node HOPR cluster via `hoprd-localcluster`.
//! 2. Boots Edgli with a pre-funded extra identity created by the cluster.
//! 3. Verifies cluster state: full P2P peer visibility and full-mesh channels.
//! 4. Lets the channel-lifecycle strategy open Edgli's outgoing channels.
//! 5. Pumps 1 MiB of random data through a 0-hop session (sanity check, no relay).
//! 6. Pumps 1 MiB of random data through a 1-hop session (full relay path).
//! 7. Verifies SHA-256 integrity of both echo round-trips.
//!
//! # Modes of operation
//!
//! ## Managed mode (default): test owns cluster lifetime
//!
//! The test starts `hoprd-localcluster`, waits for readiness, runs the session
//! pumps, then tears the cluster down. Required env vars:
//!
//! | Variable                  | Required | Description                              |
//! |---------------------------|----------|------------------------------------------|
//! | `HOPRD_LOCALCLUSTER_BIN`  | yes      | Path to `hoprd-localcluster` binary      |
//! | `HOPRD_BIN`               | yes      | Path to `hoprd` binary                   |
//! | `HOPRD_CHAIN_IMAGE`       | yes      | `bloklid-anvil` container image tag      |
//! | `HOPRD_CONTAINER_RUNTIME` | no       | Container runtime (default: docker)      |
//!
//! ```text
//! export HOPRD_LOCALCLUSTER_BIN=/path/to/hoprd-localcluster
//! export HOPRD_BIN=/path/to/hoprd
//! export HOPRD_CHAIN_IMAGE='<bloklid-anvil image tag>'
//! # --release is required: HOPR's async future chains overflow the default debug stack
//! RUST_LOG=info,edgli=debug cargo test --test edgli_session_e2e --release -- --ignored --nocapture
//! ```
//!
//! ## External mode: attach to an already-running cluster
//!
//! Start the cluster manually in one terminal, capture its stdout to a file,
//! then point the test at that file to skip cluster startup:
//!
//! ```text
//! # Terminal 1 — start cluster and tee its output
//! hoprd-localcluster --size 3 --extra-identities 1 \
//!   --api-port-base 13000 --p2p-port-base 19000 \
//!   --api-token test-token-localcluster \
//!   --chain-image '...' ... 2>&1 | tee /tmp/cluster.log
//!
//! # Wait for "localcluster running" in Terminal 1, then in Terminal 2:
//! export HOPRD_CLUSTER_SUMMARY_FILE=/tmp/cluster.log
//! RUST_LOG=info,edgli=debug cargo test --test edgli_session_e2e --release -- --ignored --nocapture
//! ```
//!
//! The test will NOT stop the external cluster when it finishes.

use anyhow::Context as _;
use std::{collections::HashMap, path::PathBuf, time::Duration};

use edgli::{
    Edgli, EdgliInitState,
    hopr_lib::{
        HopRouting, HoprKeys, HoprSessionClientConfig, IdentityRetrievalModes,
        api::{
            node::HoprSessionClientOperations,
            types::{
                internal::channels::{ChannelEntry, ChannelStatus},
                primitive::prelude::Address,
            },
        },
        config::{HoprLibConfig, HostConfig, HostType, SafeModule},
        exports::transport::SessionCapability,
        exports::transport::{HoprSession, SessionTarget},
    },
    strategy::{EdgeStrategyKind, EligibilityConfig, IncentiveConfiguration, default_strategy_cfg},
    traits::EdgeNodeApi,
};
use hopr_chain_connector::BlockchainConnectorConfig;
use rand::RngCore as _;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};

// ────────────────────────────────────────────────────────────────────────────
// Constants
// ────────────────────────────────────────────────────────────────────────────

const PAYLOAD_SIZE: usize = 1_024 * 1_024; // 1 MiB
/// HOPR session MTU (bytes that fit in a single HOPR packet payload).
/// Equal to `hopr_transport_session::SESSION_MTU = 1018`.
const SESSION_MTU: usize = 1018;
/// Packets per write-batch in `pump_and_verify`.  Keeps the Rayon encoding
/// queue well below the `PACKET_ENCODING_TIMEOUT = 150 ms` threshold even
/// when the host machine is under load running a 3-node cluster.
const PUMP_BATCH_PACKETS: usize = 16;
const PUMP_BATCH_BYTES: usize = PUMP_BATCH_PACKETS * SESSION_MTU;
const CLUSTER_SIZE: usize = 3;
const API_PORT_BASE: u16 = 13000;
const P2P_PORT_BASE: u16 = 19000;
/// Edgli's P2P port — one slot beyond the cluster nodes.
const EDGE_P2P_PORT: u16 = P2P_PORT_BASE + CLUSTER_SIZE as u16;
const API_HOST: &str = "127.0.0.1";
const API_TOKEN: &str = "test-token-localcluster";

// Timeouts (generous to handle slow CI environments and image pulls).
const CLUSTER_START_TIMEOUT: Duration = Duration::from_secs(600); // 10 min
const READYZ_TIMEOUT: Duration = Duration::from_secs(120);
const PEER_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(120);
const INTRACLUSTER_CHANNEL_TIMEOUT: Duration = Duration::from_secs(120);
const EDGLI_PEER_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(120);
const CHANNEL_OPEN_TIMEOUT: Duration = Duration::from_secs(120);
const PUMP_TIMEOUT: Duration = Duration::from_secs(120);

// ────────────────────────────────────────────────────────────────────────────
// Cluster summary types
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct NodeInfo {
    address: Address,
}

#[derive(Debug, Clone)]
struct ExtraInfo {
    safe_address: Address,
    module_address: Address,
    keystore_path: PathBuf,
    password: String,
}

#[derive(Debug, Clone)]
struct ClusterSummary {
    blokli_url: String,
    nodes: Vec<NodeInfo>,
    extras: Vec<ExtraInfo>,
}

// ────────────────────────────────────────────────────────────────────────────
// Summary parser (reads hoprd-localcluster stdout)
// ────────────────────────────────────────────────────────────────────────────

fn parse_summary(stdout: &str) -> anyhow::Result<ClusterSummary> {
    let mut blokli_url: Option<String> = None;
    let mut nodes: Vec<NodeInfo> = Vec::new();
    let mut extras: Vec<ExtraInfo> = Vec::new();
    let mut in_node: Option<HashMap<String, String>> = None;
    let mut in_extra: Option<HashMap<String, String>> = None;

    let flush_node = |in_node: &mut Option<HashMap<String, String>>,
                      nodes: &mut Vec<NodeInfo>|
     -> anyhow::Result<()> {
        if let Some(fields) = in_node.take() {
            nodes.push(parse_node_info(&fields)?);
        }
        Ok(())
    };
    let flush_extra = |in_extra: &mut Option<HashMap<String, String>>,
                       extras: &mut Vec<ExtraInfo>|
     -> anyhow::Result<()> {
        if let Some(fields) = in_extra.take() {
            extras.push(parse_extra_info(&fields)?);
        }
        Ok(())
    };

    for line in stdout.lines() {
        let trimmed = line.trim();

        if let Some(rest) = trimmed.strip_prefix("Chain (Blokli):") {
            blokli_url = Some(rest.trim().to_string());
            continue;
        }

        if trimmed.starts_with("Node ") && !trimmed.contains(':') {
            flush_node(&mut in_node, &mut nodes)?;
            flush_extra(&mut in_extra, &mut extras)?;
            in_node = Some(HashMap::new());
            continue;
        }
        if trimmed.starts_with("Extra ") && !trimmed.contains(':') {
            flush_node(&mut in_node, &mut nodes)?;
            flush_extra(&mut in_extra, &mut extras)?;
            in_extra = Some(HashMap::new());
            continue;
        }

        // "  Key  : value" field lines — skip if the key part contains '/' (URL).
        if let Some(colon_pos) = trimmed.find(':') {
            let key = trimmed[..colon_pos].trim();
            if key.contains('/') {
                continue;
            }
            let val = trimmed[colon_pos + 1..].trim().to_string();
            let key_lower = key.to_ascii_lowercase();
            if let Some(fields) = in_node.as_mut() {
                fields.insert(key_lower, val);
            } else if let Some(fields) = in_extra.as_mut() {
                fields.insert(key_lower, val);
            }
        }
    }

    flush_node(&mut in_node, &mut nodes)?;
    flush_extra(&mut in_extra, &mut extras)?;

    let blokli_url =
        blokli_url.ok_or_else(|| anyhow::anyhow!("blokli URL not found in cluster summary"))?;
    anyhow::ensure!(!nodes.is_empty(), "no nodes found in cluster summary");
    anyhow::ensure!(
        !extras.is_empty(),
        "no extra identities found in cluster summary"
    );

    Ok(ClusterSummary {
        blokli_url,
        nodes,
        extras,
    })
}

fn parse_node_info(fields: &HashMap<String, String>) -> anyhow::Result<NodeInfo> {
    let address = fields
        .get("address")
        .ok_or_else(|| anyhow::anyhow!("node: missing Address field"))?
        .parse::<Address>()?;
    Ok(NodeInfo { address })
}

fn parse_extra_info(fields: &HashMap<String, String>) -> anyhow::Result<ExtraInfo> {
    let safe_address = fields
        .get("safe address")
        .ok_or_else(|| anyhow::anyhow!("extra: missing Safe address field"))?
        .parse::<Address>()?;
    let module_address = fields
        .get("module address")
        .ok_or_else(|| anyhow::anyhow!("extra: missing Module address field"))?
        .parse::<Address>()?;
    let keystore_path = PathBuf::from(
        fields
            .get("identity file")
            .ok_or_else(|| anyhow::anyhow!("extra: missing Identity file field"))?,
    );
    let password = fields
        .get("password")
        .cloned()
        .unwrap_or_else(|| "local-cluster".to_string());
    Ok(ExtraInfo {
        safe_address,
        module_address,
        keystore_path,
        password,
    })
}

// ────────────────────────────────────────────────────────────────────────────
// Cluster RAII handle
// ────────────────────────────────────────────────────────────────────────────

struct ClusterHandle {
    /// `Some` when the test started the cluster; `None` in external mode.
    child: Option<tokio::process::Child>,
    summary: ClusterSummary,
    _tempdir: Option<tempfile::TempDir>,
    // Keeps the write end of the stdin pipe alive so the child never receives
    // EOF on stdin.  hoprd-localcluster treats stdin EOF as a shutdown signal
    // (useful in pipelines); dropping this causes graceful shutdown.
    _child_stdin: Option<tokio::process::ChildStdin>,
}

impl Drop for ClusterHandle {
    fn drop(&mut self) {
        let Some(child) = self.child.as_mut() else {
            // External cluster — do not touch it.
            return;
        };
        // SIGINT → triggers the orchestrator's Cleanup::shutdown (kills hoprd
        // subprocesses and removes the chain container).
        #[cfg(unix)]
        {
            use nix::sys::signal::{Signal, kill};
            use nix::unistd::Pid;
            if let Some(pid) = child.id() {
                let _ = kill(Pid::from_raw(pid as i32), Signal::SIGINT);
            }
        }
        // Wait up to 30 s for graceful exit, then force-kill.
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(500));
                }
                _ => {
                    let _ = child.start_kill();
                    break;
                }
            }
        }
        // _tempdir drops here and removes the data directory.
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Cluster lifecycle
// ────────────────────────────────────────────────────────────────────────────

async fn start_cluster() -> anyhow::Result<ClusterHandle> {
    // External mode: attach to an already-running cluster instead of starting one.
    if let Ok(summary_file) = std::env::var("HOPRD_CLUSTER_SUMMARY_FILE") {
        let text = std::fs::read_to_string(&summary_file)
            .with_context(|| format!("reading HOPRD_CLUSTER_SUMMARY_FILE={summary_file}"))?;
        let summary = parse_summary(&text)?;
        tracing::info!(
            blokli_url = %summary.blokli_url,
            nodes = summary.nodes.len(),
            extras = summary.extras.len(),
            "external cluster summary loaded from {summary_file}"
        );
        return Ok(ClusterHandle {
            child: None,
            summary,
            _tempdir: None,
            _child_stdin: None,
        });
    }

    let lc_bin = std::env::var("HOPRD_LOCALCLUSTER_BIN").map_err(|_| {
        anyhow::anyhow!(
            "HOPRD_LOCALCLUSTER_BIN is not set.\n\
         \n\
         Prerequisites — pick one of two modes:\n\
         \n\
         ── A. Managed mode (test owns cluster lifetime) ────────────────\n\
         \n\
         Build binaries from hoprnet/hoprnet:\n\
            cargo build --release -p hoprd-localcluster -p hoprd\n\
         \n\
         Export:\n\
            export HOPRD_LOCALCLUSTER_BIN=/path/to/hoprd-localcluster\n\
            export HOPRD_BIN=/path/to/hoprd\n\
            export HOPRD_CHAIN_IMAGE='<bloklid-anvil image tag>'\n\
            # export HOPRD_CONTAINER_RUNTIME=/path/to/container  # non-Docker runtimes\n\
         \n\
         ── B. External mode (attach to already-running cluster) ─────────\n\
         \n\
         Start the cluster in another terminal, tee output to a file:\n\
            hoprd-localcluster --size 3 --extra-identities 1 \\\n\
              --api-port-base 13000 --p2p-port-base 19000 \\\n\
              --api-token test-token-localcluster ... 2>&1 | tee /tmp/cluster.log\n\
         \n\
         Once 'localcluster running' appears, run the test:\n\
            export HOPRD_CLUSTER_SUMMARY_FILE=/tmp/cluster.log\n\
         \n\
         ── Run the test ─────────────────────────────────────────────────\n\
            RUST_LOG=info,edgli=debug \
                cargo test --test edgli_session_e2e -- --ignored --nocapture\n\
        "
        )
    })?;
    let hoprd_bin =
        std::env::var("HOPRD_BIN").map_err(|_| anyhow::anyhow!("HOPRD_BIN is not set"))?;
    let chain_image = std::env::var("HOPRD_CHAIN_IMAGE")
        .map_err(|_| anyhow::anyhow!("HOPRD_CHAIN_IMAGE is not set"))?;
    let container_runtime = std::env::var("HOPRD_CONTAINER_RUNTIME").ok();

    let tempdir = tempfile::TempDir::with_prefix("edgli-it-")?;
    let data_dir = tempdir.path().to_path_buf();

    let mut cmd = tokio::process::Command::new(&lc_bin);
    cmd.args([
        "--hoprd-bin",
        &hoprd_bin,
        "--size",
        &CLUSTER_SIZE.to_string(),
        "--extra-identities",
        "1",
        "--data-dir",
        data_dir.to_str().unwrap(),
        "--api-host",
        API_HOST,
        "--api-port-base",
        &API_PORT_BASE.to_string(),
        "--p2p-port-base",
        &P2P_PORT_BASE.to_string(),
        "--api-token",
        API_TOKEN,
        "--chain-image",
        &chain_image,
    ]);
    if let Some(runtime) = container_runtime {
        cmd.args(["--container-runtime", &runtime]);
    }
    // Disable OTel OTLP export in cluster nodes — prevents PeriodicReader failures
    // when no OTLP collector is running and keeps node logs clean.
    cmd.env("HOPRD_USE_OPENTELEMETRY", "false");
    cmd.stdout(std::process::Stdio::piped())
        .stdin(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit());

    let mut child = cmd.spawn()?;
    let stdout = child.stdout.take().expect("stdout must be captured");
    // Keep the write end of stdin alive so the cluster never sees EOF.
    // hoprd-localcluster treats stdin EOF as a shutdown signal.
    let child_stdin = child.stdin.take().expect("stdin pipe must be available");

    // Stream and collect stdout; signal when the cluster prints its ready sentinel.
    let collected = std::sync::Arc::new(tokio::sync::Mutex::new(String::new()));
    let collected_clone = collected.clone();
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<()>();
    let mut ready_tx = Some(ready_tx);

    tokio::spawn(async move {
        let mut lines = tokio::io::BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            tracing::info!(target: "localcluster", "{}", line);
            {
                let mut buf = collected_clone.lock().await;
                buf.push_str(&line);
                buf.push('\n');
            }
            if line.contains("localcluster running")
                && let Some(tx) = ready_tx.take()
            {
                let _ = tx.send(());
            }
        }
    });

    let startup_result: anyhow::Result<ClusterSummary> = async {
        tokio::time::timeout(CLUSTER_START_TIMEOUT, ready_rx)
            .await
            .map_err(|_| anyhow::anyhow!(
                "timeout ({CLUSTER_START_TIMEOUT:?}) waiting for 'localcluster running' sentinel"
            ))?
            .map_err(|_| anyhow::anyhow!(
                "localcluster stdout closed before printing 'localcluster running'"
            ))?;

        // Brief pause so any trailing summary lines in the same stdout flush are
        // buffered before we snapshot.
        tokio::time::sleep(Duration::from_millis(200)).await;

        let stdout_text = collected.lock().await.clone();
        parse_summary(&stdout_text)
    }
    .await;

    let summary = match startup_result {
        Ok(s) => s,
        Err(err) => {
            #[cfg(unix)]
            if let Some(pid) = child.id() {
                let _ = nix::sys::signal::kill(
                    nix::unistd::Pid::from_raw(pid as i32),
                    nix::sys::signal::Signal::SIGINT,
                );
            }
            let _ = child.start_kill();
            return Err(err);
        }
    };

    tracing::info!(
        blokli_url = %summary.blokli_url,
        nodes = summary.nodes.len(),
        extras = summary.extras.len(),
        "cluster summary parsed"
    );

    Ok(ClusterHandle {
        child: Some(child),
        summary,
        _tempdir: Some(tempdir),
        _child_stdin: Some(child_stdin),
    })
}

// ────────────────────────────────────────────────────────────────────────────
// Cluster node state helpers (plain reqwest)
// ────────────────────────────────────────────────────────────────────────────

const CHANNEL_STATUS_OPEN: &str = "Open";

fn node_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap()
}

fn auth_header() -> String {
    format!("Bearer {API_TOKEN}")
}

/// Loop until `check_node(node_index, port)` returns `true` for every cluster node,
/// or bail after `timeout`.
async fn poll_cluster_until<Fut>(
    timeout: Duration,
    sleep: Duration,
    success_msg: &str,
    timeout_msg: &str,
    mut check_node: impl FnMut(usize, u16) -> Fut,
) -> anyhow::Result<()>
where
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let all_ok = {
            let mut ok = true;
            for i in 0..CLUSTER_SIZE {
                if !check_node(i, API_PORT_BASE + i as u16).await {
                    ok = false;
                    break;
                }
            }
            ok
        };
        if all_ok {
            tracing::info!("{success_msg}");
            return Ok(());
        }
        anyhow::ensure!(tokio::time::Instant::now() < deadline, "{timeout_msg}");
        tokio::time::sleep(sleep).await;
    }
}

/// Poll /readyz on all cluster nodes until every one returns 200.
async fn await_nodes_ready() -> anyhow::Result<()> {
    let client = node_http_client();
    poll_cluster_until(
        READYZ_TIMEOUT,
        Duration::from_secs(3),
        &format!("all {} cluster nodes passed /readyz", CLUSTER_SIZE),
        &format!("timeout ({READYZ_TIMEOUT:?}) waiting for cluster /readyz"),
        |_i, port| {
            let client = client.clone();
            async move {
                client
                    .get(format!("http://{API_HOST}:{port}/readyz"))
                    .send()
                    .await
                    .map(|r| r.status().is_success())
                    .unwrap_or(false)
            }
        },
    )
    .await
}

/// Poll /api/v4/network/announced on each node until every node sees CLUSTER_SIZE-1 peers.
///
/// The localcluster waits for full-mesh P2P reachability (via ping_peer) before
/// opening channels, so this check typically completes immediately after
/// 'localcluster running'.
async fn await_cluster_peers_discovered() -> anyhow::Result<()> {
    let client = node_http_client();
    let expected = CLUSTER_SIZE - 1;
    poll_cluster_until(
        PEER_DISCOVERY_TIMEOUT,
        Duration::from_secs(3),
        "all cluster nodes see full peer announcement set",
        &format!("timeout ({PEER_DISCOVERY_TIMEOUT:?}) waiting for cluster peer discovery"),
        |i, port| {
            let client = client.clone();
            async move {
                let announced = async {
                    let body: serde_json::Value = client
                        .get(format!("http://{API_HOST}:{port}/api/v4/network/announced"))
                        .header("Authorization", auth_header())
                        .send()
                        .await?
                        .json()
                        .await?;
                    anyhow::Ok(body.as_array().map(|a| a.len()).unwrap_or(0))
                }
                .await
                .unwrap_or(0);
                if announced < expected {
                    tracing::debug!("node {i}: {announced}/{expected} announced peers");
                }
                announced >= expected
            }
        },
    )
    .await
}

/// Poll /api/v4/channels on each node until every node has CLUSTER_SIZE-1 Open outgoing channels.
///
/// The localcluster opens a full directed mesh between nodes before printing its
/// ready sentinel, so this check typically completes immediately after
/// 'localcluster running'.
async fn await_intracluster_channels_open() -> anyhow::Result<()> {
    let client = node_http_client();
    let expected = CLUSTER_SIZE - 1;
    poll_cluster_until(
        INTRACLUSTER_CHANNEL_TIMEOUT,
        Duration::from_secs(5),
        "full-mesh intracluster channels confirmed Open",
        &format!(
            "timeout ({INTRACLUSTER_CHANNEL_TIMEOUT:?}) waiting for intracluster channels to open"
        ),
        |i, port| {
            let client = client.clone();
            async move {
                let open = async {
                    let body: serde_json::Value = client
                        .get(format!(
                            "http://{API_HOST}:{port}/api/v4/channels?includingClosed=false"
                        ))
                        .header("Authorization", auth_header())
                        .send()
                        .await?
                        .json()
                        .await?;
                    anyhow::Ok(
                        body["outgoing"]
                            .as_array()
                            .map(|arr| {
                                arr.iter()
                                    .filter(|ch| ch["status"].as_str() == Some(CHANNEL_STATUS_OPEN))
                                    .count()
                            })
                            .unwrap_or(0),
                    )
                }
                .await
                .unwrap_or(0);
                if open < expected {
                    tracing::debug!("node {i}: {open}/{expected} outgoing channels Open");
                }
                open >= expected
            }
        },
    )
    .await
}

// ────────────────────────────────────────────────────────────────────────────
// Edge client state helpers (use EdgeNodeApi)
// ────────────────────────────────────────────────────────────────────────────

/// Wait until Edgli has at least `min_peers` cluster nodes as connected P2P peers.
async fn await_edgli_peers_connected(edgli: &Edgli, min_peers: usize) -> anyhow::Result<()> {
    let deadline = tokio::time::Instant::now() + EDGLI_PEER_DISCOVERY_TIMEOUT;
    loop {
        let peers = edgli.connected_peer_addresses().await?;
        if peers.len() >= min_peers {
            tracing::info!(peer_count = peers.len(), "Edgli P2P connectivity confirmed");
            return Ok(());
        }
        tracing::debug!("Edgli: {}/{min_peers} cluster peers connected", peers.len());
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!(
                "timeout ({EDGLI_PEER_DISCOVERY_TIMEOUT:?}) waiting for Edgli to connect to {min_peers} peer(s)"
            );
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

/// Wait until Edgli's strategy reactor has opened at least `min_open` outgoing channels.
async fn await_edgli_channels_open(edgli: &Edgli, min_open: usize) -> anyhow::Result<()> {
    let deadline = tokio::time::Instant::now() + CHANNEL_OPEN_TIMEOUT;
    loop {
        let channels: Vec<ChannelEntry> = edgli.my_outgoing_channels().await?;
        let open = channels
            .iter()
            .filter(|ch| ch.status == ChannelStatus::Open)
            .count();
        if open >= min_open {
            tracing::info!(
                open_channels = open,
                "Edgli outgoing channels confirmed Open"
            );
            return Ok(());
        }
        tracing::debug!("Edgli: {open}/{min_open} outgoing channels Open (waiting for strategy)");
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!(
                "timeout ({CHANNEL_OPEN_TIMEOUT:?}) waiting for Edgli strategy to open {min_open} channel(s)"
            );
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

/// `SessionTarget` that causes the exit node to loop data back to the entry
/// without an external TCP server.  `ExitNode(0)` maps to the built-in
/// loopback service in `HoprServerIpForwardingReactor`.
fn loopback_target() -> SessionTarget {
    SessionTarget::ExitNode(0)
}

// ────────────────────────────────────────────────────────────────────────────
// Edgli config builder
// ────────────────────────────────────────────────────────────────────────────

fn build_edgli_config(extra: &ExtraInfo) -> HoprLibConfig {
    use edgli::hopr_lib::config::{HoprProtocolConfig, TransportConfig};
    HoprLibConfig {
        host: HostConfig {
            address: HostType::IPv4("0.0.0.0".to_string()),
            port: EDGE_P2P_PORT,
        },
        publish: true,
        protocol: HoprProtocolConfig {
            transport: TransportConfig {
                announce_local_addresses: true,
                prefer_local_addresses: true,
            },
            ..Default::default()
        },
        safe_module: SafeModule {
            safe_address: extra.safe_address,
            module_address: extra.module_address,
        },
        ..Default::default()
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Bulk data pump helper
// ────────────────────────────────────────────────────────────────────────────

fn sha256_digest(data: &[u8]) -> Vec<u8> {
    let mut h = Sha256::new();
    h.update(data);
    h.finalize().to_vec()
}

/// Write `payload` into `session`, read exactly `payload.len()` bytes back from
/// the echo server, and verify SHA-256 integrity.
///
/// Writes in `PUMP_BATCH_BYTES`-sized chunks with a 100 ms pause between each
/// batch.  Without pacing, `write_all(1 MiB)` submits ~1030 HOPR packets to
/// the Rayon encoding pool simultaneously.  The pool's `PACKET_ENCODING_TIMEOUT`
/// is 150 ms; with `then_concurrent(8×cpus)` concurrent encoding futures sharing
/// a pool of only `cpus` Rayon threads, tasks queued near the end wait ≈
/// 7×encode_time ≥ 150 ms and are silently dropped, causing the echo read to
/// time out.  Writing 16 packets per batch keeps Rayon queue wait ≈ 1×encode_time
/// (≤ 30 ms), well inside the timeout window.
///
/// Does NOT call `shutdown()` on the write half: HOPR sessions do not support
/// TCP half-close.
async fn pump_and_verify(session: HoprSession, payload: &[u8], label: &str) -> anyhow::Result<()> {
    let (mut r, mut w) = tokio::io::split(session);
    let payload_bytes = payload.to_vec();
    let expected = sha256_digest(payload);
    let n = payload.len();

    let writer = tokio::spawn(async move {
        let mut offset = 0;
        while offset < payload_bytes.len() {
            let end = (offset + PUMP_BATCH_BYTES).min(payload_bytes.len());
            w.write_all(&payload_bytes[offset..end]).await?;
            w.flush().await?;
            offset = end;
            if offset < payload_bytes.len() {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
        Ok::<_, std::io::Error>(())
    });

    let mut received = vec![0u8; n];
    if let Err(err) = tokio::time::timeout(PUMP_TIMEOUT, r.read_exact(&mut received))
        .await
        .map_err(|_| anyhow::anyhow!("{label}: read timeout ({PUMP_TIMEOUT:?}) after {n} B"))
        .and_then(|r| r.map_err(|e| anyhow::anyhow!("{label}: read error: {e}")))
    {
        writer.abort();
        let _ = writer.await;
        return Err(err);
    }

    tokio::time::timeout(PUMP_TIMEOUT, writer)
        .await
        .map_err(|_| anyhow::anyhow!("{label}: writer timeout ({PUMP_TIMEOUT:?})"))?
        .map_err(|e| anyhow::anyhow!("{label}: writer panicked: {e}"))?
        .map_err(|e| anyhow::anyhow!("{label}: write error: {e}"))?;

    anyhow::ensure!(
        sha256_digest(&received) == expected,
        "{label}: SHA-256 mismatch — {n} bytes corrupted in transit"
    );
    tracing::info!("{label}: ✓ {n} B verified (SHA-256 match)");
    Ok(())
}

// ────────────────────────────────────────────────────────────────────────────
// The test
// ────────────────────────────────────────────────────────────────────────────

/// End-to-end test: 3-node localcluster + Edgli + 1 MiB session pump.
///
/// Gated behind `#[ignore]` — see module-level docs for required setup.
#[ignore]
#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn edgli_sends_one_megabyte_through_cluster() -> anyhow::Result<()> {
    // ── 1. Start the 3-node HOPR cluster ────────────────────────────────────
    // hoprd-localcluster provisions identities, deploys Safes, starts hoprd
    // processes, waits for peer discovery, opens full-mesh channels, then
    // prints 'localcluster running'. All of this happens before we proceed.
    tracing::info!("starting hoprd-localcluster (3 nodes + 1 extra identity)");
    let cluster = start_cluster().await?;
    let summary = cluster.summary.clone();

    // ── 2. Verify cluster state ──────────────────────────────────────────────
    tracing::info!("verifying cluster: /readyz");
    await_nodes_ready().await?;
    tracing::info!("verifying cluster: full P2P peer visibility");
    await_cluster_peers_discovered().await?;
    tracing::info!("verifying cluster: full-mesh outgoing channels Open");
    await_intracluster_channels_open().await?;

    // ── 3. Boot Edgli with the pre-funded extra identity ────────────────────
    let extra = &summary.extras[0];
    let hopr_keys: HoprKeys = IdentityRetrievalModes::FromFile {
        password: &extra.password,
        id_path: extra.keystore_path.to_str().unwrap(),
    }
    .try_into()?;

    tracing::info!(
        safe    = %extra.safe_address,
        module  = %extra.module_address,
        keystore = %extra.keystore_path.display(),
        "booting Edgli"
    );

    // Increase tx confirmation budget: the default (2 s) is too tight for
    // blokli's SSE indexing on the Anvil test chain.
    let connector_cfg = BlockchainConnectorConfig {
        tx_timeout_multiplier: 10,
        ..Default::default()
    };
    let edgli = Edgli::new(
        build_edgli_config(extra),
        hopr_keys,
        Some(summary.blokli_url.clone()),
        Some(connector_cfg),
        |s: EdgliInitState| tracing::info!(?s, "edgli init"),
    )
    .await?;

    // ── 5. Verify Edgli P2P connectivity ────────────────────────────────────
    // Edgli must reach at least one cluster node before the strategy can open
    // channels.  On a local setup this typically happens within seconds.
    tracing::info!("waiting for Edgli to connect to at least 1 cluster peer");
    await_edgli_peers_connected(&edgli, 1).await?;

    // ── 6. Start the channel-lifecycle strategy reactor ─────────────────────
    // desired_message_count = 1000 × expected session packets (forward + SURB
    // return). This absorbs background probe traffic and the probabilistic
    // accumulation of winning tickets: at Rotsee values (price ≈ 1e-16 wxHOPR,
    // win_prob ≈ 0.000125) the expected drain per packet is tiny, but the
    // channel must hold at least `ticket_price` per winning ticket. Scaling by
    // 1000× ensures the computed initial_balance comfortably exceeds the
    // expected winning-ticket accumulation from both test and probe traffic.
    let session_packets = (PAYLOAD_SIZE / SESSION_MTU + 1) * 2; // fwd + SURB return
    let sizing = IncentiveConfiguration {
        desired_message_count: (session_packets as u64) * 1_000,
        min_open_channels: 1,
        target_open_channels: CLUSTER_SIZE,
    };
    let mut strat_cfg = default_strategy_cfg(&edgli, sizing).await?;

    // Integration-test overrides:
    // - Fresh nodes have no graph history → quality scores are 0.0, below the
    //   default threshold of 0.5.  Set to 0 so all connected peers are eligible.
    // - Disable require_observed_since_start for a deterministic first tick.
    // - 10 s tick_interval (vs. 60 s default) fits the test budget.
    for kind in &mut strat_cfg.strategies {
        let EdgeStrategyKind::ChannelLifecycle(lc) = kind;
        lc.eligibility = EligibilityConfig {
            min_peer_quality_score: 0.0,
            require_observed_since_start: false,
            ..Default::default()
        };
        lc.tick_interval = Duration::from_secs(10);
    }

    let EdgeStrategyKind::ChannelLifecycle(lc0) = &strat_cfg.strategies[0];
    tracing::info!(
        initial_balance = ?lc0.funding.initial_balance,
        target_channels = sizing.target_open_channels,
        "strategy configured; starting reactor"
    );

    let _reactor_handle = edgli.run_reactor_from_cfg(strat_cfg)?;

    // ── 7. Wait for Edgli outgoing channels to open ──────────────────────────
    tracing::info!("waiting for strategy to open ≥1 outgoing channel");
    await_edgli_channels_open(&edgli, 1).await?;

    // ── 8. Prepare 1 MiB random payload ─────────────────────────────────────
    // Random bytes produce unique HOPR packet ciphertexts on every test run,
    // preventing false replay-detection hits in cluster nodes' in-memory
    // packet-tag caches (which persist across Edgli reconnections to the same
    // hoprd instance).
    let mut payload = vec![0u8; PAYLOAD_SIZE];
    rand::thread_rng().fill_bytes(&mut payload);

    // ── 9. 0-hop session — direct path, no relay ─────────────────────────────
    // Forward: Edgli → node 0 (0-hop in HOPR terms = direct, no relay)
    // Return:  node 0 echoes via built-in loopback service (ExitNode(0))
    //
    // NoRateControl disables the exit node's egress rate limiter (default: 10
    // pkt/s initial rate).  Without it, returning 1 MiB (~1030 packets) at
    // 10 pkt/s takes ~103 s, nearly exhausting the 120 s PUMP_TIMEOUT before
    // the rate-adaptive controller has time to ramp up.
    tracing::info!("opening 0-hop session to node 0 (loopback)");
    let (session_0h, _) = edgli
        .connect_to(
            summary.nodes[0].address,
            loopback_target(),
            HoprSessionClientConfig {
                forward_path: HopRouting::try_from(0_usize)?,
                return_path: HopRouting::try_from(0_usize)?,
                capabilities: (SessionCapability::Segmentation | SessionCapability::NoRateControl),
                ..Default::default()
            },
        )
        .await?;
    pump_and_verify(session_0h, &payload, "0-hop").await?;

    // ── 10. 1-hop session — full relay path ──────────────────────────────────
    // Forward: Edgli → relay (auto-selected) → node 2 (1-hop in HOPR terms)
    // Return:  node 2 echoes via built-in loopback service (ExitNode(0))
    // Intracluster channels (relay→node2, node2→relay) were opened by the
    // localcluster before it printed 'localcluster running'.
    tracing::info!("opening 1-hop session to node 2 (loopback)");
    let (session_1h, _) = edgli
        .connect_to(
            summary.nodes[2].address,
            loopback_target(),
            HoprSessionClientConfig {
                forward_path: HopRouting::try_from(1_usize)?,
                return_path: HopRouting::try_from(1_usize)?,
                capabilities: (SessionCapability::Segmentation | SessionCapability::NoRateControl),
                ..Default::default()
            },
        )
        .await?;
    pump_and_verify(session_1h, &payload, "1-hop").await?;

    // ── 11. Final state assertion ─────────────────────────────────────────────
    let channels: Vec<ChannelEntry> = edgli.my_outgoing_channels().await?;
    let open_count = channels
        .iter()
        .filter(|ch| ch.status == ChannelStatus::Open)
        .count();
    assert!(
        open_count >= 1,
        "expected ≥1 Open outgoing channel after pumping; got {open_count}"
    );
    tracing::info!(open_channels = open_count, "test passed ✓");

    // Drop Edgli first to cancel background tasks cleanly, then the cluster.
    drop(edgli);
    drop(cluster);

    Ok(())
}
