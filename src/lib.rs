pub mod errors;

use std::{fmt::Formatter, path::PathBuf, str::FromStr};

use clap::Parser;
use futures::{StreamExt, future::AbortHandle};
use async_signal::{Signal, Signals};
use hopr_lib::{config::HoprLibConfig, HoprKeys, HoprLibProcesses, ToHex};
use signal_hook::low_level;
use tracing::info;

use crate::errors::EdgliError;


/// Takes all CLI arguments whose structure is known at compile-time.
/// Arguments whose structure, e.g. their default values depend on
/// file contents need be specified using `clap`s builder API
#[derive(Clone, Parser)]
#[command(author, version, about, long_about = None)]
pub struct CliArgs {
    /// Identity file password
    #[arg(
        long,
        env = "HOPR_EDGE_IDENTITY_FILE_PASSWORD",
        help = "Password for the identity file provided",
        required = true
    )]
    pub identity_password: String,

    /// Identity file path
    #[arg(
        long,
        env = "HOPR_EDGE_IDENTITY_FILE_PATH",
        help = "The path to the identity file to use",
        required = true,
    )]
    pub identity_file_path: PathBuf,

    /// HOPR configuration file path
    #[arg(
        long,
        env = "HOPR_EDGE_CONFIG_FILE_PATH",
        help = "The path to the configuration path for the HOPR client",
        required = true
    )]
    pub config: PathBuf,
}

enum EdgliProcesses {
    HoprLib(HoprLibProcesses, AbortHandle),
}

// Manual implementation needed, since Strum does not support skipping arguments
impl std::fmt::Display for EdgliProcesses {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            EdgliProcesses::HoprLib(p, _) => write!(f, "HoprLib process: {p}"),
        }
    }
}

impl std::fmt::Debug for EdgliProcesses {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        // Intentionally same as Display
        write!(f, "{self}")
    }
}

pub async fn run_hopr_edge_node(cfg: HoprLibConfig, hopr_keys: HoprKeys) -> anyhow::Result<()> {
    if let hopr_lib::HostType::IPv4(address) = &cfg.host.address {
        let ipv4 = std::net::Ipv4Addr::from_str(address).map_err(|e| EdgliError::ConfigError(e.to_string()))?;

        if ipv4.is_loopback() && !cfg.transport.announce_local_addresses {
            Err(hopr_lib::errors::HoprLibError::GeneralError(
                "Cannot announce a loopback address".into(),
            ))?;
        }
    }

    info!(
        packet_key = hopr_lib::Keypair::public(&hopr_keys.packet_key).to_peerid_str(),
        blockchain_address = hopr_lib::Keypair::public(&hopr_keys.chain_key).to_address().to_hex(),
        "Node public identifiers"
    );

    // Create the node instance
    info!("Creating the HOPR edge node instance from hopr-lib");
    let node = hopr_lib::Hopr::new(
        cfg.clone(),
        &hopr_keys.packet_key,
        &hopr_keys.chain_key,
    )?;

    let mut processes: Vec<EdgliProcesses> = Vec::new();

    let (_hopr_socket, hopr_processes) = node
        .run()
        .await?;

    processes.extend(hopr_processes.into_iter().map(|(k, v)| EdgliProcesses::HoprLib(k, v)));

    let mut signals = Signals::new([Signal::Hup, Signal::Int]).map_err(|e| EdgliError::OsError(e.to_string()))?;
    while let Some(Ok(signal)) = signals.next().await {
        match signal {
            Signal::Hup => {
                info!("Received the HUP signal... not doing anything");
            }
            Signal::Int => {
                info!("Received the INT signal... tearing down the node");
                futures::stream::iter(processes)
                    .then(|process| async move {
                        let mut abort_handles: Vec<AbortHandle> = Vec::new();
                        info!("Stopping process '{process}'");
                        match process {
                            EdgliProcesses::HoprLib(_, ah) => abort_handles.push(ah),
                        }
                        futures::stream::iter(abort_handles)
                    })
                    .flatten()
                    .for_each_concurrent(None, |ah| async move { ah.abort() })
                    .await;

                info!("All processes stopped... emulating the default handler...");
                low_level::emulate_default_handler(signal as i32)?;
                info!("Shutting down!");
                break;
            }
            _ => low_level::emulate_default_handler(signal as i32)?,
        }
    }

    Ok(())
}
