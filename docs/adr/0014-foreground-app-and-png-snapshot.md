# 0014 — `ForegroundApp` type + `FrameSnapshot.png: Option<Vec<u8>>` multimodal channel

## Context

`ai-device-kernel` had `DeviceState { focused_window, focused_node, screen_on, uptime_ms }` for hardware-state bits, and `FrameSnapshot { width, height, codec, is_keyframe, pts, scene_change_score }` for the H.265 frame metadata. Neither carried app-identity (`package` / `activity`) nor raw PNG bytes for LLM-vision fallback.

Grill session (2026-07-01) ratified:
- **ADR 0005 (Q6)**: `app_switch` is a top-level PlanMessage variant returning `ForegroundApp { package, activity, focused, since_ms }` as the single source of truth for "what app am I acting on". No per-intent `app=` field.
- **ADR 0009 (Q9)**: `Observation` returns `{ui_tree, foreground_app, snapshot_png}` for the LLM-vision channel. PNG is multimodal (not in JSON token stream).

## Decision

### 1. New type `ForegroundApp`

```rust
pub struct ForegroundApp {
    pub package: String,    // e.g. "com.example"
    pub activity: String,   // e.g. ".MainActivity"
    pub focused: bool,      // settled vs transitioning
    pub since_ms: u64,      // wall-clock ms since observed
}
```

`ForegroundApp::component() -> String` returns `com.example/.MainActivity` for log lines.

### 2. `Observation.foreground_app: Option<ForegroundApp>`

Single source of truth — no per-intent `app=` field exists. Marked `#[serde(default)]` for backward compat with pre-v3.4 snapshots.

### 3. `FrameSnapshot.png: Option<Vec<u8>>`

Raw PNG bytes for LLM vision. Optional because the kernel-internal hot path (per Phase 6.5 0.78ms p50) doesn't carry PNG. Only LLM-vision observations populate this. Removed `Copy` derive on `FrameSnapshot` (`Vec<u8>` is not Copy). Replaced `Eq` requirement with `PartialEq` (`f32` already broke Eq).

### 4. Existing `Action::Launch` keeps `LaunchBy { Component, Package, Intent }` enum

For backwards compat, `Action::Launch` is preserved as the cross-app primitive. ADR 0005's "app_switch is a top-level PlanMessage variant" is a grill-side semantic distinction — in ai-device-kernel, the `app_switch` is realised as `Action::Launch { by: LaunchBy::Package(pkg) | LaunchBy::Component(c) | LaunchBy::Intent{...} }` and the `ForegroundApp` is returned in the resulting `ActionResult.ground_truth` (extracted from `DeviceEvent::ActivityResumed { component }`).

This is **not** a 1:1 mapping with grill ADR 0005's "`app_switch` as top-level PlanMessage variant" — Phase 5.2 第二批 will introduce a separate `app_switch` atomic that wraps `Action::Launch` with `ForegroundApp` extraction. For Phase 5.2 第一批, `Action::Launch` is the workhorse.

## Why

1. **Aligned with ADR 0005 / ADR 0009** — `ForegroundApp` is the canonical app-identity type. `FrameSnapshot.png` enables the LLM-vision fallback (selector_miss rescue per grill Q9).
2. **PNG optional** — kernel-internal hot path (Phase 6.5 0.78ms p50) is unaffected because `png = None` is the default; only LLM-vision observations pay the cost.
3. **No new field collisions** — `Observation` gains one field, `FrameSnapshot` gains one field. Both are `#[serde(default)]` for back compat.
4. **Removed `Copy` is correct** — `Vec<u8>` is heap-allocated; a `Copy` `FrameSnapshot` would be a footgun. `Clone` suffices.

## Considered Options

- **A. New `ForegroundApp` + `FrameSnapshot.png` (this ADR)** — Selected.
- **B. Embed `package: String, activity: String` directly in `DeviceState`** — Rejected: `DeviceState` is hardware-state; app-identity is semantically separate. Mixing makes `DeviceState` ambiguous (is `package` the focused window's owning app, or the top-activity?).
- **C. Store PNG as `Arc<Vec<u8>>` for cheap clone** — Rejected: `Arc` adds a layer of indirection not justified by current call patterns. `Vec<u8>` + `Clone` is the conservative choice; switch to `Arc<Bytes>` if profiling shows clone overhead matters.

## Consequences

- `FrameSnapshot` is no longer `Copy`. Existing call sites that rely on `Copy` (none in this crate) would break. `PartialEq` is preserved.
- `Observation` literals require `..Default::default()` (or explicit `foreground_app: None`) at the new field. `impl Default for Observation` is provided.
- `FrameSnapshot` literals require `..Default::default()`. `impl Default for FrameSnapshot` is provided.
- The `app_switch` ↔ `Action::Launch` mapping is **incomplete in this ADR** — full realisation lands in Phase 5.2 第二批 with the host-side intent JSON bridge.

## 关联

- 上游:ADR 0001(Intent),ADR 0005(多 app 编排)
- 下游:ADR 0009(SessionState includes `foreground_app_history`),ADR 0010(ScenarioLibrary templates are `PlanMessage` JSON)
- 验证:`tests/phase5_2_alignment.rs::foreground_app_*`, `frame_snapshot_png_*` — 4 tests, all green
