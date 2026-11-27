//! Frees Safe wxHOPR for a PIX refill by closing channels, without the channel strategy knowing why.
//!
//! The reactor rebuilds the channel-lifecycle strategy with a [`lifecycle_config`] for the current
//! [`LifecycleMode`]. In [`LifecycleMode::Reclaim`] that config retires every channel down to
//! `keep`, so the unchanged strategy closes its own worst-ranked channels and finalizes them.

use std::time::Duration;

use futures::future::Either;
use hopr_lib::api::types::primitive::prelude::{HoprBalance, U256};
use hopr_strategy::channel_lifecycle::{
    ChannelLifecycleConfig, RestartGuardConfig, SelectorProfile,
};
use hopr_strategy::strategy::Strategy;
use tokio::sync::watch;

use crate::client::StrategyStateHandle;

/// Which config the channel-lifecycle strategy runs with.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum LifecycleMode {
    #[default]
    Normal,
    /// Close channels, worst-ranked first, until `keep` remain.
    #[cfg_attr(
        not(feature = "pix-curvy"),
        allow(dead_code, reason = "only a Curvy PIX pool reclaims channels")
    )]
    Reclaim { keep: usize },
}

/// `base` for [`LifecycleMode::Normal`]; for [`LifecycleMode::Reclaim`], `base` with every channel
/// marked drained, the population pinned to `keep`, no startup close guard and a selector that
/// ranks its closes.
pub(crate) fn lifecycle_config(
    base: &ChannelLifecycleConfig,
    mode: LifecycleMode,
) -> ChannelLifecycleConfig {
    let mut cfg = base.clone();
    if let LifecycleMode::Reclaim { keep } = mode {
        // Drained channels are retired, not refunded, and bypass the selector's anonymity veto.
        cfg.closure.close_when_drained_below = HoprBalance::from(U256::MAX);
        cfg.population.min_open_channels = keep;
        cfg.population.target_open_channels = keep;
        // The default selector closes in connector order, which can retire the best relay.
        if cfg.selector == SelectorProfile::Default {
            cfg.selector = SelectorProfile::Balanced;
        }
        // The node's view is warm; only this strategy instance is new.
        cfg.restart = RestartGuardConfig {
            startup_observation_period: Duration::ZERO,
            startup_close_grace_period: Duration::ZERO,
        };
    }
    cfg
}

/// What the Safe and the node's outgoing channels hold.
#[cfg(feature = "pix-curvy")]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct SafeView {
    pub safe: HoprBalance,
    /// Balances of the open outgoing channels.
    pub open: Vec<HoprBalance>,
    /// Stake in outgoing channels already closing, on its way back to the Safe.
    pub closing: HoprBalance,
}

/// How many of `open` channels to keep so the rest cover `need` whichever ones close.
///
/// Counts the smallest balances first. Never below one.
#[cfg(feature = "pix-curvy")]
pub(crate) fn channels_to_keep(open: &[HoprBalance], need: HoprBalance) -> usize {
    let mut smallest_first = open.to_vec();
    smallest_first.sort();
    let mut freed = HoprBalance::zero();
    let closed = smallest_first
        .iter()
        .take_while(|balance| {
            let short = freed < need;
            freed += **balance;
            short
        })
        .count();
    open.len().saturating_sub(closed).max(1)
}

/// The mode while the PIX pool is low, with `reserved` of the Safe claimed for its refill.
///
/// Reclaims once the Safe and stake already closing fall short of `reserved`, closing enough for a
/// whole `top_up` so one round covers the refill. Back to normal once they cover it.
#[cfg(feature = "pix-curvy")]
pub(crate) fn next_mode(
    current: LifecycleMode,
    reserved: HoprBalance,
    top_up: HoprBalance,
    view: &SafeView,
) -> LifecycleMode {
    let covered = view.safe + view.closing;
    let keep = || channels_to_keep(&view.open, top_up - covered);
    match current {
        LifecycleMode::Normal if covered >= reserved => LifecycleMode::Normal,
        LifecycleMode::Normal => match keep() {
            keep if keep < view.open.len() => LifecycleMode::Reclaim { keep },
            _ => LifecycleMode::Normal,
        },
        LifecycleMode::Reclaim { .. } if covered >= top_up => LifecycleMode::Normal,
        // Only ever down: a rebuild mid-round would restart the closes it already chose.
        LifecycleMode::Reclaim { keep: kept } => LifecycleMode::Reclaim {
            keep: kept.min(keep()),
        },
    }
}

