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
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use hopr_lib::{
    api::types::primitive::prelude::Address,
    errors::HoprLibError,
    exports::transport::{
        TransportObserver,
        observer::{PathDirection, TransportEvent},
    },
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
    let seconds = match std::env::var(INTERVAL_ENV) {
        Ok(value) => value.trim().parse::<u64>().map_err(|_| {
            HoprLibError::GeneralError(format!("{INTERVAL_ENV} must be whole seconds"))
        })?,
        Err(std::env::VarError::NotPresent) => return Ok(Some(DEFAULT_INTERVAL)),
        Err(_) => {
            return Err(HoprLibError::GeneralError(format!(
                "{INTERVAL_ENV} is not valid UTF-8"
            )));
        }
    };
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
        let observer: TransportObserver = {
            let paths = paths.clone();
            Arc::new(move |event| paths.record(event))
        };
        let instance = self.instance;

        Some(StateDump::new(Some(observer), async move {
            let (chain_result, graph_result, network_result, ()) = futures::join!(
                // No funding policy yet: the chain line reports funding values as unknown.
                chain.run_state_reporting(&instance, safe, None),
                // No path sample: the graph line covers edges and searches only.
                graph.run_state_reporting(&instance, Vec::new),
                network.run_state_reporting(&instance),
                report_paths(&paths, &instance, interval, &sections),
            );
            for (component, result) in [
                ("chain", chain_result),
                ("graph", graph_result),
                ("transport", network_result),
            ] {
                if let Err(error) = result {
                    tracing::warn!(component, %error, "component state reporting stopped");
                }
            }
        }))
    }
}

/// What happened for one destination in the current interval.
#[derive(Debug, Default, Clone, Serialize)]
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
        self.forward_resolved + self.return_resolved + self.failures() + self.surbs_minted
    }
}

#[derive(Default)]
struct PathCounters {
    window: Mutex<HashMap<String, DestinationCounts>>,
    /// Events for destinations beyond [`MAX_TRACKED_DESTINATIONS`].
    untracked: AtomicU64,
}

impl PathCounters {
    /// Called on the packet path, so the lock is held only for one map update.
    fn record(&self, event: TransportEvent) {
        let key = match &event {
            TransportEvent::PathResolved { destination, .. }
            | TransportEvent::PathSearched { destination, .. } => destination.to_string(),
            TransportEvent::Surb { destination, .. } => destination.to_string(),
        };
        let mut window = self.window.lock().unwrap_or_else(|e| e.into_inner());
        if !window.contains_key(&key) && window.len() >= MAX_TRACKED_DESTINATIONS {
            self.untracked.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let counts = window.entry(key).or_default();
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
        let window = std::mem::take(&mut *self.window.lock().unwrap_or_else(|e| e.into_inner()));
        let mut destinations: Vec<DestinationReport> = window
            .into_iter()
            .map(|(destination, counts)| DestinationReport {
                destination,
                counts,
            })
            .collect();
        destinations.sort_by(|a, b| {
            b.counts
                .failures()
                .cmp(&a.counts.failures())
                .then(b.counts.total().cmp(&a.counts.total()))
        });
        let destinations_omitted = destinations.len().saturating_sub(MAX_REPORTED_DESTINATIONS);
        destinations.truncate(MAX_REPORTED_DESTINATIONS);
        PathsState {
            destinations,
            destinations_omitted,
            untracked_events: self.untracked.swap(0, Ordering::Relaxed),
            sections: HashMap::new(),
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
}

impl PathsState {
    /// Whether this interval needs a closer look, and why, in the components' wording style.
    fn assess(&self) -> (bool, &'static str) {
        let counts = || self.destinations.iter().map(|d| &d.counts);
        if counts().any(|c| {
            c.forward_failed + c.empty_searches > 0 && c.forward_resolved == 0
        }) {
            (true, "no_path_to_destination")
        } else if counts().any(|c| c.surbs_minted > 0 && c.surbs_returned == 0) {
            (true, "surbs_not_returning")
        } else if self.destinations.is_empty() {
            (false, "no_path_activity")
        } else {
            (false, "paths_ok")
        }
    }
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
        state.sections = sections.iter().map(|(name, read)| (*name, read())).collect();
        let (analysis_required, reason) = state.assess();
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
    use hopr_lib::{api::types::internal::NodeId, exports::transport::observer::PhaseCounts};

    use super::*;

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

        let state = paths.take();
        assert_eq!(state.destinations.len(), 1);
        let counts = &state.destinations[0].counts;
        assert_eq!(
            (
                counts.forward_resolved,
                counts.return_failed,
                counts.empty_searches
            ),
            (1, 1, 1)
        );
        assert!(paths.take().destinations.is_empty(), "take starts a new interval");
    }

    #[test]
    fn a_destination_without_any_working_forward_path_needs_analysis() {
        let paths = PathCounters::default();
        paths.record(resolved(1, PathDirection::Forward, false));
        paths.record(resolved(2, PathDirection::Forward, true));

        let state = paths.take();
        assert_eq!(state.assess(), (true, "no_path_to_destination"));
        assert_eq!(state.destinations[0].counts.forward_failed, 1, "failures come first");
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
    }

    #[test]
    fn an_idle_interval_needs_no_analysis() {
        assert_eq!(
            PathCounters::default().take().assess(),
            (false, "no_path_activity")
        );
    }
}
