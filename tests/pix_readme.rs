//! The PIX example from the README, compiled and run.
//!
//! An integration test rather than a doctest, for two reasons. The README is not included as crate
//! documentation, so nothing would compile the snippet otherwise — and a documented example is the
//! first thing to rot. And a `tests/` binary is an *external* consumer of `edgli`, so this also
//! checks what only a consumer can hit: that the re-exports resolve, and that
//! `EdgeStrategyKind::Pix` is still constructible from outside despite the enum being
//! `#[non_exhaustive]`. That attribute restricts matching rather than construction, which is worth
//! pinning here, since the opposite would leave PIX documented and unusable.
#![cfg(feature = "pix")]

use edgli::hopr_lib::HoprSessionClientConfig;
use edgli::strategy::{EdgeStrategyKind, IncentiveConfiguration, default_strategy_cfg};
use edgli::{PixEntryConfig, PixEntryStrategy, pix_ssa_quota, quota_per_ssa};

#[test]
fn readme_pix_snippet_compiles_and_runs() -> anyhow::Result<()> {
    // Pay: add the PIX strategy alongside the default channel-lifecycle one. Only the pricing half
    // is set here — `pool` is whichever pool the build selected, and its defaults are fine.
    let mut strategies = default_strategy_cfg(&IncentiveConfiguration::default())?;
    strategies
        .strategies
        .push(EdgeStrategyKind::Pix(PixEntryConfig {
            strategy: PixEntryStrategy {
                price_per_byte: "0.0001 wxHOPR".parse()?,
                max_ssa_allocation: "10 wxHOPR".parse()?,
                ..Default::default()
            },
            ..Default::default()
        }));
    assert_eq!(strategies.strategies.len(), 2);

    // Ask: `Edgli::with_pix` and `Edgli::pix_ssa_quota` need a live node, but both are thin
    // wrappers over the free function below, which is the half that decides the answer.
    let cfg = edgli::hopr_lib::config::HoprLibConfig::default();
    let params = pix_ssa_quota(&cfg)?;
    assert!(
        quota_per_ssa(&params) > 0,
        "the shipped dimensions must price a non-empty quota"
    );

    // The base a caller hands to `with_pix`; every other field of it is passed through.
    let base = HoprSessionClientConfig::default();
    assert!(
        base.pix_ssa_quota.is_none(),
        "PIX must stay opt-in: a default Session announces none"
    );

    Ok(())
}
