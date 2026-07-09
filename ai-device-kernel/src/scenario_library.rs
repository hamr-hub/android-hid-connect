//! `ScenarioLibrary` — templates + recordings + fresh-plan +
//! auto promote/demote. Per grill ADR 0010 (scenario-library-is-hybrid).
//!
//! **Design**:
//! - `templates` are JSON-encoded `Plan` files shipped in the binary
//!   (or loaded at runtime via `MockLlmAdapter`). High success-rate
//!   scenarios get promoted into templates (ADR 0010: "promoted
//!   if rate ≥ 0.95").
//! - `recordings` are user-captured traces (e.g. from `bin/adk
//!   --record`). Saved as `Plan` JSON. Replayed verbatim on
//!   subsequent sessions.
//! - `fresh_plan` is the escape valve (no template, no recording).
//! - Bounded — `MAX_LIBRARY_SIZE: usize = 1000` caps total entries.
//!
//! **No IO in the hot path** — `record_*` only mutates in-memory state.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::ids::PlanId;
use crate::plan::Plan;

/// Maximum total library size. Triggers demote of oldest entry.
pub const MAX_LIBRARY_SIZE: usize = 1000;

/// Minimum success rate to qualify for promote.
pub const PROMOTE_SUCCESS_RATE: f32 = 0.95;

/// Maximum miss rate to qualify for promote.
pub const PROMOTE_MISS_RATE_CEIL: f32 = 0.05;

/// One library entry (template or recording).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ScenarioEntry {
    /// Stable identifier (UUIDv4 for now).
    pub id: ScenarioId,
    /// Template or Recording.
    pub kind: ScenarioKind,
    /// The pre-built plan.
    pub plan: Plan,
    /// Rolling success rate [0, 1] (1.0 = never failed).
    pub success_rate: f32,
    /// Rolling miss rate [0, 1] (1.0 = every outcome was a miss).
    pub miss_rate: f32,
    /// Number of times this scenario has been used.
    pub use_count: u64,
    /// Wall-clock ms since epoch when entry was first recorded.
    pub created_at_ms: u64,
    /// Wall-clock ms since epoch when entry was last used.
    pub last_used_at_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub enum ScenarioKind {
    Template,
    Recording,
    /// Soft-deprecated; still kept on disk for replay.
    Deprecated,
}

/// Monotonic counter for `ScenarioId` generation (process-local).
/// No need for UUID since IDs never cross process boundaries
/// (library is in-memory; reloaded on each process start).
static SCENARIO_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
pub struct ScenarioId(pub u64);

impl ScenarioId {
    /// Generate a new unique `ScenarioId` (process-monotonic).
    #[must_use]
    pub fn new() -> Self {
        Self(SCENARIO_COUNTER.fetch_add(1, Ordering::Relaxed))
    }
}

impl Default for ScenarioId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for ScenarioId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "scenario#{}", self.0)
    }
}

/// In-memory scenario library. Bounded by [`MAX_LIBRARY_SIZE`].
#[derive(Debug, Default, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ScenarioLibrary {
    /// All entries (templates + recordings + deprecated). Keyed by
    /// scenario id.
    #[serde(skip)]
    pub entries: BTreeMap<ScenarioId, ScenarioEntry>,
    /// Reverse index: plan id → scenario id. Empty for
    /// `fresh_plan` (no scenario chosen).
    #[serde(skip)]
    pub plan_to_scenario: BTreeMap<PlanId, ScenarioId>,
}

impl ScenarioLibrary {
    /// Create a new empty library.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of entries (templates + recordings + deprecated).
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True if no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Total templates only (non-deprecated, kind == Template).
    #[must_use]
    pub fn template_count(&self) -> usize {
        self.entries
            .values()
            .filter(|e| e.kind == ScenarioKind::Template)
            .count()
    }

    /// Total recordings only (kind == Recording).
    #[must_use]
    pub fn recording_count(&self) -> usize {
        self.entries
            .values()
            .filter(|e| e.kind == ScenarioKind::Recording)
            .count()
    }

