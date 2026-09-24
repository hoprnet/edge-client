#[cfg(feature = "telemetry")]
use tracing_subscriber::prelude::*;

#[cfg(feature = "telemetry")]
use std::{collections::HashMap, str::FromStr, time::Duration};

use hopr_lib::{HoprKeys, api::types::primitive::traits::ToHex, builder::Keypair};
#[cfg(feature = "telemetry")]
use opentelemetry::{
    Key, KeyValue,
    logs::{AnyValue, LogRecord as _, Logger as _, LoggerProvider as _, Severity},
    trace::TracerProvider,
};
#[cfg(feature = "telemetry")]
use opentelemetry_otlp::WithExportConfig as _;
#[cfg(feature = "telemetry")]
use opentelemetry_sdk::{
    logs::{SdkLogger, SdkLoggerProvider},
    metrics::SdkMeterProvider,
    trace::{RandomIdGenerator, Sampler, SdkTracerProvider},
};
#[cfg(feature = "telemetry")]
use tracing::field::{Field, Visit};

// User-facing OTLP configuration environment variables. These mirror the
// `HOPRD_*` variables used by `hoprd` so the two share the same operator flow,
// minus the explicit enable flag: setting an endpoint is enough to enable
// export here.
#[cfg(feature = "telemetry")]
const EDGE_OTEL_SIGNALS_ENV: &str = "EDGE_OTEL_SIGNALS";
// User-facing endpoint. Mirrors `HOPRD_OTLP_ENDPOINT`; at startup it is copied
// into the standard `OTEL_EXPORTER_OTLP_ENDPOINT` that the OTLP SDK reads.
#[cfg(feature = "telemetry")]
const EDGE_OTLP_ENDPOINT_ENV: &str = "EDGE_OTLP_ENDPOINT";
// Standard OTLP endpoint honoured by the SDK; also accepted as a legacy fallback.
#[cfg(feature = "telemetry")]
const LEGACY_OTLP_ENDPOINT_ENV: &str = "OTEL_EXPORTER_OTLP_ENDPOINT";
// Default metric export cadence override (single duration; `ms` integer or
// `ms`/`s`/`m` suffixes). Mirrors the default-interval part of
// `HOPRD_METRIC_EXPORT_INTERVAL` without the per-prefix overrides.
#[cfg(feature = "telemetry")]
const EDGE_METRIC_EXPORT_INTERVAL_ENV: &str = "EDGE_METRIC_EXPORT_INTERVAL";
#[cfg(feature = "telemetry")]
const OTEL_SERVICE_NAME_ENV: &str = "OTEL_SERVICE_NAME";

#[cfg_attr(not(feature = "telemetry"), allow(dead_code))]
#[derive(Clone, Debug)]
struct TelemetryIdentity {
    node_address: String,
    node_peer_id: String,
    extra_labels: Vec<(String, String)>,
}

impl TelemetryIdentity {
    fn from_hopr_keys_with_labels<K, V>(
        hopr_keys: &HoprKeys,
        extra_labels: impl IntoIterator<Item = (K, V)>,
    ) -> Self
    where
        K: Into<String>,
        V: Into<String>,
    {
        let node_address = Keypair::public(&hopr_keys.chain_key).to_address().to_hex();
        let node_peer_id = Keypair::public(&hopr_keys.packet_key).to_peerid_str();
        let extra_labels = extra_labels
            .into_iter()
            .map(|(k, v)| (k.into(), v.into()))
            .collect();
        Self {
            node_address,
            node_peer_id,
            extra_labels,
        }
    }
}

#[cfg(feature = "telemetry")]
impl TelemetryIdentity {
    fn resource_attributes(&self) -> Vec<KeyValue> {
        let mut attrs = vec![
            KeyValue::new("node_address", self.node_address.clone()),
            KeyValue::new("node_peer_id", self.node_peer_id.clone()),
        ];
        for (k, v) in &self.extra_labels {
            attrs.push(KeyValue::new(k.clone(), v.clone()));
        }
        attrs
    }
}

#[cfg(feature = "telemetry")]
flagset::flags! {
    #[repr(u8)]
    #[derive(PartialOrd, Ord, strum::EnumString, strum::Display)]
    pub enum OtlpSignal: u8 {
        #[strum(serialize = "traces")]
        Traces = 0b0000_0001,

        #[strum(serialize = "logs")]
        Logs = 0b0000_0010,

        #[strum(serialize = "metrics")]
        Metrics = 0b0000_0100,
    }
}

