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

### PIX

PIX pays the Exit for the traffic it delivers. This node, as the Entry, deposits
wxHOPR to a per-Session stealth address; the Exit reconstructs that address's
key from the SSA shares its spent SURBs carried and sweeps the deposit into its
Safe. Only the Entry half is implemented here — an edge client never terminates
a Session, so it never reconstructs or sweeps anything.

Pick exactly one deposit pool and build with it, then opt a Session in _and_ run
the deposit strategy. Both halves are needed: with only the strategy nothing
announces PIX and no deposit is ever requested, and with only the opt-in the
node announces PIX it cannot pay for and the Exit closes the Session on its
deposit deadline.

| feature     | deposit address    | status                                                    |
| ----------- | ------------------ | --------------------------------------------------------- |
| `pix-curvy` | Baby JubJub        | **stub** — starts and serves, panics on the first deposit |
| `pix-test`  | Ethereum (visible) | works; **tests and demos only**, forfeits PIX's anonymity |

They are mutually exclusive — enabling both is a compile error, because
`hopr-lib` resolves the conflict in favour of secp256k1 _silently_, so a build
asking for the anonymous pool would settle to visible addresses with nothing to
say so. `pix` on its own is the umbrella both turn on; enabling it alone selects
no pool and is also a compile error.

```rust,ignore
use edgli::{PixEntryConfig, PixEntryStrategy, quota_per_ssa};
use edgli::strategy::{EdgeStrategyKind, IncentiveConfiguration, default_strategy_cfg};
use edgli::hopr_lib::HoprSessionClientConfig;

// Pay: add the PIX strategy to the reactor. `strategy` is pricing and is the same whichever pool
// the build selected; `pool` is that pool's own knobs, and its defaults are usually fine.
let mut strategies = default_strategy_cfg(&IncentiveConfiguration::default())?;
strategies.strategies.push(EdgeStrategyKind::Pix(PixEntryConfig {
    strategy: PixEntryStrategy {
        price_per_byte: "0.0001 wxHOPR".parse()?,
        max_ssa_allocation: "10 wxHOPR".parse()?,
        ..Default::default()
    },
    ..Default::default()
}));
let _reactor = edgli.run_reactor_from_cfg(strategies)?;

// Ask: opt a Session in. The quota is read from this node's own `protocol.pix`
// dimensions, so there is nothing to state and nothing to get out of step.
let session_cfg = edgli.with_pix(HoprSessionClientConfig::default())?;

// What one SSA cycle buys, for sizing the float below.
let bytes_per_deposit = quota_per_ssa(&edgli.pix_ssa_quota()?);
```

The generator dimensions themselves live in `protocol.pix` of the
`HoprLibConfig` passed to `Edgli::new`, alongside the rest of the node
configuration. Leave `additional_shares` unset unless you have measured your
return-path loss: unset derives a surplus from the threshold, and the surplus is
billed — a cycle emits it whether or not a share is lost, so it is insurance
charged on purchase rather than on claim.

#### Deposits are paid from the node's own account, not the Safe

Under `pix-test`. The deposit is a direct `HoprToken.transfer` signed by the
node key — the one call the Safe payload generator does not route through the
Safe module — so the wxHOPR comes off the node address, while `deploy_safe`
sweeps that balance _into_ the Safe during onboarding, leaving it at zero.

An operator running PIX therefore has to leave a wxHOPR (and xDai for gas) float
on the node address, sized against
`price_per_byte × quota_per_ssa × expected SSA cycles`. A node that runs dry
stops depositing, and the Exit closes the Session on its deposit deadline, with
nothing logged as an error at this end.
`Edgli::describe_current_capacity_allocations` reports the remaining float as
its `node` allocation, which is the figure to watch.

Where `pix-curvy`'s funds will come from is not yet settled — its
`deposit_funds_to` is unimplemented — so this section is about `pix-test` only.

#### Not for production

Neither pool is deployable today. `pix-test` works but settles fully visibly
on-chain, forfeiting the anonymity PIX exists to provide; `pix-curvy` is the
anonymous one and is a stub whose methods panic. So there is currently no
production PIX path, here or in `hoprd`. The `pix-curvy` wiring is carried
anyway, compiled and linted on every PR, so that when the settlement logic lands
upstream this crate needs a dependency bump rather than a design.

### Feature flags

| flag            | default | effect                                                    |
| --------------- | :-----: | --------------------------------------------------------- |
| `runtime-tokio` |   yes   | Tokio runtime integration                                 |
| `blokli`        |   yes   | Blokli-backed trustful blockchain connector               |
| `pix`           |   no    | Entry-side PIX; umbrella, selects no pool on its own      |
| `pix-test`      |   no    | PIX with the secp256k1 pool — tests and demos (see above) |
| `pix-curvy`     |   no    | PIX with the Baby JubJub pool — stub (see above)          |
| `telemetry`     |   no    | OpenTelemetry OTLP export                                 |
| `testing`       |   no    | Test-only helpers from `hopr-lib`                         |
| `prof`          |   no    | `tokio-console` subscriber (needs `--cfg tokio_unstable`) |

The concrete `Edgli` client and the `edgli` binary require both `runtime-tokio`
and `blokli`. Other feature combinations still build the feature-independent
library modules.

## Testing

Unit tests (lib `#[cfg(test)]` modules + the `tests/` binaries):

```bash
nix develop -c cargo nextest run

# PIX code is behind non-default features, so it needs naming
nix develop -c cargo nextest run --features pix-test
nix develop -c cargo nextest run --features pix-curvy
```

Full check suite (clippy, rustdoc, audit, licenses, tests) via Nix:

```bash
nix flake check
```

Coverage (lcov at `coverage.lcov`):

```bash
nix run .#coverage-unit
```

### Integration & throughput tests

Full-stack integration lives in
**[`hoprnet/hoprd-test`](https://github.com/hoprnet/hoprd-test)**, which
consumes `edgli` as a library and runs it against a real network. That repo
owns:

- **Local-cluster session throughput** (0-hop / 1-hop over a
  `hoprd-localcluster`),
- **Rotsee public-testnet** sessions (funded Gnosis identity via
  `EDGLI_ROTSEE_*`), and
- **executor-yield profiling** (tokio-console + Perfetto traces).

This crate keeps only fast, self-contained unit tests (no network, no external
binaries): the inline `#[cfg(test)]` modules in `src/` and
`tests/mixer_config.rs`.

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
- **Profiling.** Build with `cargo build --profile tracer --features prof` and
  attach `tokio-console` (the `tracer` profile keeps TRACE-level task spans
  compiled in — see `[profile.tracer]`). (`.cargo/config.toml` already supplies
  `--cfg tokio_unstable`; do **not** export `RUSTFLAGS`, as it replaces the
  target rustflags and would drop the aarch64 AES intrinsics.)
- **Reporting issues.** <https://github.com/hoprnet/edge-client/issues>