enum Wake {
    Ended(hopr_strategy::errors::Result<()>),
    ModeChanged,
    ModeFixed,
}

/// Runs `strategy` and replaces it with `build(mode)` whenever `mode` changes; never returns while
/// `mode` can still change.
///
/// Awaits `settled` between the two: a new strategy knows nothing of the writes the old one left
/// unconfirmed, and would close or open over them.
pub(crate) async fn supervise_lifecycle<B, S, SFut>(
    mut strategy: Box<dyn Strategy + Send>,
    build: B,
    settled: S,
    mut mode: watch::Receiver<LifecycleMode>,
    state: StrategyStateHandle,
) where
    B: Fn(LifecycleMode) -> hopr_strategy::errors::Result<Box<dyn Strategy + Send>>,
    S: Fn() -> SFut,
    SFut: Future<Output = ()>,
{
    loop {
        let wake = {
            let run = strategy.run();
            let changed = mode.changed();
            futures::pin_mut!(run, changed);
            match futures::future::select(run, changed).await {
                Either::Left((ended, _)) => Wake::Ended(ended),
                Either::Right((Ok(()), _)) => Wake::ModeChanged,
                Either::Right((Err(_), _)) => Wake::ModeFixed,
            }
        };
        match wake {
            Wake::Ended(ended) => {
                match ended {
                    Ok(()) => tracing::warn!("channel-lifecycle strategy stopped"),
                    Err(error) => tracing::error!(%error, "channel-lifecycle strategy failed"),
                }
                if mode.changed().await.is_err() {
                    return;
                }
            }
            Wake::ModeFixed => {
                if let Err(error) = strategy.run().await {
                    tracing::error!(%error, "channel-lifecycle strategy failed");
                }
                return;
            }
            Wake::ModeChanged => {}
        }
        settled().await;
        let next = *mode.borrow_and_update();
        match build(next) {
            Ok(rebuilt) => {
                state.follow(rebuilt.state_handle());
                strategy = rebuilt;
                tracing::info!(mode = ?next, "channel-lifecycle strategy rebuilt");
            }
            Err(error) => tracing::error!(
                %error,
                mode = ?next,
                "channel-lifecycle strategy not rebuilt; the current one keeps running"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fmt::{Display, Formatter};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use hopr_strategy::strategy::{AtomicStrategyState, StrategyState};

    use super::*;

    #[cfg(feature = "pix-curvy")]
    fn wxhopr(amount: u64) -> HoprBalance {
        HoprBalance::new_base(amount)
    }

    #[cfg(feature = "pix-curvy")]
    fn view(safe: u64, open: &[u64], closing: u64) -> SafeView {
        SafeView {
            safe: wxhopr(safe),
            open: open.iter().copied().map(wxhopr).collect(),
            closing: wxhopr(closing),
        }
    }

    #[cfg(feature = "pix-curvy")]
    #[test]
    fn keeps_every_channel_when_nothing_is_needed() {
        assert_eq!(
            channels_to_keep(&[wxhopr(5), wxhopr(9)], HoprBalance::zero()),
            2
        );
    }

    #[cfg(feature = "pix-curvy")]
    #[test]
    fn closes_enough_for_the_need_whichever_channels_close() {
        let open = [wxhopr(10), wxhopr(4), wxhopr(6), wxhopr(20)];
        assert_eq!(channels_to_keep(&open, wxhopr(4)), 3);
        assert_eq!(channels_to_keep(&open, wxhopr(5)), 2);
        assert_eq!(channels_to_keep(&open, wxhopr(10)), 2);
        assert_eq!(channels_to_keep(&open, wxhopr(11)), 1);
    }

    #[cfg(feature = "pix-curvy")]
    #[test]
    fn always_keeps_one_channel() {
        assert_eq!(channels_to_keep(&[wxhopr(1), wxhopr(1)], wxhopr(100)), 1);
        assert_eq!(channels_to_keep(&[], wxhopr(100)), 1);
    }

    #[cfg(feature = "pix-curvy")]
    #[test]
    fn a_safe_covering_the_reserve_closes_nothing() {
        let mode = next_mode(
            LifecycleMode::Normal,
            wxhopr(18),
            wxhopr(35),
            &view(10, &[5, 9, 12], 8),
        );
        assert_eq!(mode, LifecycleMode::Normal);
    }

    #[cfg(feature = "pix-curvy")]
    #[test]
    fn a_short_safe_reclaims_enough_for_a_whole_top_up() {
        // Needs 35 − 10 = 25: the smallest three (5 + 9 + 12) cover it, so one channel stays.
        let mode = next_mode(
            LifecycleMode::Normal,
            wxhopr(18),
            wxhopr(35),
            &view(10, &[5, 9, 12, 30], 0),
        );
        assert_eq!(mode, LifecycleMode::Reclaim { keep: 1 });

        // Needs 35 − 30 = 5: the smallest channel covers it.
        let mode = next_mode(
            LifecycleMode::Normal,
            wxhopr(32),
            wxhopr(35),
            &view(30, &[5, 9, 12, 30], 0),
        );
        assert_eq!(mode, LifecycleMode::Reclaim { keep: 3 });
    }

    #[cfg(feature = "pix-curvy")]
    #[test]
    fn a_single_channel_is_never_reclaimed() {
        let mode = next_mode(
            LifecycleMode::Normal,
            wxhopr(35),
            wxhopr(35),
            &view(0, &[50], 0),
        );
        assert_eq!(mode, LifecycleMode::Normal);
    }

    #[cfg(feature = "pix-curvy")]
    #[test]
    fn reclaiming_never_raises_what_it_keeps() {
        // Recomputed alone, the rest would keep two of these three.
        let mode = next_mode(
            LifecycleMode::Reclaim { keep: 1 },
            wxhopr(30),
            wxhopr(35),
            &view(5, &[20, 30, 40], 14),
        );
        assert_eq!(mode, LifecycleMode::Reclaim { keep: 1 });
    }

    #[cfg(feature = "pix-curvy")]
    #[test]
    fn reclaiming_ends_once_closing_stake_covers_a_top_up() {
        let mode = next_mode(
            LifecycleMode::Reclaim { keep: 2 },
            wxhopr(30),
            wxhopr(35),
            &view(10, &[20, 30], 25),
        );
        assert_eq!(mode, LifecycleMode::Normal);
    }

    #[test]
    fn the_normal_config_is_the_base() {
        let base = ChannelLifecycleConfig::default();
        assert_eq!(lifecycle_config(&base, LifecycleMode::Normal), base);
    }

    #[test]
    fn the_reclaim_config_changes_only_drain_population_restart_and_selector() {
        let base = ChannelLifecycleConfig::default();
        let reclaim = lifecycle_config(&base, LifecycleMode::Reclaim { keep: 3 });

        assert_eq!(
            reclaim.closure.close_when_drained_below,
            HoprBalance::from(U256::MAX)
        );
        assert_eq!(reclaim.population.min_open_channels, 3);
        assert_eq!(reclaim.population.target_open_channels, 3);
        assert_eq!(reclaim.restart.startup_observation_period, Duration::ZERO);
        assert_eq!(reclaim.restart.startup_close_grace_period, Duration::ZERO);
        assert_eq!(reclaim.selector, SelectorProfile::Balanced);

        let mut restored = reclaim.clone();
        restored.closure.close_when_drained_below = base.closure.close_when_drained_below;
        restored.population = base.population.clone();
        restored.restart = base.restart.clone();
        restored.selector = base.selector.clone();
        assert_eq!(restored, base);
    }

    #[test]
    fn the_reclaim_config_keeps_a_selector_that_already_ranks_closes() {
        let base = ChannelLifecycleConfig {
            selector: SelectorProfile::LowLatency,
            ..Default::default()
        };
        let reclaim = lifecycle_config(&base, LifecycleMode::Reclaim { keep: 3 });
        assert_eq!(reclaim.selector, SelectorProfile::LowLatency);
    }

    struct Fake {
        mode: LifecycleMode,
        state: Arc<AtomicStrategyState>,
    }

    impl Display for Fake {
        fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
            write!(f, "fake({:?})", self.mode)
        }
    }

    #[async_trait::async_trait]
    impl Strategy for Fake {
        async fn run(&mut self) -> hopr_strategy::errors::Result<()> {
            futures::future::pending().await
        }

        fn state_handle(&self) -> Arc<AtomicStrategyState> {
            Arc::clone(&self.state)
        }
    }

    fn fake(mode: LifecycleMode, state: StrategyState) -> Box<dyn Strategy + Send> {
        Box::new(Fake {
            mode,
            state: Arc::new(AtomicStrategyState::new(state)),
        })
    }

    /// Count of the node's outgoing channels that are open and closing, and the open balances.
    async fn outgoing<C: hopr_lib::api::chain::HoprChainApi>(
        chain: &C,
    ) -> anyhow::Result<(usize, usize, Vec<HoprBalance>)> {
        use futures::StreamExt;
        use hopr_lib::api::chain::ChannelSelector;
        use hopr_lib::api::types::internal::channels::ChannelStatus;

        let channels: Vec<_> = chain
            .stream_channels(ChannelSelector::default().with_source(*chain.me()))?
            .collect()
            .await;
        let open: Vec<_> = channels
            .iter()
            .filter(|c| c.status == ChannelStatus::Open)
            .map(|c| c.balance)
            .collect();
        let closing = channels
            .iter()
            .filter(|c| matches!(c.status, ChannelStatus::PendingToClose(_)))
            .count();
        Ok((open.len(), closing, open))
    }

    type TestConnector = hopr_chain_connector::testing::TestChainConnector<
        hopr_chain_connector::testing::BlokliTestClient<
            hopr_chain_connector::testing::FullStateEmulator,
        >,
    >;

    /// A node with a registered Safe of 1000 wxHOPR and an open channel of `stake` to each peer.
    async fn node_with_channels(
        peers: &[hopr_lib::api::types::primitive::prelude::Address],
        stake: HoprBalance,
    ) -> anyhow::Result<(
        Arc<TestConnector>,
        hopr_lib::api::types::primitive::prelude::Address,
    )> {
        use hopr_chain_connector::testing::{
            BlokliTestStateBuilder, create_test_blokli_connector, register_test_safe,
        };
        use hopr_lib::api::chain::ChannelEntry;
        use hopr_lib::api::types::crypto::prelude::{ChainKeypair, Keypair};
        use hopr_lib::api::types::internal::channels::ChannelStatus;
        use hopr_lib::api::types::primitive::prelude::{Address, XDaiBalance};

        let key = ChainKeypair::random();
        let me = key.public().to_address();
        let channels = peers
            .iter()
            .map(|peer| {
                ChannelEntry::builder()
                    .between(me, *peer)
                    .amount(stake.amount())
                    .ticket_index(0)
                    .status(ChannelStatus::Open)
                    .epoch(0)
                    .build()
            })
            .collect::<Result<Vec<_>, _>>()?;
        let accounts: Vec<&Address> = peers.iter().chain([&me]).collect();
        let module = Address::from([0x11; 20]);
        let client = BlokliTestStateBuilder::default()
            .with_generated_accounts(
                &accounts,
                // Announced: the strategy resolves a peer's graph edge by its published key.
                true,
                XDaiBalance::new_base(1),
                HoprBalance::new_base(1000),
            )
            .with_channels(channels)
            .build_dynamic_client(module)
            .with_tx_simulation_delay(Duration::ZERO);
        let connector = Arc::new(create_test_blokli_connector(&key, client, module).await?);
        register_test_safe(&*connector, me).await?;
        Ok((connector, me))
    }

    #[tokio::test]
    async fn the_unchanged_strategy_closes_down_to_keep_and_funds_nothing() -> anyhow::Result<()> {
        use hopr_lib::api::types::primitive::prelude::Address;
        use hopr_strategy::channel_lifecycle::ChannelLifecycleStrategy;
        use hopr_strategy::testing::LifecycleNode;

        let peers: Vec<Address> = (1..=5u8).map(|i| Address::from([i; 20])).collect();
        let stake = HoprBalance::new_base(1);
        let (connector, _) = node_with_channels(&peers, stake).await?;

        let base = ChannelLifecycleConfig {
            selector: SelectorProfile::LowLatency,
            tick_interval: Duration::from_millis(100),
            jitter: Duration::ZERO,
            ..Default::default()
        };
        let mut strategy = ChannelLifecycleStrategy::new(lifecycle_config(
            &base,
            LifecycleMode::Reclaim { keep: 2 },
        ))
        .build(Arc::new(LifecycleNode::new(Arc::clone(&connector))))?;
        let run = tokio::spawn(async move { strategy.run().await });

        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while outgoing(&*connector).await?.1 < 3 && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        // Several more ticks: a strategy that overshot `keep` would close another by now.
        tokio::time::sleep(Duration::from_millis(500)).await;
        run.abort();

        let (open, closing, balances) = outgoing(&*connector).await?;
        assert_eq!(
            (open, closing),
            (2, 3),
            "closed down to keep, and no further"
        );
        assert!(
            balances.iter().all(|balance| *balance == stake),
            "the Safe held 1000 wxHOPR, yet no kept channel was topped up"
        );
        Ok(())
    }

    /// A graph that has probed each peer at the given success rate.
    mod probed {
        use std::collections::HashMap;
        use std::time::Duration;

        use futures::StreamExt;
        use futures::stream::BoxStream;
        use hopr_lib::api::graph::traits::{
            Balance, EdgeNetworkObservableRead, EdgeObservableWrite, EdgeProtocolObservable,
            EdgeTransportMeasurement, EdgeWeightType,
        };
        use hopr_lib::api::graph::{
            EdgeImmediateProtocolObservable, EdgeLinkObservable, EdgeObservableRead,
            NetworkGraphConnectivity, NetworkGraphTraverse, NetworkGraphView, ValueFn,
        };
        use hopr_lib::api::types::crypto::prelude::OffchainPublicKey;

        pub struct Graph {
            pub identity: OffchainPublicKey,
            pub success_rates: HashMap<OffchainPublicKey, f64>,
        }

        #[derive(Clone)]
        pub struct Edge(Probes);

        #[derive(Clone)]
        pub struct Probes(f64);

        impl NetworkGraphView for Graph {
            type NodeId = OffchainPublicKey;
            type Observed = Edge;

            fn ticket_face_value(&self) -> Option<Balance> {
                None
            }

            fn path_slot(&self, _key: &Self::NodeId) -> Option<u64> {
                None
            }

            fn node_count(&self) -> usize {
                self.success_rates.len()
            }

            fn contains_node(&self, key: &Self::NodeId) -> bool {
                self.success_rates.contains_key(key)
            }

            fn nodes(&self) -> BoxStream<'static, Self::NodeId> {
                let nodes: Vec<_> = self.success_rates.keys().copied().collect();
                futures::stream::iter(nodes).boxed()
            }

            fn edge(&self, src: &Self::NodeId, dest: &Self::NodeId) -> Option<Self::Observed> {
                let rate = self.success_rates.get(dest)?;
                (*src == self.identity).then_some(Edge(Probes(*rate)))
            }

            fn identity(&self) -> &Self::NodeId {
                &self.identity
            }
        }

        impl NetworkGraphConnectivity for Graph {
            type NodeId = OffchainPublicKey;
            type Observed = Edge;

            fn connected_edges(&self) -> Vec<(Self::NodeId, Self::NodeId, Self::Observed)> {
                Vec::new()
            }

            fn reachable_edges(&self) -> Vec<(Self::NodeId, Self::NodeId, Self::Observed)> {
                Vec::new()
            }
        }

        impl NetworkGraphTraverse for Graph {
            type NodeId = OffchainPublicKey;
            type Observed = Edge;

            fn simple_paths<V: ValueFn<Weight = Self::Observed>>(
                &self,
                _source: &Self::NodeId,
                _destination: &Self::NodeId,
                _length: usize,
                _take_count: Option<usize>,
                _value_fn: V,
            ) -> Vec<(Vec<Self::NodeId>, [u64; 5], V::Value)> {
                Vec::new()
            }

            fn simple_paths_from<V: ValueFn<Weight = Self::Observed>>(
                &self,
                _source: &Self::NodeId,
                _length: usize,
                _take_count: Option<usize>,
                _value_fn: V,
            ) -> Vec<(Vec<Self::NodeId>, [u64; 5], V::Value)> {
                Vec::new()
            }

            fn simple_loopback_to_self(
                &self,
                _length: usize,
                _take_count: Option<usize>,
            ) -> Vec<(Vec<Self::NodeId>, [u64; 5])> {
                Vec::new()
            }
        }

        impl EdgeObservableRead for Edge {
            type ImmediateMeasurement = Probes;
            type IntermediateMeasurement = Probes;

            fn last_update(&self) -> Duration {
                Duration::from_secs(1)
            }

            fn immediate_qos(&self) -> Option<&Self::ImmediateMeasurement> {
                Some(&self.0)
            }

            fn intermediate_qos(&self) -> Option<&Self::IntermediateMeasurement> {
                None
            }

            fn score(&self) -> Option<f64> {
                Some(self.0.0)
            }
        }

        impl EdgeObservableWrite for Edge {
            fn record(&mut self, _measurement: EdgeWeightType) {}
        }

        impl EdgeLinkObservable for Probes {
            fn record(&mut self, _measurement: EdgeTransportMeasurement) {}

            fn average_latency(&self) -> Option<Duration> {
                Some(Duration::from_millis(50))
            }

            fn average_probe_rate(&self) -> Option<f64> {
                Some(self.0)
            }

            fn score(&self) -> Option<f64> {
                Some(self.0)
            }
        }

        impl EdgeNetworkObservableRead for Probes {
            fn is_connected(&self) -> Option<bool> {
                None
            }
        }

        impl EdgeImmediateProtocolObservable for Probes {
            fn ack_rate(&self) -> Option<f64> {
                Some(self.0)
            }
        }

        impl EdgeProtocolObservable for Probes {
            fn balance(&self) -> Option<Balance> {
                None
            }
        }
    }

    #[tokio::test]
    async fn reclaiming_under_the_default_selector_keeps_the_best_relay() -> anyhow::Result<()> {
        use hopr_lib::api::types::primitive::prelude::Address;
        use hopr_strategy::channel_lifecycle::ChannelLifecycleStrategy;
        use hopr_strategy::testing::{EmptyNetworkView, LifecycleNode, test_offchain_key};

        let peers: Vec<Address> = (1..=3u8).map(|i| Address::from([i; 20])).collect();
        // Whatever order the connector lists the channels in, one of these runs has the best first.
        for best in &peers {
            let (connector, me) = node_with_channels(&peers, HoprBalance::new_base(1)).await?;
            let graph = probed::Graph {
                identity: test_offchain_key(&me),
                success_rates: peers
                    .iter()
                    .map(|peer| {
                        (
                            test_offchain_key(peer),
                            if peer == best { 1.0 } else { 0.2 },
                        )
                    })
                    .collect(),
            };
            let base = ChannelLifecycleConfig {
                tick_interval: Duration::from_millis(100),
                jitter: Duration::ZERO,
                ..Default::default()
            };
            assert_eq!(base.selector, SelectorProfile::Default);
            let mut strategy = ChannelLifecycleStrategy::new(lifecycle_config(
                &base,
                LifecycleMode::Reclaim { keep: 1 },
            ))
            .build(Arc::new(LifecycleNode::with_views(
                Arc::clone(&connector),
                graph,
                EmptyNetworkView,
            )))?;
            let run = tokio::spawn(async move { strategy.run().await });

            let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
            while outgoing(&*connector).await?.1 < 2 && tokio::time::Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            run.abort();

            let kept = open_destinations(&*connector).await?;
            assert_eq!(kept, vec![*best], "the best-probed relay is the one kept");
        }
        Ok(())
    }

    async fn open_destinations<C: hopr_lib::api::chain::HoprChainApi>(
        chain: &C,
    ) -> anyhow::Result<Vec<hopr_lib::api::types::primitive::prelude::Address>> {
        use futures::StreamExt;
        use hopr_lib::api::chain::ChannelSelector;
        use hopr_lib::api::types::internal::channels::ChannelStatus;

        Ok(chain
            .stream_channels(ChannelSelector::default().with_source(*chain.me()))?
            .filter(|channel| std::future::ready(channel.status == ChannelStatus::Open))
            .map(|channel| channel.destination)
            .collect()
            .await)
    }

    #[tokio::test]
    async fn a_mode_change_rebuilds_and_the_state_follows() {
        let initial = fake(LifecycleMode::Normal, StrategyState::Running);
        let state = StrategyStateHandle::new(initial.state_handle());
        let builds = Arc::new(AtomicUsize::new(0));
        let (mode, modes) = watch::channel(LifecycleMode::Normal);

        let counted = Arc::clone(&builds);
        let supervisor = tokio::spawn(supervise_lifecycle(
            initial,
            move |mode| {
                counted.fetch_add(1, Ordering::SeqCst);
                Ok(fake(mode, StrategyState::Degraded))
            },
            || std::future::ready(()),
            modes,
            state.clone(),
        ));

        mode.send_replace(LifecycleMode::Reclaim { keep: 2 });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(builds.load(Ordering::SeqCst), 1);
        assert_eq!(state.state(), StrategyState::Degraded);

        mode.send_replace(LifecycleMode::Normal);
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(builds.load(Ordering::SeqCst), 2);

        supervisor.abort();
    }

    #[tokio::test]
    async fn a_rebuild_waits_for_the_writes_of_the_strategy_it_replaces() {
        let initial = fake(LifecycleMode::Normal, StrategyState::Running);
        let state = StrategyStateHandle::new(initial.state_handle());
        let builds = Arc::new(AtomicUsize::new(0));
        let (mode, modes) = watch::channel(LifecycleMode::Normal);
        let (confirm, confirmed) = watch::channel(false);

        let counted = Arc::clone(&builds);
        let supervisor = tokio::spawn(supervise_lifecycle(
            initial,
            move |mode| {
                counted.fetch_add(1, Ordering::SeqCst);
                Ok(fake(mode, StrategyState::Running))
            },
            move || {
                let mut confirmed = confirmed.clone();
                async move {
                    let _ = confirmed.wait_for(|confirmed| *confirmed).await;
                }
            },
            modes,
            state,
        ));

        mode.send_replace(LifecycleMode::Reclaim { keep: 1 });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(builds.load(Ordering::SeqCst), 0);

        confirm.send_replace(true);
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(builds.load(Ordering::SeqCst), 1);

        supervisor.abort();
    }

    #[tokio::test]
    async fn a_failed_rebuild_keeps_the_current_strategy() {
        let initial = fake(LifecycleMode::Normal, StrategyState::Running);
        let state = StrategyStateHandle::new(initial.state_handle());
        let (mode, modes) = watch::channel(LifecycleMode::Normal);

        let supervisor = tokio::spawn(supervise_lifecycle(
            initial,
            |_| {
                Err(hopr_strategy::errors::StrategyError::InvalidConfiguration(
                    "refused".into(),
                ))
            },
            || std::future::ready(()),
            modes,
            state.clone(),
        ));

        mode.send_replace(LifecycleMode::Reclaim { keep: 2 });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(state.state(), StrategyState::Running);
        assert!(!supervisor.is_finished());

        supervisor.abort();
    }

    #[tokio::test]
    async fn without_a_mode_source_the_strategy_just_runs() {
        let initial = fake(LifecycleMode::Normal, StrategyState::Running);
        let state = StrategyStateHandle::new(initial.state_handle());
        let (mode, modes) = watch::channel(LifecycleMode::Normal);
        drop(mode);

        let supervisor = tokio::spawn(supervise_lifecycle(
            initial,
            |_| panic!("nothing to rebuild for"),
            || std::future::ready(()),
            modes,
            state,
        ));
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!supervisor.is_finished());

        supervisor.abort();
    }
}
