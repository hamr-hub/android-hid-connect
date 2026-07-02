# 0013 — `ActionResult` gains structured `code: Option<AtomicErrorCode>` and `retryable: bool`

## Context

`ai-device-kernel/src/action.rs` defined `ActionResult { id, landed, ground_truth, elapsed_ms }` (commit 2026-06-30). The `landed: bool` discriminated success/failure only; failure semantics lived in the `ground_truth.events` blob as free-text `DeviceEvent::ActionCompleted { landed: false, ... }` strings.

Grill session (2026-07-01) ratified ADR 0004: structured `AtomicResult { ok | code, reason, retryable, duration_ms }`. The LLM must decide retry vs branch vs abort without re-deriving failure semantics from a free-text reason.

## Decision

**Two new fields on `ActionResult`** (appended to the end, post-`elapsed_ms`, per PRD-A AC-1 backwards-compat):

```rust
pub struct ActionResult {
    pub id: ActionId,
    pub landed: bool,
    pub ground_truth: GroundTruth,
    pub elapsed_ms: u32,
    pub code: Option<AtomicErrorCode>,   // ← new
    pub retryable: bool,                  // ← new
}
```

`AtomicErrorCode` is a fixed 6-variant enum:

| Variant | Meaning | Retryable? |
|---|---|---|
| `SelectorMiss` | Selector (resource-id/text/content-desc) didn't match any node | in principle (UI drift) |
| `KernelError` | Kernel-internal error (capability refused, panic caught) | depends |
| `Timeout` | Daemon-side `deadline_ms` elapsed | yes (network hiccup) |
| `PermissionDenied` | Capability profile (e.g. `phantom`) refused | no |
| `AppNotForeground` | `app_switch` needed first | no |
| `NotImplemented` | Variant not yet wired | no (needs kernel update) |

JSON serialization uses `snake_case` discriminators (`"selector_miss"`, `"app_not_foreground"`, …). Postcard keeps the underlying u8.

`retryable: bool` is **advisory** — the LLM is free to ignore (e.g. after N retries it should abort).

`AtomicResult::is_structured_failure() -> bool` is the convenience check: `!landed && code.is_some()`.

## Why

1. **Aligned with ADR 0004** — structured `AtomicResult` shape is grill session Q5 decision.
2. **Backwards compatible** — fields are **appended**; postcard encoding is tag-based per-field, so old daemons that only know the 4-field shape will skip the unknown tags (postcard 1.x default behaviour) and succeed. New daemons reading old wire get `code: None, retryable: false` defaults. **Critical**: `#[serde(skip_serializing_if = "Option::is_none")]` is **NOT** applied to `code` — keeping a 1-byte `None` tag maintains struct-fixed-field decoder symmetry. Without it, decode panics with `DeserializeUnexpectedEnd` (verified by test).
3. **Six variants cover the LLM's decision surface** — the LLM can pattern-match on `code` to decide retry / branch / abort without reading free-text `reason` strings.
4. **No new `AtomicErrorCode` without a SemVer minor bump** — `Plan::version` (ADR-0012) gates evolution. The variant list is closed.

## Considered Options

- **A. Append fields (this ADR)** — `code: Option<AtomicErrorCode>` + `retryable: bool`. Selected.
- **B. Replace `landed: bool` with `AtomicResult` enum** — breaks every existing `ActionResult` literal across 5+ files. Rejected: violates the additive-only contract (PRD-A AC-1).
- **C. Add fields mid-struct** — Rejected: postcard's tag encoding is field-position-independent but tests in the existing codebase (`src/plan.rs` test module) would break compile-error-style.

## Consequences

- All `ActionResult` literals across the workspace now require `..Default::default()` or explicit `code: None, retryable: false`. `impl Default for ActionResult` is provided.
- `is_structured_failure()` is the new canonical LLM flow-control predicate.
- Any consumer of `ActionResult` that pattern-matches on the struct's fields will need to update. In-tree consumers are limited to `ai-device-kernel` itself; out-of-tree consumers are small (the `ai-device-kernel` commit is one day old at the time of this ADR).
- Adding a 7th variant (`biometric_required`?) is a SemVer minor bump on `Plan::version` per ADR-0012.

## 关联

- 上游:ADR 0001(Intent),ADR 0002(flow control 归 LLM)
- 下游:ADR 0008(EscalationPolicy uses `code` to decide escalation)
- 验证:`tests/phase5_2_alignment.rs::action_result_*` — 4 tests, all green