#[cfg(feature = "telemetry")]
#[derive(Clone, Copy, Debug, Eq, PartialEq, strum::EnumString, strum::Display)]
pub enum OtlpTransport {
    #[strum(serialize = "grpc")]
    Grpc,

    #[strum(serialize = "http", serialize = "https")]
    Http,
}

#[cfg(feature = "telemetry")]
impl OtlpTransport {
    fn from_endpoint(endpoint: Option<&str>) -> Self {
        endpoint
            .and_then(|raw_url| {
                Self::from_str(
                    raw_url
                        .trim()
                        .split_once("://")
                        .map(|(scheme, _)| scheme)
                        .unwrap_or(""),
                )
                .ok()
            })
            .unwrap_or(Self::Grpc)
    }
}

/// Copies the user-facing [`EDGE_OTLP_ENDPOINT_ENV`] into the standard
/// [`LEGACY_OTLP_ENDPOINT_ENV`] that the OTLP SDK reads, so operators only ever
/// configure the `EDGE_`-prefixed variable. Mirrors `hoprd`'s behaviour: if both
/// are set and differ, the `EDGE_`-prefixed value wins.
#[cfg(feature = "telemetry")]
fn apply_edge_otlp_endpoint_override() {
    let Ok(value) = std::env::var(EDGE_OTLP_ENDPOINT_ENV) else {
        return;
    };

    let endpoint = value.trim();
    if endpoint.is_empty() {
        tracing::warn!(
            env_key = EDGE_OTLP_ENDPOINT_ENV,
            "empty OTLP endpoint value ignored"
        );
        return;
    }

    if let Ok(existing) = std::env::var(LEGACY_OTLP_ENDPOINT_ENV) {
        let existing = existing.trim();
        if !existing.is_empty() && existing != endpoint {
            tracing::warn!(
                env_key = EDGE_OTLP_ENDPOINT_ENV,
                overridden_env_key = LEGACY_OTLP_ENDPOINT_ENV,
                "custom EDGE OTLP endpoint overrides OTEL exporter endpoint"
            );
        }
    }

    unsafe { std::env::set_var(LEGACY_OTLP_ENDPOINT_ENV, endpoint) };
}

/// Resolves the service name from [`OTEL_SERVICE_NAME_ENV`], falling back to the
/// crate name when unset or blank.
#[cfg(feature = "telemetry")]
fn resolve_service_name() -> String {
    match std::env::var(OTEL_SERVICE_NAME_ENV) {
        Ok(service_name) if !service_name.trim().is_empty() => service_name.trim().to_string(),
        _ => env!("CARGO_PKG_NAME").to_string(),
    }
}

/// Parses a single export-interval value: a bare integer is milliseconds, or an
/// integer with a `ms`/`s`/`m` suffix. Returns `None` for empty, zero, or
/// unparseable input.
#[cfg(feature = "telemetry")]
fn parse_export_interval(value: &str) -> Option<Duration> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Ok(ms) = trimmed.parse::<u64>() {
        return (ms != 0).then(|| Duration::from_millis(ms));
    }

    let normalized = trimmed.to_ascii_lowercase();
    if let Some(ms) = normalized
        .strip_suffix("ms")
        .and_then(|v| v.trim().parse::<u64>().ok())
    {
        return (ms != 0).then(|| Duration::from_millis(ms));
    }
    if let Some(secs) = normalized
        .strip_suffix('s')
        .and_then(|v| v.trim().parse::<u64>().ok())
    {
        return (secs != 0).then(|| Duration::from_secs(secs));
    }
    if let Some(mins) = normalized
        .strip_suffix('m')
        .and_then(|v| v.trim().parse::<u64>().ok())
    {
        return (mins != 0).then(|| Duration::from_secs(mins.saturating_mul(60)));
    }

    None
}

#[cfg(feature = "telemetry")]
#[derive(Debug, Clone)]
pub struct OtlpConfig {
    pub enabled: bool,
    pub service_name: String,
    pub transport: OtlpTransport,
    pub signals: flagset::FlagSet<OtlpSignal>,
    metric_export_interval: Option<Duration>,
    invalid_signals: Vec<String>,
}

