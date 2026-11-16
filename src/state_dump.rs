//! The edge node's periodic state lines.
//!
//! Every interval each part of the node logs one INFO line with `event="component_state"`:
//! `chain` (Safe balance and allowance, channels, announced peers), `graph` (own edges with their
//! probe rates and latency, empty path searches), `transport` (connected and discovered peers) and
//! `paths`, which this module builds from transport events: per destination, how many path
//! resolves worked or failed and how many SURBs went out and came back.
//!
//! `HOPR_STATE_DUMP_INTERVAL` sets the interval in seconds: 60 by default, 0 turns the lines off,
//! otherwise 5 to 86400. The components read the same variable.

use std::{
    collections::HashMap,
    future::Future,
    sync::{Arc, Mutex},
    time::Duration,
};

use hopr_lib::{
    api::types::{internal::NodeId, primitive::prelude::Address},
    errors::HoprLibError,
    exports::transport::observer::{PathDirection, TransportEvent},
    state_dump::{StateDump, StateDumpFactory, StateDumpSections},
};
use hopr_network_graph::SharedChannelGraph;
use hopr_transport_p2p::HoprNetwork;
use serde::Serialize;

use crate::client::EdgeChainConnector;

const INTERVAL_ENV: &str = "HOPR_STATE_DUMP_INTERVAL";
const DEFAULT_INTERVAL: Duration = Duration::from_secs(60);
/// Destinations counted per interval; events for more destinations are only counted in total.
const MAX_TRACKED_DESTINATIONS: usize = 64;
/// Destinations written per line, the ones with failures first.
const MAX_REPORTED_DESTINATIONS: usize = 16;

/// Reads the reporting interval the same way the components do; `None` means turned off.
fn interval_from_env() -> Result<Option<Duration>, HoprLibError> {
    match std::env::var(INTERVAL_ENV) {
        Ok(value) => parse_interval(&value),
        Err(std::env::VarError::NotPresent) => Ok(Some(DEFAULT_INTERVAL)),
        Err(_) => Err(HoprLibError::GeneralError(format!(
            "{INTERVAL_ENV} is not valid UTF-8"
        ))),
    }
}

fn parse_interval(value: &str) -> Result<Option<Duration>, HoprLibError> {
    // Match the component reporters: whitespace is invalid, too. Accepting it
    // here would let startup succeed while those reporters immediately fail.
    let seconds = value
        .parse::<u64>()
        .map_err(|_| HoprLibError::GeneralError(format!("{INTERVAL_ENV} must be whole seconds")))?;
    match seconds {
        0 => Ok(None),
        5..=86400 => Ok(Some(Duration::from_secs(seconds))),
        _ => Err(HoprLibError::GeneralError(format!(
            "{INTERVAL_ENV} must be 0 or 5..=86400 seconds"
        ))),
    }
}

/// Installs the node's state reporting through `HoprBuilder::with_state_dump`.
pub struct EdgeStateDump {
    /// Names this node in every line, so lines from several logs can be told apart.
    instance: String,
    interval: Option<Duration>,
}

impl EdgeStateDump {
    pub fn new(instance: impl Into<String>) -> Self {
        Self {
            instance: instance.into(),
            interval: None,
        }
    }
}

impl StateDumpFactory<Arc<EdgeChainConnector>, SharedChannelGraph, HoprNetwork> for EdgeStateDump {
    fn prepare(&mut self) -> Result<(), HoprLibError> {
        self.interval = interval_from_env()?;
        Ok(())
    }

    fn create(
        self,
        chain: Arc<EdgeChainConnector>,
        graph: SharedChannelGraph,
        network: HoprNetwork,
        safe: Address,
        sections: StateDumpSections,
    ) -> Option<StateDump> {
        let interval = self.interval?;
        let paths = Arc::new(PathCounters::default());
        let observer = {
            let paths = paths.clone();
            Arc::new(move |event| paths.record(event))
        };
        let instance = self.instance;

        Some(StateDump::new(Some(observer), async move {
            futures::join!(
                // No funding policy yet: the chain line reports funding values as unknown.
                report_component("chain", chain.run_state_reporting(&instance, safe, None)),
                // No path sample: the graph line covers edges and searches only.
                report_component("graph", graph.run_state_reporting(&instance, Vec::new)),
                report_component("transport", network.run_state_reporting(&instance)),
                report_paths(&paths, &instance, interval, &sections),
            );
        }))
    }
}

