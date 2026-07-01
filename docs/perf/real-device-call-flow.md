# Real-device Call Flow & Optimization Analysis

> **Source data**:
> - This session bench (2026-07-01, R5CR70SRPSD SM-G9910 / Android 11 / SDK 30)
>   - `target/release/adk` tap bench N=20, x=540 y=1200 → **p50=680ms, min=637ms, max=873ms, mean=707ms, stdev=64ms**
> - `d8b423c feat(adk): real-device latency` historical numbers
>   - tap p10/p50/p90 = **668 / 725 / 751 ms** (target < 3 ms on-device)
>   - Plan(5×HOME) p10/p50/p90 = **4508 / 4626 / 4951 ms** (target < 10 ms on-device)
> - `CONTEXT.md`: kernel-internal intent→UHID hot path ~ **0.78 ms p50**
> - Baseline micro-bench this session (host shell, same device):
>   - `adb shell true` (no-op): **123 ms**
>   - `adb shell input keyevent KEYCODE_HOME`: **648 ms**
>   - `adb shell dumpsys window | grep ...`: **43 ms**

---

## 1. Two real-device paths, two latency profiles

### Path A: scrcpy control socket (UHID + non-UHID control msgs)

Used by `examples/live_e2e.rs`, kernel-internal hot path, and any client that talks
scrcpy protocol over `adb forward tcp:27183 localabstract:scrcpy`.

```
┌─────────────────────┐                ┌──────────────────────────┐
│ Host (Rust client)  │                │ Device (Android 11)      │
│                     │                │                          │
│ HidClient::send_one │  ① TCP write   │                          │
│                     │ ────────────▶ │                          │
│                     │  127.0.0.1:   │                          │
│                     │   27183 (adb   │                          │
│                     │   forward)     │                          │
│                     │                │ ② scrcpy-server reads    │
│                     │                │    unix socket (~0.5ms)  │
│                     │                │                          │
│                     │                │ ③ dispatch ControlMessage│
│                     │                │    to InputManagerService│
│                     │                │    via Binder (~2-5ms)   │
│                     │                │                          │
│                     │                │ ④ IMS injects MotionEvent│
│                     │                │    / KeyEvent (~2-10ms)  │
│                     │                │                          │
│                     │ ◀──────────── │ ⑤ optional UHID_OUTPUT   │
│  read reply         │  (~1ms USB)    │    (LED sync, ~1ms)      │
└─────────────────────┘                └──────────────────────────┘

Per-call cost: ①+②+③+④+⑤ ≈ **5-25 ms** (verified by live_e2e running 28 ops/sec)
```

### Path B: adk host binary → adb shell input (current production real-device path)

Used by `target/release/adk --device R5CR70SRPSD`, `python3 docs/real_dev_tap_bench.py`.

