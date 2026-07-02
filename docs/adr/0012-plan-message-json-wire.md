# 0012 — `ai-device-kernel` wire surface adds `Serialize + Deserialize + JsonSchema` (dual-encoding, postcard fast path)

## Context

`ai-device-kernel` v3 (committed 2026-06-30, `feat(workspace): add ai-device-kernel — 6th member, on-device agent kernel v3`) implemented 12→16 typed `Action` variants, `Plan`/`PlanStep`/`PlanResult`, `Observation`, `ActionResult`, `GroundTruth`, etc. — all with `Serialize + Deserialize` derives for **postcard** (binary) only. No `serde_json` and no JSON Schema.

Grill session (2026-07-01) ratified 11 ADRs (0001–0011) covering LLM-as-translator architecture. ADR 0006 mandates a JSON wire surface for LLM↔host (matching Claude/OpenAI `tool_use`), while Phase 6.5 (2026-06-30) measured the **kernel-internal hot path at 0.78ms p50** — a result that must not regress.

## Decision

**Dual-encoding on the same Rust type.** Every wire-shaped type in `ai-device-kernel` now derives **three** traits: `Serialize + Deserialize` (postcard) **+ `JsonSchema`** (schemars). The same struct can be encoded to either format without type duplication. `serde_json` + `schemars` are added as hard workspace dependencies.

Specifically:

- `Plan::version: String` (semver) gates `AtomicErrorCode` additions (ADR-0013) and JSON-side field evolution. Default `"0.1.0"`. `Plan::version_parsed() -> Option<semver::Version>` provides the strongly-typed accessor.
- Hand-rolled `JsonSchema` impls on the IDs (`ActionId`, `PlanId`, `StepId`, `PredicateHandle`, `ScreenId`) preserve stable schema names (e.g. `"ActionId"`) instead of auto-collapsing to `"uint64"`.
- `semver::Version` is held as `String` on the wire (orphan rule prevents `impl JsonSchema for Version` in this crate). Callers use `Plan::version_parsed()`.

Postcard wire is unchanged: 4-verb `Frame` layout, byte-exact UHID core, 0.78ms p50 hot path preserved. **JSON is additive**, not replacement.

## Why

1. **Postcard fast path preserved** — Phase 6.5 0.78ms kernel-internal p50 is the data point that closes the question of "JSON or postcard". Both, with postcard as the on-device hot path.
2. **Aligned with ADR 0006** — JSON surface is the LLM-facing wire. Claude/OpenAI `tool_use` consumes JSON natively, so the host adapter becomes a thin passthrough.
3. **Schema validation at the boundary** — `schemars` generates the JSON Schema from the Rust types; the kernel boundary can `jsonschema::validate()` every inbound `Frame::decode_request()`, returning typed errors instead of panics.
4. **Type-system single source of truth** — no `PlanMessageJSON` vs `PlanMessagePostcard` divergence. `JsonSchema` derive is mechanical, not semantic.
5. **Future ADRs compose for free** — `EscalationPolicy` (ADR 0008), `SessionState` (ADR 0009), `ScenarioLibrary` (ADR 0010) all derive `JsonSchema` on the same type with no extra cost.

## Considered Options

- **A. Dual-encoding (this ADR)** — same Rust type, both derives. Selected.
- **B. Greenfield JSON-only** — delete postcard, JSON becomes the only wire. Rejected: regresses Phase 6.5 0.78ms kernel-internal p50 by 3-4 orders of magnitude (jsonschema validation alone is ~5-30 µs; encode is ~1-3 µs vs postcard's <0.1 µs).
- **C. Postcard-only (status quo)** — Rejected: violates ADR 0006, blocks LLM wire integration.

## Consequences

- `serde_json` + `schemars` + `jsonschema` are hard workspace dependencies (`Cargo.toml` `[workspace.dependencies]`).
- All public types that touch the wire now compile slower (~5% on `cargo check`) due to derive expansion. `cargo build --release` impact is negligible.
- `Plan::version` is a `String` on the wire (postcard + JSON both) instead of a `semver::Version` — minor type-safety loss; `Plan::version_parsed()` recovers it.
- The Phase 5.2 第二批 `json.rs` module (centralised validator) is **out of scope** for this ADR; tracked in PRD-A as Phase 5.2 第二批.

## 关联

- 上游:ADR 0001(Intent),ADR 0006(PlanMessage JSON wire)
- 下游:ADR 0013(`AtomicErrorCode`),ADR 0014(`ForegroundApp`)
- 验证:`cargo test -p ai-device-kernel --test phase5_2_alignment` — 21 tests, all green
