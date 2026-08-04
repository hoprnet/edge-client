//! Shared test harness for end-to-end session integration tests.
//!
//! Used by both `edgli_session_e2e` (local cluster) and `edgli_session_rotsee`
//! (Rotsee testnet).  Each test file declares `mod common;` and calls
//! [`run_session_throughput_test`] with its [`Network`] variant.

#![allow(dead_code)]

use anyhow::Context as _;
use std::{path::PathBuf, time::Duration};
use tracing_subscriber::{
    EnvFilter, Layer as _, layer::SubscriberExt as _, util::SubscriberInitExt as _,
};

use edgli::{
    BlokliEndpoint, Edgli, EdgliInitState, PathPlannerConfig,
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
        exports::transport::{FlowControlConfig, HoprSession, SessionTarget, SurbBalancerConfig},
    },
    latency_path_planner_config,
    strategy::{EdgeStrategyKind, IncentiveConfiguration, SelectorProfile, default_strategy_cfg},
    traits::EdgeNodeApi,
};
use hopr_chain_connector::BlockchainConnectorConfig;
use rand::RngExt as _;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};

// ────────────────────────────────────────────────────────────────────────────
// Constants
// ────────────────────────────────────────────────────────────────────────────

pub const PAYLOAD_SIZE: usize = 20 * 1_024 * 1_024; // 20 MiB
/// Bytes that fit in a single HOPR packet payload. Re-exported rather than
/// hardcoded so it cannot drift from the pinned transport.
pub use edgli::hopr_lib::SESSION_MTU;
/// Packets per write-batch in `pump_and_verify`.  16 packets at SESSION_MTU
/// bytes each ≈ 16 kB per flush, giving natural backpressure granularity.
pub const PUMP_BATCH_PACKETS: usize = 16;
pub const PUMP_BATCH_BYTES: usize = PUMP_BATCH_PACKETS * SESSION_MTU;
/// Paced send rate (HOPR packets/sec). We send the *entire* payload but pace it so the
/// writer does not burst it all into the 16384-segment session socket buffer at once (which
/// masks transport backpressure and triggers the return-path feedback stall). 1000 pkt/sec
/// (~1.0 MB/s) stays comfortably under EXIT's echo ceiling (target 7000 / 5 s = 1400 pkt/sec),
/// so SURB supply keeps pace and the pool never depletes.
pub const PUMP_SEND_RATE_PPS: u64 = 1000;
/// Inter-batch delay derived from the send-rate cap: `PUMP_BATCH_PACKETS` packets per tick.
/// Rounded **up** (`div_ceil`) so the effective send rate never exceeds `PUMP_SEND_RATE_PPS`
/// after retuning — truncating division would silently push the rate above the cap.
pub const PUMP_BATCH_DELAY_MS: u64 =
    (PUMP_BATCH_PACKETS as u64 * 1000).div_ceil(PUMP_SEND_RATE_PPS);
/// Minimum acceptable receive throughput (KiB/s). At 1000 pkt/sec paced send the echo returns at
/// the same rate; this floor sits well below that so it only catches a genuine stall.
pub const PUMP_MIN_RECV_RATE_KIBS: f64 = 100.0;
/// Maximum acceptable packet loss percentage.
pub const PUMP_MAX_LOSS_PCT: f64 = 5.0;
/// How long the reader waits with no new bytes before concluding delivery has drained.
/// Must exceed the session frame timeout (3 s) so a legitimately delayed tail frame still
/// counts; kept short so the test does not block on the unflushable end-of-stream tail.
pub const PUMP_RECV_IDLE_TIMEOUT: Duration = Duration::from_secs(15);
/// Cluster node count (hoprd nodes, **excluding** the edge client). Sized per run by
/// `EDGLI_E2E_CLUSTER_SIZE` so each hop count gets a minimal cluster: an N-hop forward path
/// `E → r₁…r_N → X` spans `N+2` distinct nodes (no node may repeat, RFC-0014 §6.3), i.e. the
/// edge client + `N` relays + the exit — so it needs `N+1` cluster nodes (`max(N+1, 2)`; ≥2 so
/// target selection always has two distinct peers). Counting the edge client, that is 3 total for
/// 0/1-hop, 4 for 2-hop, 5 for 3-hop. Defaults to 3 when unset (covers the default 0/1-hop run).
pub fn cluster_size() -> usize {
    match std::env::var("EDGLI_E2E_CLUSTER_SIZE") {
        // Unset (or empty) → keep the documented 3-node default.
        Err(_) => 3,
        Ok(v) if v.trim().is_empty() => 3,
        // Set-but-invalid must fail loudly in a measurement harness rather than silently
        // provisioning a topology the operator did not request.
        Ok(v) => {
            let n: usize = v.trim().parse().unwrap_or_else(|e| {
                panic!("EDGLI_E2E_CLUSTER_SIZE={v:?} is not a valid usize: {e}")
            });
            assert!(
                n >= 2,
                "EDGLI_E2E_CLUSTER_SIZE={n} is below the minimum of 2 (target selection needs two \
                 distinct peers)"
            );
            n
        }
    }
}
const API_PORT_BASE: u16 = 13000;
const P2P_PORT_BASE: u16 = 19000;
/// Edgli's P2P port — one slot beyond the cluster nodes.
fn edge_p2p_port() -> u16 {
    P2P_PORT_BASE + cluster_size() as u16
}
const API_HOST: &str = "127.0.0.1";
const API_TOKEN: &str = "test-token-localcluster";