async fn report_component(
    component: &'static str,
    reporting: impl Future<Output = Result<(), String>>,
) {
    // Report failures immediately: the other reporters are long-running, so
    // waiting for the outer join before inspecting results would hide errors.
    if let Err(error) = reporting.await {
        tracing::warn!(component, %error, "component state reporting stopped");
    }
}

/// What happened for one destination in the current interval.
#[derive(Debug, Default, Serialize)]
struct DestinationCounts {
    forward_resolved: u64,
    forward_failed: u64,
    return_resolved: u64,
    return_failed: u64,
    /// Graph searches whose last phase found no usable path.
    empty_searches: u64,
    surbs_minted: u64,
    surbs_returned: u64,
}

impl DestinationCounts {
    fn failures(&self) -> u64 {
        self.forward_failed + self.return_failed + self.empty_searches
    }

    fn total(&self) -> u64 {
        self.forward_resolved
            + self.return_resolved
            + self.failures()
            + self.surbs_minted
            + self.surbs_returned
    }
}

#[derive(Default)]
struct PathCounters {
    window: Mutex<PathWindow>,
}

#[derive(Default)]
struct PathWindow {
    destinations: HashMap<NodeId, DestinationCounts>,
    /// Events for destinations beyond [`MAX_TRACKED_DESTINATIONS`].
    untracked_events: u64,
}

impl PathWindow {
    fn assess(&self) -> (bool, &'static str) {
        if self.destinations.values().any(|counts| {
            counts.forward_failed + counts.empty_searches > 0 && counts.forward_resolved == 0
        }) {
            (true, "no_path_to_destination")
        } else if self
            .destinations
            .values()
            .any(|counts| counts.surbs_minted > 0 && counts.surbs_returned == 0)
        {
            (true, "surbs_not_returning")
        } else if self.untracked_events > 0 {
            (true, "destination_tracking_limit")
        } else if self.destinations.is_empty() {
            (false, "no_path_activity")
        } else {
            (false, "paths_ok")
        }
    }
}

impl PathCounters {
    /// Called on the packet path, so the lock is held only for one map update.
    fn record(&self, event: TransportEvent) {
        let destination = match &event {
            TransportEvent::PathResolved { destination, .. }
            | TransportEvent::PathSearched { destination, .. } => *destination,
            TransportEvent::Surb { destination, .. } => NodeId::Offchain(*destination),
        };
        let mut window = self.window.lock().unwrap_or_else(|e| e.into_inner());
        if window.destinations.len() >= MAX_TRACKED_DESTINATIONS
            && !window.destinations.contains_key(&destination)
        {
            window.untracked_events += 1;
            return;
        }
        let counts = window.destinations.entry(destination).or_default();
        match event {
            TransportEvent::PathResolved {
                direction,
                succeeded,
                ..
            } => match (direction, succeeded) {
                (PathDirection::Forward, true) => counts.forward_resolved += 1,
                (PathDirection::Forward, false) => counts.forward_failed += 1,
                (PathDirection::Return, true) => counts.return_resolved += 1,
                (PathDirection::Return, false) => counts.return_failed += 1,
            },
            TransportEvent::PathSearched { phase1, phase2, .. } => {
                if phase2.unwrap_or(phase1).eligible == 0 {
                    counts.empty_searches += 1;
                }
            }
            TransportEvent::Surb {
                minted, returned, ..
            } => {
                counts.surbs_minted += minted;
                counts.surbs_returned += returned;
            }
        }
    }