#[cfg(feature = "telemetry")]
impl OtlpConfig {
    /// Builds the config from the environment. The user-facing
    /// [`EDGE_OTLP_ENDPOINT_ENV`] must already have been folded into
    /// [`LEGACY_OTLP_ENDPOINT_ENV`] via [`apply_edge_otlp_endpoint_override`]
    /// before calling this. Export is enabled whenever a non-empty endpoint is
    /// present — there is no separate enable flag.
    pub fn from_env() -> Self {
        let service_name = resolve_service_name();
        let otlp_endpoint = std::env::var(LEGACY_OTLP_ENDPOINT_ENV)
            .ok()
            .map(|endpoint| endpoint.trim().to_string())
            .filter(|endpoint| !endpoint.is_empty());
        let transport = OtlpTransport::from_endpoint(otlp_endpoint.as_deref());
        let enabled = otlp_endpoint.is_some();

        let mut signals = flagset::FlagSet::empty();
        let mut invalid_signals = Vec::new();
        // Distinguish "unset" (default to traces, matching hoprd) from "set but
        // empty/invalid" (also falls back to traces via the guard below).
        match std::env::var(EDGE_OTEL_SIGNALS_ENV) {
            Ok(raw_signals) => {
                for signal in raw_signals.split(',') {
                    let signal = signal.trim();
                    if signal.is_empty() {
                        continue;
                    }
                    match OtlpSignal::from_str(signal) {
                        Ok(parsed) => signals |= parsed,
                        Err(_) => invalid_signals.push(signal.to_string()),
                    }
                }
            }
            Err(_) => signals |= OtlpSignal::Traces,
        }

        if signals.is_empty() {
            signals |= OtlpSignal::Traces;
        }

        let metric_export_interval = std::env::var(EDGE_METRIC_EXPORT_INTERVAL_ENV)
            .ok()
            .and_then(|raw| parse_export_interval(&raw));

        Self {
            enabled,
            service_name,
            transport,
            signals,
            metric_export_interval,
            invalid_signals,
        }
    }

    fn has_signal(&self, signal: OtlpSignal) -> bool {
        self.signals.contains(signal)
    }
}

#[cfg(feature = "telemetry")]
#[derive(Clone)]
struct OtelLogsLayer {
    logger: SdkLogger,
}

#[cfg(feature = "telemetry")]
impl OtelLogsLayer {
    fn new(logger: SdkLogger) -> Self {
        Self { logger }
    }
}

#[cfg(feature = "telemetry")]
impl<S> tracing_subscriber::Layer<S> for OtelLogsLayer
where
    S: tracing::Subscriber,
{
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let metadata = event.metadata();
        let mut visitor = TracingEventVisitor::default();
        event.record(&mut visitor);

        let mut record = self.logger.create_log_record();
        let event_timestamp = visitor.timestamp.unwrap_or(std::time::SystemTime::now());

        let (severity_number, severity_text) = match *metadata.level() {
            tracing::Level::ERROR => (Severity::Error, "ERROR"),
            tracing::Level::WARN => (Severity::Warn, "WARN"),
            tracing::Level::INFO => (Severity::Info, "INFO"),
            tracing::Level::DEBUG => (Severity::Debug, "DEBUG"),
            tracing::Level::TRACE => (Severity::Trace, "TRACE"),
        };

        record.set_timestamp(event_timestamp);
        record.set_observed_timestamp(event_timestamp);
        record.set_target(metadata.target().to_string());
        record.set_severity_number(severity_number);
        record.set_severity_text(severity_text);

        if let Some(message) = visitor.body.take() {
            let body = HashMap::from([(Key::new("message"), AnyValue::String(message.into()))]);
            record.set_body(AnyValue::Map(Box::new(body)));
        }
        if let Some(module_path) = metadata.module_path() {
            record.add_attribute("module_path", module_path.to_string());
        }
        if let Some(file) = metadata.file() {
            record.add_attribute("file", file.to_string());
        }
        if let Some(line) = metadata.line() {
            record.add_attribute("line", i64::from(line));
        }

        if !visitor.attributes.is_empty() {
            record.add_attributes(visitor.attributes);
        }

        self.logger.emit(record);
    }
}

#[cfg(feature = "telemetry")]
#[derive(Default)]
struct TracingEventVisitor {
    body: Option<String>,
    attributes: Vec<(String, AnyValue)>,
    timestamp: Option<std::time::SystemTime>,
}