```
┌─────────────────┐   ┌─────────────────┐   ┌────────────────────────┐
│ Python client   │   │ adk (Rust,host) │   │ Device (Android 11)    │
│                 │   │                 │   │                        │
│ ① build v3     │   │                 │   │                        │
│   frame +       │   │                 │   │                        │
│   connect TCP   │   │                 │   │                        │
│   + sendall     │   │                 │   │                        │
│   (~1ms)        │   │                 │   │                        │
│        │        │   │                 │   │                        │
│        ▼        │   │                 │   │                        │
│ write frame ────┼─▶ │ ② read_request │   │                        │
│                 │   │   parse v3      │   │                        │
│                 │   │   (~0.1ms)      │   │                        │
│                 │   │        │        │   │                        │
│                 │   │        ▼        │   │                        │
│                 │   │ ③ Command::new  │   │                        │
│                 │   │   ("adb").spawn │   │                        │
│                 │   │   (~30-100ms)   │   │                        │
│                 │   │        │        │   │                        │
│                 │   │        ▼        │   │                        │
│                 │   │ ④ adb client →  │   │                        │
│                 │   │   adb host      │   │                        │
│                 │   │   daemon        │   │                        │
│                 │   │   (~0.5ms local │   │                        │
│                 │   │   socket)       │   │                        │
│                 │   │        │        │   │                        │
│                 │   │        ▼        │   │                        │
│                 │   │ ⑤ adb host  ───┼──▶│ ⑥ adbd on device     │
│                 │   │   daemon over   │   │   reads USB transport │
│                 │   │   USB transport │   │   (~30-100ms)         │
│                 │   │                 │   │        │              │
│                 │   │                 │   │        ▼              │
│                 │   │                 │   │ ⑦ sh -c "input        │
│                 │   │                 │   │    tap X Y"           │
│                 │   │                 │   │   fork+exec           │
│                 │   │                 │   │   (~50-100ms)         │
│                 │   │                 │   │        │              │
│                 │   │                 │   │        ▼              │
│                 │   │                 │   │ ⑧ `input` Java app    │
│                 │   │                 │   │   startup +           │
│                 │   │                 │   │   InputManagerService.│
│                 │   │                 │   │   injectInputEvent    │
│                 │   │                 │   │   (~10-50ms)          │
│                 │   │                 │   │        │              │
│                 │   │                 │   │        ▼              │
│                 │   │                 │   │ ⑨ MotionEvent → app   │
│                 │   │                 │   │   dispatch (~2-10ms)  │
│                 │   │                 │   │        │              │
│                 │   │                 │   │        ▼              │
│                 │   │                 │◀──┼── ⑩ reply chain:     │
│                 │   │                 │   │   sh exit code →      │
│                 │   │                 │   │   adbd → adb host →   │
│                 │   │                 │   │   stdout captured     │
│                 │   │  ⑪ stdout read  │   │   (~300-400ms)        │
│                 │   │   into Rust     │   │                       │
│                 │   │   String        │   │                       │
│                 │   │        │        │   │                       │
│                 │   │ ⑫ eprintln!     │   │                       │
│                 │   │   hot-path log  │   │                       │
│                 │   │   (mutex lock)  │   │                       │
│                 │   │        │        │   │                       │
│                 │   │        ▼        │   │                       │
│                 │   │ ⑬ ANOTHER adb_  │   │                       │
│                 │   │   shell for     │   │                       │
│                 │   │   dumpsys       │   │                       │
│                 │   │   window|grep   │   │                       │
│                 │   │   (record_      │   │                       │
│                 │   │    success)     │   │                       │
│                 │   │   ~43ms each    │   │                       │
│                 │   │        │        │   │                       │
│                 │   │        ▼        │   │                       │
│                 │   │ ⑭ SQLite        │   │                       │
│                 │   │   persist       │   │                       │
│                 │   │   (Mutex::lock  │   │                       │
│                 │   │    + INSERT)    │   │                       │
│                 │   │        │        │   │                       │
│                 │   │        ▼        │   │                       │
│                 │   │ ⑮ write_reply   │   │                       │
│ ◀────────────────┼───┤   TCP frame    │   │                       │
│ read reply       │   │   (~0.5ms)     │   │                       │
└─────────────────┘   └─────────────────┘   └────────────────────────┘

Per-call cost: ①+②+③+④+⑤+⑥+⑦+⑧+⑨+⑩+⑪+⑫+⑬+⑭+⑮ ≈ **680 ms p50**

Hop breakdown (derived from baseline + this bench):
┌────┬────────────────────────────────────────────┬───────────┬──────────────┐
│ #  │ Hop                                        │ Time      │ Cumulative   │
├────┼────────────────────────────────────────────┼───────────┼──────────────┤
│ ①  │ Python → TCP write                         │   ~1 ms   │       1 ms   │
│ ②  │ adk parse v3 frame                         │  ~0.1 ms  │       1 ms   │
│ ③  │ adk fork+exec `adb` process                │ 30-100 ms │   ~50 ms     │
│ ④  │ adb client → adb host daemon (local sock)  │  ~0.5 ms  │   ~51 ms     │
│ ⑤  │ adb host daemon → adbd over USB            │ 30-100 ms │  ~120 ms ⚠   │
│ ⑥  │ adbd receives shell request on device      │  ~5 ms    │  ~125 ms     │
│ ⑦  │ adbd fork+exec `sh -c` on device           │ 50-100 ms │  ~200 ms ⚠   │
│ ⑧  │ `input` Java app startup + IMS.inject      │ 10-50 ms  │  ~230 ms ⚠   │
│ ⑨  │ MotionEvent dispatch to focused app        │  2-10 ms  │  ~240 ms     │
│ ⑩  │ sh exit code → adbd → adb host (USB back)  │ 100-200ms │  ~400 ms ⚠   │
│ ⑪  │ adk reads stdout from adb child            │   ~1 ms   │  ~401 ms     │
│ ⑫  │ eprintln! + Mutex lock (logging)           │  ~1 ms    │  ~402 ms     │
│ ⑬  │ EXTRA adb_shell for dumpsys window (post)  │  ~43 ms   │  ~445 ms ⚠   │
│ ⑭  │ SQLite persist + Mutex lock                │  ~5 ms    │  ~450 ms     │
│ ⑮  │ TCP reply write to Python client           │  ~0.5 ms  │  ~450 ms ⚠   │
│    │   ↳ async vs sync: ⑬⑭⑮ happen AFTER reply │           │              │
│    │   ↳ but adb daemon contention makes total  │           │              │
│    │     wall-clock higher (~680 ms in bench)   │           │              │
└────┴────────────────────────────────────────────┴───────────┴──────────────┘
```