    /// Takes this interval's counts and starts a new interval.
    fn take(&self) -> PathsState {
        // Take the counts and overflow total under the same lock, so both refer
        // to the same interval even while new events arrive.
        let window = std::mem::take(&mut *self.window.lock().unwrap_or_else(|e| e.into_inner()));
        // Assess every tracked destination before shortening the log payload.
        let assessment = window.assess();
        let mut destinations: Vec<DestinationReport> = window
            .destinations
            .into_iter()
            .map(|(destination, counts)| DestinationReport {
                destination: destination.to_string(),
                counts,
            })
            .collect();
        destinations.sort_by(|a, b| {
            b.counts
                .failures()
                .cmp(&a.counts.failures())
                .then(b.counts.total().cmp(&a.counts.total()))
                .then(a.destination.cmp(&b.destination))
        });
        let destinations_omitted = destinations.len().saturating_sub(MAX_REPORTED_DESTINATIONS);
        destinations.truncate(MAX_REPORTED_DESTINATIONS);
        PathsState {
            destinations,
            destinations_omitted,
            untracked_events: window.untracked_events,
            sections: HashMap::new(),
            assessment,
        }
    }
}

#[derive(Debug, Serialize)]
struct DestinationReport {
    destination: String,
    #[serde(flatten)]
    counts: DestinationCounts,
}

#[derive(Debug, Serialize)]
struct PathsState {
    destinations: Vec<DestinationReport>,
    destinations_omitted: usize,
    untracked_events: u64,
    /// Extra sections registered with `HoprBuilder::with_state_dump_section`.
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    sections: HashMap<&'static str, serde_json::Value>,
    /// Health of the complete interval, including destinations omitted above.
    #[serde(skip)]
    assessment: (bool, &'static str),
}

async fn report_paths(
    paths: &PathCounters,
    instance: &str,
    interval: Duration,
    sections: &StateDumpSections,
) {
    let mut ticks = tokio::time::interval(interval);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // The first tick fires at once; the first report covers a full interval.
    ticks.tick().await;
    let mut sequence = 0u64;
    loop {
        ticks.tick().await;
        let mut state = paths.take();
        state.sections = sections
            .iter()
            .map(|(name, read)| (*name, read()))
            .collect();
        let (analysis_required, reason) = state.assessment;
        match serde_json::to_string(&state) {
            Ok(state) => tracing::info!(
                event = "component_state",
                component = "paths",
                instance,
                sequence,
                reason,
                analysis_required,
                state = %state,
                "component operational state"
            ),
            Err(error) => tracing::info!(
                event = "component_state",
                component = "paths",
                instance,
                sequence,
                reason = "encoding_error",
                analysis_required = true,
                %error,
                "component operational state"
            ),
        }
        sequence += 1;
    }
}

#[cfg(test)]
mod tests {
    use hopr_lib::{api::graph::ForwardAndReturnPath, exports::transport::observer::PhaseCounts};

    use super::*;