#[cfg(feature = "telemetry")]
impl TracingEventVisitor {
    fn record_body_or_attribute<V>(&mut self, field: &Field, value: V)
    where
        V: Into<AnyValue> + ToString,
    {
        if field.name() == "message" {
            self.body = Some(value.to_string());
        } else {
            self.attributes
                .push((field.name().to_string(), value.into()));
        }
    }

    fn maybe_record_unix_timestamp_millis(&mut self, field: &Field, value: u64) {
        if field.name() == "timestamp" && self.timestamp.is_none() {
            self.timestamp =
                std::time::UNIX_EPOCH.checked_add(std::time::Duration::from_millis(value));
        }
    }
}

#[cfg(feature = "telemetry")]
impl Visit for TracingEventVisitor {
    fn record_i64(&mut self, field: &Field, value: i64) {
        if let Ok(u) = u64::try_from(value) {
            self.maybe_record_unix_timestamp_millis(field, u);
        }
        if field.name() != "timestamp" {
            self.record_body_or_attribute(field, value);
        }
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.maybe_record_unix_timestamp_millis(field, value);
        if field.name() != "timestamp" {
            if value <= i64::MAX as u64 {
                self.record_body_or_attribute(field, value as i64);
            } else {
                self.record_body_or_attribute(field, value.to_string());
            }
        }
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.record_body_or_attribute(field, value);
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.record_body_or_attribute(field, value.to_string());
    }

    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        self.record_body_or_attribute(field, value.to_string());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.record_body_or_attribute(field, format!("{value:?}"));
    }
}

#[derive(Default)]
pub struct TelemetryHandles {
    #[cfg(feature = "telemetry")]
    tracer_provider: Option<SdkTracerProvider>,
    #[cfg(feature = "telemetry")]
    logger_provider: Option<SdkLoggerProvider>,
    #[cfg(feature = "telemetry")]
    meter_provider: Option<SdkMeterProvider>,
}

impl Drop for TelemetryHandles {
    fn drop(&mut self) {
        #[cfg(feature = "telemetry")]
        if let Some(tracer_provider) = self.tracer_provider.take() {
            let _ = tracer_provider.shutdown();
        }
        #[cfg(feature = "telemetry")]
        if let Some(logger_provider) = self.logger_provider.take() {
            let _ = logger_provider.shutdown();
        }
        #[cfg(feature = "telemetry")]
        if let Some(meter_provider) = self.meter_provider.take() {
            let _ = meter_provider.shutdown();
        }
    }
}

#[cfg(feature = "telemetry")]
fn build_otel_resource(
    config: &OtlpConfig,
    node_identity: &TelemetryIdentity,
) -> opentelemetry_sdk::Resource {
    opentelemetry_sdk::Resource::builder()
        .with_service_name(config.service_name.clone())
        .with_attributes(node_identity.resource_attributes())
        .build()
}