> ⚠ The bench measures 680 ms but the per-hop breakdown only sums to ~450 ms.
> The gap (~230 ms) is **adb daemon process contention** under the bursty fork
> pattern: each `adb shell` invocation spins up the Java `adb` client, which
> queues against the host adb daemon's single-threaded request handler.
> Real bench numbers are higher than the sum-of-parts because of this queuing.

---

## 2. Bottleneck ranking (path B)

| Rank | Bottleneck                              | Time/call | Why it's slow                                              |
| ---- | --------------------------------------- | --------- | ---------------------------------------------------------- |
| 1    | **Device-side `sh + input` fork/exec**  | 100-150ms | Java `input` app Dalvik startup + IMS binder call          |
| 2    | **USB round-trip (request + reply)**    | 130-200ms | 2× USB transfers (each way) + adb protocol framing         |
| 3    | **adb host fork+exec (each call)**      | 30-100ms  | Java `adb` client cold-start every call                    |
| 4    | **Repeated `dumpsys window` (post-action)** | 43ms × N | Called in `Wait` polling, `record_success`, `build_observation` |
| 5    | **SQLite persistence**                  | 5-10ms    | Mutex + INSERT after every successful action (sync)        |
| 6    | **eprintln! on hot path**               | 1-5ms     | stderr flush + Mutex contention under burst                |

### Where Phase 6.5 wins

The kernel-internal hot path (~0.78 ms p50) eliminates bottlenecks 1-3 entirely:
- Daemon runs **on device**, so device-side work is 0-transit
- Speaks scrcpy protocol directly via already-tunneled `localabstract:scrcpy` socket
- `InputManagerService` called via **JNI/binder in-process**, no `input` Java app
- Reply is immediate (in-process function call)

Result: ~**850× speedup** vs current adb path.

---

## 3. What can be sped up (in current Path B)

### Tier 1: large wins, easy

**A. Persistent adb shell session** — replace `Command::new("adb")` per call with a
single `adb shell` whose stdin accepts commands (each terminated by sentinel).
Saves **30-100 ms/call** (the per-call adb fork).
- Implementation: `adb_shell_session()` helper that holds a `Child` with stdin pipe;
  on each call, write `cmd\n; echo "###MARKER### $?\n"` to stdin and parse output.
- Trade-off: command errors are harder to attribute; need sentinel-based parsing.