// Timeouts (generous to handle slow CI environments and image pulls).
const CLUSTER_START_TIMEOUT: Duration = Duration::from_secs(600); // 10 min
const READYZ_TIMEOUT: Duration = Duration::from_secs(120);
const PEER_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(120);
const INTRACLUSTER_CHANNEL_TIMEOUT: Duration = Duration::from_secs(120);
const EDGLI_PEER_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(120);
const EXIT_PEER_PROBE_TIMEOUT: Duration = Duration::from_secs(120);
/// How long to wait for the edge client's network graph to observe the whole cluster (all nodes
/// probed, channels indexed via SSE) before routing multi-hop sessions.
const GRAPH_POPULATE_TIMEOUT: Duration = Duration::from_secs(240);

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
    /// Whether to probe private/local peer addresses received in announcements.
    /// True only on a same-host cluster, where peers reach each other over
    /// RFC-1918 addresses.
    pub probe_local: bool,
    /// Channel-lifecycle strategy tick interval.
    pub strategy_tick: Duration,
    /// Channel-lifecycle selection policy.  Determines which peers qualify for
    /// an outgoing payment channel and how candidates are ranked.  Passed
    /// directly into [`ChannelLifecycleConfig::selector`] when the strategy
    /// reactor is started.
    pub selector: SelectorProfile,
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
    /// Path-planner / selector configuration passed to the HOPR protocol.
    /// Set on `cfg.protocol.path_planner` before constructing the Edgli instance.
    pub path_planner: PathPlannerConfig,
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
            probe_local: true,
            strategy_tick: Duration::from_secs(10),
            selector: SelectorProfile::LowLatency,
            channel_open_timeout: Duration::from_secs(120),
            exit_node: None,
            // 300 s outer budget accommodates the 20 MiB paced payload (~24 s pump plus
            // cluster/channel latency and the end-of-stream tail).
            pump_timeout: Duration::from_secs(300),
            // Use 0.0: Edgli just started and no probes have completed yet when the
            // 1-hop session is attempted.  With min_ack_rate=0.1 the path planner
            // finds no eligible relays (ack_rate=0 for all) and session initiation
            // times out repeatedly.  Routing via channel topology (0.0 floor) is
            // correct for a full-mesh local cluster where all channels are open.
            min_ack_rate: 0.0,
            path_planner: latency_path_planner_config(0.0),
        }
    }

    fn rotsee() -> Self {
        Self {
            connector_cfg: BlockchainConnectorConfig::default(),
            prefer_local_addresses: false,
            announce_local: false,
            probe_local: false,
            strategy_tick: Duration::from_secs(30),
            // Rotsee peers have ~150-200 ms RTT; latency_score caps at 0.3 for
            // that range, so even a perfect probe rate yields at most 0.30.
            // Setting 0.1 accepts any peer that has had at least one successful probe.
            selector: SelectorProfile::LowLatency,
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
            path_planner: latency_path_planner_config(0.0),
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
// Wire types for `hoprd-localcluster status` JSON  (hoprd/localcluster/src/summary.rs)
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum ClusterStateWire {
    NotRunning,
    Initializing,
    Starting,
    Running,
    ShuttingDown,
    Failed,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, serde::Deserialize)]
struct ClusterSummaryWire {
    state: ClusterStateWire,
    #[serde(default)]
    blokli_url: Option<String>,
    #[serde(default)]
    nodes: Vec<NodeSummaryWire>,
    #[serde(default)]
    extras: Vec<ExtraSummaryWire>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct NodeSummaryWire {
    address: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct ExtraSummaryWire {
    safe_address: String,
    module_address: String,
    keystore_path: String,
    password: String,
}

fn wire_into_summary(wire: ClusterSummaryWire) -> anyhow::Result<ClusterSummary> {
    let blokli_url = wire
        .blokli_url
        .ok_or_else(|| anyhow::anyhow!("blokli_url missing from running cluster status"))?;

    let nodes = wire
        .nodes
        .into_iter()
        .map(|n| {
            let address = n
                .address
                .ok_or_else(|| anyhow::anyhow!("node address is null in running cluster status"))?
                .parse::<Address>()
                .context("invalid node address")?;
            Ok(NodeInfo { address })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;

    anyhow::ensure!(!nodes.is_empty(), "no nodes in cluster status");

    let extras = wire
        .extras
        .into_iter()
        .map(|e| {
            let safe_address = e
                .safe_address
                .parse::<Address>()
                .context("invalid safe_address")?;
            let module_address = e
                .module_address
                .parse::<Address>()
                .context("invalid module_address")?;
            Ok(ExtraInfo {
                safe_address,
                module_address,
                keystore_path: PathBuf::from(e.keystore_path),
                password: e.password,
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;

    anyhow::ensure!(!extras.is_empty(), "no extra identities in cluster status");

    Ok(ClusterSummary {
        blokli_url,
        nodes,
        extras,
    })
}

fn parse_summary_json(json: &str) -> anyhow::Result<ClusterSummary> {
    let wire: ClusterSummaryWire =
        serde_json::from_str(json).context("failed to parse cluster status JSON")?;
    wire_into_summary(wire)
}

// ────────────────────────────────────────────────────────────────────────────
// Cluster RAII handle
// ────────────────────────────────────────────────────────────────────────────

pub struct ClusterHandle {
    /// `Some` when the test started the cluster; `None` in external mode.
    child: Option<tokio::process::Child>,
    pub summary: ClusterSummary,
    _tempdir: Option<tempfile::TempDir>,
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
    if let Ok(data_dir) = std::env::var("HOPRD_CLUSTER_DATA_DIR") {
        let lc_bin = std::env::var("HOPRD_LOCALCLUSTER_BIN").map_err(|_| {
            anyhow::anyhow!(
                "HOPRD_LOCALCLUSTER_BIN is not set (required even in external mode to run \
                 `hoprd-localcluster status --data-dir {data_dir}`)"
            )
        })?;
        let out = tokio::process::Command::new(&lc_bin)
            .args(["status", "--data-dir", &data_dir])
            .output()
            .await
            .with_context(|| format!("running `{lc_bin} status --data-dir {data_dir}`"))?;
        let json = String::from_utf8_lossy(&out.stdout);
        let wire: ClusterSummaryWire =
            serde_json::from_str(&json).context("failed to parse cluster status JSON")?;
        anyhow::ensure!(
            matches!(wire.state, ClusterStateWire::Running),
            "HOPRD_CLUSTER_DATA_DIR is set but cluster state is '{:?}' (not 'running').\n\
             Wait until `hoprd-localcluster status --data-dir {data_dir}` reports \
             \"state\": \"running\" before running this test.",
            wire.state
        );
        let summary = wire_into_summary(wire)?;
        tracing::info!(
            blokli_url = %summary.blokli_url,
            nodes = summary.nodes.len(),
            extras = summary.extras.len(),
            "external cluster summary loaded from data-dir {data_dir}"
        );
        return Ok(ClusterHandle {
            child: None,
            summary,
            _tempdir: None,
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
         Build binaries from hoprnet/hoprd:\n\
            cargo build --release -p hoprd-localcluster -p hoprd\n\
         \n\
         Export:\n\
            export HOPRD_LOCALCLUSTER_BIN=/path/to/hoprd-localcluster\n\
            export HOPRD_BIN=/path/to/hoprd\n\
            export HOPRD_CHAIN_IMAGE='<bloklid-anvil image tag>'\n\
            # export HOPRD_CONTAINER_RUNTIME=container  # Apple native runtime\n\
         \n\
         ── B. External mode (attach to already-running cluster) ─────────\n\
         \n\
         Start the cluster in another terminal:\n\
            hoprd-localcluster --size 3 --extra-identities 1 \\\n\
              --api-port-base 13000 --p2p-port-base 19000 \\\n\
              --api-token test-token-localcluster \\\n\
              --data-dir /tmp/edgli-cluster ...\n\
         \n\
         Once `hoprd-localcluster status --data-dir /tmp/edgli-cluster` reports\n\
         \"state\": \"running\", run the test:\n\
            export HOPRD_LOCALCLUSTER_BIN=/path/to/hoprd-localcluster\n\
            export HOPRD_CLUSTER_DATA_DIR=/tmp/edgli-cluster\n\
         \n\
         ── Run the test ─────────────────────────────────────────────────\n\
            RUST_LOG=info,edgli=debug \
                cargo test --test edgli_session_e2e --release -- --ignored --nocapture\n\
        "
        )
    })?;
    let hoprd_bin =
        std::env::var("HOPRD_BIN").map_err(|_| anyhow::anyhow!("HOPRD_BIN is not set"))?;
    let chain_image = std::env::var("HOPRD_CHAIN_IMAGE")
        .map_err(|_| anyhow::anyhow!("HOPRD_CHAIN_IMAGE is not set"))?;
    let container_runtime = std::env::var("HOPRD_CONTAINER_RUNTIME").ok();

    // When EDGLI_DATA_DIR is set, use a persistent data dir (not auto-removed) so the hoprd
    // node logs under <data_dir>/logs/hoprd_{id}.log survive the test for post-mortem analysis.
    // Otherwise use a tempdir that is cleaned up on drop.
    let (data_dir, tempdir) = match std::env::var("EDGLI_DATA_DIR") {
        Ok(dir) if !dir.trim().is_empty() => {
            let path = PathBuf::from(dir.trim());
            std::fs::create_dir_all(&path).context("failed to create EDGLI_DATA_DIR")?;
            // A non-empty dir carries stale cluster state (node DBs, keys, channels) from a
            // previous run, which perturbs the very throughput/loss measurement this harness
            // makes. Warn loudly so the operator clears it before a measurement run.
            let non_empty = std::fs::read_dir(&path)
                .map(|mut d| d.next().is_some())
                .unwrap_or(false);
            if non_empty {
                tracing::warn!(
                    data_dir = %path.display(),
                    "EDGLI_DATA_DIR is not empty — stale cluster state from a previous run may perturb \
                     measurements; clear it for a clean measurement run"
                );
            }
            tracing::warn!(data_dir = %path.display(), "using persistent EDGLI_DATA_DIR — not cleaned up");
            (path, None)
        }
        _ => {
            let td = tempfile::TempDir::with_prefix("edgli-it-")?;
            let path = td.path().to_path_buf();
            (path, Some(td))
        }
    };
    tracing::info!(data_dir = %data_dir.display(), "cluster data dir (hoprd node logs under <data_dir>/logs/)");

    let mut cmd = tokio::process::Command::new(&lc_bin);
    cmd.args([
        "--hoprd-bin",
        &hoprd_bin,
        "--size",
        &cluster_size().to_string(),
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
        .stderr(std::process::Stdio::inherit());

    let mut child = cmd.spawn()?;
    let stdout = child.stdout.take().expect("stdout must be captured");

    // Stream stdout to the test logger for observability; readiness is determined
    // by polling the `status` subcommand rather than scanning for a sentinel line.
    tokio::spawn(async move {
        let mut lines = tokio::io::BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            tracing::info!(target: "localcluster", "{}", line);
        }
    });

    let lc_bin_path = std::path::Path::new(&lc_bin);
    let summary = match wait_status_running(
        lc_bin_path,
        &data_dir,
        CLUSTER_START_TIMEOUT,
        &mut child,
    )
    .await
    {
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
        _tempdir: tempdir,
    })
}

/// Poll `hoprd-localcluster status --data-dir <dir>` every 3 s until the cluster
/// reports `state == "running"`.  Bails on `state == "failed"`, on premature
/// child exit, and on `timeout`.
async fn wait_status_running(
    lc_bin: &std::path::Path,
    data_dir: &std::path::Path,
    timeout: Duration,
    child: &mut tokio::process::Child,
) -> anyhow::Result<ClusterSummary> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            anyhow::bail!("hoprd-localcluster exited prematurely with status {status:?}");
        }

        let out = tokio::process::Command::new(lc_bin)
            .args(["status", "--data-dir", data_dir.to_str().unwrap()])
            .output()
            .await
            .context("failed to run `hoprd-localcluster status`")?;
        let json = String::from_utf8_lossy(&out.stdout);

        match serde_json::from_str::<ClusterSummaryWire>(&json) {
            Ok(wire) => match wire.state {
                ClusterStateWire::Running => return wire_into_summary(wire),
                ClusterStateWire::Failed => {
                    let error = wire.error.as_deref().unwrap_or("unknown error").to_owned();
                    anyhow::bail!("localcluster failed: {error}");
                }
                state => tracing::debug!("cluster status: {state:?}"),
            },
            Err(_) => tracing::debug!("cluster status: response not yet parseable"),
        }

        anyhow::ensure!(
            tokio::time::Instant::now() < deadline,
            "timeout ({timeout:?}) waiting for cluster to reach 'running' state"
        );
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
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
            (0..cluster_size()).map(|i| check_node(i, API_PORT_BASE + i as u16)),
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
        &format!("all {} cluster nodes passed /readyz", cluster_size()),
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
    let expected = cluster_size() - 1;
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
    let expected = cluster_size() - 1;
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
                "timeout ({timeout:?}) waiting for Edgli strategy to open {min_open} channel(s) to connected peers"
            )
        },
        || async {
            let peers = edgli.connected_peer_addresses().await?;
            let peer_set: std::collections::HashSet<Address> = peers.into_iter().collect();
            let channels: Vec<ChannelEntry> = edgli.my_outgoing_channels().await?;
            let open = channels
                .iter()
                .filter(|ch| ch.status == ChannelStatus::Open && peer_set.contains(&ch.destination))
                .count();
            if open >= min_open {
                tracing::info!(
                    open_channels = open,
                    "Edgli outgoing channels confirmed Open"
                );
                return Ok(true);
            }
            tracing::debug!(
                "Edgli: {open}/{min_open} outgoing channels to peers Open (waiting for strategy)"
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

/// Wait until the edge client's network graph is populated enough for multi-hop routing.
///
/// Multi-hop path construction is only as good as the graph it runs on: path-finding enumerates
/// simple paths over the observed channel graph and validates each non-final edge against on-chain
/// channels (RFC-0014 §6.3). A freshly-booted edge client indexes the cluster's channels via SSE
/// and probes its neighbours over successive rounds — until every cluster node is *in* the graph,
/// deeper paths (and their SURB return paths) can route through not-yet-observed relays and stall
/// at the tail. This polls `all_network_peers(0.0)` until every cluster node has been observed.
async fn await_graph_populated(
    edgli: &Edgli,
    min_peers: usize,
    timeout: Duration,
) -> anyhow::Result<()> {
    poll_edgli_until(
        timeout,
        Duration::from_secs(5),
        || format!("timeout ({timeout:?}) waiting for network graph to observe {min_peers} cluster peers"),
        || async {
            let peers = edgli
                .transport()
                .all_network_peers(0.0)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            if peers.len() >= min_peers {
                tracing::info!(observed = peers.len(), min_peers, "network graph populated");
                return Ok(true);
            }
            tracing::debug!(observed = peers.len(), min_peers, "network graph still populating");
            Ok(false)
        },
    )
    .await
}

/// Select session destinations that work for both 0-hop and 1-hop.
///
/// 0-hop target: the first open outgoing channel whose destination is also a
/// connected peer (Edgli has a direct channel to it, so a 0-hop session is
/// routable).  Pre-existing genesis channels to non-running accounts are
/// excluded by intersecting with the connected-peer set.
///
/// 1-hop target: any connected peer that is different from the 0-hop target
/// (the 0-hop target acts as the relay, with its intranetwork channels
/// carrying the forward path).
async fn select_session_targets(edgli: &Edgli) -> anyhow::Result<(Address, Address)> {
    let (raw_channels, peers) = tokio::try_join!(
        edgli.my_outgoing_channels(),
        edgli.connected_peer_addresses()
    )?;
    let peer_set: std::collections::HashSet<Address> = peers.iter().copied().collect();
    let channels: Vec<ChannelEntry> = raw_channels
        .into_iter()
        .filter(|c| c.status == ChannelStatus::Open && peer_set.contains(&c.destination))
        .collect();
    anyhow::ensure!(
        !channels.is_empty(),
        "no open outgoing channels to connected peers"
    );
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

/// Truthy env flag: `1` or `true` (case-insensitive), else false.
fn env_flag(var: &str) -> bool {
    std::env::var(var)
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Hop counts to exercise this run, selected by `EDGLI_E2E_HOPS` (comma-separated, e.g. `"2"` or
/// `"0,1,2,3"`). Defaults to `[0, 1]` (the original behaviour). Run one hop count per invocation
/// (e.g. `EDGLI_E2E_HOPS=3`) to get a **fresh cluster per hop** — each binary run provisions and
/// tears down its own cluster.
pub fn hop_counts() -> Vec<usize> {
    match std::env::var("EDGLI_E2E_HOPS") {
        Ok(v) if !v.trim().is_empty() => {
            // Reject unparsable entries loudly: silently dropping them could yield an empty
            // vector and a vacuous pass that transfers no payload.
            let hops: Vec<usize> = v
                .split(',')
                .map(|s| {
                    s.trim().parse::<usize>().unwrap_or_else(|e| {
                        panic!(
                            "EDGLI_E2E_HOPS entry {:?} is not a valid usize: {e}",
                            s.trim()
                        )
                    })
                })
                .collect();
            assert!(
                !hops.is_empty(),
                "EDGLI_E2E_HOPS={v:?} parsed to no hop counts"
            );
            hops
        }
        _ => vec![0, 1],
    }
}

/// Seconds to let the cluster settle after readiness so nodes complete several probing rounds
/// before path-finding runs (`EDGLI_E2E_PROBE_SETTLE_SECS`, default 60 s). At hoprd's local probe
/// cadence this comfortably covers ≥3 rounds, warming edge scores for reliable multi-hop paths
/// (RFC-0010 §6.2, RFC-0014 §6.3).
fn probe_settle_duration() -> Duration {
    let secs = match std::env::var("EDGLI_E2E_PROBE_SETTLE_SECS") {
        Err(_) => 60,
        Ok(v) if v.trim().is_empty() => 60,
        Ok(v) => v.trim().parse::<u64>().unwrap_or_else(|e| {
            panic!("EDGLI_E2E_PROBE_SETTLE_SECS={v:?} is not a valid u64: {e}")
        }),
    };
    Duration::from_secs(secs)
}

/// The flow-control config the harness applies to the loopback session, **selected by env for test
/// parameterization** across the matrix below. This is *not* a production mechanism — production sets
/// [`HoprSessionClientConfig::flow_control`] directly; the test just picks which config to build.
///
/// * `HOPR_SESSION_FLOW_CONTROL` unset → `None`: flow control off (the original hand-paced harness).
/// * set → `Some(FlowControlConfig::default())`: the clean profile (production path / ship gate).
/// * set + `HOPR_SESSION_FLOW_CONTROL_ROBUST` → `Some(FlowControlConfig::robust())`: the opt-in
///   tail-tolerance bundle (persist probe + larger retransmission budget).
pub fn flow_control_config() -> Option<FlowControlConfig> {
    if !env_flag("HOPR_SESSION_FLOW_CONTROL") {
        return None;
    }
    Some(if env_flag("HOPR_SESSION_FLOW_CONTROL_ROBUST") {
        FlowControlConfig::robust()
    } else {
        FlowControlConfig::default()
    })
}

/// Whether flow control is enabled for this run (the session then runs **reliable** so the honest
/// ack clock exists, and the pump sends **unpaced** — the window replaces manual pacing).
pub fn flow_control_enabled() -> bool {
    flow_control_config().is_some()
}

/// Human-readable label of the active flow-control profile, for self-documenting run logs:
/// `paced` (off) / `fc:clean` (default profile) / `fc:robust` (tail-tolerance bundle).
pub fn flow_control_profile() -> &'static str {
    match flow_control_config() {
        None => "paced",
        Some(cfg) if cfg == FlowControlConfig::robust() => "fc:robust",
        Some(_) => "fc:clean",
    }
}

/// Loopback session config with symmetric `hops`-hop forward/return paths and SURBs maxed out (so
/// the pump isn't SURB-starved). Reliable (`RetransmissionAck`) when flow control is enabled so the
/// window has an honest delivery clock; Segmentation-only otherwise.
///
/// `HOPR_SESSION_MAX_SURBS_PER_SEC`, when set, overrides the SURB balancer's `max_surbs_per_sec`
/// (raising the return-path drain ceiling) so the flow-control window's scaling can be swept without
/// code changes between points.
fn loopback_session_config(hops: usize) -> anyhow::Result<HoprSessionClientConfig> {
    let capabilities = if flow_control_enabled() {
        SessionCapability::Segmentation | SessionCapability::RetransmissionAck
    } else {
        SessionCapability::Segmentation.into()
    };
    let mut cfg = HoprSessionClientConfig {
        forward_path: HopRouting::try_from(hops)?,
        return_path: HopRouting::try_from(hops)?,
        capabilities,
        always_max_out_surbs: true,
        // Client's dial: apply the selected flow-control profile as a real config field (no env is
        // read node-side). `None` = paced; `Some(clean)` / `Some(robust)` per the profile.
        flow_control: flow_control_config(),
        ..Default::default()
    };
    if let Ok(v) = std::env::var("HOPR_SESSION_MAX_SURBS_PER_SEC")
        && !v.trim().is_empty()
    {
        let max_surbs_per_sec = v.trim().parse::<u64>().unwrap_or_else(|e| {
            panic!("HOPR_SESSION_MAX_SURBS_PER_SEC={v:?} is not a valid u64: {e}")
        });
        cfg.surb_management = Some(SurbBalancerConfig {
            max_surbs_per_sec,
            ..cfg.surb_management.unwrap_or_default()
        });
    }
    Ok(cfg)
}

// ────────────────────────────────────────────────────────────────────────────
// Edgli config builder
// ────────────────────────────────────────────────────────────────────────────

pub fn build_edgli_config(extra: &ExtraInfo, tuning: &EdgliTuning) -> HoprLibConfig {
    use edgli::hopr_lib::config::{HoprProtocolConfig, MixerConfig, TransportConfig};
    HoprLibConfig {
        host: HostConfig {
            address: HostType::IPv4("0.0.0.0".to_string()),
            port: edge_p2p_port(),
        },
        publish: true,
        protocol: HoprProtocolConfig {
            transport: TransportConfig {
                announce_local_addresses: tuning.announce_local,
                prefer_local_addresses: tuning.prefer_local_addresses,
            },
            mixer: MixerConfig {
                min_delay: std::time::Duration::ZERO,
                delay_range: std::time::Duration::from_millis(1),
                ..Default::default()
            },
            path_planner: tuning.path_planner,
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

/// Throughput of `bytes` transferred over `elapsed`, expressed in KiB/s (1 KiB = 1024 B).
fn throughput_kibs(bytes: usize, elapsed: Duration) -> f64 {
    bytes as f64 / elapsed.as_secs_f64().max(f64::EPSILON) / 1024.0
}

/// Sends `payload` as fast as backpressure allows over `session`, reads the echo back,
/// then reports and asserts send/receive throughput and packet-loss metrics.
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
    let n = payload.len();
    let overall_start = std::time::Instant::now();
    tracing::info!(
        "{label}: starting pump [profile={}, {} B]",
        flow_control_profile(),
        n
    );

    let writer_offset = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let writer_offset_clone = writer_offset.clone();
    // Resolve the pacing mode once, up front: `flow_control_enabled()` reads the process
    // environment (taking the global env lock and allocating a config), so calling it per
    // batch inside the timed loop would perturb the very `send_kbs` we are measuring.
    let paced = !flow_control_enabled();
    let mut writer = tokio::spawn(async move {
        let write_start = tokio::time::Instant::now();
        let mut offset = 0usize;
        while offset < payload_bytes.len() {
            let end = (offset + PUMP_BATCH_BYTES).min(payload_bytes.len());
            w.write_all(&payload_bytes[offset..end]).await?;
            w.flush().await?;
            offset = end;
            writer_offset_clone.store(offset, std::sync::atomic::Ordering::Relaxed);
            // Pace the send under the Exit's echo/ack ceiling. HOPR Segmentation-only sessions
            // have no end-to-end flow control, and a full-speed burst overruns the return path
            // (Exit ack-amplification + mixer saturation) — see the investigation writeup. We
            // still send the entire payload, just metered to ~PUMP_SEND_RATE_PPS packets/sec.
            //
            // When client-side flow control is enabled the adaptive send window replaces this
            // manual pacing, so we send unpaced and let the window self-size against the drain rate.
            if offset < payload_bytes.len() && paced {
                tokio::time::sleep(Duration::from_millis(PUMP_BATCH_DELAY_MS)).await;
            }
        }
        Ok::<_, std::io::Error>(write_start.elapsed())
    });

    let mut received = vec![0u8; n];
    let mut cursor = 0usize;
    let deadline = tokio::time::Instant::now() + timeout;
    // Log progress roughly every 5% of total payload.
    let progress_interval = (n / 20).max(1);
    let mut next_progress = progress_interval;

    // Monitor task: log writer send progress every 15 s. (The reader logs its own receive
    // progress inline from the loop below, keyed off `next_progress`.)
    let monitor_offset = writer_offset.clone();
    let monitor_label = label.to_string();
    let monitor_n = n;
    let monitor_deadline = deadline;
    let monitor = tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(15));
        interval.tick().await; // skip the immediate first tick
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    let wo = monitor_offset.load(std::sync::atomic::Ordering::Relaxed);
                    tracing::info!(
                        "[monitor] {monitor_label}: writer_sent={wo}/{monitor_n} B ({pct}%)",
                        pct = wo * 100 / monitor_n.max(1)
                    );
                }
                _ = tokio::time::sleep_until(monitor_deadline) => break,
            }
        }
    });

    // Reader: accumulate bytes until all received, the overall deadline expires, or the
    // stream goes idle. HOPR sessions have no half-close, so the sender never signals
    // end-of-stream: after the last echo arrives, a small tail may remain unflushed on the
    // sender forever. Rather than block until the 300 s deadline (which would make the
    // throughput metric meaningless), we stop after `PUMP_RECV_IDLE_TIMEOUT` with no new
    // bytes and report the residual as loss. Throughput is measured against the instant of
    // the *last received byte*, not the deadline, so it reflects real delivery speed.
    let mut last_recv = overall_start;
    'recv: loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            break 'recv;
        }
        let wait = PUMP_RECV_IDLE_TIMEOUT.min(deadline - now);
        match tokio::time::timeout(wait, r.read(&mut received[cursor..])).await {
            Ok(Ok(0)) => break 'recv,
            Ok(Ok(nbytes)) => {
                cursor += nbytes;
                last_recv = std::time::Instant::now();
                if cursor >= next_progress || cursor >= n {
                    let pct = cursor * 100 / n;
                    let elapsed = overall_start.elapsed();
                    let kibs = throughput_kibs(cursor, elapsed);
                    let wo = writer_offset.load(std::sync::atomic::Ordering::Relaxed);
                    tracing::info!(
                        "{label}: recv progress {cursor}/{n} B ({pct}%) in {elapsed:.2?} ({kibs:.1} KiB/s) [writer_sent={wo}]"
                    );
                    next_progress = cursor + progress_interval;
                }
                if cursor >= n {
                    break 'recv;
                }
            }
            Ok(Err(e)) => {
                monitor.abort();
                writer.abort();
                let _ = writer.await;
                return Err(anyhow::anyhow!("{label}: read error: {e}"));
            }
            Err(_) => {
                // No new bytes for PUMP_RECV_IDLE_TIMEOUT — delivery has drained. The
                // remaining bytes (if any) are an unflushable tail; report them as loss.
                tracing::info!(
                    "{label}: recv idle for {PUMP_RECV_IDLE_TIMEOUT:?} at {cursor}/{n} B — ending read"
                );
                break 'recv;
            }
        }
    }
    monitor.abort();
    // Throughput reflects time-to-last-byte, not the idle/deadline wait after delivery ended.
    let recv_elapsed = last_recv
        .saturating_duration_since(overall_start)
        .max(Duration::from_millis(1));
    let bytes_received = cursor;

    // Wait for the writer to finish. The reader loop above already ran until `deadline`
    // (pump start + `timeout`), so bound this join to the *remaining* budget (with a small
    // floor to reap a writer that is essentially done) — a second full `timeout` here would let
    // `pump_and_verify` run for up to twice its stated budget.
    let writer_budget = deadline
        .saturating_duration_since(tokio::time::Instant::now())
        .max(Duration::from_secs(5));
    let write_elapsed = match tokio::time::timeout(writer_budget, &mut writer).await {
        Ok(Ok(Ok(dur))) => dur,
        Ok(Ok(Err(e))) => return Err(anyhow::anyhow!("{label}: write error: {e}")),
        Ok(Err(e)) => return Err(anyhow::anyhow!("{label}: writer panicked: {e}")),
        Err(_) => {
            // Dropping a `JoinHandle` detaches the task rather than cancelling it. Abort the
            // writer explicitly (matching the read-error path above) so it stops competing for
            // the executor and return path during the next measurement, and report progress so
            // the stall is diagnosable.
            let sent = writer_offset.load(std::sync::atomic::Ordering::Relaxed);
            writer.abort();
            let _ = writer.await;
            return Err(anyhow::anyhow!(
                "{label}: writer timeout (sent {sent}/{n} B, received {bytes_received}/{n} B)"
            ));
        }
    };

    // Metrics
    let send_kibs = throughput_kibs(n, write_elapsed);
    let recv_kibs = throughput_kibs(bytes_received, recv_elapsed);
    let loss_pct = (n - bytes_received) as f64 / n as f64 * 100.0;
    tracing::info!(
        "{label}: send {send_kibs:.1} KiB/s ({:.2?}) | recv {recv_kibs:.1} KiB/s ({:.2?}) | loss {loss_pct:.2}% ({bytes_received}/{n} B)",
        write_elapsed,
        recv_elapsed,
    );

    // Verify the integrity of whatever arrived — not only the full-payload case. The echo of a
    // byte range must equal the same range of the payload; checking only `bytes_received == n`
    // would let a corrupted-but-lossy run (within PUMP_MAX_LOSS_PCT) pass the loss assertion
    // below without any integrity check.
    if bytes_received == n {
        // Only hash the payload here — on a lossy run we take the prefix-compare branch
        // below and never need the full-payload digest.
        anyhow::ensure!(
            sha256_digest(&received) == sha256_digest(payload),
            "{label}: SHA-256 mismatch — {n} bytes corrupted in transit"
        );
        tracing::info!("{label}: SHA-256 OK");
    } else {
        anyhow::ensure!(
            received[..bytes_received] == payload[..bytes_received],
            "{label}: content mismatch in the {bytes_received} received bytes — corruption in transit"
        );
        tracing::info!("{label}: received-prefix integrity OK ({bytes_received}/{n} B)");
    }

    // Assertions.
    anyhow::ensure!(
        loss_pct <= PUMP_MAX_LOSS_PCT,
        "{label}: packet loss {loss_pct:.1}% > {PUMP_MAX_LOSS_PCT}% max (received {bytes_received}/{n} B)"
    );
    anyhow::ensure!(
        recv_kibs >= PUMP_MIN_RECV_RATE_KIBS,
        "{label}: recv throughput {recv_kibs:.1} KiB/s < {PUMP_MIN_RECV_RATE_KIBS} KiB/s min"
    );

    Ok(())
}

/// Write `payload` into `session` in a single un-paced `write_all`, read back
/// the echo, verify integrity, and report wall-clock throughput.
///
/// ## Why this matters
///
/// Production callers typically copy data into a session via
/// `transfer_session` / `copy_duplex`.  The underlying `poll_copy` loop reads
/// from the application's source and calls `poll_write` on the HOPR session in
/// a tight loop.  Because `CrossfireSink` (the outgoing packet channel) has a
/// capacity of **200,000 slots**, `AsyncWriteSink::poll_write` almost always
/// returns `Poll::Ready` without ever issuing a `Poll::Pending`, so the loop
/// never yields a tokio worker thread back to the executor.
///
/// Consequences:
///
/// 1. **Executor starvation**: one tokio thread is monopolised for the entire
///    write.  On machines with few workers (e.g. 2 threads on CI), this leaves
///    no thread for the SURB balancer or ack-processing tasks.
/// 2. **SURB drought**: the SURB balancer can't run → SURB replenishment stalls
///    → the echo return path blocks waiting for SURBs.
/// 3. **Throughput collapse**: the echo arrives much more slowly than the paced
///    baseline, reproducing the production "10× slower" phenomenon.
///
/// Use this function together with the `--features prof --profile tracer`
/// build (see `tests/edgli_profiling.rs`) and `tokio-console` to observe:
/// - A single long-poll of the writer task (the entire `write_all`).
/// - High idle time on the SURB balancer and ack tasks.
///
/// ## Note
///
/// Unlike `pump_and_verify`, this function does **not** assert SHA-256
/// integrity on read timeout — it logs the throughput and returns `Ok` even if
/// the read times out, so the comparison test can complete both variants and
/// report both numbers.
pub async fn pump_continuous(
    session: HoprSession,
    payload: &[u8],
    label: &str,
    timeout: Duration,
) -> anyhow::Result<()> {
    let (mut r, mut w) = tokio::io::split(session);
    let payload_bytes = payload.to_vec();
    let expected = sha256_digest(payload);
    let n = payload.len();
    let start = std::time::Instant::now();

    // Single write_all — no inter-batch sleep, no explicit yield_now.
    // The write task submits all ~1030 packets to the channel without ever
    // returning Poll::Pending to the executor.  This is the production
    // anti-pattern we want tokio-console to make visible.
    let writer = tokio::spawn(async move {
        w.write_all(&payload_bytes).await?;
        w.flush().await?;
        Ok::<_, std::io::Error>(())
    });

    let mut received = vec![0u8; n];
    let read_result = tokio::time::timeout(timeout, r.read_exact(&mut received)).await;

    let elapsed = start.elapsed();
    let throughput_kibs = throughput_kibs(n, elapsed);

    match read_result {
        Ok(Ok(_)) => {
            tracing::info!(
                "{label}: ✓ {n} B in {elapsed:.2?} ({throughput_kibs:.0} KiB/s) — continuous"
            );
            writer
                .await
                .map_err(|e| anyhow::anyhow!("{label}: writer panicked: {e}"))?
                .map_err(|e| anyhow::anyhow!("{label}: write error: {e}"))?;
            anyhow::ensure!(
                sha256_digest(&received) == expected,
                "{label}: SHA-256 mismatch — {n} bytes corrupted in transit"
            );
        }
        Ok(Err(e)) => {
            writer.abort();
            let _ = writer.await;
            anyhow::bail!("{label}: read error: {e}");
        }
        Err(_timeout) => {
            writer.abort();
            let _ = writer.await;
            // Not a hard error — log the stall so the comparison test can report both
            // variants and still complete. `read_exact` reports no partial-byte count on
            // timeout, so we deliberately do NOT print a received-throughput figure here — it
            // would be fabricated from the full payload size that never fully arrived.
            tracing::warn!(
                "{label}: read timeout ({timeout:?}) after {elapsed:.2?} — the {n} B payload did \
                 not fully arrive. This likely indicates SURB starvation from executor yielding \
                 issues: the write_all held a tokio worker thread without yielding, starving the \
                 SURB balancer. See tests/edgli_profiling.rs."
            );
        }
    }

    Ok(())
}

/// Write `payload` into `session` in MTU-sized chunks with a cooperative
/// `yield_now()` between each chunk — no inter-batch sleep.
///
/// This models what `poll_copy` should do once it calls
/// `tokio::task::coop::poll_proceed(cx)` at the top of each write iteration.
/// The yield hands the tokio worker thread back to the executor between every
/// batch so the SURB balancer and ack tasks can schedule without starvation,
/// while still driving the data as fast as the network allows.
///
/// Expected result: frame-discard count ≈ 0 (same as `pump_and_verify`),
/// throughput higher than paced (no 100 ms inter-batch sleep).
pub async fn pump_yielding(
    session: HoprSession,
    payload: &[u8],
    label: &str,
    timeout: Duration,
) -> anyhow::Result<()> {
    let (mut r, mut w) = tokio::io::split(session);
    let payload_bytes = payload.to_vec();
    let expected = sha256_digest(payload);
    let n = payload.len();
    let start = std::time::Instant::now();

    let writer = tokio::spawn(async move {
        let mut offset = 0;
        while offset < payload_bytes.len() {
            let end = (offset + PUMP_BATCH_BYTES).min(payload_bytes.len());
            w.write_all(&payload_bytes[offset..end]).await?;
            offset = end;
            // Cooperative yield — no sleep, immediate re-schedule.
            tokio::task::yield_now().await;
        }
        w.flush().await?;
        Ok::<_, std::io::Error>(())
    });

    let mut received = vec![0u8; n];
    let read_result = tokio::time::timeout(timeout, r.read_exact(&mut received)).await;

    let elapsed = start.elapsed();
    let throughput_kibs = throughput_kibs(n, elapsed);

    match read_result {
        Ok(Ok(_)) => {
            tracing::info!(
                "{label}: ✓ {n} B in {elapsed:.2?} ({throughput_kibs:.0} KiB/s) — yielding"
            );
            writer
                .await
                .map_err(|e| anyhow::anyhow!("{label}: writer panicked: {e}"))?
                .map_err(|e| anyhow::anyhow!("{label}: write error: {e}"))?;
            anyhow::ensure!(
                sha256_digest(&received) == expected,
                "{label}: SHA-256 mismatch — {n} bytes corrupted in transit"
            );
        }
        Ok(Err(e)) => {
            writer.abort();
            let _ = writer.await;
            anyhow::bail!("{label}: read error: {e}");
        }
        Err(_timeout) => {
            writer.abort();
            let _ = writer.await;
            tracing::warn!(
                "{label}: read timeout ({timeout:?}) after {elapsed:.2?} \
                 ({throughput_kibs:.0} KiB/s before stall)"
            );
        }
    }

    Ok(())
}

/// Throughput comparison: run 0-hop and 1-hop sessions twice — once with
/// pacing (`pump_and_verify`) and once without (`pump_continuous`) — and log
/// throughput for both.
///
/// This is the primary entry point for the profiling tests in
/// `tests/edgli_profiling.rs`.  Run with `--features prof --profile tracer`
/// and attach `tokio-console` to observe executor starvation in the continuous
/// variant.
pub async fn run_throughput_comparison_test(net: Network) -> anyhow::Result<()> {
    // ── 1. Provision ─────────────────────────────────────────────────────────
    let (summary, _guard, tuning) = provision(net).await?;

    if matches!(net, Network::Local) {
        await_nodes_ready().await?;
        await_cluster_peers_discovered().await?;
        await_intracluster_channels_open().await?;
    }

    // ── 2. Boot Edgli ────────────────────────────────────────────────────────
    let extra = &summary.extras[0];
    let hopr_keys: HoprKeys = IdentityRetrievalModes::FromFile {
        password: &extra.password,
        id_path: extra.keystore_path.to_str().unwrap(),
    }
    .try_into()?;

    let edgli = Edgli::new(
        build_edgli_config(extra, &tuning),
        hopr_keys,
        BlokliEndpoint::from_optional_url(Some(&summary.blokli_url))?,
        Some(tuning.connector_cfg),
        tuning.probe_local,
        |s: EdgliInitState| tracing::info!(?s, "edgli init"),
    )
    .await?;

    await_edgli_peers_connected(&edgli, 2).await?;

    // ── 3. Strategy ──────────────────────────────────────────────────────────
    let sizing = IncentiveConfiguration {
        min_open_channels: 1,
        target_open_channels: cluster_size(),
        ..Default::default()
    };
    let mut strat_cfg = default_strategy_cfg(&edgli, &sizing).await?;
    for kind in &mut strat_cfg.strategies {
        let EdgeStrategyKind::ChannelLifecycle(lc) = kind;
        lc.selector = tuning.selector.clone();
        lc.tick_interval = tuning.strategy_tick;
    }
    let _reactor_handle = edgli.run_reactor_from_cfg(strat_cfg)?;
    await_edgli_channels_open(&edgli, 1, tuning.channel_open_timeout).await?;

    let (dest_0h, dest_1h) = if let Some(exit) = tuning.exit_node {
        (exit, exit)
    } else {
        select_session_targets(&edgli).await?
    };

    // Wait for dest_1h to appear in the probe graph before attempting 1-hop
    // sessions — same reason as in run_session_throughput_test (§6.3).
    tracing::info!(%dest_1h, "waiting for 1-hop destination to be probed");
    await_edgli_exit_peer_ready(&edgli, dest_1h).await?;

    let mut payload = vec![0u8; PAYLOAD_SIZE];
    rand::rng().fill(&mut payload[..]);

    let surb_cfg = Some(SurbBalancerConfig {
        target_surb_buffer_size: 600,
        max_surbs_per_sec: 300,
        ..SurbBalancerConfig::default()
    });

    // ── 4. Paced baseline (pump_and_verify) ──────────────────────────────────
    tracing::info!("=== PACED BASELINE: opening 0-hop session ===");
    let (session_0h_paced, _) = edgli
        .connect_to(
            dest_0h,
            loopback_target(),
            HoprSessionClientConfig {
                forward_path: HopRouting::try_from(0_usize)?,
                return_path: HopRouting::try_from(0_usize)?,
                capabilities: (SessionCapability::Segmentation | SessionCapability::NoRateControl),
                surb_management: surb_cfg,
                ..Default::default()
            },
        )
        .await?;
    let t0 = std::time::Instant::now();
    pump_and_verify(
        session_0h_paced,
        &payload,
        "paced 0-hop",
        tuning.pump_timeout,
    )
    .await?;
    tracing::info!(
        "paced 0-hop: {:.0} KiB/s",
        throughput_kibs(PAYLOAD_SIZE, t0.elapsed())
    );

    // ── 5. Continuous variant (pump_continuous) ───────────────────────────────
    tracing::info!("=== CONTINUOUS (NO PACING): opening 0-hop session ===");
    let (session_0h_cont, _) = edgli
        .connect_to(
            dest_0h,
            loopback_target(),
            HoprSessionClientConfig {
                forward_path: HopRouting::try_from(0_usize)?,
                return_path: HopRouting::try_from(0_usize)?,
                capabilities: (SessionCapability::Segmentation | SessionCapability::NoRateControl),
                surb_management: surb_cfg,
                ..Default::default()
            },
        )
        .await?;
    pump_continuous(
        session_0h_cont,
        &payload,
        "continuous 0-hop",
        tuning.pump_timeout,
    )
    .await?;

    // ── 5b. 0-hop yielding variant (cooperative yield, no sleep) ─────────────
    tracing::info!("=== YIELDING (COOPERATIVE): opening 0-hop session ===");
    let (session_0h_yield, _) = edgli
        .connect_to(
            dest_0h,
            loopback_target(),
            HoprSessionClientConfig {
                forward_path: HopRouting::try_from(0_usize)?,
                return_path: HopRouting::try_from(0_usize)?,
                capabilities: (SessionCapability::Segmentation | SessionCapability::NoRateControl),
                surb_management: surb_cfg,
                ..Default::default()
            },
        )
        .await?;
    pump_yielding(
        session_0h_yield,
        &payload,
        "yielding 0-hop",
        tuning.pump_timeout,
    )
    .await?;

    // ── 6. 1-hop paced baseline ───────────────────────────────────────────────
    tracing::info!("=== PACED BASELINE: opening 1-hop session ===");
    let (session_1h_paced, _) = edgli
        .connect_to(
            dest_1h,
            loopback_target(),
            HoprSessionClientConfig {
                forward_path: HopRouting::try_from(1_usize)?,
                return_path: HopRouting::try_from(1_usize)?,
                capabilities: (SessionCapability::Segmentation | SessionCapability::NoRateControl),
                surb_management: surb_cfg,
                ..Default::default()
            },
        )
        .await?;
    let t1 = std::time::Instant::now();
    pump_and_verify(
        session_1h_paced,
        &payload,
        "paced 1-hop",
        tuning.pump_timeout,
    )
    .await?;
    tracing::info!(
        "paced 1-hop: {:.0} KiB/s",
        throughput_kibs(PAYLOAD_SIZE, t1.elapsed())
    );

    // ── 7. 1-hop continuous variant ───────────────────────────────────────────
    tracing::info!("=== CONTINUOUS (NO PACING): opening 1-hop session ===");
    let (session_1h_cont, _) = edgli
        .connect_to(
            dest_1h,
            loopback_target(),
            HoprSessionClientConfig {
                forward_path: HopRouting::try_from(1_usize)?,
                return_path: HopRouting::try_from(1_usize)?,
                capabilities: (SessionCapability::Segmentation | SessionCapability::NoRateControl),
                surb_management: surb_cfg,
                ..Default::default()
            },
        )
        .await?;
    pump_continuous(
        session_1h_cont,
        &payload,
        "continuous 1-hop",
        tuning.pump_timeout,
    )
    .await?;

    // ── 7b. 1-hop yielding variant ────────────────────────────────────────────
    tracing::info!("=== YIELDING (COOPERATIVE): opening 1-hop session ===");
    let (session_1h_yield, _) = edgli
        .connect_to(
            dest_1h,
            loopback_target(),
            HoprSessionClientConfig {
                forward_path: HopRouting::try_from(1_usize)?,
                return_path: HopRouting::try_from(1_usize)?,
                capabilities: (SessionCapability::Segmentation | SessionCapability::NoRateControl),
                surb_management: surb_cfg,
                ..Default::default()
            },
        )
        .await?;
    pump_yielding(
        session_1h_yield,
        &payload,
        "yielding 1-hop",
        tuning.pump_timeout,
    )
    .await?;

    drop(edgli);
    Ok(())
}

// ────────────────────────────────────────────────────────────────────────────
// Top-level harness
// ────────────────────────────────────────────────────────────────────────────

/// Initialise a layered tracing subscriber.
///
/// Always installs an `EnvFilter`-gated `fmt` layer for console output.
/// When `CHROME_TRACE_OUTPUT` is set, also installs a
/// [`tracing_chrome::ChromeLayer`] that writes a Perfetto/Chrome-DevTools
/// trace JSON to that path (open in `chrome://tracing` or
/// <https://ui.perfetto.dev>).
/// When `FLAME_OUTPUT` is set, also installs a [`tracing_flame::FlameLayer`]
/// that writes folded stacks to that path (usable with `inferno-flamegraph`).
///
/// Returns guards that must be held for the duration of the test; dropping
/// them flushes the trace files.
pub fn init_tracing() -> (
    Option<tracing_chrome::FlushGuard>,
    Option<tracing_flame::FlushGuard<std::io::BufWriter<std::fs::File>>>,
) {
    // Strip high-volume tokio/runtime targets from the fmt filter: formatting
    // and writing thousands of tokio poll events to stdout per second consumes
    // CPU on every tokio worker thread, starving the session drive task.
    // Those targets are still captured by the Chrome layer (async writer).
    let stdout_filter = {
        let rust_log = std::env::var("RUST_LOG").unwrap_or_default();
        let stripped: String = rust_log
            .split(',')
            .filter(|d| {
                let target = d.split('=').next().unwrap_or("").trim();
                target != "tokio" && target != "runtime"
            })
            .collect::<Vec<_>>()
            .join(",");
        EnvFilter::new(if stripped.is_empty() {
            "info".to_string()
        } else {
            stripped
        })
    };

    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_ansi(true)
        .with_filter(stdout_filter);

    let chrome_path = std::env::var("CHROME_TRACE_OUTPUT").ok();
    let flame_path = std::env::var("FLAME_OUTPUT").ok();

    // Chrome layer keeps the full RUST_LOG filter (including tokio=trace).
    // The writer runs on a background thread and does not block tokio.
    let (chrome_layer, chrome_guard) = if let Some(ref path) = chrome_path {
        let (layer, guard) = tracing_chrome::ChromeLayerBuilder::new()
            .file(path)
            .include_args(true)
            .build();
        // Fall back to a trace-capturing filter if RUST_LOG is unset/empty — an empty
        // `from_default_env` would enable nothing and the Chrome trace would be empty.
        let chrome_filter = EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| EnvFilter::new("info,tokio=trace,runtime=trace"));
        (Some(layer.with_filter(chrome_filter)), Some(guard))
    } else {
        (None, None)
    };

    let (flame_layer, flame_guard) = if let Some(ref path) = flame_path {
        let (layer, guard) =
            tracing_flame::FlameLayer::with_file(path).expect("cannot open FLAME_OUTPUT file");
        (Some(layer), Some(guard))
    } else {
        (None, None)
    };

    // `try_init` (not `init`) so this coexists with a subscriber already installed by the
    // `#[test_log::test]` attribute: if one is set, we keep it rather than panicking. But when a
    // subscriber is already installed our Chrome/flame layers are silently dropped — the profiling
    // files would be created yet never written. Surface that and return no guards, so the caller
    // cannot mistake a dead layer for a live one (an empty FLAME_OUTPUT read as real profiling data).
    let profiling_layers = chrome_guard.is_some() || flame_guard.is_some();
    if tracing_subscriber::registry()
        .with(fmt_layer)
        .with(chrome_layer)
        .with(flame_layer)
        .try_init()
        .is_err()
    {
        if profiling_layers {
            eprintln!(
                "init_tracing: a subscriber is already installed; Chrome/flame profiling layers \
                 were NOT activated — ignoring CHROME_TRACE/FLAME_OUTPUT for this run"
            );
        }
        return (None, None);
    }

    (chrome_guard, flame_guard)
}

/// Run the session throughput pump (0-hop + 1-hop) against the given network.
///
/// Sends a `PAYLOAD_SIZE` payload (currently 20 MiB) each way, measuring throughput and loss.
/// Orchestrates: provision → Edgli boot → strategy reactor → channel wait →
/// target selection → 0-hop pump → 1-hop pump → final assertion.
pub async fn run_session_throughput_test(net: Network) -> anyhow::Result<()> {
    let (_chrome_guard, _flame_guard) = init_tracing();

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
        BlokliEndpoint::from_optional_url(Some(&summary.blokli_url))?,
        Some(tuning.connector_cfg),
        tuning.probe_local,
        |s: EdgliInitState| tracing::info!(?s, "edgli init"),
    )
    .await?;

    // ── 4. Verify Edgli P2P connectivity ─────────────────────────────────────
    // Wait for at least 2 peers so select_session_targets can pick distinct
    // destinations for the 0-hop and 1-hop sessions.
    tracing::info!("waiting for Edgli to connect to at least 2 peers");
    await_edgli_peers_connected(&edgli, 2).await?;

    // ── 5. Start the channel-lifecycle strategy reactor ──────────────────────
    // Channel capacity is sized by the strategy's built-in winning-ticket buffer
    // (see edgli::strategy::compute_funding_config): only winning tickets drain
    // channel balance, so a few face values per channel cover both test and
    // background probe traffic.
    //
    // The hardcoded extra identity (EXTRA_KEYS[0]) may have pre-existing Open
    // channels to genesis accounts that are not running hoprd nodes.  The
    // channel-lifecycle strategy counts ALL open channels, so those genesis
    // channels reduce the deficit.  We count them upfront and add them to the
    // target so the strategy still opens CLUSTER_SIZE channels to the actual
    // running cluster nodes.
    let connected_at_start = edgli.connected_peer_addresses().await?;
    let peer_set_at_start: std::collections::HashSet<Address> =
        connected_at_start.into_iter().collect();
    let genesis_channel_count = edgli
        .my_outgoing_channels()
        .await?
        .into_iter()
        .filter(|c| c.status == ChannelStatus::Open && !peer_set_at_start.contains(&c.destination))
        .count();
    if genesis_channel_count > 0 {
        tracing::info!(
            genesis_channel_count,
            "compensating for pre-existing genesis channels in target"
        );
    }
    let sizing = IncentiveConfiguration {
        min_open_channels: 1,
        target_open_channels: cluster_size() + genesis_channel_count,
        ..Default::default()
    };
    let mut strat_cfg = default_strategy_cfg(&edgli, &sizing).await?;

    for kind in &mut strat_cfg.strategies {
        let EdgeStrategyKind::ChannelLifecycle(lc) = kind;
        lc.selector = tuning.selector.clone();
        lc.tick_interval = tuning.strategy_tick;
        // Disable the peer-quality gate in tests so the strategy opens channels
        // to cluster nodes even before the first probe response arrives.
        lc.eligibility.min_peer_quality_score = 0.0;
    }

    let EdgeStrategyKind::ChannelLifecycle(lc0) = &strat_cfg.strategies[0];
    tracing::info!(
        initial_capacity = ?lc0.funding.initial_capacity,
        target_channels = sizing.target_open_channels,
        "strategy configured; starting reactor"
    );

    let _reactor_handle = edgli.run_reactor_from_cfg(strat_cfg)?;

    // ── 6. Wait for Edgli outgoing channels to open ──────────────────────────
    tracing::info!("waiting for strategy to open ≥1 outgoing channel");
    await_edgli_channels_open(&edgli, 1, tuning.channel_open_timeout).await?;

    // ── 7. Select the loopback exit and validate topology depth ───────────────
    // The exit is the session destination (final forward hop / first return hop); path-finding
    // fills the intermediate relays from the channel graph. Any peer works as the exit.
    let hops = hop_counts();
    let max_hops = hops.iter().copied().max().unwrap_or(0);
    let exit = if let Some(exit) = tuning.exit_node {
        tracing::info!(%exit, "using configured exit node for the loopback session(s)");
        exit
    } else {
        select_session_targets(&edgli).await?.0
    };

    // An N-hop forward path is `N+1` edges over `N` distinct relays plus the exit, with no repeated
    // node (RFC-0014 §6.3) — so we need at least `max_hops + 1` distinct peers besides ourselves.
    if matches!(net, Network::Local) {
        let peers = edgli.connected_peer_addresses().await?;
        anyhow::ensure!(
            peers.len() > max_hops,
            "need ≥{} connected peers for {}-hop path-finding, have {} (raise EDGLI_E2E_CLUSTER_SIZE)",
            max_hops + 1,
            max_hops,
            peers.len()
        );
    }

    // ── 8. Warm the network: exit probed + several probing rounds ─────────────
    // Path-finding scores edges from probe results (RFC-0010 §6.2). Without any probe observation
    // the destination edge is not yet a graph entry and path construction times out; and for
    // multi-hop we want the intermediate edges scored too. Wait for the exit to be probed, then let
    // the cluster complete a few more rounds so multi-hop paths are stable (RFC-0014 §6.3).
    tracing::info!(%exit, "waiting for loopback exit to be probed");
    await_edgli_exit_peer_ready(&edgli, exit).await?;
    if matches!(net, Network::Local) {
        // Multi-hop needs the WHOLE cluster in the graph — not just the exit — so path-finding and
        // its SURB return paths route over observed relays. Poll until every cluster node appears.
        tracing::info!(
            "waiting for the full network graph to populate (all cluster nodes observed)"
        );
        await_graph_populated(&edgli, cluster_size(), GRAPH_POPULATE_TIMEOUT).await?;
    }
    let settle = probe_settle_duration();
    tracing::info!(
        ?settle,
        "letting the graph settle and probing rounds mature before routing"
    );
    tokio::time::sleep(settle).await;

    // ── 9. Prepare the random payload ─────────────────────────────────────────
    // Random bytes produce unique HOPR packet ciphertexts on every test run, preventing false
    // replay-detection hits in cluster nodes' in-memory packet-tag caches.
    let mut payload = vec![0u8; PAYLOAD_SIZE];
    rand::rng().fill(&mut payload[..]);

    // ── 10. Run each requested hop count over the (warm) cluster ──────────────
    //
    // NOTE on cross-hop comparability: only the first iteration starts from the freshly settled
    // state (steps 8–9); every later one inherits the residual state the previous 20 MiB pump left
    // behind (drained SURB pool, warm mixer queues, exit ticket state). The SURB pool depth is not
    // exposed to the harness, so we cannot gate on it directly. To make consecutive iterations
    // start from a comparable state we re-apply the same `probe_settle_duration()` mature-window
    // before every iteration after the first, on top of the per-hop SURB pre-fill below. For
    // strictly comparable absolute numbers still run ONE hop count per process (`EDGLI_E2E_HOPS=N`),
    // as `hop_counts()` documents — each run then provisions and settles its own fresh cluster.
    for (i, &h) in hops.iter().enumerate() {
        if i > 0 {
            let resettle = probe_settle_duration();
            tracing::info!(
                hops = h,
                ?resettle,
                "re-settling before the next hop so it starts from a state comparable to the first"
            );
            tokio::time::sleep(resettle).await;
        }
        tracing::info!(hops = h, "opening {h}-hop loopback session");
        let (session, _) = edgli
            .connect_to(exit, loopback_target(), loopback_session_config(h)?)
            .await?;
        // Let SURBs accumulate before pumping. The default balancer targets ~7000 SURBs; longer
        // return paths replenish the pool more slowly, so scale the pre-fill with the hop count.
        let prefill = Duration::from_secs(5 * (h as u64 + 1));
        tokio::time::sleep(prefill).await;
        pump_and_verify(session, &payload, &format!("{h}-hop"), tuning.pump_timeout).await?;
    }

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

// ────────────────────────────────────────────────────────────────────────────
// Unit tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const RUNNING_SNAPSHOT: &str = r#"{
  "state": "running",
  "pid": 4242,
  "blokli_url": "http://127.0.0.1:8545",
  "nodes": [
    { "id": 0, "state": "channels_open", "address": "0x1111111111111111111111111111111111111111", "api_url": "http://127.0.0.1:13000", "api_token": "test-token-localcluster", "p2p": "127.0.0.1:19000", "node_admin_url": "http://localhost:4677/", "pid": 100 },
    { "id": 1, "state": "channels_open", "address": "0x2222222222222222222222222222222222222222", "api_url": "http://127.0.0.1:13001", "api_token": "test-token-localcluster", "p2p": "127.0.0.1:19001", "node_admin_url": "http://localhost:4677/", "pid": 101 },
    { "id": 2, "state": "channels_open", "address": "0x3333333333333333333333333333333333333333", "api_url": "http://127.0.0.1:13002", "api_token": "test-token-localcluster", "p2p": "127.0.0.1:19002", "node_admin_url": "http://localhost:4677/", "pid": 102 }
  ],
  "extras": [
    { "id": 0, "address": "0x4444444444444444444444444444444444444444", "safe_address": "0x5555555555555555555555555555555555555555", "module_address": "0x6666666666666666666666666666666666666666", "keystore_path": "/tmp/edgli-cluster/extra_id_0.id", "password": "local-cluster" }
  ]
}"#;

    #[test]
    fn parse_summary_json_running_snapshot() {
        let summary = parse_summary_json(RUNNING_SNAPSHOT).unwrap();
        assert_eq!(summary.blokli_url, "http://127.0.0.1:8545");
        assert_eq!(summary.nodes.len(), 3);
        assert_eq!(summary.extras.len(), 1);
        assert_eq!(
            summary.nodes[0].address.to_string(),
            "0x1111111111111111111111111111111111111111"
        );
        assert_eq!(
            summary.extras[0].keystore_path,
            PathBuf::from("/tmp/edgli-cluster/extra_id_0.id")
        );
        assert_eq!(summary.extras[0].password, "local-cluster");
    }

    #[test]
    fn parse_summary_json_rejects_null_address() {
        let json = r#"{
  "state": "running",
  "blokli_url": "http://127.0.0.1:8545",
  "nodes": [
    { "id": 0, "state": "ready", "address": null, "api_url": "http://127.0.0.1:13000", "api_token": null, "p2p": "127.0.0.1:19000", "node_admin_url": "http://localhost:4677/", "pid": null }
  ],
  "extras": [
    { "id": 0, "address": "0x4444444444444444444444444444444444444444", "safe_address": "0x5555555555555555555555555555555555555555", "module_address": "0x6666666666666666666666666666666666666666", "keystore_path": "/tmp/extra.id", "password": "pw" }
  ]
}"#;
        assert!(parse_summary_json(json).is_err());
    }
}