#[cfg(feature = "telemetry")]
fn enabled_signal_names(config: &OtlpConfig, signals: &[OtlpSignal]) -> String {
    signals
        .iter()
        .copied()
        .filter(|signal| config.signals.contains(*signal))
        .map(|signal| signal.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

pub fn init_logging(hopr_keys: &HoprKeys) -> anyhow::Result<TelemetryHandles> {
    init_logging_with_extra_labels(hopr_keys, Vec::<(String, String)>::new())
}

pub fn init_logging_with_extra_labels<K, V>(
    hopr_keys: &HoprKeys,
    extra_labels: impl IntoIterator<Item = (K, V)>,
) -> anyhow::Result<TelemetryHandles>
where
    K: Into<String>,
    V: Into<String>,
{
    init_logging_with_identity(TelemetryIdentity::from_hopr_keys_with_labels(
        hopr_keys,
        extra_labels,
    ))
}

fn init_logging_with_identity(
    node_identity: TelemetryIdentity,
) -> anyhow::Result<TelemetryHandles> {
    #[cfg(feature = "telemetry")]
    {
        let mut telemetry_handles = TelemetryHandles::default();
        let registry = crate::telemetry_common::build_base_subscriber()?;
        apply_edge_otlp_endpoint_override();
        let config = OtlpConfig::from_env();

        if config.enabled {
            let resource = build_otel_resource(&config, &node_identity);

            let trace_layer = if config.has_signal(OtlpSignal::Traces) {
                let exporter = match config.transport {
                    OtlpTransport::Grpc => opentelemetry_otlp::SpanExporter::builder()
                        .with_tonic()
                        .with_protocol(opentelemetry_otlp::Protocol::Grpc)
                        .with_timeout(Duration::from_secs(5))
                        .build()?,
                    OtlpTransport::Http => opentelemetry_otlp::SpanExporter::builder()
                        .with_http()
                        .with_protocol(opentelemetry_otlp::Protocol::HttpBinary)
                        .with_timeout(Duration::from_secs(5))
                        .build()?,
                };
                let batch_processor =
                    opentelemetry_sdk::trace::span_processor_with_async_runtime::BatchSpanProcessor::builder(
                        exporter,
                        opentelemetry_sdk::runtime::Tokio,
                    )
                    .build();
                let tracer_provider = SdkTracerProvider::builder()
                    .with_span_processor(batch_processor)
                    .with_sampler(Sampler::AlwaysOn)
                    .with_id_generator(RandomIdGenerator::default())
                    .with_max_events_per_span(64)
                    .with_max_attributes_per_span(16)
                    .with_resource(resource.clone())
                    .build();
                let tracer = tracer_provider.tracer(env!("CARGO_PKG_NAME"));
                telemetry_handles.tracer_provider = Some(tracer_provider);
                Some(tracing_opentelemetry::layer().with_tracer(tracer))
            } else {
                None
            };

            let logs_layer = if config.has_signal(OtlpSignal::Logs) {
                let exporter = match config.transport {
                    OtlpTransport::Grpc => opentelemetry_otlp::LogExporter::builder()
                        .with_tonic()
                        .with_protocol(opentelemetry_otlp::Protocol::Grpc)
                        .with_timeout(Duration::from_secs(5))
                        .build()?,
                    OtlpTransport::Http => opentelemetry_otlp::LogExporter::builder()
                        .with_http()
                        .with_protocol(opentelemetry_otlp::Protocol::HttpBinary)
                        .with_timeout(Duration::from_secs(5))
                        .build()?,
                };

                let batch_processor =
                    opentelemetry_sdk::logs::log_processor_with_async_runtime::BatchLogProcessor::builder(
                        exporter,
                        opentelemetry_sdk::runtime::Tokio,
                    )
                    .build();
                let logger_provider = SdkLoggerProvider::builder()
                    .with_log_processor(batch_processor)
                    .with_resource(resource.clone())
                    .build();
                let logger = logger_provider.logger(env!("CARGO_PKG_NAME"));
                telemetry_handles.logger_provider = Some(logger_provider);
                Some(OtelLogsLayer::new(logger))
            } else {
                None
            };
            let enabled_signals =
                enabled_signal_names(&config, &[OtlpSignal::Traces, OtlpSignal::Logs]);
            let metrics_requested = config.has_signal(OtlpSignal::Metrics);

            match (trace_layer, logs_layer) {
                (Some(trace_layer), Some(logs_layer)) => tracing::subscriber::set_global_default(
                    registry.with(trace_layer).with(logs_layer),
                )?,
                (Some(trace_layer), None) => {
                    tracing::subscriber::set_global_default(registry.with(trace_layer))?
                }
                (None, Some(logs_layer)) => {
                    tracing::subscriber::set_global_default(registry.with(logs_layer))?
                }
                (None, None) => tracing::subscriber::set_global_default(registry)?,
            }

            tracing::info!(
                otel_service_name = %config.service_name,
                otel_signals = %enabled_signals,
                otel_metrics_deferred = metrics_requested,
                otel_protocol = %config.transport.to_string(),
                node_address = %node_identity.node_address,
                node_peer_id = %node_identity.node_peer_id,
                "OpenTelemetry initialized"
            );
        } else {
            tracing::subscriber::set_global_default(registry)?;
        }

        for bad in &config.invalid_signals {
            tracing::warn!(
                otel_signal = %bad,
                "Invalid OpenTelemetry signal in EDGE_OTEL_SIGNALS; ignored"
            );
        }

        Ok(telemetry_handles)
    }
    #[cfg(not(feature = "telemetry"))]
    {
        let _ = node_identity;
        let registry = crate::telemetry_common::build_base_subscriber()?;
        tracing::subscriber::set_global_default(registry)?;
        Ok(TelemetryHandles::default())
    }
}

pub fn init_base_logging() -> anyhow::Result<TelemetryHandles> {
    let registry = crate::telemetry_common::build_base_subscriber()?;
    tracing::subscriber::set_global_default(registry)?;
    Ok(TelemetryHandles::default())
}

pub fn init_metrics(
    telemetry_handles: &mut TelemetryHandles,
    hopr_keys: &HoprKeys,
) -> anyhow::Result<()> {
    init_metrics_with_extra_labels(telemetry_handles, hopr_keys, Vec::<(String, String)>::new())
}

pub fn init_metrics_with_extra_labels<K, V>(
    telemetry_handles: &mut TelemetryHandles,
    hopr_keys: &HoprKeys,
    extra_labels: impl IntoIterator<Item = (K, V)>,
) -> anyhow::Result<()>
where
    K: Into<String>,
    V: Into<String>,
{
    init_metrics_with_identity(
        telemetry_handles,
        TelemetryIdentity::from_hopr_keys_with_labels(hopr_keys, extra_labels),
    )
}

fn init_metrics_with_identity(
    telemetry_handles: &mut TelemetryHandles,
    node_identity: TelemetryIdentity,
) -> anyhow::Result<()> {
    #[cfg(feature = "telemetry")]
    {
        if telemetry_handles.meter_provider.is_some() {
            return Ok(());
        }

        apply_edge_otlp_endpoint_override();
        let config = OtlpConfig::from_env();
        if !config.enabled || !config.has_signal(OtlpSignal::Metrics) {
            return Ok(());
        }

        let resource = build_otel_resource(&config, &node_identity);
        let exporter = match config.transport {
            OtlpTransport::Grpc => opentelemetry_otlp::MetricExporter::builder()
                .with_tonic()
                .with_protocol(opentelemetry_otlp::Protocol::Grpc)
                .with_timeout(Duration::from_secs(5))
                .build()?,
            OtlpTransport::Http => opentelemetry_otlp::MetricExporter::builder()
                .with_http()
                .with_protocol(opentelemetry_otlp::Protocol::HttpBinary)
                .with_timeout(Duration::from_secs(5))
                .build()?,
        };

        let mut reader_builder =
            opentelemetry_sdk::metrics::periodic_reader_with_async_runtime::PeriodicReader::builder(
                exporter,
                opentelemetry_sdk::runtime::Tokio,
            );
        if let Some(interval) = config.metric_export_interval {
            reader_builder = reader_builder.with_interval(interval);
        }
        let reader = reader_builder.build();
        let meter_provider = SdkMeterProvider::builder()
            .with_reader(reader)
            .with_resource(resource)
            .build();
        opentelemetry::global::set_meter_provider(meter_provider.clone());
        telemetry_handles.meter_provider = Some(meter_provider);

        let enabled_signals = enabled_signal_names(&config, &[OtlpSignal::Metrics]);
        let metric_export_interval_ms = config
            .metric_export_interval
            .unwrap_or(Duration::from_secs(60))
            .as_millis() as u64;
        tracing::info!(
            otel_service_name = %config.service_name,
            otel_signals = %enabled_signals,
            otel_protocol = %config.transport.to_string(),
            otel_metric_export_interval_ms = metric_export_interval_ms,
            node_address = %node_identity.node_address,
            node_peer_id = %node_identity.node_peer_id,
            "OpenTelemetry metrics initialized"
        );

        Ok(())
    }
    #[cfg(not(feature = "telemetry"))]
    {
        let _ = telemetry_handles;
        let _ = node_identity;
        Ok(())
    }
}

pub fn init_telemetry(hopr_keys: &HoprKeys) -> anyhow::Result<TelemetryHandles> {
    init_telemetry_with_extra_labels(hopr_keys, Vec::<(String, String)>::new())
}

pub fn init_telemetry_with_extra_labels<K, V>(
    hopr_keys: &HoprKeys,
    extra_labels: impl IntoIterator<Item = (K, V)>,
) -> anyhow::Result<TelemetryHandles>
where
    K: Into<String>,
    V: Into<String>,
{
    let node_identity = TelemetryIdentity::from_hopr_keys_with_labels(hopr_keys, extra_labels);
    let mut telemetry_handles = init_logging_with_identity(node_identity.clone())?;
    init_metrics_with_identity(&mut telemetry_handles, node_identity)?;
    Ok(telemetry_handles)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn telemetry_handles_drop_no_panic() {
        drop(TelemetryHandles::default());
    }

    #[cfg(feature = "telemetry")]
    mod telemetry_tests {
        use super::super::*;
        use std::sync::{Arc, Mutex, OnceLock};

        #[derive(Clone)]
        struct EventCapture(Arc<Mutex<Option<TracingEventVisitor>>>);

        fn env_lock() -> std::sync::MutexGuard<'static, ()> {
            static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
            ENV_LOCK
                .get_or_init(|| Mutex::new(()))
                .lock()
                .expect("env lock poisoned")
        }

        fn set_env_var(key: &str, value: &str) {
            unsafe {
                std::env::set_var(key, value);
            }
        }

        fn remove_env_var(key: &str) {
            unsafe {
                std::env::remove_var(key);
            }
        }

        impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for EventCapture {
            fn on_event(
                &self,
                event: &tracing::Event<'_>,
                _ctx: tracing_subscriber::layer::Context<'_, S>,
            ) {
                let mut visitor = TracingEventVisitor::default();
                event.record(&mut visitor);
                *self.0.lock().unwrap() = Some(visitor);
            }
        }

        fn capture_visitor<F: FnOnce()>(f: F) -> TracingEventVisitor {
            let captured = Arc::new(Mutex::new(None::<TracingEventVisitor>));
            let layer = EventCapture(captured.clone());
            let sub = tracing_subscriber::Registry::default().with(layer);
            tracing::subscriber::with_default(sub, f);
            captured.lock().unwrap().take().unwrap_or_default()
        }

        #[test]
        fn transport_grpc_scheme() {
            assert_eq!(
                OtlpTransport::from_endpoint(Some("grpc://localhost:4317")),
                OtlpTransport::Grpc
            );
        }

        #[test]
        fn transport_http_scheme() {
            assert_eq!(
                OtlpTransport::from_endpoint(Some("http://localhost:4318")),
                OtlpTransport::Http
            );
        }

        #[test]
        fn transport_https_scheme() {
            assert_eq!(
                OtlpTransport::from_endpoint(Some("https://otel.example.com")),
                OtlpTransport::Http
            );
        }

        #[test]
        fn transport_none_defaults_grpc() {
            assert_eq!(OtlpTransport::from_endpoint(None), OtlpTransport::Grpc);
        }

        #[test]
        fn transport_empty_defaults_grpc() {
            assert_eq!(OtlpTransport::from_endpoint(Some("")), OtlpTransport::Grpc);
        }

        #[test]
        fn config_disabled_when_no_endpoint() {
            let _guard = env_lock();
            remove_env_var("OTEL_EXPORTER_OTLP_ENDPOINT");
            let config = OtlpConfig::from_env();
            assert!(!config.enabled);
        }

        #[test]
        fn config_enabled_with_endpoint() {
            let _guard = env_lock();
            set_env_var("OTEL_EXPORTER_OTLP_ENDPOINT", "http://localhost:4318");
            let config = OtlpConfig::from_env();
            remove_env_var("OTEL_EXPORTER_OTLP_ENDPOINT");
            assert!(config.enabled);
        }

        #[test]
        fn config_default_signals_is_traces_only() {
            let _guard = env_lock();
            set_env_var("OTEL_EXPORTER_OTLP_ENDPOINT", "http://localhost:4318");
            remove_env_var("EDGE_OTEL_SIGNALS");
            let config = OtlpConfig::from_env();
            remove_env_var("OTEL_EXPORTER_OTLP_ENDPOINT");
            assert!(config.signals.contains(OtlpSignal::Traces));
            assert!(!config.signals.contains(OtlpSignal::Logs));
            assert!(!config.signals.contains(OtlpSignal::Metrics));
        }

        #[test]
        fn config_set_but_empty_signals_falls_back_to_traces() {
            let _guard = env_lock();
            set_env_var("OTEL_EXPORTER_OTLP_ENDPOINT", "http://localhost:4318");
            set_env_var("EDGE_OTEL_SIGNALS", " , ");
            let config = OtlpConfig::from_env();
            remove_env_var("OTEL_EXPORTER_OTLP_ENDPOINT");
            remove_env_var("EDGE_OTEL_SIGNALS");
            assert!(config.signals.contains(OtlpSignal::Traces));
            assert!(!config.signals.contains(OtlpSignal::Logs));
            assert!(!config.signals.contains(OtlpSignal::Metrics));
        }

        #[test]
        fn edge_endpoint_override_enables_and_maps_to_legacy() {
            let _guard = env_lock();
            remove_env_var("OTEL_EXPORTER_OTLP_ENDPOINT");
            set_env_var("EDGE_OTLP_ENDPOINT", "http://localhost:4318");
            apply_edge_otlp_endpoint_override();
            let config = OtlpConfig::from_env();
            let mapped = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT");
            remove_env_var("EDGE_OTLP_ENDPOINT");
            remove_env_var("OTEL_EXPORTER_OTLP_ENDPOINT");
            assert!(config.enabled);
            assert_eq!(mapped.as_deref(), Ok("http://localhost:4318"));
        }

        #[test]
        fn metric_export_interval_parsing() {
            assert_eq!(
                parse_export_interval("15000"),
                Some(Duration::from_millis(15000))
            );
            assert_eq!(
                parse_export_interval("500ms"),
                Some(Duration::from_millis(500))
            );
            assert_eq!(parse_export_interval("10s"), Some(Duration::from_secs(10)));
            assert_eq!(parse_export_interval("2m"), Some(Duration::from_secs(120)));
            assert_eq!(parse_export_interval("0"), None);
            assert_eq!(parse_export_interval(""), None);
            assert_eq!(parse_export_interval("garbage"), None);
        }

        #[test]
        fn config_subset_signals_parsed() {
            let _guard = env_lock();
            set_env_var("OTEL_EXPORTER_OTLP_ENDPOINT", "http://localhost:4318");
            set_env_var("EDGE_OTEL_SIGNALS", "traces,metrics");
            let config = OtlpConfig::from_env();
            remove_env_var("OTEL_EXPORTER_OTLP_ENDPOINT");
            remove_env_var("EDGE_OTEL_SIGNALS");
            assert!(config.signals.contains(OtlpSignal::Traces));
            assert!(!config.signals.contains(OtlpSignal::Logs));
            assert!(config.signals.contains(OtlpSignal::Metrics));
        }

        #[test]
        fn config_invalid_signal_collected_not_panicked() {
            let _guard = env_lock();
            set_env_var("OTEL_EXPORTER_OTLP_ENDPOINT", "http://localhost:4318");
            set_env_var("EDGE_OTEL_SIGNALS", "traces,notasignal");
            let config = OtlpConfig::from_env();
            remove_env_var("OTEL_EXPORTER_OTLP_ENDPOINT");
            remove_env_var("EDGE_OTEL_SIGNALS");
            assert_eq!(config.invalid_signals, vec!["notasignal"]);
            assert!(config.signals.contains(OtlpSignal::Traces));
        }

        #[test]
        fn visitor_message_goes_to_body() {
            let v = capture_visitor(|| {
                tracing::info!("hello world");
            });
            assert_eq!(v.body.as_deref(), Some("hello world"));
            assert!(!v.attributes.iter().any(|(k, _)| k == "message"));
        }

        #[test]
        fn visitor_extra_field_goes_to_attributes() {
            let v = capture_visitor(|| {
                tracing::info!(key = "value", "msg");
            });
            assert!(v.attributes.iter().any(|(k, _)| k == "key"));
        }

        #[test]
        fn visitor_u64_timestamp_not_in_attributes() {
            let v = capture_visitor(|| {
                tracing::info!(timestamp = 1_700_000_000_000u64, "msg");
            });
            assert!(v.timestamp.is_some());
            assert!(!v.attributes.iter().any(|(k, _)| k == "timestamp"));
        }

        #[test]
        fn visitor_i64_timestamp_not_in_attributes() {
            let v = capture_visitor(|| {
                tracing::info!(timestamp = 1_700_000_000_000i64, "msg");
            });
            assert!(v.timestamp.is_some());
            assert!(!v.attributes.iter().any(|(k, _)| k == "timestamp"));
        }

        #[test]
        fn visitor_large_u64_stored_as_string() {
            let v = capture_visitor(|| {
                tracing::info!(count = u64::MAX, "msg");
            });
            let val = v
                .attributes
                .iter()
                .find(|(k, _)| k == "count")
                .map(|(_, v)| v);
            assert!(
                matches!(val, Some(opentelemetry::logs::AnyValue::String(_))),
                "u64::MAX should be stored as a string AnyValue"
            );
        }
    }
}
