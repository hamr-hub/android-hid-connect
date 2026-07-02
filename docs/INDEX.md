# Documentation Index — `android-hid-connect`

> The project is an **AI middleware for controlling Android devices**.
> Per [ADR-0015](adr/0015-ai-middleware-consolidation.md) (2026-07-01),
> the workspace is consolidated into 3 crates with a single binary
> `adk` and a single AI façade (`ai-device-kernel`).

---

## First-time readers — pick your path

### Path A — "I want to drive an Android device from an AI agent"

1. [`README.md`](../README.md) — protocol overview + minimal example
2. [`docs/ai-agent-integration.md`](ai-agent-integration.md) —
   `ai_device_kernel::{Plan, Action, Observation}` API + LLM
   integration recipe
3. [`docs/architecture.md`](architecture.md) — module layering +
   the single-binary/single-port/single-protocol invariant

### Path B — "I want to modify the kernel"

1. [`AGENTS.md`](../AGENTS.md) — collaboration rules + allowed/forbidden
2. [`docs/architecture.md`](architecture.md) — module dependencies
3. [`docs/ai-device-kernel-v3-design.md`](ai-device-kernel-v3-design.md) —
   the v3 design that the kernel implements

### Path C — "I want to understand a wire-level decision"

1. [`docs/wire-format.md`](wire-format.md) — byte-level layout
2. [`docs/scrcpy-protocol-compatibility.md`](scrcpy-protocol-compatibility.md) —
   scrcpy v2.7 byte-exact contract + known caveats
3. [`docs/adr/`](adr/) — the 15 ADRs that pin down every
   protocol-level decision

---

## Crates

| Crate | Cargo name | Role | LOC |
| ----- | ---------- | ---- | --- |
| Workspace root | `android-hid-connect` | Byte-exact scrcpy UHID core (library); AI middleware coordinates | ~13 k |
| Protocol types | [`android-hid-protocol`](../android-hid-protocol/) | Wire-level types (`Frame`, `Verb`, error codes, k=v parser) — zero deps | ~1.8 k |
| AI middleware | [`ai-device-kernel`](../ai-device-kernel/) | THE AI façade — 1 binary `adk`, 1 port 9008, 1 protocol, 1 typed `Action` | ~7.3 k |

See ADR-0015 for the consolidation rationale (4 sibling crates
deleted 2026-07-01).

---

## Topics

| Topic | File | Read this when … |
| ----- | ---- | ---------------- |
| Architecture | [`architecture.md`](architecture.md) | you need to know where to add a feature without breaking layering |
| AI agent integration | [`ai-agent-integration.md`](ai-agent-integration.md) | you want to embed the middleware in an LLM agent runtime |
| Bridge module (Action → UHID) | [`ai-device-kernel/src/bridge.rs`](../ai-device-kernel/src/bridge.rs) | you need to map a typed `Action` to a real device input |
| Wire format | [`wire-format.md`](wire-format.md) | you're chasing a byte-level mismatch with scrcpy-server |
| Scrcpy compat | [`scrcpy-protocol-compatibility.md`](scrcpy-protocol-compatibility.md) | you're bumping the pinned scrcpy version or reporting a byte drift |
| v3 kernel design | [`ai-device-kernel-v3-design.md`](ai-device-kernel-v3-design.md) | you want the why behind the 12 typed `Action`s |
| Development loop | [`development.md`](development.md) | you're setting up the dev env, want CI commands, or want to run real-device E2E |
| Performance | [`perf/real-device-call-flow.md`](perf/real-device-call-flow.md) | you want to compare host-adb vs kernel-internal intent→UHID hot paths |

---

## ADRs (Architecture Decision Records)

The 15 ADRs are the authoritative contract for every protocol-level
and middleware-level decision. Read in order for full context:

| # | Title | Pinning down … |
| - | ----- | -------------- |
| [0001](adr/0001-llm-minimum-unit-is-intent.md) | LLM minimum unit is an Intent | the "what" not "how" boundary |
| [0002](adr/0002-flow-control-is-llm-owned.md) | Flow control is LLM-owned | retry/branch/wait live outside the kernel |
| [0003](adr/0003-planning-granularity-is-per-chunk.md) | Planning granularity is per-chunk | bounded chunk size ≤ 4 atomics |
| [0004](adr/0004-error-contract-is-structured-atomicresult.md) | Errors are typed `AtomicResult` | symmetric, value-based error envelope |
| [0005](adr/0005-multi-app-orchestration-hybrid-app-switch.md) | Multi-app orchestration is hybrid | explicit `app_switch` atomic |
| [0007](adr/0007-llm-deployment-is-hybrid.md) | LLM deployment is hybrid | on-device fast path + cloud escalation |
| [0008](adr/0008-egress-policy-text-and-snapshot-only.md) | Egress policy: text + snapshot only | 操作轨迹 never leaves the device |
| [0009](adr/0009-session-state-is-bounded-ondevice-only.md) | Session state is bounded on-device | bounded working memory per task |
| [0010](adr/0010-scenario-library-is-hybrid.md) | Scenario library is hybrid | templates + recordings + fresh-plan fallback |
| [0011](adr/0011-cold-start-strategy-is-tiered.md) | Cold start is tiered | ship-time → per-device → per-user |
| [0012](adr/0012-plan-message-json-wire.md) | Plan message wire format is JSON | zero-translation prompt-engineering |
| [0013](adr/0013-atomic-result-error-enum.md) | Atomic error code enum | fixed 6-variant `AtomicErrorCode` |
| [0014](adr/0014-foreground-app-and-png-snapshot.md) | ForegroundApp + PNG snapshot channels | mandatory foreground state + multimodal vision |
| [0015](adr/0015-ai-middleware-consolidation.md) | **AI middleware consolidation** | **2026-07-01**: 4 crates + root `src/agent/` deleted |

---

## Baselines

- [`v3-phase1-acceptance-2026-06-30.md`](baselines/v3-phase1-acceptance-2026-06-30.md) — Phase 1 acceptance (AC source of truth)
- [`v3-phase6-baseline-2026-06-30.md`](baselines/v3-phase6-baseline-2026-06-30.md) — Phase 6 baseline (latest AI middleware baseline)
- [`v3-phase6-latency-2026-06-30.md`](baselines/v3-phase6-latency-2026-06-30.md) — Phase 6 latency profile (kernel-internal vs host-adb)
- [`v3-acceptance-all-phases-2026-06-30.md`](baselines/v3-acceptance-all-phases-2026-06-30.md) — full multi-phase acceptance roll-up
- 2 × `v3-phase6-sm-g9910-*.png` — real-device screenshots

---

## Companion files

| File | What |
| ---- | ---- |
| [`../AGENTS.md`](../AGENTS.md) | collaboration rules (allowed/forbidden + AI-agent meta-rule) |
| [`../ACCEPTANCE.md`](../ACCEPTANCE.md) | AC-C/H/S/T/R acceptance + real-device regression log + history |
| [`../CHANGELOG.md`](../CHANGELOG.md) | keep-a-changelog log (release-please reads this) |
| [`../Cargo.toml`](../Cargo.toml) | workspace manifest |
| [`../.github/workflows/ci.yml`](../.github/workflows/ci.yml) | CI matrix (ubuntu/macos/windows + MSRV + adk-bench smoke) |

---

Last updated: 2026-07-01 (post-ADR-0015 consolidation).