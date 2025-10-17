use std::path::PathBuf;

use async_signal::{Signal, Signals};
use clap::Parser;
use futures::{future::AbortHandle, StreamExt};
use hopr_lib::{config::HoprLibConfig, HoprKeys, IdentityRetrievalModes};
use signal_hook::low_level;
use tracing::{info, warn};
use tracing_subscriber::prelude::*;

#[cfg(feature = "telemetry")]
use {
    opentelemetry::trace::TracerProvider,
    opentelemetry_otlp::WithExportConfig as _,
    opentelemetry_sdk::trace::{RandomIdGenerator, Sampler},
};

use edgli::{
    //cli::CliArgs,
    errors::EdgliError,
    EdgliProcesses,
};

// Avoid musl's default allocator due to degraded performance
// https://nickb.dev/blog/default-musl-allocator-considered-harmful-to-performance
#[cfg(target_os = "linux")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimallo::MiMalloc;

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
}

fn init_logger() -> anyhow::Result<()> {
    let env_filter = match tracing_subscriber::EnvFilter::try_from_default_env() {
        Ok(filter) => filter,
        Err(_) => tracing_subscriber::filter::EnvFilter::new("info")
            .add_directive("libp2p_swarm=info".parse()?)
            .add_directive("libp2p_mplex=info".parse()?)
            .add_directive("libp2p_tcp=info".parse()?)
            .add_directive("libp2p_dns=info".parse()?)
            .add_directive("multistream_select=info".parse()?)
            .add_directive("isahc=error".parse()?)
            .add_directive("sea_orm=warn".parse()?)
            .add_directive("sqlx=warn".parse()?)
            .add_directive("hyper_util=warn".parse()?),
    };

    #[cfg(feature = "prof")]
    let registry = tracing_subscriber::Registry::default()
        .with(
            env_filter
                .add_directive("tokio=trace".parse()?)
                .add_directive("runtime=trace".parse()?),
        )
        .with(console_subscriber::spawn());

    #[cfg(not(feature = "prof"))]
    let registry = tracing_subscriber::Registry::default().with(env_filter);

    let format = tracing_subscriber::fmt::layer()
        .with_level(true)
        .with_target(true)
        .with_thread_ids(true)
        .with_thread_names(false);

    let format = if std::env::var("HOPRD_LOG_FORMAT")
        .map(|v| v.to_lowercase() == "json")
        .unwrap_or(false)
    {
        format.json().boxed()
    } else {
        format.boxed()
    };

    let registry = registry.with(format);

    cfg_if::cfg_if! {
        if #[cfg(feature = "telemetry")] {

            if std::env::var("HOPR_EDGE_USE_OPENTELEMETRY").map(v == "true").unwrap_or(false) {
            let exporter = opentelemetry_otlp::SpanExporter::builder()
                .with_tonic()
                .with_protocol(opentelemetry_otlp::Protocol::Grpc)
                .with_timeout(std::time::Duration::from_secs(5))
                .build()?;

            let tracer = opentelemetry_sdk::trace::SdkTracerProvider::builder()
                .with_batch_exporter(exporter)
                .with_sampler(Sampler::AlwaysOn)
                .with_id_generator(RandomIdGenerator::default())
                .with_max_events_per_span(64)
                .with_max_attributes_per_span(16)
                .with_resource(
                    opentelemetry_sdk::Resource::builder()
                        .with_service_name(
                            std::env::var("OTEL_SERVICE_NAME").unwrap_or(env!("CARGO_PKG_NAME").into()),
                        )
                        .build(),
                )
                .build()
                .tracer(env!("CARGO_PKG_NAME"));

                tracing::subscriber::set_global_default(registry.with(racing_opentelemetry::layer().with_tracer(tracer)))?}

            else {
                tracing::subscriber::set_global_default(registry)?
            }
        }
        else {
            tracing::subscriber::set_global_default(registry)?
        }
    }

    Ok(())
}

#[cfg_attr(feature = "runtime-tokio", tokio::main)]
async fn main() -> anyhow::Result<()> {
    init_logger()?;

    if cfg!(debug_assertions) {
        warn!("Executable was built using the DEBUG profile.");
    } else {
        info!("Executable was built using the RELEASE profile.");
    }

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
            args.identity_file_path.display()
        ))
        .into());
    }

    let cfg: HoprLibConfig = serde_yaml::from_str(&std::fs::read_to_string(args.config)?)?;

    // Find or create an identity
    let hopr_keys: HoprKeys = IdentityRetrievalModes::FromFile {
        password: &args.identity_password,
        id_path: &args.identity_file_path.display().to_string(),
    }
    .try_into()?;

    let git_hash = option_env!("VERGEN_GIT_SHA").unwrap_or("unknown");
    info!(
        version = hopr_lib::constants::APP_VERSION,
        hash = git_hash,
        ?cfg,
        "Starting Edgli"
    );

    // TODO: not doing anything much, an edge node without the possibility of externally calling it.
    //
    // Pending decision on future interfaces (e.g. REST, gRPC,...)
    let (_hopr, processes) = edgli::run_hopr_edge_node(cfg, hopr_keys).await?;
    let processes = processes.await?;

    let mut signals =
        Signals::new([Signal::Hup, Signal::Int]).map_err(|e| EdgliError::OsError(e.to_string()))?;
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
                            EdgliProcesses::Hopr(ah) => abort_handles.push(ah),
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
