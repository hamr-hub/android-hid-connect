# Development — `android-hid-connect`

> Local development loop, real-device E2E steps, CI matrix, and
> benchmarks. Per [ADR-0015](adr/0015-ai-middleware-consolidation.md)
> (2026-07-01) the workspace is 3 crates + 1 binary (`adk`).
>
> Pre-PR checklist: [`../AGENTS.md`](../AGENTS.md) §5.

---

## 1. Prerequisites

- **Rust**: `rustup toolchain install 1.87` (workspace MSRV)
- **Android SDK / `adb`** (only for real-device examples)
- **scrcpy-server v2.7** (only for real-device examples)
  - `scrcpy-server` jar must be at workspace root for the
    `examples/live_*.rs` scripts.

---

## 2. Dev loop

```bash
# Format + lint + test (the standard pre-PR loop)
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace

# Bench the intent→UHID hot path (CI gate, p99 < 5 ms)
cargo build --release -p ai-device-kernel --bin adk-bench
./target/release/adk-bench --iterations=1000

# UHID throughput bench (root crate)
cargo bench --bench uhid_throughput -- --quick
```

---

## 3. Running the `adk` daemon

```bash
# Build
cargo build --release -p ai-device-kernel --bin adk

# Forward the device port (after `adb push adk` to device + run)
adb forward tcp:9008 localabstract:adk

# Smoke-test the wire protocol (in another terminal)
echo -ne '\x00\x00\x00\x01ping' | nc 127.0.0.1 9008
```

The `adk` binary listens on port 9008 and speaks the
postcard/4-verb protocol defined in
[`ai-device-kernel/src/protocol.rs`](../ai-device-kernel/src/protocol.rs).
For bridge-capable writes (`Tap`, `Swipe`, `TypeText`, `Key`, `Launch`,
`SetClipboard`, `GamepadFrame`, `InjectRaw`), `adk` lazily opens the scrcpy
control socket at `--scrcpy-host 127.0.0.1 --scrcpy-port 27183` and uses the
same byte-exact bridge as the in-process API. Start scrcpy-server with
`control=true tunnel_forward=true send_dummy_byte=true` and forward
`tcp:27183 localabstract:scrcpy` before sending these actions.

---

## 4. Real-device E2E

The canonical real-device end-to-end coverage is the
[`examples/live_e2e.rs`](../examples/live_e2e.rs) byte-exact driver.
It requires a connected Android device with `scrcpy-server` running:

```bash
# 1. Push + start scrcpy-server on device
adb push scrcpy-server /data/local/tmp/scrcpy-server
adb forward tcp:27183 localabstract:scrcpy
adb shell 'nohup sh -c "CLASSPATH=/data/local/tmp/scrcpy-server \
  app_process / com.genymobile.scrcpy.Server 2.7 \
  video=false audio=false control=true clipboard_autosync=false \
  tunnel_forward=true send_dummy_byte=true" > /dev/null 2>&1 &'

# 2. Run the E2E driver
cargo run --release --example live_e2e
```

---

## 5. In-process micro-bench

The `adk-bench` binary is the in-process CI gate (no device
needed). It runs `bridge::ActionInjector` over `MockTransport`:

```bash
cargo build --release -p ai-device-kernel --bin adk-bench
./target/release/adk-bench
# → reports p50 / p95 / p99 / max latency for the canonical
#   5-step plan (Tap, Swipe, TypeText, Key, GamepadFrame).
# → exits 2 if p99 > 5 ms.
```

Typical numbers on x86_64-linux (release, opt-level z):
- p50: 1 µs
- p95: 3 µs
- p99: 5 µs
- mean: 1.7 µs / sample

---

## 6. UHID throughput bench (root crate)

```bash
cargo bench --bench uhid_throughput
# 240 Hz gamepad bench (SPSC sanity)
cargo bench --bench gamepad_240hz_bench -- --quick
```

These exercise the byte-exact UHID path end-to-end over a
`MockTransport`. 240 Hz is the SLO for high-frequency gamepad
input.

---

## 7. CI matrix

`.github/workflows/ci.yml` runs on every PR:

| Stage | Command | Notes |
| ----- | ------- | ----- |
| Format | `cargo fmt --all -- --check` | 0 diff required |
| Lint | `cargo clippy --workspace --all-targets -- -D warnings` | 0 warnings required |
| Test | `cargo test --workspace` | root + protocol + ADK suites |
| Bench smoke | `cargo run --release -p ai-device-kernel --bin adk-bench` | p99 < 5 ms gate |

Three-OS matrix: ubuntu-latest, macos-latest, windows-latest.
MSRV job: ubuntu + rust 1.87.

---

## 8. Adding a new module

Per [`AGENTS.md` §2](../AGENTS.md):

- Pure types → `hid/`, `control/`, `ai/`, `types/` (no I/O).
- I/O code → `session`, `client`, `device`, `transport`, etc.
- New `Action` variant → `ai-device-kernel/src/action.rs` +
  bridge mapping in `bridge.rs::apply` if it's a write.
- New wire verb → `android-hid-protocol/src/verb.rs` +
  dispatch in `ai-device-kernel/src/bin/adk.rs`.

---

Last updated: 2026-07-01 (post-ADR-0015 consolidation).
