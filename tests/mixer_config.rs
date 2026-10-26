use std::time::Duration;

use hopr_lib::config::{HoprLibConfig, MixerConfig};

#[test]
fn mixer_section_round_trips_through_yaml() {
    let mut cfg = HoprLibConfig::default();
    cfg.protocol.mixer = MixerConfig {
        min_delay: Duration::from_millis(1),
        delay_range: Duration::from_millis(25),
        capacity: 2_048,
        ..Default::default()
    };
    let yaml = serde_yaml::to_string(&cfg).unwrap();
    assert!(!yaml.contains("metric_delay_window"));
    let parsed: HoprLibConfig = serde_yaml::from_str(&yaml).unwrap();
    assert_eq!(cfg, parsed);
}

#[test]
fn missing_mixer_section_deserialises_as_default() {
    let parsed: HoprLibConfig = serde_yaml::from_str("{}").unwrap();
    assert_eq!(parsed.protocol.mixer, MixerConfig::default());
}

/// An operator's configured mixer timing must survive startup unchanged
/// when no env-var override is given (regression test for #221).
#[test]
fn resolve_mixer_timings_honors_operator_config_without_overrides() {
    let operator_mixer = MixerConfig {
        min_delay: Duration::from_millis(50),
        delay_range: Duration::from_millis(200),
        ..Default::default()
    };
    let resolved = edgli::resolve_mixer_timings(operator_mixer, None, None);
    assert_eq!(resolved, operator_mixer);
}

/// An explicit env-var override still takes effect when given.
#[test]
fn resolve_mixer_timings_applies_explicit_overrides() {
    let operator_mixer = MixerConfig {
        min_delay: Duration::from_millis(50),
        delay_range: Duration::from_millis(200),
        ..Default::default()
    };
    let resolved = edgli::resolve_mixer_timings(
        operator_mixer,
        Some(Duration::from_millis(5)),
        Some(Duration::from_millis(10)),
    );
    assert_eq!(resolved.min_delay, Duration::from_millis(5));
    assert_eq!(resolved.delay_range, Duration::from_millis(10));
}
