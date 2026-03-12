use std::path::PathBuf;

use async_signal::{Signal, Signals};
use clap::Parser;
use futures::StreamExt;
use hopr_lib::{HoprKeys, IdentityRetrievalModes, config::HoprLibConfig};
use signal_hook::low_level;
use tracing::info;

use edgli::errors::EdgliError;

// Avoid musl's default allocator due to degraded performance
// https://nickb.dev/blog/default-musl-allocator-considered-harmful-to-performance
#[cfg(target_os = "linux")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

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
        required = true
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

    /// Blokli URL
    #[arg(
        long,
        env = "HOPR_EDGE_BLOKLI_URL",
        help = "The URL of the blokli provider to use",
        required = false
    )]
    pub blokli_url: Option<String>,
}

#[cfg_attr(feature = "runtime-tokio", tokio::main)]
async fn main() -> anyhow::Result<()> {
    let args = <CliArgs as clap::Parser>::parse();
    if !args.identity_file_path.exists() {
        return Err(EdgliError::ConfigError(format!(
            "The identity file '{}' does not exist",
            args.identity_file_path.display()
        ))
        .into());
    }

    if !args.config.exists() {
        return Err(EdgliError::ConfigError(format!(
            "The configuration file '{}' does not exist",
            args.config.display()
        ))
        .into());
    }

    let mut cfg: HoprLibConfig = serde_yaml::from_str(&std::fs::read_to_string(args.config)?)?;
    // Edge-client specific mixer defaults (0 ms min, 1 ms range); env vars override.
    let read_ms = |var: &str, default_ms: u64| -> std::time::Duration {
        std::env::var(var)
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .map(std::time::Duration::from_millis)
            .unwrap_or_else(|| std::time::Duration::from_millis(default_ms))
    };
    cfg.protocol.mixer.min_delay = read_ms("HOPR_INTERNAL_MIXER_MINIMUM_DELAY_IN_MS", 0);
    cfg.protocol.mixer.delay_range = read_ms("HOPR_INTERNAL_MIXER_DELAY_RANGE_IN_MS", 1);

    // Find or create an identity
    let hopr_keys: HoprKeys = IdentityRetrievalModes::FromFile {
        password: &args.identity_password,
        id_path: &args.identity_file_path.display().to_string(),
    }
    .try_into()?;

    let _telemetry = edgli::telemetry::init_telemetry(&hopr_keys)?;

    info!(
        version = hopr_lib::constants::APP_VERSION,
        ?cfg,
        "Starting Edgli"
    );

    let edgli = edgli::Edgli::new(cfg, hopr_keys, args.blokli_url, None, |s| {
        info!(?s, "Initialization stage");
    })
    .await?;

    let mut signals =
        Signals::new([Signal::Hup, Signal::Int]).map_err(|e| EdgliError::OsError(e.to_string()))?;
    while let Some(Ok(signal)) = signals.next().await {
        match signal {
            Signal::Hup => {
                info!("Received the HUP signal... not doing anything");
            }
            Signal::Int => {
                info!("Received the INT signal... tearing down the node");
                // Dropping Edgli aborts all background tasks via AbortableList.
                drop(edgli);
                info!("All processes stopped... emulating the default handler...");
                low_level::emulate_default_handler(signal as i32)?;
                break;
            }
            _ => low_level::emulate_default_handler(signal as i32)?,
        }
    }

    Ok(())
}
