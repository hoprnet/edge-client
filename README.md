# edge-client

[![codecov](https://codecov.io/gh/hoprnet/edge-client/branch/main/graph/badge.svg)](https://codecov.io/gh/hoprnet/edge-client)

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

- `OTEL_EXPORTER_OTLP_ENDPOINT` (required to enable OpenTelemetry export)
- `EDGE_OTEL_SIGNALS` (optional comma-separated subset: `traces,logs,metrics`;
  defaults to all signals)