    #[derive(Clone, Default)]
    struct CapturedLogs(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for CapturedLogs {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl CapturedLogs {
        fn json_subscriber(&self) -> impl tracing::Subscriber + Send + Sync + 'static {
            let writer = self.clone();
            tracing_subscriber::fmt()
                .json()
                .without_time()
                .with_max_level(tracing::Level::INFO)
                .with_writer(move || writer.clone())
                .finish()
        }

        fn records(&self) -> Vec<serde_json::Value> {
            self.0
                .lock()
                .unwrap()
                .split(|byte| *byte == b'\n')
                .filter(|line| !line.is_empty())
                .map(|line| serde_json::from_slice(line).expect("valid JSON log record"))
                .collect()
        }
    }

    fn offchain(seed: u8) -> hopr_lib::api::types::crypto::prelude::OffchainPublicKey {
        use hopr_lib::api::types::crypto::prelude::{Keypair, OffchainKeypair};
        *OffchainKeypair::from_secret(&[seed.wrapping_add(1); 32])
            .expect("valid test secret")
            .public()
    }

    fn resolved(seed: u8, direction: PathDirection, succeeded: bool) -> TransportEvent {
        TransportEvent::PathResolved {
            direction,
            destination: NodeId::Offchain(offchain(seed)),
            succeeded,
        }
    }

    fn surbs(seed: u8, minted: u64, returned: u64) -> TransportEvent {
        TransportEvent::Surb {
            destination: offchain(seed),
            paths: ForwardAndReturnPath {
                forward: [0, 1, 0, 0, 0],
                reply: [1, 0, 0, 0, 0],
            },
            minted,
            returned,
        }
    }

    #[test]
    fn interval_validation_matches_component_reporters() {
        assert_eq!(parse_interval("0").unwrap(), None);
        for seconds in [5, 60, 86400] {
            assert_eq!(
                parse_interval(&seconds.to_string()).unwrap(),
                Some(Duration::from_secs(seconds))
            );
        }
        for invalid in ["", "-1", "1", "4", "86401", "1.5", " 60", "60 ", "invalid"] {
            assert!(parse_interval(invalid).is_err(), "accepted {invalid:?}");
        }
    }

    #[test]
    fn factory_prepares_interval_from_the_startup_environment() {
        const CHILD_CASE: &str = "EDGLI_TEST_STATE_DUMP_ENV_CASE";
        if let Ok(case) = std::env::var(CHILD_CASE) {
            let mut factory = EdgeStateDump::new("test-node");
            let result = factory.prepare();
            assert_eq!(factory.instance, "test-node");
            match case.as_str() {
                "default" => {
                    result.unwrap();
                    assert_eq!(factory.interval, Some(DEFAULT_INTERVAL));
                }
                "disabled" => {
                    result.unwrap();
                    assert_eq!(factory.interval, None);
                }
                "enabled" => {
                    result.unwrap();
                    assert_eq!(factory.interval, Some(Duration::from_secs(5)));
                }
                "invalid" => assert!(result.is_err()),
                "non-utf8" => assert!(result.unwrap_err().to_string().contains("UTF-8")),
                _ => panic!("unknown case {case}"),
            }
            return;
        }

        // Each case gets a fresh process: changing environment variables inside
        // the parallel test runner would race with other component reporters.
        let run = |case: &str, interval: Option<std::ffi::OsString>| {
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    "state_dump::tests::factory_prepares_interval_from_the_startup_environment",
                    "--nocapture",
                ])
                .env(CHILD_CASE, case)
                .env_remove(INTERVAL_ENV);
            if let Some(interval) = interval {
                command.env(INTERVAL_ENV, interval);
            }
            let output = command.output().expect("start environment test child");
            assert!(
                output.status.success(),
                "{case}: {}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        };
        run("default", None);
        run("disabled", Some("0".into()));
        run("enabled", Some("5".into()));
        run("invalid", Some("4".into()));
        run("invalid", Some(" 60 ".into()));
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            run("non-utf8", Some(std::ffi::OsString::from_vec(vec![0xff])));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn periodic_reports_emit_info_with_fresh_counts_and_sections() {
        use std::sync::atomic::{AtomicU64, Ordering};

        let logs = CapturedLogs::default();
        let _subscriber = tracing::subscriber::set_default(logs.json_subscriber());
        let paths = PathCounters::default();
        let interval = Duration::from_secs(5);
        let revision = Arc::new(AtomicU64::new(1));
        let section_revision = revision.clone();
        let sections: StateDumpSections = vec![(
            "application",
            Arc::new(
                move || serde_json::json!({"revision": section_revision.load(Ordering::Relaxed)}),
            ),
        )];
        let mut reporter = Box::pin(report_paths(&paths, "test-node", interval, &sections));
        assert!(futures::poll!(reporter.as_mut()).is_pending());
        paths.record(resolved(1, PathDirection::Forward, true));
        paths.record(resolved(1, PathDirection::Return, true));
        paths.record(surbs(1, 4, 3));

        tokio::time::advance(Duration::from_secs(4)).await;
        assert!(futures::poll!(reporter.as_mut()).is_pending());
        assert!(
            logs.records().is_empty(),
            "wait for a complete first interval"
        );
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(futures::poll!(reporter.as_mut()).is_pending());
        let first = logs.records();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0]["level"], "INFO");
        let fields = &first[0]["fields"];
        assert_eq!(fields["event"], "component_state");
        assert_eq!(fields["component"], "paths");
        assert_eq!(fields["instance"], "test-node");
        assert_eq!(fields["sequence"], 0);
        assert_eq!(fields["reason"], "paths_ok");
        assert_eq!(fields["analysis_required"], false);
        let state: serde_json::Value =
            serde_json::from_str(fields["state"].as_str().unwrap()).unwrap();
        assert_eq!(
            state["destinations"][0]["destination"],
            offchain(1).to_string()
        );
        assert_eq!(state["destinations"][0]["forward_resolved"], 1);
        assert_eq!(state["destinations"][0]["return_resolved"], 1);
        assert_eq!(state["destinations"][0]["surbs_minted"], 4);
        assert_eq!(state["destinations"][0]["surbs_returned"], 3);
        assert_eq!(state["sections"]["application"]["revision"], 1);

        revision.store(2, Ordering::Relaxed);
        paths.record(resolved(1, PathDirection::Forward, false));
        tokio::time::advance(interval).await;
        assert!(futures::poll!(reporter.as_mut()).is_pending());
        let records = logs.records();
        assert_eq!(records.len(), 2);
        let fields = &records[1]["fields"];
        assert_eq!(records[1]["level"], "INFO");
        assert_eq!(fields["sequence"], 1);
        assert_eq!(fields["analysis_required"], true);
        assert_eq!(fields["reason"], "no_path_to_destination");
        let state: serde_json::Value =
            serde_json::from_str(fields["state"].as_str().unwrap()).unwrap();
        assert_eq!(state["destinations"][0]["forward_failed"], 1);
        assert_eq!(state["destinations"][0]["forward_resolved"], 0);
        assert_eq!(state["destinations"][0]["surbs_minted"], 0);
        assert_eq!(state["sections"]["application"]["revision"], 2);

        tokio::time::advance(interval).await;
        assert!(futures::poll!(reporter.as_mut()).is_pending());
        let records = logs.records();
        assert_eq!(records.len(), 3);
        assert_eq!(records[2]["fields"]["sequence"], 2);
        assert_eq!(records[2]["fields"]["reason"], "no_path_activity");
        assert_eq!(records[2]["fields"]["analysis_required"], false);
        drop(reporter);
        tokio::time::advance(interval * 2).await;
        assert_eq!(
            logs.records().len(),
            3,
            "dropping the reporter stops emission"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn missed_intervals_do_not_burst_empty_reports() {
        let logs = CapturedLogs::default();
        let _subscriber = tracing::subscriber::set_default(logs.json_subscriber());
        let paths = PathCounters::default();
        let sections = Vec::new();
        let interval = Duration::from_secs(5);
        let mut reporter = Box::pin(report_paths(&paths, "test-node", interval, &sections));
        assert!(futures::poll!(reporter.as_mut()).is_pending());
        tokio::time::advance(interval * 4).await;
        assert!(futures::poll!(reporter.as_mut()).is_pending());
        assert_eq!(logs.records().len(), 1);
        let state: serde_json::Value =
            serde_json::from_str(logs.records()[0]["fields"]["state"].as_str().unwrap()).unwrap();
        assert!(state.get("sections").is_none());
        tokio::time::advance(Duration::from_secs(4)).await;
        assert!(futures::poll!(reporter.as_mut()).is_pending());
        assert_eq!(logs.records().len(), 1);
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(futures::poll!(reporter.as_mut()).is_pending());
        assert_eq!(logs.records().len(), 2);
        assert_eq!(logs.records()[1]["fields"]["sequence"], 1);
    }

    #[test]
    fn counts_resolves_searches_and_surbs_per_destination() {
        let paths = PathCounters::default();
        paths.record(resolved(1, PathDirection::Forward, true));
        paths.record(resolved(1, PathDirection::Return, false));
        paths.record(TransportEvent::PathSearched {
            direction: PathDirection::Forward,
            destination: NodeId::Offchain(offchain(1)),
            phase1: PhaseCounts {
                returned: 3,
                eligible: 1,
            },
            phase2: Some(PhaseCounts::default()),
            retained: 0,
        });
        paths.record(surbs(1, 3, 2));

        let state = paths.take();
        assert_eq!(state.destinations.len(), 1);
        let counts = &state.destinations[0].counts;
        assert_eq!(
            (
                counts.forward_resolved,
                counts.return_failed,
                counts.empty_searches,
                counts.surbs_minted,
                counts.surbs_returned,
            ),
            (1, 1, 1, 3, 2)
        );
        assert!(
            paths.take().destinations.is_empty(),
            "take starts a new interval"
        );
    }

    #[test]
    fn a_destination_without_any_working_forward_path_needs_analysis() {
        let paths = PathCounters::default();
        paths.record(resolved(1, PathDirection::Forward, false));
        paths.record(resolved(2, PathDirection::Forward, true));

        let state = paths.take();
        assert_eq!(state.assessment, (true, "no_path_to_destination"));
        assert_eq!(
            state.destinations[0].counts.forward_failed, 1,
            "failures come first"
        );
    }

    #[test]
    fn destinations_beyond_the_limit_are_counted_but_not_tracked() {
        let paths = PathCounters::default();
        for seed in 0..=MAX_TRACKED_DESTINATIONS as u8 {
            paths.record(resolved(seed, PathDirection::Forward, true));
        }

        let state = paths.take();
        assert_eq!(state.untracked_events, 1);
        assert_eq!(state.destinations.len(), MAX_REPORTED_DESTINATIONS);
        assert_eq!(
            state.destinations_omitted,
            MAX_TRACKED_DESTINATIONS - MAX_REPORTED_DESTINATIONS
        );
        assert_eq!(state.assessment, (true, "destination_tracking_limit"));

        // A new interval has capacity again, and does not inherit overflow.
        paths.record(resolved(
            MAX_TRACKED_DESTINATIONS as u8,
            PathDirection::Forward,
            true,
        ));
        let next = paths.take();
        assert_eq!(next.destinations.len(), 1);
        assert_eq!(next.untracked_events, 0);
        assert_eq!(next.assessment, (false, "paths_ok"));
    }

    #[test]
    fn omitted_destinations_still_contribute_to_the_health_assessment() {
        for (event, reason) in [
            (
                resolved(63, PathDirection::Forward, false),
                "no_path_to_destination",
            ),
            (surbs(63, 1, 0), "surbs_not_returning"),
        ] {
            let paths = PathCounters::default();
            // These recovered destinations rank ahead of the unhealthy one.
            for seed in 0..MAX_REPORTED_DESTINATIONS as u8 {
                paths.record(resolved(seed, PathDirection::Forward, false));
                paths.record(resolved(seed, PathDirection::Forward, false));
                paths.record(resolved(seed, PathDirection::Forward, true));
            }
            paths.record(event);

            let state = paths.take();
            assert_eq!(state.destinations.len(), MAX_REPORTED_DESTINATIONS);
            assert_eq!(state.destinations_omitted, 1);
            assert!(
                state
                    .destinations
                    .iter()
                    .all(|d| d.destination != offchain(63).to_string())
            );
            assert_eq!(state.assessment, (true, reason));
            assert!(
                serde_json::to_value(state)
                    .unwrap()
                    .get("assessment")
                    .is_none()
            );
        }
    }

    #[test]
    fn equal_activity_is_reported_in_a_stable_order() {
        let ascending = PathCounters::default();
        let descending = PathCounters::default();
        for seed in 0..=MAX_REPORTED_DESTINATIONS as u8 {
            ascending.record(resolved(seed, PathDirection::Forward, true));
        }
        for seed in (0..=MAX_REPORTED_DESTINATIONS as u8).rev() {
            descending.record(resolved(seed, PathDirection::Forward, true));
        }
        assert_eq!(
            serde_json::to_value(ascending.take()).unwrap(),
            serde_json::to_value(descending.take()).unwrap(),
        );
    }

    #[test]
    fn component_errors_are_logged_while_other_reporters_keep_running() {
        let logs = CapturedLogs::default();
        let writer = logs.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            futures::executor::block_on(async {
                let mut reporters = std::pin::pin!(futures::future::join(
                    report_component("chain", async { Err("invalid interval".to_owned()) }),
                    std::future::pending::<()>(),
                ));
                assert!(futures::poll!(reporters.as_mut()).is_pending());
            });
        });
        let output = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
        assert!(
            output.contains("component state reporting stopped"),
            "{output}"
        );
        assert!(
            output.contains("chain") && output.contains("invalid interval"),
            "{output}"
        );
    }

    #[test]
    fn an_idle_interval_needs_no_analysis() {
        assert_eq!(
            PathCounters::default().take().assessment,
            (false, "no_path_activity")
        );
    }
}
