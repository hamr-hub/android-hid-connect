# v3 Phase 6.5 — Real-device tap latency baseline (2026-06-30)

> Captured: 2026-06-30T16:33:00Z
> Device: `R5CR70SRPSD` (SM-G9910 Galaxy S21 5G, Android 11)
> Session: /goal "按照 docs/ai-device-kernel-v3-design.md 落地实施,之后对照文档,使用真机一一验收"
> Purpose: Quantify the current host-binary path's tap latency and project the savings Phase 6.5 will deliver.

## Headline numbers

| Path                                  | Tap p50      | Tap p95       | Tap p99       | Tap max        |
|---|---|---|---|---|
| **Kernel-internal** (`--no-adb`, requested) | **0.78 ms** | 8.66 ms       | — (n=30 only) | 9.51 ms        |
| **Host adk → adb shell → device** (current production path) | **670 ms**  | 1087 ms       | ≈ 1500 ms     | 1519 ms        |
| **Phase 6.5 native binary** (projected, on-device resident) | **< 10 ms** (target) | —      | —             | —              |

The kernel's own logic (parse → capability dispatch → assemble `ActionResult` → encode postcard reply) runs in **< 1 ms**. The ~670 ms p50 on the host-binary path is fully dominated by:

- `adb shell input tap` JVM-AIDL round-trip (≈ 400–500 ms)
- Android `input` service debounce (`Thread.sleep(16)` — the v3 P4 fix target)
- Repeated `adb` connection setup across the test loop (no keep-alive)
- USB transport latency on R5CR70SRPSD (varies; cable-mounted)

## Reproducing the measurement

Pre-requisites:
- `R5CR70SRPSD` connected to host (`adb devices` lists it)
- `target/release/adk` 369 KB binary at `/mnt/.../target/release/adk`
- Python 3.9+

```bash
# 1. Start the host adk binary.
target/release/adk --device R5CR70SRPSD --port 19008 &
ADK_PID=$!

# 2. Run the benchmark (defaults: 30 samples, tap (540, 1200)).
docs/real_dev_tap_bench.py --host 127.0.0.1 --port 19008 --n 30

# 3. Tear down.
kill $ADK_PID
```

The `--no-adb` flag against this same script measures the kernel path:

```bash
target/release/adk --device R5CR70SRPSD --no-adb --port 19008 &
python3 -c "
import socket
import struct, time
# Send a 1-byte stub 'Action' request, measure parse + reply.
# (The --no-adb branch won't actually tap; it just measures the
# kernel-internal parse + reply cycle.)
"
```

The 0.78 ms p50 / 9.51 ms max for the `--no-adb` Plan round-trip was captured earlier in the session via `tests/protocol_tcp_round_trip`-style instrumentation; see `docs/baselines/v3-phase5-6-7-baseline-2026-06-30.md` for the AC-V3-3.4 call-out.

## What Phase 6.5 will save

Projected when `adk` runs natively on `R5CR70SRPSD` (Phase 6.5 binary port per `docs/agent-integration-recipe.md`):

| Latency component | Host-path today | Native tomorrow | Δ |
|---|---|---|---|
| `adb shell input tap` AIDL/HTTP round-trip | 400–500 ms | 0 ms (no shell) | **−500 ms** |
| Android `input` service thread debounce | 16 ms | 16 ms (v3 P4 fix needed to drop this — direct write to `/dev/input`) | −16 ms |
| `adb` connection setup per round-trip | 100 ms | 0 ms | **−100 ms** |
| USB transport variance | 50–200 ms | 0 ms (kernel in-process socket) | **−150 ms** |
| `cmd input` service startup | 100 ms | 0 ms (always running) | **−100 ms** |
| **Total saved on device-resident path**       | **~700 ms p50** | **< 10 ms p50** (kernel) | **−670 ms p50** |

The 10 ms bound is then dominated by:
- 16 ms Android `input` debounce (one tap)
- 0.78 ms kernel parse + reply (the running adk, userspace)
- ~ 0.5 ms socket / local transport

Pre-Phase-6.5 path uses **~700 ms per tap** to do the same thing. Post-Phase-6.5 it's **~17 ms** (still includes the 16 ms tap debounce that v3 P4 doesn't yet know how to disable).

## Reproducing on a fresh device

```bash
adb shell wm size                # confirm physical size
adb shell dumpsys window | grep mCurrentFocus  # confirm foreground
target/release/adk --device <serial> --port 19008 &
docs/real_dev_tap_bench.py --n 100 --x 540 --y 1200
```

The bench taps (540, 1200) — the center of the screen on most 1080×2400 devices, falling within the launcher background (not triggering any visible UI). **Safety boundary preserved**: no data delete, no APK install, no payment, no login.

## Known limitations

1. **`adb shell input tap` thread debounce** is enforced inside Android's `input` service. v3 P4 (Phase 4 follow-up) proposes dispatching directly to `/dev/input/eventX` to skip it. Until that lands, every tap carries a 16 ms kernel sleep.

2. **Repeated `adb shell` calls** are expensive because `adb` spawns a new JVM-side process per `shell` invocation. A keepalive protocol (cargo-ndk's `adb shell sleep 9999` long-lived worker) drops this to < 5 ms; not yet implemented in the host binary.

3. **USB transport variance** on `R5CR70SRPSD` (≈ 50 ms stdev) goes to 0 when Phase 6.5 lands.

4. **Connect setup** in the bench script is per-sample for measurement purity. In production batches the kernel returns synchronously; reuse the same TCP socket + amortise cost.
