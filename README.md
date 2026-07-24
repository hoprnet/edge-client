# edge-client

[![codecov](https://codecov.io/gh/hoprnet/edge-client/branch/main/graph/badge.svg)](https://codecov.io/gh/hoprnet/edge-client)

An edge client implementing the HOPR protocol without heavy integration of an
RPC provider or blockchain data processing.

> [!NOTE]
> The `edgli` binary target is present but lacks a complete operator interface
> (identity generation, config scaffolding, runtime control). Until that is in
> place, `edge-client` is intended to be consumed as a **library** (the `edgli`
> crate) and embedded in a host application that supplies those concerns.

## Getting started

All tooling (Rust toolchain, linkers, formatters, `cargo-nextest`,
`cargo-llvm-cov`, …) is pinned through Nix — nothing else needs to be installed
locally.

```bash
# enter a dev shell with everything wired up
nix develop

# build the library
cargo build

# or let Nix build and cache it
nix build .#lib-edgli
```

Cross-compiled library artifacts are exposed as
`lib-edgli-{x86_64,aarch64}-{linux,darwin}`.

## Library usage

Embed the client by constructing an `Edgli` instance. Initialization is reported
through a visitor callback that receives `EdgliInitState` transitions.

```rust
use edgli::{Edgli, EdgliInitState, hopr_lib::{HoprKeys, config::HoprLibConfig}};

async fn run(cfg: HoprLibConfig, keys: HoprKeys) -> anyhow::Result<()> {
    let edgli = Edgli::new(
        cfg,
        keys,
        None,  // blokli URL (optional)
        None,  // blokli DNS override (optional)
        None,  // BlockchainConnectorConfig (optional)
        false, // probe_local_addresses: filter non-public peer addresses
        |state: EdgliInitState| tracing::info!(?state, "init"),
    )
    .await?;

    // `Edgli` derefs to `Hopr`, so the hopr-lib API is available directly.
    let _ = edgli.me_onchain();
    Ok(())
}
```

See `src/client.rs` for `run_hopr_edge_node_with` (spawn helper) and
`Edgli::run_reactor_from_cfg` (edge strategy reactor: channel funding,
pending-close sweeping) when the `blokli` feature is enabled.

### Feature flags

| flag             | default | effect                                                    |
| ---------------- | :-----: | --------------------------------------------------------- |
| `runtime-tokio`  |   yes   | Tokio runtime integration                                 |
| `prometheus`     |   yes   | Prometheus metrics via `hopr-lib`                         |
| `blokli`         |   yes   | Blokli-backed trustful blockchain connector               |
| `session-server` |   no    | Enables the session-server side of `hopr-lib`             |
| `telemetry`      |   no    | OpenTelemetry OTLP export                                 |
| `testing`        |   no    | Test-only helpers from `hopr-lib`                         |
| `prof`           |   no    | `tokio-console` subscriber (needs `--cfg tokio_unstable`) |

## Testing

Unit tests:

```bash
nix develop -c cargo nextest run --lib
```

Full check suite (clippy, rustdoc, audit, licenses, tests) via Nix:

```bash
nix flake check
```

Coverage (lcov at `coverage.lcov`):

```bash
nix run .#coverage-unit
```

## Architecture

```
               ┌──────────────────────┐
               │    host application  │
               │  (your binary/tool)  │
               └──────────┬───────────┘
                          │ embeds
                          ▼
┌──────────────────────────────────────────────┐
│                  edgli (lib)                 │
│   Edgli::new ← HoprLibConfig + HoprKeys      │
│   optional: MultiStrategy reactor            │
└───┬───────────────────────┬──────────────────┘
    │ hopr-lib              │ hopr-chain-connector
    ▼                       ▼
HOPR mixnet            Blokli (read-only
(QUIC transport,       chain events; no
 session client)       local RPC node)
```

Key inputs handed to `Edgli::new`:

- `HoprLibConfig` — host / transport / safe-module configuration.
- `HoprKeys` — packet key + chain key pair.
- `db_data_path` — persistent node DB directory. Edge clients are
  ticket-originators only and therefore do not store received tickets.
- `blokli_url` / `BlockchainConnectorConfig` — blokli endpoint and connector
  tuning (both optional; defaults applied when omitted).

## Troubleshooting

- **Logging.** Controlled by `RUST_LOG` (see `tracing_subscriber`). Set
  `HOPRD_LOG_FORMAT=json` for structured output. Sensible defaults are applied
  when `RUST_LOG` is unset.
- **Loopback address rejected.** `Edgli::new` refuses to announce a loopback
  host unless `protocol.transport.prefer_local_addresses = true`.
- **Local peers not probed.** By default non-public (private, loopback,
  link-local) peer addresses from announcements are filtered before dialing.
  Pass `--probe-local-addresses` (or `HOPR_EDGE_PROBE_LOCAL_ADDRESSES=true`, or
  the `probe_local_addresses` argument to `Edgli::new`) to probe them (e.g. a
  same-host test cluster).
- **Profiling.** Build with
  `RUSTFLAGS="--cfg tokio_unstable" cargo build --features prof` and attach
  `tokio-console`.
- **Reporting issues.** <https://github.com/hoprnet/edge-client/issues>

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

The configuration mirrors `hoprd`'s OTLP flow (same `EDGE_`-prefixed variables
in place of `HOPRD_`), with one simplification: there is **no enable flag**.
Setting an endpoint is what turns export on.

Environment variables:

- `EDGE_OTLP_ENDPOINT` (setting this enables OpenTelemetry export). Transport is
  inferred from the scheme (`grpc://`, `http://`, `https://`). At startup it is
  copied into the standard `OTEL_EXPORTER_OTLP_ENDPOINT`; if both are set and
  differ, `EDGE_OTLP_ENDPOINT` wins.
- `OTEL_EXPORTER_OTLP_ENDPOINT` (legacy fallback; also enables export if set)
- `EDGE_OTEL_SIGNALS` (optional comma-separated subset of `traces,logs,metrics`;
  defaults to `traces`)
- `EDGE_METRIC_EXPORT_INTERVAL` (optional default metric export cadence; accepts
  a bare millisecond integer or an `ms`/`s`/`m` suffix, e.g. `15000`, `10s`,
  `1m`; defaults to `60s`)
- `OTEL_SERVICE_NAME` (optional; defaults to the crate name)
