# edge-client

An edge client implementing the HOPR protocol without heavy integration of an
RPC provider or blockchain data processing.

## Telemetry

`edgli` exposes telemetry setup from the library so host applications can wire
tracing/OTLP themselves.

```rust
use edgli::telemetry::{init_telemetry, init_telemetry_with_extra_labels};

let _telemetry = init_telemetry(&hopr_keys)?;
let _telemetry = init_telemetry_with_extra_labels(
    &hopr_keys,
    vec![("type", "client")],
)?;
```

Keep the returned handle alive for the full process lifetime so exporters are
not dropped early.

Environment variables:

- `EDGE_OTEL_SIGNALS` (comma-separated: `traces,logs,metrics`; setting this
  enables OpenTelemetry)
- `OTEL_EXPORTER_OTLP_ENDPOINT`
