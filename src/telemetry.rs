#[cfg(feature = "telemetry")]
use tracing_subscriber::prelude::*;

#[cfg(feature = "telemetry")]
use std::{collections::HashMap, str::FromStr, time::Duration};

use hopr_lib::{HoprKeys, Keypair, ToHex};
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

#[cfg(feature = "telemetry")]
const EDGE_OTEL_SIGNALS_ENV: &str = "EDGE_OTEL_SIGNALS";

#[cfg_attr(not(feature = "telemetry"), allow(dead_code))]
#[derive(Clone, Debug)]
struct TelemetryIdentity {
    node_address: String,
    node_peer_id: String,
    extra_labels: Vec<(String, String)>,
}

impl TelemetryIdentity {
    fn from_hopr_keys_with_labels(
        hopr_keys: &HoprKeys,
        extra_labels: Vec<(String, String)>,
    ) -> Self {
        let node_address = Keypair::public(&hopr_keys.chain_key).to_address().to_hex();
        let node_peer_id = Keypair::public(&hopr_keys.packet_key).to_peerid_str();
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

#[cfg(feature = "telemetry")]
#[derive(Debug, Clone)]
pub struct OtlpConfig {
    pub enabled: bool,
    pub service_name: String,
    pub transport: OtlpTransport,
    pub signals: flagset::FlagSet<OtlpSignal>,
}

#[cfg(feature = "telemetry")]
impl OtlpConfig {
    pub fn from_env() -> Self {
        let service_name =
            std::env::var("OTEL_SERVICE_NAME").unwrap_or_else(|_| env!("CARGO_PKG_NAME").into());
        let otlp_endpoint = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
            .ok()
            .map(|endpoint| endpoint.trim().to_string())
            .filter(|endpoint| !endpoint.is_empty());
        let transport = OtlpTransport::from_endpoint(otlp_endpoint.as_deref());
        let mut signals = flagset::FlagSet::empty();
        let raw_signals = std::env::var(EDGE_OTEL_SIGNALS_ENV).ok();
        let enabled = otlp_endpoint.is_some();

        if let Some(raw_signals) = raw_signals {
            for signal in raw_signals.split(',') {
                let signal = signal.trim();
                if signal.is_empty() {
                    continue;
                }
                match OtlpSignal::from_str(signal) {
                    Ok(parsed) => signals |= parsed,
                    Err(_) => tracing::warn!(
                        otel_signal = %signal,
                        "Invalid OpenTelemetry signal specified in edge-client OTEL signals environment variable"
                    ),
                }
            }
        }

        if signals.is_empty() {
            signals |= OtlpSignal::Traces;
            signals |= OtlpSignal::Logs;
            signals |= OtlpSignal::Metrics;
        }

        Self {
            enabled,
            service_name,
            transport,
            signals,
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

        record.add_attribute("target", metadata.target().to_string());
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
        if let Ok(value) = u64::try_from(value) {
            self.maybe_record_unix_timestamp_millis(field, value);
        }
        self.record_body_or_attribute(field, value);
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.maybe_record_unix_timestamp_millis(field, value);
        if value <= i64::MAX as u64 {
            self.record_body_or_attribute(field, value as i64);
        } else {
            self.record_body_or_attribute(field, value.to_string());
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
    init_logging_with_extra_labels(hopr_keys, Vec::new())
}

pub fn init_logging_with_extra_labels(
    hopr_keys: &HoprKeys,
    extra_labels: Vec<(String, String)>,
) -> anyhow::Result<TelemetryHandles> {
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
                        .with_protocol(opentelemetry_otlp::Protocol::HttpJson)
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

pub fn init_metrics(
    telemetry_handles: &mut TelemetryHandles,
    hopr_keys: &HoprKeys,
) -> anyhow::Result<()> {
    init_metrics_with_extra_labels(telemetry_handles, hopr_keys, Vec::new())
}

pub fn init_metrics_with_extra_labels(
    telemetry_handles: &mut TelemetryHandles,
    hopr_keys: &HoprKeys,
    extra_labels: Vec<(String, String)>,
) -> anyhow::Result<()> {
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

        let reader = opentelemetry_sdk::metrics::periodic_reader_with_async_runtime::PeriodicReader::builder(
            exporter,
            opentelemetry_sdk::runtime::Tokio,
        )
        .build();
        let meter_provider = SdkMeterProvider::builder()
            .with_reader(reader)
            .with_resource(resource)
            .build();
        opentelemetry::global::set_meter_provider(meter_provider.clone());
        telemetry_handles.meter_provider = Some(meter_provider);

        let enabled_signals = enabled_signal_names(&config, &[OtlpSignal::Metrics]);
        tracing::info!(
            otel_service_name = %config.service_name,
            otel_signals = %enabled_signals,
            otel_protocol = %config.transport.to_string(),
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
    init_telemetry_with_extra_labels(hopr_keys, Vec::new())
}

pub fn init_telemetry_with_extra_labels(
    hopr_keys: &HoprKeys,
    extra_labels: Vec<(&str, &str)>,
) -> anyhow::Result<TelemetryHandles> {
    let extra_labels = extra_labels
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect::<Vec<_>>();
    let node_identity = TelemetryIdentity::from_hopr_keys_with_labels(hopr_keys, extra_labels);
    let mut telemetry_handles = init_logging_with_identity(node_identity.clone())?;
    init_metrics_with_identity(&mut telemetry_handles, node_identity)?;
    Ok(telemetry_handles)
}
