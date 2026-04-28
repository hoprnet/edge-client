#[cfg(feature = "runtime-tokio")]
pub mod client;
pub mod errors;

#[cfg(feature = "blokli")]
pub mod blokli;

pub mod strategy;
pub mod traits;

pub use hopr_lib;

#[cfg(feature = "blokli")]
pub use blokli::*;
pub use hopr_chain_connector::BlockchainConnectorConfig;

#[cfg(feature = "runtime-tokio")]
pub use client::*;
pub use traits::{EdgeNodeApi, NodeBalances};

// Re-export types that appear in EdgeNodeApi method signatures so consumers
// do not need to dig into hopr_lib internal module paths.
pub use hopr_lib::api::types::{
    internal::channels::ChannelEntry,
    primitive::prelude::{Balance, XDai},
};

#[cfg(feature = "telemetry")]
pub use hopr_lib::collect_hopr_metrics;