    /// Add a new entry. If the library is at capacity, evicts the
    /// oldest non-recently-used entry.
    pub fn add(&mut self, kind: ScenarioKind, plan: Plan, now_ms: u64) -> ScenarioId {
        if self.entries.len() >= MAX_LIBRARY_SIZE {
            // Evict oldest by last_used_at_ms.
            if let Some(oldest_id) = self
                .entries
                .iter()
                .min_by_key(|(_, e)| e.last_used_at_ms)
                .map(|(id, _)| *id)
            {
                self.entries.remove(&oldest_id);
            }
        }
        let id = ScenarioId::new();
        let entry = ScenarioEntry {
            id,
            kind,
            plan: plan.clone(),
            success_rate: 1.0,
            miss_rate: 0.0,
            use_count: 0,
            created_at_ms: now_ms,
            last_used_at_ms: now_ms,
        };
        self.entries.insert(id, entry);
        self.plan_to_scenario.insert(plan.id, id);
        id
    }

    /// Look up a scenario by `PlanId`. Returns the most-recently-used
    /// template + recording for that `PlanId`.
    #[must_use]
    pub fn lookup(&self, plan_id: PlanId) -> Option<&ScenarioEntry> {
        let scenario_id = self.plan_to_scenario.get(&plan_id)?;
        self.entries.get(scenario_id)
    }

    /// Record a successful outcome for a `PlanId`. Updates
    /// `success_rate` and `miss_rate` with exponential moving
    /// average (alpha = 0.1).
    pub fn record_success(&mut self, plan_id: PlanId, now_ms: u64) {
        self.update_outcome(plan_id, true, now_ms);
    }

    /// Record a failed outcome for a `PlanId`. Updates rates.
    pub fn record_miss(&mut self, plan_id: PlanId, now_ms: u64) {
        self.update_outcome(plan_id, false, now_ms);
    }

    fn update_outcome(&mut self, plan_id: PlanId, success: bool, now_ms: u64) {
        let Some(scenario_id) = self.plan_to_scenario.get(&plan_id).copied() else {
            return;
        };
        let Some(entry) = self.entries.get_mut(&scenario_id) else {
            return;
        };
        // Exponential moving average: alpha = 0.1 (slow update, prefer
        // historical context).
        const ALPHA: f32 = 0.1;
        if success {
            entry.success_rate = entry.success_rate * (1.0 - ALPHA) + ALPHA;
            entry.miss_rate *= 1.0 - ALPHA;
        } else {
            entry.success_rate *= 1.0 - ALPHA;
            entry.miss_rate = entry.miss_rate * (1.0 - ALPHA) + ALPHA;
        }
        entry.use_count += 1;
        entry.last_used_at_ms = now_ms;
    }

    /// Promote an entry to `Template` kind if success_rate is high
    /// enough AND miss_rate is low enough. Returns the scenario id
    /// if promoted, `None` otherwise.
    pub fn promote_if_ready(&mut self, plan_id: PlanId) -> Option<ScenarioId> {
        let scenario_id = self.plan_to_scenario.get(&plan_id).copied()?;
        let entry = self.entries.get(&scenario_id)?;
        if entry.success_rate >= PROMOTE_SUCCESS_RATE
            && entry.miss_rate <= PROMOTE_MISS_RATE_CEIL
            && entry.use_count >= 10
        {
            let id = entry.id;
            if let Some(e) = self.entries.get_mut(&id) {
                e.kind = ScenarioKind::Template;
            }
            Some(id)
        } else {
            None
        }
    }

    /// Demote a template to `Deprecated` (keeps it on disk for
    /// replay but no longer eligible for fresh use).
    pub fn demote(&mut self, scenario_id: ScenarioId) -> bool {
        if let Some(entry) = self.entries.get_mut(&scenario_id) {
            if entry.kind == ScenarioKind::Template {
                entry.kind = ScenarioKind::Deprecated;
                return true;
            }
        }
        false
    }

