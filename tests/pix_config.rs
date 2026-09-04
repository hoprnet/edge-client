//! The Entry-side PIX generator dimensions as they arrive from a config file.
//!
//! Deliberately not gated on `pix-secp256k1`. `protocol.pix` is part of `HoprProtocolConfig` in
//! every build, so a config file carrying it has to survive a build that cannot act on it — an
//! operator who turns the feature on later should not find that their dimensions were being
//! dropped in the meantime.

use hopr_lib::config::HoprLibConfig;
use hopr_lib::exports::transport::config::PixGlobalConfig;

#[test]
fn pix_section_round_trips_through_yaml() {
    let mut cfg = HoprLibConfig::default();
    cfg.protocol.pix = PixGlobalConfig {
        num_ssa_parts: 64,
        ssa_part_size: 16,
        additional_shares: Some(4),
        max_ssas_per_request: 3,
        ..Default::default()
    };

    let yaml = serde_yaml::to_string(&cfg).unwrap();
    let parsed: HoprLibConfig = serde_yaml::from_str(&yaml).unwrap();
    assert_eq!(cfg.protocol.pix, parsed.protocol.pix);
}

#[test]
fn missing_pix_section_deserialises_as_default() {
    let parsed: HoprLibConfig = serde_yaml::from_str("{}").unwrap();
    assert_eq!(parsed.protocol.pix, PixGlobalConfig::default());
}

/// An omitted `additional_shares` has to stay omitted through a write-read cycle rather than
/// being materialised as a number. The field is `Option` because the surplus defaults to a
/// function of `ssa_part_size`, so freezing today's derived value into a config file would
/// silently stop it tracking a later change to the threshold.
#[test]
fn an_unset_surplus_survives_as_unset() {
    let mut cfg = HoprLibConfig::default();
    cfg.protocol.pix.additional_shares = None;

    let parsed: HoprLibConfig =
        serde_yaml::from_str(&serde_yaml::to_string(&cfg).unwrap()).unwrap();
    assert!(parsed.protocol.pix.additional_shares.is_none());
    assert!(
        parsed.protocol.pix.surplus_shares() > 0,
        "the accessor must still derive a surplus from ssa_part_size"
    );
}
