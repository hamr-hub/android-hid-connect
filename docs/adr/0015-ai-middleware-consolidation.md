# ADR-0015: AI middleware consolidation — single AI façade

**Date**: 2026-07-01
**Status**: Accepted
**Deciders**: AI Device Kernel maintainers
**Supersedes**: implicit workspace decisions in ADR-0012 (plan-message-json-wire)

## Context

`android-hid-connect` started as a pure-Rust port of scrcpy's UHID
control surface (a library crate). Over 7 phases (see
`docs/ai-device-kernel-v3-design.md`), the project evolved into an
**AI middleware** for controlling Android devices — the kernel
sits between an LLM and the device, translating Intent → typed
`Action` → UHID bytes.

Phase 6.5 (2026-06-30) shipped the AI middleware in three sibling
crates plus the root crate:

- `android-hid-protocol` — wire types
- `android-hid-connect` (root) — UHID core + a 20.6 kLOC `src/agent/` facade
- `android-hid-daemon` — on-device daemon (1.7 kLOC)
- `android-hid-agent` — host-side typed facade (3.3 kLOC)
- `android-hid-cli` (`ah`) — CLI stub (42 LOC)
- `android-hid-py` (`android_hid`) — Python stub (33 LOC)
- `ai-device-kernel` — the actual AI middleware (7.3 kLOC + 968 LOC tests)

Audit of the workspace found **zero internal callers** for
`android-hid-daemon`, `android-hid-agent`, `android-hid-cli`, and
`android-hid-py` — they were referenced only in docs and in each
other's Cargo.toml descriptions. The root crate's `src/agent/`
facade duplicated the AI middleware surface (AgentAction /
AgentControlSession vs Action / ActionInjector) and was unused
by any `examples/` or sibling crate.

Three SLOs drive this consolidation:

1. **Performance** — fewer crates → fewer Cargo.lock entries →
   `cargo build` time -50%; the kernel-internal intent→UHID hot
   path now goes `ai-device-kernel::bridge` →
   `android-hid-connect::HidSession` directly, no daemon
   socket hop.
2. **Stability** — fewer surface areas → fewer breaking-change
   release channels. The `adk` binary is the single deployment
   artifact.
3. **AI compatibility** — single façade means LLM integration docs
   point to one crate, one `Action` enum, one
   `AtomicErrorCode` (ADR-0013).

## Decision

Consolidate to a 3-crate workspace with a single AI façade:

| Crate | Role |
| ----- | ---- |
| `android-hid-protocol` | Wire types (no I/O, no deps) |
| `android-hid-connect` | Byte-exact scrcpy UHID core + `bridge` consumer |
| `ai-device-kernel` | **THE AI middleware**: 1 binary (`adk`), 1 port (9008), 1 protocol, 1 typed `Action`, 1 `AtomicErrorCode` |

**Removed** (2026-07-01, no compatibility shims — see Consequences):

- `android-hid-daemon/` (entire crate)
- `android-hid-agent/` (entire crate)
- `android-hid-cli/` (entire crate)
- `android-hid-py/` (entire crate)
- `src/agent/` from root (8 files, 20.6 kLOC)
- `docs/agent-v2-design.md`, `docs/roadmap-exceed-handsets.md`,
  `docs/comparison-with-handsets.md` (out-of-scope historical)
- `docs/adr/0006-plan-wire-format-is-json.md` (superseded by
  ADR-0012, the JSON wire format)
- `docs/baselines/v3-phase{2,3,4,5.1,5-6-7}-*` (intermediate; kept
  phase 1 + phase 6)

**Added**:

- `ai-device-kernel/src/bridge.rs` — typed `Action` → UHID byte
  execution path (replaces the un-wired TODOs in `Action::inject_hid`)
- `ai-device-kernel/src/bin/adk-bench.rs` — in-process micro-bench
  for the intent→UHID hot path (CI gate, p99 < 5 ms)
- This ADR (0015)

## Consequences

### Positive

- **One binary to deploy** — `adk` (≤ 4 MB on release, opt-level z +
  LTO + strip per workspace profile).
- **One port to forward** — `adb forward tcp:9008 localabstract:adk`
  replaces the dual-port 27183 (scrcpy) + 9008 (daemon) setup.
- **One façade for LLM integration** — `ai_device_kernel::{Plan,
  Action, ActionResult, Observation}`. No more choosing between
  `AgentControlSession` and `ActionInjector`.
- **Bridge directly wired** — `ai-device-kernel::bridge::ActionInjector`
  is the single function that turns an `Action` into Android input,
  measured at p99 = 5 µs in-process (`cargo run --release --bin
  adk-bench`).
- **CI surface -40%** — 3 release-please targets instead of 5.

### Negative

- **Breaking change for any external user of `AgentControlSession`
  / `AgentAction` / etc.** — pin to `v0.1.x-legacy-agent` in
  git history if you depended on the old facade. The git tag
  `pre-adr-0015` marks the last commit before this consolidation.
- **No CLI replacement** — the `ah` binary is gone. Operators use
  `adb forward tcp:9008 localabstract:adk + netcat` for ad-hoc
  testing, or the `adk-bench` binary for performance verification.
- **No Python SDK yet** — PyO3 binding was a stub. Will land as a
  separate workspace crate when there's concrete demand (the
  `adk` postcard/4-verb protocol is straightforward to bind from
  Python via `pyo3-postcard` or any postcard-aware library).

### Reversibility

The legacy crate directories are still in git history. `git tag
pre-adr-0015` points at the last commit before the consolidation;
the four removed crates can be re-checked-out if a future use case
demands them.

## References

- v3 design: [`docs/ai-device-kernel-v3-design.md`](../ai-device-kernel-v3-design.md)
- Bridge module: [`ai-device-kernel/src/bridge.rs`](../../ai-device-kernel/src/bridge.rs)
- Bench binary: [`ai-device-kernel/src/bin/adk-bench.rs`](../../ai-device-kernel/src/bin/adk-bench.rs)
- Phase 6.6 baseline: [`docs/baselines/v3-phase6-baseline-2026-06-30.md`](../baselines/v3-phase6-baseline-2026-06-30.md)