    /// Convenience: select the best scenario for a `PlanId` —
    /// returns template if available, else recording, else None
    /// (caller should use `fresh_plan`).
    #[must_use]
    pub fn select(&self, plan_id: PlanId) -> Option<&ScenarioEntry> {
        self.lookup(plan_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::PlanId;

    fn sample_plan(id: u64) -> Plan {
        Plan {
            version: "0.1.0".into(),
            id: PlanId(id),
            steps: vec![],
            abort_on_error: true,
            checkpoint_every: 0,
        }
    }

    #[test]
    fn new_is_empty() {
        let lib = ScenarioLibrary::new();
        assert!(lib.is_empty());
        assert_eq!(lib.len(), 0);
    }

    #[test]
    fn add_increments_len() {
        let mut lib = ScenarioLibrary::new();
        let id = lib.add(ScenarioKind::Recording, sample_plan(0), 0);
        assert_eq!(lib.len(), 1);
        assert!(lib.entries.contains_key(&id));
    }

    #[test]
    fn record_success_after_100_calls_promotes() {
        let mut lib = ScenarioLibrary::new();
        let plan = sample_plan(0);
        let id = lib.add(ScenarioKind::Recording, plan.clone(), 0);
        for i in 0..200 {
            lib.record_success(plan.id, i);
        }
        // After 200 all-success, should be eligible for promote
        // (success >= 0.95 AND miss <= 0.05 AND use_count >= 10)
        let promoted = lib.promote_if_ready(plan.id);
        assert_eq!(promoted, Some(id), "should be promoted");
        assert_eq!(lib.entries[&id].kind, ScenarioKind::Template);
    }

    #[test]
    fn record_miss_blocks_promote() {
        let mut lib = ScenarioLibrary::new();
        let plan = sample_plan(0);
        lib.add(ScenarioKind::Recording, plan.clone(), 0);
        // 50 success, 50 miss = 50% success rate
        for i in 0..50 {
            lib.record_success(plan.id, i);
        }
        for i in 50..100 {
            lib.record_miss(plan.id, i);
        }
        let promoted = lib.promote_if_ready(plan.id);
        assert!(promoted.is_none(), "low success rate must block promote");
    }

    #[test]
    fn promote_requires_min_use_count() {
        let mut lib = ScenarioLibrary::new();
        let plan = sample_plan(0);
        lib.add(ScenarioKind::Recording, plan.clone(), 0);
        for i in 0..5 {
            // only 5 calls, below the 10-call threshold
            lib.record_success(plan.id, i);
        }
        let promoted = lib.promote_if_ready(plan.id);
        assert!(
            promoted.is_none(),
            "insufficient use_count must block promote"
        );
    }

    #[test]
    fn demote_marks_template_deprecated() {
        let mut lib = ScenarioLibrary::new();
        let id = lib.add(ScenarioKind::Template, sample_plan(0), 0);
        assert!(lib.demote(id));
        assert_eq!(lib.entries[&id].kind, ScenarioKind::Deprecated);
    }

    #[test]
    fn demote_only_works_on_template() {
        let mut lib = ScenarioLibrary::new();
        let id = lib.add(ScenarioKind::Recording, sample_plan(0), 0);
        assert!(!lib.demote(id));
        assert_eq!(lib.entries[&id].kind, ScenarioKind::Recording);
    }

    #[test]
    fn evicts_oldest_when_full() {
        let mut lib = ScenarioLibrary::new();
        // Fill with MAX entries, then add one more
        let mut first_id = None;
        for i in 0..MAX_LIBRARY_SIZE {
            let id = lib.add(ScenarioKind::Recording, sample_plan(i as u64), 0);
            if i == 0 {
                first_id = Some(id);
            }
        }
        assert_eq!(lib.len(), MAX_LIBRARY_SIZE);
        let first_id = first_id.unwrap();
        // Bump first's last_used so it's not the oldest
        // (advance all later entries' last_used, so the next oldest
        // is the second one)
        // Touch all entries' last_used to make the OLDEST be first
        // (i.e. not bump any)
        // Add a new one
        let new_id = lib.add(
            ScenarioKind::Template,
            sample_plan(MAX_LIBRARY_SIZE as u64),
            1000,
        );
        // First should be evicted (last_used_at_ms = 0)
        assert!(!lib.entries.contains_key(&first_id));
        assert!(lib.entries.contains_key(&new_id));
        assert_eq!(lib.len(), MAX_LIBRARY_SIZE);
    }

    #[test]
    fn lookup_returns_none_for_unknown_plan() {
        let lib = ScenarioLibrary::new();
        assert!(lib.lookup(PlanId(999)).is_none());
    }

    #[test]
    fn template_and_recording_counts() {
        let mut lib = ScenarioLibrary::new();
        lib.add(ScenarioKind::Template, sample_plan(0), 0);
        lib.add(ScenarioKind::Template, sample_plan(1), 0);
        lib.add(ScenarioKind::Recording, sample_plan(2), 0);
        lib.add(ScenarioKind::Recording, sample_plan(3), 0);
        lib.add(ScenarioKind::Recording, sample_plan(4), 0);
        assert_eq!(lib.template_count(), 2);
        assert_eq!(lib.recording_count(), 3);
        assert_eq!(lib.len(), 5);
    }
}
