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
use edgli::{BlokliEndpoint, Edgli, EdgliInitState, hopr_lib::{HoprKeys, config::HoprLibConfig}};

async fn run(cfg: HoprLibConfig, keys: HoprKeys) -> anyhow::Result<()> {
    let edgli = Edgli::new(
        cfg,
        keys,
        BlokliEndpoint::default(), // production endpoint, system DNS
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

To reach Blokli when system DNS is unavailable, pin the endpoint host to a fixed
address. The request URL is not rewritten, so the HTTP `Host` header, TLS SNI
and certificate validation still use the original hostname:

```rust
use edgli::{BlokliDnsOverride, BlokliEndpoint};

let endpoint = BlokliEndpoint::default()
    .with_dns_override("10.1.2.1:3002".parse::<BlokliDnsOverride>()?);
```

IPv6 addresses with a port use bracketed socket syntax, for example
`[::1]:3002`. An unbracketed value such as `::1:3002` is treated as an IPv6
address without a separate port.

The same `BlokliEndpoint` is accepted by `make_incentive_operations`, so the
on-boarding flow (balances, ticket pricing, Safe deployment, withdrawals)
honours the override too.

`BlokliEndpoint`, `BlokliDnsOverride` and `make_incentive_operations` are
blokli-specific: they are only available with the `blokli` feature enabled (it
is on by default).

See `src/client.rs` for `run_hopr_edge_node_with` (spawn helper) and
`Edgli::run_reactor_from_cfg` (edge strategy reactor: channel funding,
pending-close sweeping) when the `blokli` feature is enabled.

### Feature flags

| flag            | default | effect                                                    |
| --------------- | :-----: | --------------------------------------------------------- |
| `runtime-tokio` |   yes   | Tokio runtime integration                                 |
| `blokli`        |   yes   | Blokli-backed trustful blockchain connector               |
| `telemetry`     |   no    | OpenTelemetry OTLP export                                 |
| `testing`       |   no    | Test-only helpers from `hopr-lib`                         |
| `prof`          |   no    | `tokio-console` subscriber (needs `--cfg tokio_unstable`) |

The concrete `Edgli` client and the `edgli` binary require both `runtime-tokio`
and `blokli`. Other feature combinations still build the feature-independent
library modules.

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

### Tests excluded from CI (`#[ignore]` / feature-gated)

Some integration tests are **not** part of `cargo nextest run --lib` or
`nix flake check`: they need external binaries, a container runtime, a funded
testnet identity, or a non-default build profile. They are marked `#[ignore]`
(and some are additionally gated behind a Cargo feature), so a normal test run
skips them and CI never invokes them. Run them explicitly with `--ignored`.

To list what exists without running anything:

```bash
# every ignored test EXCEPT the feature-gated profiling ones, with its module path
cargo test --no-default-features --features runtime-tokio,blokli \
  --tests -- --ignored --list

# the profiling tests are gated behind the `prof` feature, so list them separately
# (`.cargo/config.toml` supplies `--cfg tokio_unstable`; do not export RUSTFLAGS)
cargo test --no-default-features --features runtime-tokio,blokli,prof \
  --test edgli_profiling -- --ignored --list
```

| Test file                       | Test(s)                                     | What it needs                                 |
| ------------------------------- | ------------------------------------------- | --------------------------------------------- |
| `tests/edgli_session_e2e.rs`    | `edgli_sends_payload_through_local_cluster` | `hoprd*` binaries + container runtime         |
| `tests/edgli_session_rotsee.rs` | `edgli_sends_payload_through_rotsee`        | funded Rotsee identity (`EDGLI_ROTSEE_*` env) |
| `tests/edgli_profiling.rs`      | 3 executor-yield profiling tests            | `--features prof` + `--cfg tokio_unstable`    |

The authoritative setup for each lives in the module docs at the top of the
corresponding test file; the summaries below are the fast path.

#### 1. Local-cluster session throughput (`edgli_session_e2e.rs`)

Spins up a real 3-node HOPR cluster via `hoprd-localcluster`, boots Edgli
against it, and pumps a 20 MiB payload through 0-hop and 1-hop sessions
(measuring throughput and packet loss, verifying SHA-256 integrity).

Prerequisites:

- `hoprd` and `hoprd-localcluster` binaries, built from the
  [`hoprnet/hoprd`](https://github.com/hoprnet/hoprd) repo:
  ```bash
  cargo build --release -p hoprd -p hoprd-localcluster
  ```
- A container runtime for the chain image. On macOS use Apple's native
  `container` (`container system start`); elsewhere Docker/Podman.
- The `bloklid-anvil` chain image pulled (see below).

Run (managed mode — the test starts and tears down its own cluster):

```bash
export HOPRD_LOCALCLUSTER_BIN=/path/to/hoprd/target/release/hoprd-localcluster
export HOPRD_BIN=/path/to/hoprd/target/release/hoprd
# Use the `latest` tag — the tag pinned in hoprd's docker-compose can lag the contract
# schema the hoprd binaries expect (fails with `missing field 'xhopr_token'`).
export HOPRD_CHAIN_IMAGE='europe-west3-docker.pkg.dev/hoprassociation/docker-images/bloklid-anvil:latest'
export HOPRD_CONTAINER_RUNTIME=container   # macOS Apple runtime; default is docker

# `--release` is required: HOPR's async future chains overflow the default debug stack.
RUST_LOG=info,edgli=debug \
  cargo test --test edgli_session_e2e --release -- --ignored --nocapture
```

The run reports, per hop count:

```text
0-hop: send … KiB/s | recv … KiB/s | loss …% (bytes)
1-hop: send … KiB/s | recv … KiB/s | loss …% (bytes)
```

See the module docs at the top of `tests/edgli_session_e2e.rs` for the full
env-var table and the "external mode" (attach to an already-running cluster)
variant.

#### 2. Rotsee public-testnet session (`edgli_session_rotsee.rs`)

Same pump/verify flow as the local-cluster test, but against the **Rotsee**
public testnet instead of a managed cluster. Needs a HOPR identity that is
already funded and registered with a Safe + HOPR module on Gnosis Chain — there
is no cluster to boot, so all configuration comes from `EDGLI_ROTSEE_*` env
vars:

```bash
export EDGLI_ROTSEE_BLOKLI_URL=https://blokli.rotsee.gnosisvpn.io
export EDGLI_ROTSEE_IDENTITY_FILE=/path/to/identity.json
# Read the identity password interactively so it never lands in shell history.
read -rsp 'Rotsee identity password: ' EDGLI_ROTSEE_IDENTITY_PASSWORD
printf '\n'
export EDGLI_ROTSEE_IDENTITY_PASSWORD
export EDGLI_ROTSEE_SAFE_ADDRESS=0x...
export EDGLI_ROTSEE_MODULE_ADDRESS=0x...

# --release is required for the same stack-depth reason as above.
RUST_LOG=info,edgli=debug \
  cargo test --test edgli_session_rotsee --release -- --ignored --nocapture
```

#### 3. Executor-yield profiling (`edgli_profiling.rs`)

Runs the session pump under a `tokio-console` + `tracing-chrome` subscriber to
capture executor-starvation traces (a fast writer monopolising a worker thread
and starving the SURB balancer). These tests are **doubly gated**: `#[ignore]`
_and_ `#[cfg(feature = "prof")]`, and they only see tokio's task spans when
built with `--cfg tokio_unstable` under the `tracer` profile (which re-enables
the TRACE callsites `hopr-lib`'s `release_max_level_debug` would otherwise
compile out). They reuse the same local-cluster / Rotsee prerequisites as the
tests above.

The simplest way to run them is the wrapper script, which wires up the env vars,
build flags, and result collection:

```bash
./scripts/profile-executor-yield.sh
```

Or manually (writes Chrome traces to `$EDGLI_TRACE_DIR`, load them at
<https://ui.perfetto.dev>):

```bash
export HOPRD_LOCALCLUSTER_BIN=/path/to/hoprd/target/release/hoprd-localcluster
export HOPRD_BIN=/path/to/hoprd/target/release/hoprd
# For reproducible profiling numbers, pin the chain image to an immutable digest
# (`bloklid-anvil@sha256:<digest>`) rather than `:latest` — a remote `:latest`
# update can shift throughput without any code change. Resolve the digest of a
# tag known to match your hoprd binaries with e.g.
# `docker buildx imagetools inspect <image>:latest`, then substitute it below.
export HOPRD_CHAIN_IMAGE='europe-west3-docker.pkg.dev/hoprassociation/docker-images/bloklid-anvil@sha256:<pinned-digest>'
# macOS Apple `container` runtime; on Linux/others use `docker` or `podman`.
export HOPRD_CONTAINER_RUNTIME=container
export EDGLI_TRACE_DIR=./profiling-results
export RUST_LOG=info,edgli=debug,tokio=trace,runtime=trace

# Do NOT export RUSTFLAGS: `.cargo/config.toml` already supplies
# `--cfg tokio_unstable`, and RUSTFLAGS would *replace* (not append) the target
# rustflags, silently dropping the aarch64 AES intrinsics (`+aes,+neon`).
cargo nextest run \
  --test edgli_profiling --profile tracer --features prof \
  --run-ignored ignored-only --no-capture --test-threads 1
```

See the module docs at the top of `tests/edgli_profiling.rs` for what each trace
should show and why all three build flags are required together.

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
- `BlokliEndpoint` — blokli service URL plus an optional DNS override that
  bypasses system DNS for that host. `BlokliEndpoint::default()` uses the
  production endpoint and system DNS.
- `BlockchainConnectorConfig` — connector tuning (optional; defaults applied
  when omitted).

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
- **Profiling.** Build with `cargo build --features prof` and attach
  `tokio-console`. (`.cargo/config.toml` already supplies
  `--cfg tokio_unstable`; do **not** export `RUSTFLAGS`, as it replaces the
  target rustflags and would drop the aarch64 AES intrinsics.)
- **Reporting issues.** <https://github.com/hoprnet/edge-client/issues>