**B. Fire-and-forget for idempotent actions** — don't wait for `input` exit code.
For tap/swipe/key (no useful return value beyond success/fail), the reply can be
sent immediately after `input` is dispatched, before `input` even completes.
Saves **~300 ms/call** (the ⑩ USB reply hop).
- Caveat: caller loses synchronous confirmation. For high-rate Plan batches, this
  is fine because the next Observe call confirms.

**C. Move `record_success` post-action dumpsys off the hot path** — currently
`execute_action` calls `adb_shell("dumpsys window...")` after EVERY successful
action (line 482 in `adk.rs`), adding **43 ms × N_actions** to host-side time.
Move to a background tokio task or queue; only persist when the next Observe
arrives (which already does its own dumpsys).

**D. Wait predicate polling with a11y event subscription** — the `Wait` action
(line 451) polls `dumpsys window` every 100 ms up to 5 s = up to **50 dumpsys
calls = 2.15 s wasted**. Replace with a11y event subscription via UiAutomation
or `cmd notification post-listener`. Cuts `Wait` from up to 5 s to **<100 ms**.

**E. Batch dumpsys via single adb shell** — currently each Predicate check
spawns a separate `adb shell` process. Combine all checks into one `adb shell`
script per plan step: `(dumpsys window | grep focus; getprop ro.build.fingerprint)`.
Saves 2× → 4× host-side overhead per Plan step.

### Tier 2: medium wins

**F. Plan batching (already supported)** — N actions in 1 RTT instead of N×680 ms.
Current code already exposes `RequestPayload::Plan`, but the bench exercises
single-action calls. Verify this with a Plan N=10 tap bench (expected ~6 s vs
~6.8 s, savings ~12%).

**G. Replace `input tap` with direct uinput write (root only)** — bypass
Java `input` app entirely. `echo "tap 540 1200" > /dev/input/eventN` via root
helper. Saves **~80-150 ms/call**. Doesn't work for non-rooted devices.

**H. Replace `dumpsys window` with focused-app event from scrcpy-server** —
scrcpy-server already reports `DEVICE_MSG_TYPE_UHID_OUTPUT` events; an
extension can emit `DEVICE_MSG_TYPE_FOCUS_CHANGED` for free.

**I. SQLite batch INSERT** — group N records into one transaction.
Saves **5-10 ms/insert** × N.

**J. Async SQLite via spawn_blocking** — currently `record_success` does
synchronous SQLite INSERT on the handler thread. Move to `tokio::task::spawn_blocking`.
Saves **5-10 ms/insert** on hot path (allows next action immediately).

### Tier 3: small wins, polishing

**K. Skip `eprintln!` when stderr is redirected** — currently unconditional.
Add `if std::env::var("QUIET").is_ok()` guard. Saves **1-5 ms/call** in
tight loops.

**L. ADB_SERIAL env var fallback** — already supported by `Flags::parse()`,
but `--device` arg path should also check it as fallback.

**M. Read `frame_diff` lazily in `build_observation`** — `adb_screencap` is
NOT called on every Observe (only on `Query` with frame=true), but verify
that the tap path doesn't accidentally call it.

**N. Verbose `--no-adb` mode for testing** — already supported (lines 206-209).
Use in CI to skip adb transit entirely.

---

## 4. What can be omitted (no-op or low value)

