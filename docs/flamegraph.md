# Flamegraph profiling

Profiles the `edgli_session_rotsee` integration test against the Rotsee testnet.

## Identity setup

Export the required env vars (see `tests/edgli_session_rotsee.rs` for the full list):

```sh
# Required
export EDGLI_ROTSEE_BLOKLI_URL='https://blokli.rotsee.gnosisvpn.io'
export EDGLI_ROTSEE_IDENTITY_FILE="$HOME/.fun/gnosis/rotsee/gnosisvpn-hopr.id"
export EDGLI_ROTSEE_IDENTITY_PASSWORD="$(cat "$HOME/.fun/gnosis/rotsee/gnosisvpn-hopr.pass")"
export EDGLI_ROTSEE_SAFE_ADDRESS='0x...'      # from gnosisvpn-hopr.safe
export EDGLI_ROTSEE_MODULE_ADDRESS='0x...'    # from gnosisvpn-hopr.safe
export RUST_LOG='info,edgli=debug'
```

```sh
# Optional — pins a specific Rotsee exit-service node (relay nodes do not run exit).
# If unset, the test picks one from on-chain discovery.
export EDGLI_ROTSEE_EXIT_NODE='0x...'
```

---

## macOS

### Prerequisites

```sh
nix develop
```

### Running

`cargo flamegraph --root` launches the binary via Instruments, which does not
inherit the calling shell's environment. Use `samply` instead — it runs the
binary as a direct child process so env vars are inherited naturally.

```sh
cargo build --profile=flamegraph --test edgli_session_rotsee
BINARY=$(ls target/flamegraph/deps/edgli_session_rotsee-* | grep -v '\.d$' | head -1)
samply record -o "/tmp/$(date +%Y%m%d-%H%M%S).json" \
  "$BINARY" --ignored --nocapture edgli_sends_one_megabyte_through_rotsee
```

Load the saved profile:

```sh
samply load /tmp/<timestamp>.json
```

This opens the [Firefox Profiler](https://profiler.firefox.com) in your browser.

### Verifying the result

- Exit code is `0` and both session pumps report a SHA-256 match in stdout.

---

## Linux

### Prerequisites

Relax kernel sampling permissions (resets on reboot), then enter the devshell:

```sh
sudo sysctl -w kernel.perf_event_paranoid=1
nix develop
```

### Running

```sh
cargo flamegraph \
  --profile=flamegraph \
  --test edgli_session_rotsee \
  --output "/tmp/$(date +%Y%m%d-%H%M%S).svg" \
  -- --ignored --nocapture edgli_sends_one_megabyte_through_rotsee
```

### Verifying the result

- Exit code is `0` and both session pumps report a SHA-256 match in stdout.
- The `.svg` is >200 KB — a smaller file means the sampler captured no useful data.
- Open the `.svg` in a browser. If frames show `??` addresses, the binary was stripped; confirm `--profile=flamegraph` was passed.
