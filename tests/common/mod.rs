//! Shared test harness for end-to-end session integration tests.
//!
//! Used by both `edgli_session_e2e` (local cluster) and `edgli_session_rotsee`
//! (Rotsee testnet).  Each test file declares `mod common;` and calls
//! [`run_one_megabyte_session_test`] with its [`Network`] variant.

#![allow(dead_code)]

use anyhow::Context as _;
use std::{collections::HashMap, path::PathBuf, time::Duration};

use edgli::{
    Edgli, EdgliInitState,
    hopr_lib::{
        HopRouting, HoprKeys, HoprSessionClientConfig, IdentityRetrievalModes,
        api::{
            chain::ChainKeyOperations as _,
            node::{HasChainApi as _, HasTransportApi as _, HoprSessionClientOperations},
            types::{
                internal::channels::{ChannelEntry, ChannelStatus},
                primitive::prelude::Address,
            },
        },
        config::{HoprLibConfig, HostConfig, HostType, SafeModule},
        exports::transport::SessionCapability,
        exports::transport::{HoprSession, SessionTarget, SurbBalancerConfig},
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

pub const PAYLOAD_SIZE: usize = 1_024 * 1_024; // 1 MiB
/// HOPR session MTU (bytes that fit in a single HOPR packet payload).
/// Equal to `hopr_transport_session::SESSION_MTU = 1018`.
pub const SESSION_MTU: usize = 1018;
/// Packets per write-batch in `pump_and_verify`.  Keeps the Rayon encoding
/// queue well below the `PACKET_ENCODING_TIMEOUT = 150 ms` threshold even
/// when the host machine is under load running a 3-node cluster.
pub const PUMP_BATCH_PACKETS: usize = 16;
pub const PUMP_BATCH_BYTES: usize = PUMP_BATCH_PACKETS * SESSION_MTU;
pub const CLUSTER_SIZE: usize = 3;
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
const EXIT_PEER_PROBE_TIMEOUT: Duration = Duration::from_secs(120);

// ────────────────────────────────────────────────────────────────────────────
// Network selector
// ────────────────────────────────────────────────────────────────────────────

/// Which network to run the session pump against.
#[derive(Clone, Copy)]
pub enum Network {
    /// 3-node `hoprd-localcluster` on the local machine (Anvil chain).
    Local,
    /// Public Rotsee testnet (Gnosis chain).  Requires a pre-funded identity
    /// supplied via `EDGLI_ROTSEE_*` env vars.
    Rotsee,
}

// ────────────────────────────────────────────────────────────────────────────
// Per-network tuning knobs
// ────────────────────────────────────────────────────────────────────────────

pub struct EdgliTuning {
    /// Blockchain connector configuration (controls tx-confirmation timeouts).
    /// Use a `tx_timeout_multiplier` of 10 for Anvil (slow SSE indexing) and
    /// the default (1) for Gnosis Chain / Rotsee.
    pub connector_cfg: BlockchainConnectorConfig,
    /// Whether to announce and prefer local (RFC-1918) addresses.  True only
    /// on a same-host cluster.
    pub prefer_local_addresses: bool,
    pub announce_local: bool,
    /// Channel-lifecycle strategy tick interval.
    pub strategy_tick: Duration,
    /// Minimum peer quality score for channel eligibility.
    pub min_peer_quality: f64,
    /// Whether to require the peer has been observed since Edgli started.
    pub require_observed: bool,
    /// How long to wait for the strategy to open at least one outgoing channel.
    /// Rotsee needs more headroom: 60 s tick + Gnosis Chain confirmation latency.
    pub channel_open_timeout: Duration,
    /// Fixed exit-node address for session targets.  When set, both the
    /// first and second sessions are directed to this node instead of a
    /// dynamically discovered channel peer.  Required for Rotsee, where relay
    /// nodes do not run the exit-node service.
    pub exit_node: Option<Address>,
    /// Timeout for each `pump_and_verify` call (0-hop and 1-hop separately).
    /// Rotsee needs more headroom: real-network latency + possible rate-limiter
    /// ramp-up on the exit node's loopback path.
    pub pump_timeout: Duration,
    /// Minimum ack rate required for relay edges to be eligible for path
    /// selection. On a local cluster neighbour probes succeed, so the default
    /// 0.1 is fine. On Rotsee the edge client runs behind NAT: neighbor probes
    /// use a 0-hop return path, which requires relays to dial the client
    /// directly — impossible behind NAT. All relay probes therefore timeout
    /// and ack_rate stays 0, blocking 1-hop path selection entirely. Setting
    /// 0.0 falls back to pure channel-topology routing (channels exist in the
    /// graph from SSE events) without requiring any probe success.
    pub min_ack_rate: f64,
}

impl EdgliTuning {
    fn local() -> Self {
        Self {
            connector_cfg: BlockchainConnectorConfig {
                // Increase tx confirmation budget: the default is too tight for
                // blokli's SSE indexing on the Anvil test chain.
                tx_timeout_multiplier: 10,
                ..Default::default()
            },
            prefer_local_addresses: true,
            announce_local: true,
            strategy_tick: Duration::from_secs(10),
            min_peer_quality: 0.0,
            require_observed: false,
            channel_open_timeout: Duration::from_secs(120),
            exit_node: None,
            pump_timeout: Duration::from_secs(120),
            min_ack_rate: 0.1, // local cluster probes succeed — use default quality gate
        }
    }

    fn rotsee() -> Self {
        Self {
            connector_cfg: BlockchainConnectorConfig::default(),
            prefer_local_addresses: false,
            announce_local: false,
            strategy_tick: Duration::from_secs(30),
            // Rotsee peers have ~150-200 ms RTT; latency_score caps at 0.3 for
            // that range, so even a perfect probe rate yields at most 0.30.
            // Setting 0.1 accepts any peer that has had at least one successful probe.
            min_peer_quality: 0.1,
            require_observed: true,
            // 30 s tick + Gnosis Chain confirmation + on-chain sync latency.
            // Allow several ticks before giving up.
            channel_open_timeout: Duration::from_secs(300),
            exit_node: None, // filled in by provision_rotsee from EDGLI_ROTSEE_EXIT_NODE
            // Real-network loopback: allow for rate-limiter ramp-up and latency
            // variation on the echo path.
            pump_timeout: Duration::from_secs(300),
            // Rotsee: client runs behind NAT. Neighbour probes use a 0-hop
            // return path (relay dials client directly), which fails behind NAT.
            // All relay probe acks timeout → ack_rate stays 0 → min_ack_rate=0.1
            // blocks all 1-hop path selection. Setting 0.0 allows the path
            // selector to route through existing payment channels (populated via
            // SSE events) without requiring any probe history.
            min_ack_rate: 0.0,
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Cluster summary types
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct NodeInfo {
    pub address: Address,
}

#[derive(Debug, Clone)]
pub struct ExtraInfo {
    pub safe_address: Address,
    pub module_address: Address,
    pub keystore_path: PathBuf,
    pub password: String,
}

#[derive(Debug, Clone)]
pub struct ClusterSummary {
    pub blokli_url: String,
    pub nodes: Vec<NodeInfo>,
    pub extras: Vec<ExtraInfo>,
}

// ────────────────────────────────────────────────────────────────────────────
// RAII guard
// ────────────────────────────────────────────────────────────────────────────

/// Owns the resource backing the test network.  Dropping this cleans up.
pub enum NetworkGuard {
    Local(Box<ClusterHandle>),
    /// No-op guard for external / pre-existing networks.
    Rotsee,
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

pub struct ClusterHandle {
    /// `Some` when the test started the cluster; `None` in external mode.
    child: Option<tokio::process::Child>,
    pub summary: ClusterSummary,
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
// Provisioning
// ────────────────────────────────────────────────────────────────────────────

/// Provision the test environment for the given network.
///
/// Returns the cluster summary, a RAII guard (drop to clean up), and the
/// per-network Edgli tuning knobs.
pub async fn provision(
    net: Network,
) -> anyhow::Result<(ClusterSummary, NetworkGuard, EdgliTuning)> {
    match net {
        Network::Local => {
            let handle = provision_local().await?;
            let summary = handle.summary.clone();
            Ok((
                summary,
                NetworkGuard::Local(Box::new(handle)),
                EdgliTuning::local(),
            ))
        }
        Network::Rotsee => {
            let (summary, guard, exit_node) = provision_rotsee()?;
            Ok((
                summary,
                guard,
                EdgliTuning {
                    exit_node,
                    ..EdgliTuning::rotsee()
                },
            ))
        }
    }
}

async fn provision_local() -> anyhow::Result<ClusterHandle> {
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

/// Read Rotsee identity + network config from environment variables.
///
/// Required env vars:
/// - `EDGLI_ROTSEE_BLOKLI_URL`        — Blokli endpoint
/// - `EDGLI_ROTSEE_IDENTITY_FILE`     — path to funded keystore JSON
/// - `EDGLI_ROTSEE_IDENTITY_PASSWORD` — keystore password
/// - `EDGLI_ROTSEE_SAFE_ADDRESS`      — Safe contract address (0x…)
/// - `EDGLI_ROTSEE_MODULE_ADDRESS`    — HOPR module contract address (0x…)
fn provision_rotsee() -> anyhow::Result<(ClusterSummary, NetworkGuard, Option<Address>)> {
    fn required(var: &str) -> anyhow::Result<String> {
        std::env::var(var).map_err(|_| {
            anyhow::anyhow!(
                "{var} is not set.\n\
                 \n\
                 Required env vars for the Rotsee test:\n\
                   EDGLI_ROTSEE_BLOKLI_URL        — Blokli endpoint\n\
                   EDGLI_ROTSEE_IDENTITY_FILE     — path to funded keystore JSON\n\
                   EDGLI_ROTSEE_IDENTITY_PASSWORD — keystore password\n\
                   EDGLI_ROTSEE_SAFE_ADDRESS      — Safe contract address (0x…)\n\
                   EDGLI_ROTSEE_MODULE_ADDRESS    — HOPR module contract address (0x…)\n\
                 \n\
                 Run the test with:\n\
                   RUST_LOG=info,edgli=debug \
                       cargo test --test edgli_session_rotsee -- --ignored --nocapture\n\
                "
            )
        })
    }

    let blokli_url = required("EDGLI_ROTSEE_BLOKLI_URL")?;
    let keystore_path = PathBuf::from(required("EDGLI_ROTSEE_IDENTITY_FILE")?);
    let password = required("EDGLI_ROTSEE_IDENTITY_PASSWORD")?;
    let safe_address = required("EDGLI_ROTSEE_SAFE_ADDRESS")?
        .parse::<Address>()
        .context("EDGLI_ROTSEE_SAFE_ADDRESS: invalid address")?;
    let module_address = required("EDGLI_ROTSEE_MODULE_ADDRESS")?
        .parse::<Address>()
        .context("EDGLI_ROTSEE_MODULE_ADDRESS: invalid address")?;

    let summary = ClusterSummary {
        blokli_url,
        nodes: vec![], // Rotsee peers are discovered after Edgli boots
        extras: vec![ExtraInfo {
            safe_address,
            module_address,
            keystore_path,
            password,
        }],
    };

    let exit_node = std::env::var("EDGLI_ROTSEE_EXIT_NODE")
        .ok()
        .map(|s| s.parse::<Address>())
        .transpose()
        .context("EDGLI_ROTSEE_EXIT_NODE: invalid address")?;

    tracing::info!(
        blokli_url = %summary.blokli_url,
        safe    = %summary.extras[0].safe_address,
        module  = %summary.extras[0].module_address,
        keystore = %summary.extras[0].keystore_path.display(),
        ?exit_node,
        "Rotsee provisioning from env vars"
    );

    Ok((summary, NetworkGuard::Rotsee, exit_node))
}

// ────────────────────────────────────────────────────────────────────────────
// Cluster node state helpers (plain reqwest — local cluster only)
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
        let results = futures::future::join_all(
            (0..CLUSTER_SIZE).map(|i| check_node(i, API_PORT_BASE + i as u16)),
        )
        .await;
        if results.into_iter().all(|ok| ok) {
            tracing::info!("{success_msg}");
            return Ok(());
        }
        anyhow::ensure!(tokio::time::Instant::now() < deadline, "{timeout_msg}");
        tokio::time::sleep(sleep).await;
    }
}

/// Poll /readyz on all cluster nodes until every one returns 200.
pub async fn await_nodes_ready() -> anyhow::Result<()> {
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
pub async fn await_cluster_peers_discovered() -> anyhow::Result<()> {
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
pub async fn await_intracluster_channels_open() -> anyhow::Result<()> {
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

async fn poll_edgli_until<F, Fut>(
    timeout: Duration,
    sleep: Duration,
    timeout_msg: impl Fn() -> String,
    mut check: F,
) -> anyhow::Result<()>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<bool>>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if check().await? {
            return Ok(());
        }
        anyhow::ensure!(tokio::time::Instant::now() < deadline, "{}", timeout_msg());
        tokio::time::sleep(sleep).await;
    }
}

/// Wait until Edgli has at least `min_peers` connected P2P peers.
pub async fn await_edgli_peers_connected(edgli: &Edgli, min_peers: usize) -> anyhow::Result<()> {
    poll_edgli_until(
        EDGLI_PEER_DISCOVERY_TIMEOUT,
        Duration::from_secs(3),
        || format!("timeout ({EDGLI_PEER_DISCOVERY_TIMEOUT:?}) waiting for Edgli to connect to {min_peers} peer(s)"),
        || async {
            let peers = edgli.connected_peer_addresses().await?;
            if peers.len() >= min_peers {
                tracing::info!(peer_count = peers.len(), "Edgli P2P connectivity confirmed");
                return Ok(true);
            }
            tracing::debug!("Edgli: {}/{min_peers} peers connected", peers.len());
            Ok(false)
        },
    )
    .await
}

/// Wait until Edgli's strategy reactor has opened at least `min_open` outgoing channels.
pub async fn await_edgli_channels_open(
    edgli: &Edgli,
    min_open: usize,
    timeout: Duration,
) -> anyhow::Result<()> {
    poll_edgli_until(
        timeout,
        Duration::from_secs(5),
        || {
            format!(
                "timeout ({timeout:?}) waiting for Edgli strategy to open {min_open} channel(s)"
            )
        },
        || async {
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
                return Ok(true);
            }
            tracing::debug!(
                "Edgli: {open}/{min_open} outgoing channels Open (waiting for strategy)"
            );
            Ok(false)
        },
    )
    .await
}

/// Wait until `target` is physically connected and has received at least one probe response.
///
/// Resolves the chain address to an offchain key upfront (forward lookup, always available
/// for on-chain registered nodes), then polls `all_network_peers(0.0)` which returns
/// connected peers that have any probe observation — i.e. probed at least once regardless
/// of quality score.
async fn await_edgli_exit_peer_ready(edgli: &Edgli, target: Address) -> anyhow::Result<()> {
    // Forward lookup: chain address → offchain key. This uses the on-chain registry
    // and succeeds as soon as the chain connector has indexed the peer's account
    // (always true for announced Rotsee nodes by the time channels are open).
    let offchain_key = edgli
        .chain_api()
        .chain_key_to_packet_key(&target)
        .map_err(|e| anyhow::anyhow!("{e}"))?
        .ok_or_else(|| {
            anyhow::anyhow!("exit peer {target} has no offchain key — not registered on chain")
        })?;

    poll_edgli_until(
        EXIT_PEER_PROBE_TIMEOUT,
        Duration::from_secs(5),
        || format!("timeout ({EXIT_PEER_PROBE_TIMEOUT:?}) waiting for exit peer {target} to be connected and probed"),
        || async {
            // quality floor 0.0 = connected AND has any probe observation
            let peers = edgli
                .transport()
                .all_network_peers(0.0)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;

            if peers.iter().any(|(k, _)| k == &offchain_key) {
                tracing::info!(%target, "exit peer connected and probed");
                return Ok(true);
            }
            tracing::debug!(%target, qualified_peers = peers.len(), "exit peer not yet connected/probed");
            Ok(false)
        },
    )
    .await
}

/// Select session destinations that work for both 0-hop and 1-hop.
///
/// 0-hop target: the first open outgoing channel counterparty (Edgli has a
/// direct channel to it, so a 0-hop session is routable).
///
/// 1-hop target: any connected peer that is different from the 0-hop target
/// (the 0-hop target acts as the relay, with its intranetwork channels
/// carrying the forward path).
async fn select_session_targets(edgli: &Edgli) -> anyhow::Result<(Address, Address)> {
    let (raw_channels, peers) = tokio::try_join!(
        edgli.my_outgoing_channels(),
        edgli.connected_peer_addresses()
    )?;
    let channels: Vec<ChannelEntry> = raw_channels
        .into_iter()
        .filter(|c| c.status == ChannelStatus::Open)
        .collect();
    anyhow::ensure!(!channels.is_empty(), "no open outgoing channels");
    let zero_hop = channels[0].destination;

    let one_hop = peers
        .into_iter()
        .find(|a| *a != zero_hop)
        .ok_or_else(|| anyhow::anyhow!("need ≥2 distinct connected peers for 1-hop test"))?;

    tracing::info!(
        zero_hop = %zero_hop,
        one_hop  = %one_hop,
        "session targets selected"
    );
    Ok((zero_hop, one_hop))
}

/// `SessionTarget` that causes the exit node to loop data back to the entry
/// without an external TCP server.  `ExitNode(0)` maps to the built-in
/// loopback service in `HoprServerIpForwardingReactor`.
pub fn loopback_target() -> SessionTarget {
    SessionTarget::ExitNode(0)
}

// ────────────────────────────────────────────────────────────────────────────
// Edgli config builder
// ────────────────────────────────────────────────────────────────────────────

pub fn build_edgli_config(extra: &ExtraInfo, tuning: &EdgliTuning) -> HoprLibConfig {
    use edgli::hopr_lib::config::{HoprProtocolConfig, TransportConfig};
    use edgli::hopr_lib::exports::transport::path::PathPlannerConfig;
    HoprLibConfig {
        host: HostConfig {
            address: HostType::IPv4("0.0.0.0".to_string()),
            port: EDGE_P2P_PORT,
        },
        publish: true,
        protocol: HoprProtocolConfig {
            transport: TransportConfig {
                announce_local_addresses: tuning.announce_local,
                prefer_local_addresses: tuning.prefer_local_addresses,
            },
            path_planner: PathPlannerConfig {
                min_ack_rate: tuning.min_ack_rate,
                ..Default::default()
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

pub fn sha256_digest(data: &[u8]) -> Vec<u8> {
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
pub async fn pump_and_verify(
    session: HoprSession,
    payload: &[u8],
    label: &str,
    timeout: Duration,
) -> anyhow::Result<()> {
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
    if let Err(err) = tokio::time::timeout(timeout, r.read_exact(&mut received))
        .await
        .map_err(|_| anyhow::anyhow!("{label}: read timeout ({timeout:?}) after {n} B"))
        .and_then(|r| r.map_err(|e| anyhow::anyhow!("{label}: read error: {e}")))
    {
        writer.abort();
        let _ = writer.await;
        return Err(err);
    }

    tokio::time::timeout(timeout, writer)
        .await
        .map_err(|_| anyhow::anyhow!("{label}: writer timeout ({timeout:?})"))?
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
// Top-level harness
// ────────────────────────────────────────────────────────────────────────────

/// Run the 1 MiB session pump (0-hop + 1-hop) against the given network.
///
/// Orchestrates: provision → Edgli boot → strategy reactor → channel wait →
/// target selection → 0-hop pump → 1-hop pump → final assertion.
pub async fn run_one_megabyte_session_test(net: Network) -> anyhow::Result<()> {
    // ── 1. Provision the network ─────────────────────────────────────────────
    let (summary, _guard, tuning) = provision(net).await?;

    // ── 2. Verify cluster state (local only) ─────────────────────────────────
    if matches!(net, Network::Local) {
        tracing::info!("verifying cluster: /readyz");
        await_nodes_ready().await?;
        tracing::info!("verifying cluster: full P2P peer visibility");
        await_cluster_peers_discovered().await?;
        tracing::info!("verifying cluster: full-mesh outgoing channels Open");
        await_intracluster_channels_open().await?;
    }

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

    let edgli = Edgli::new(
        build_edgli_config(extra, &tuning),
        hopr_keys,
        Some(summary.blokli_url.clone()),
        Some(tuning.connector_cfg),
        |s: EdgliInitState| tracing::info!(?s, "edgli init"),
    )
    .await?;

    // ── 4. Verify Edgli P2P connectivity ─────────────────────────────────────
    // Wait for at least 2 peers so select_session_targets can pick distinct
    // destinations for the 0-hop and 1-hop sessions.
    tracing::info!("waiting for Edgli to connect to at least 2 peers");
    await_edgli_peers_connected(&edgli, 2).await?;

    // ── 5. Start the channel-lifecycle strategy reactor ──────────────────────
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
        ..Default::default()
    };
    let mut strat_cfg = default_strategy_cfg(&edgli, sizing).await?;

    for kind in &mut strat_cfg.strategies {
        let EdgeStrategyKind::ChannelLifecycle(lc) = kind;
        lc.eligibility = EligibilityConfig {
            min_peer_quality_score: tuning.min_peer_quality,
            require_observed_since_start: tuning.require_observed,
            ..Default::default()
        };
        lc.tick_interval = tuning.strategy_tick;
    }

    let EdgeStrategyKind::ChannelLifecycle(lc0) = &strat_cfg.strategies[0];
    tracing::info!(
        initial_balance = ?lc0.funding.initial_balance,
        target_channels = sizing.target_open_channels,
        "strategy configured; starting reactor"
    );

    let _reactor_handle = edgli.run_reactor_from_cfg(strat_cfg)?;

    // ── 6. Wait for Edgli outgoing channels to open ──────────────────────────
    tracing::info!("waiting for strategy to open ≥1 outgoing channel");
    await_edgli_channels_open(&edgli, 1, tuning.channel_open_timeout).await?;

    // ── 7. Wait for exit peer to be connected and probed (when pinned) ───────
    // `all_network_peers` returns only connected peers with an observed quality
    // score above the threshold — i.e. at least one successful probe response.
    // Without this gate the session open can race against the probe subsystem
    // and fail with "no route to host" on Rotsee.
    if let Some(exit) = tuning.exit_node {
        tracing::info!(%exit, "waiting for exit peer to be physically connected and probed");
        await_edgli_exit_peer_ready(&edgli, exit).await?;
    }

    // ── 8. Select session targets ─────────────────────────────────────────────
    let (dest_0h, dest_1h) = if let Some(exit) = tuning.exit_node {
        tracing::info!(%exit, "using configured exit node for both 0-hop and 1-hop sessions");
        (exit, exit)
    } else {
        select_session_targets(&edgli).await?
    };

    // ── 9. Prepare 1 MiB random payload ─────────────────────────────────────
    // Random bytes produce unique HOPR packet ciphertexts on every test run,
    // preventing false replay-detection hits in cluster nodes' in-memory
    // packet-tag caches (which persist across Edgli reconnections to the same
    // hoprd instance).
    let mut payload = vec![0u8; PAYLOAD_SIZE];
    rand::thread_rng().fill_bytes(&mut payload);

    // ── 10. 0-hop session — direct path, no relay ────────────────────────────
    // NoRateControl disables the exit node's egress rate limiter (default: 10
    // pkt/s initial rate).  Without it, returning 1 MiB (~1030 packets) at
    // 10 pkt/s takes ~103 s, nearly exhausting the pump timeout before the
    // rate-adaptive controller has time to ramp up.
    tracing::info!("opening 0-hop session (loopback)");
    let (session_0h, _) = edgli
        .connect_to(
            dest_0h,
            loopback_target(),
            HoprSessionClientConfig {
                forward_path: HopRouting::try_from(0_usize)?,
                return_path: HopRouting::try_from(0_usize)?,
                capabilities: (SessionCapability::Segmentation | SessionCapability::NoRateControl),
                // Keep the SURB pre-fill burst below the per-peer send-channel capacity
                // (1000 slots). The default target of 7000 SURBs saturates the channel,
                // causing session data to time out and be dropped silently with no
                // reconnect (hoprnet eb21c1354c removed cache invalidation on timeout).
                // 600 SURBs fit in one burst; the balancer replenishes as they are consumed.
                surb_management: Some(SurbBalancerConfig {
                    target_surb_buffer_size: 600,
                    max_surbs_per_sec: 300,
                    ..SurbBalancerConfig::default()
                }),
                ..Default::default()
            },
        )
        .await?;
    pump_and_verify(session_0h, &payload, "0-hop", tuning.pump_timeout).await?;

    // ── 11. 1-hop session — full relay path ──────────────────────────────────
    tracing::info!("opening 1-hop session (loopback)");
    let (session_1h, _) = edgli
        .connect_to(
            dest_1h,
            loopback_target(),
            HoprSessionClientConfig {
                forward_path: HopRouting::try_from(1_usize)?,
                return_path: HopRouting::try_from(1_usize)?,
                capabilities: (SessionCapability::Segmentation | SessionCapability::NoRateControl),
                surb_management: Some(SurbBalancerConfig {
                    target_surb_buffer_size: 600,
                    max_surbs_per_sec: 300,
                    ..SurbBalancerConfig::default()
                }),
                ..Default::default()
            },
        )
        .await?;
    pump_and_verify(session_1h, &payload, "1-hop", tuning.pump_timeout).await?;

    // ── 12. Final state assertion ─────────────────────────────────────────────
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

    // Drop Edgli first to cancel background tasks cleanly.
    drop(edgli);
    // _guard drops here (kills local cluster if managed mode; no-op for Rotsee).

    Ok(())
}