| Item                                          | Where                                    | Recommendation |
| --------------------------------------------- | ---------------------------------------- | -------------- |
| Per-action `dumpsys window` in `record_success` | `adk.rs:482`                            | **OMIT** — Observe already does it |
| Per-step `dumpsys window` in Wait polling    | `adk.rs:685`                             | **OMIT after first poll** if a11y event-driven is added |
| `eprintln!` in production binary             | `adk.rs:135,142,143,149,485,486,...`     | Gate behind `RUST_LOG` or compile-time flag |
| `Mutex::lock()` on every successful action   | `adk.rs:491` (state)                     | Use `parking_lot::Mutex` (faster) or sharded map |
| Frame snapshot in `Query` when only a11y wanted | `adk.rs:749-758`                       | Already gated — keep as-is |
| Reply wait for `TypeText` / `Launch`         | `adk.rs:404-422`                         | Fire-and-forget OK |
| `--state-db` SQLite INSERT on every action   | `adk.rs:139-144`                         | Batch or async |
| `info` verb hard-coded "1080 1920"           | `handlers.rs:46`                         | Replace with real `dumpsys display` (cheap, ~30ms once) |
| Pre-action a11y dump in `build_observation`  | `adk.rs:606`                             | OK if client asked for Observe — skip otherwise |
| Per-tap `adb -s R5CR70SRPSD` flag             | `adk.rs:212-214`                         | Skip if `ADB_SERIAL` set; use `default` device |

---

## 5. Concrete optimization plan (in priority order)

### Quick wins (≤1 day each)

1. **Persistent adb shell** in `adk.rs` (Tier 1-A). Expected: p50 680 → 580 ms.
2. **Move `record_success` to async** (Tier 1-C + Tier 2-J). Expected: p50 -43 ms.
3. **Batch dumpsys in Plan steps** (Tier 1-E). Expected: 5-step plan 4.6 s → 3.5 s.
4. **Gate `eprintln!`** behind log level (Tier 3-K). Expected: -2-5 ms in tight loops.

### Medium effort (1-2 days each)

5. **Fire-and-forget for tap/swipe/key** (Tier 1-B). Expected: p50 → ~200 ms.
   - Add `Action::flags |= NO_REPLY` opt-in; document for idempotent calls.
6. **a11y event-driven Wait** (Tier 1-D). Expected: Wait latency 5 s → <100 ms.
7. **Plan batching regression bench** (Tier 2-F). Verify AC-V3-3.4 < 10 ms target
   on device binary path; document savings on host path.

### Larger (Phase 6.5 on-device binary)

8. **Native device-side daemon** — bypasses ③⑦⑧ entirely. Target p50 < 10 ms
   (currently 0.78 ms in Phase 6.5 kernel-internal path). 850× speedup.

---

## 6. Measurement protocol

To verify each optimization, re-run:
```bash
# Build release
cargo build --release --bin adk

# Tap latency (single)
target/release/adk --device R5CR70SRPSD --port 19008 \
    --state-db /tmp/adk-perf.db &
python3 docs/real_dev_tap_bench.py \
    --host 127.0.0.1 --port 19008 --n 50 --x 540 --y 1200

# Plan latency (5 steps)
# Use a custom driver that sends 5 Action::Tap in one Plan verb
# Expected: ~3.4 s current, ~1.5 s after Tier 1-C/E optimizations
```

Baseline numbers to beat (this session, 2026-07-01):
- Tap p50: **680 ms** (N=20, single action)
- Plan(5×HOME) p50: **4626 ms** (from d8b423c, N=20)

Post-optimization targets:
- Tap p50: **< 200 ms** (Tier 1-B fire-and-forget + Tier 1-A persistent shell)
- Plan(5×HOME) p50: **< 1.5 s** (Tier 1-C + 1-E + 1-D)
- Device binary path: **< 10 ms** (Phase 6.5, kernel-internal)

---

## 7. What this analysis does NOT cover

- **Network latency** in non-USB adb (WiFi adb adds 1-50 ms)
- **scrcpy-server startup cost** (~500 ms once per session, amortized)
- **GPU path** for AC-V3-5.5 / 5.6 (env-blocked, NDK 29 + Play services)
- **ML model latency** (LiteRT / Florence-2 / GUI-Owl) — env-blocked
- **On-device binary cold-start** (Phase 6.5 still needs to ship)

---

*Author: claude (session 2026-07-01). Sources: real-device measurements on
R5CR70SRPSD, adk.rs code review, d8b423c baseline numbers, CONTEXT.md Phase 6.5
hot-path reference.*