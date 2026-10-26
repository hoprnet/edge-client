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
