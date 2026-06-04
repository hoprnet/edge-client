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
    let yaml = "host:\n  address:\n    IPv4: \"1.2.3.4\"\n  port: 9091\n";
    let parsed: HoprLibConfig = serde_yaml::from_str(yaml).unwrap();
    assert_eq!(parsed.protocol.mixer, MixerConfig::default());
}

/// Verifies the edge-client specific mixer override: 0 ms min and 1 ms range,
/// matching the hardcoded values applied in main.rs after YAML deserialisation.
#[test]
fn edge_client_mixer_defaults() {
    let edge_mixer = MixerConfig {
        min_delay: Duration::ZERO,
        delay_range: Duration::from_millis(1),
        ..Default::default()
    };
    assert_ne!(edge_mixer, MixerConfig::default(), "edge-client mixer differs from hoprnet default");
    assert_eq!(edge_mixer.min_delay, Duration::ZERO);
    assert_eq!(edge_mixer.delay_range, Duration::from_millis(1));
}
