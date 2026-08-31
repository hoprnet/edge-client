use std::time::Duration;

use edgli::latency_path_planner_config;

/// Value guard for the relaxed return-path routing profile: a soft `100 ms` latency half-life with
/// the diversity cap disabled (`min_paths_anonymity_floor == 0`). Guards against silently
/// reintroducing the former hard `20 ms` / floor `2` cap, which pruned every relayer but the two
/// fastest and left the degradation detector without a sibling to corroborate a dead relayer against
/// — the shape of a 2026-08-28 tunnel outage. The behavioural proof that floor `0` keeps a sibling
/// end-to-end lives where the pruning does, in hopr-lib.
#[test]
fn latency_path_planner_config_keeps_all_relayers_with_soft_latency() {
    let cfg = latency_path_planner_config(0.1);

    assert_eq!(
        Duration::from_millis(100),
        cfg.latency_halflife,
        "latency stays a soft weight: a ~100 ms one-hop return path stays in contention",
    );
    assert_eq!(
        0, cfg.min_paths_anonymity_floor,
        "the diversity cap must stay disabled so every reachable relayer is kept",
    );
    assert_eq!(
        0.1, cfg.min_ack_rate,
        "min_ack_rate is threaded through from the argument"
    );
}
