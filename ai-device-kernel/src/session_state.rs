//! `SessionState` — bounded, on-device-only state for one user task.
//!
//! Per grill ADR 0009 (session-state-bounded-ondevice-only) +
//! PRD-B ADR 0016. Holds 5 fields, **all on-device only**:
//!
//! 1. `task: TaskSpec` — what the user wants
//! 2. `scenario_progress: ChunkIndex` — where in the scenario we are
//! 3. `recent_atomics: VecDeque<AtomicRecord>` — bounded 20 (FIFO)
//! 4. `selector_cache: HashMap<String, ResolvedTarget>` — invalidates on miss
//! 5. `foreground_app_history: VecDeque<ForegroundApp>` — bounded 10
//! 6. `deadline: Instant` — 5min default
//!
//! `history` (atomic execution log + lifecycle events) lives in
//! `EgressPolicy::allow_history = false` zone — never crosses the
//! device boundary per ADR 0008. `SessionState` is a different
//! concept: a *bounded working memory* the LLM uses for
//! re-planning, not an audit log.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::action::{Action, ActionResult, AtomicErrorCode};
use crate::ids::ActionId;
use crate::observation::ForegroundApp;

/// Default cap on `recent_atomics`. 20 = ~5 chunks at N=4 (per
/// ADR 0003 chunk granularity).
pub const MAX_RECENT_ATOMICS: usize = 20;

/// Default cap on `foreground_app_history`. 10 = ~10 app-switches
/// in a typical multi-app scenario.
pub const MAX_FOREGROUND_HISTORY: usize = 10;

/// Default session deadline. 5 minutes is the upper bound for a
/// typical LLM task; sessions past that are presumed abandoned.
pub const DEFAULT_DEADLINE: Duration = Duration::from_secs(5 * 60);

/// One task the LLM is trying to accomplish. LLM-side intent;
/// populated by the host on session start.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub struct TaskSpec {
    /// Free-text task description (e.g. "open WeChat and search
    /// contact 'alice'"). The LLM uses this to bootstrap
    /// re-planning after a `selector_miss` cascade.
    pub description: String,
    /// Optional per-task timeout. Defaults to [`DEFAULT_DEADLINE`].
    pub timeout: Option<Duration>,
    /// Tags for telemetry grouping (e.g. `["benchmark", "scenario=login"]`).
    pub tags: Vec<String>,
}

/// Index into the active scenario's chunk list. Used to answer
/// "where are we in the scenario" without re-walking the chunk
/// history.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub struct ChunkIndex {
    /// 0-based index of the current chunk in the scenario.
    pub current: u32,
    /// Total chunks in the scenario (0 if not yet known).
    pub total: u32,
}

impl ChunkIndex {
    #[must_use]
    pub const fn new(current: u32, total: u32) -> Self {
        Self { current, total }
    }

    /// True if `current` has run off the end of `total`.
    #[must_use]
    pub fn is_exhausted(&self) -> bool {
        self.total > 0 && self.current >= self.total
    }
}

/// One record of an atomic execution. Trimmed form (no
/// `ground_truth` blob — full ground truth is in
/// `ActionResult.ground_truth` on the wire, not in working memory).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct AtomicRecord {
    /// `ActionId` of the executed atomic.
    pub id: ActionId,
    /// The action that was executed.
    pub atomic: Action,
    /// The full `ActionResult` (including ground truth).
    pub result: ActionResult,
    /// Wall-clock ms since the session started.
    pub ts_ms: u64,
}

/// A resolved selector target — what `selector_miss` should NOT
/// re-attempt. Cached in `selector_cache` to avoid redundant UI
/// tree dumps.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub struct ResolvedTarget {
    /// The original selector string.
    pub selector: String,
    /// Resolved a11y node id (matches `A11yTree.node_id`).
    pub node_id: u32,
    /// Resolved bounds (pixel-space rect).
    pub bounds: (i32, i32, i32, i32), // (x, y, w, h)
}

/// The full bounded state for one LLM session.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct SessionState {
    /// What the user wants. Set at session start; immutable for
    /// the session's lifetime.
    pub task: TaskSpec,
    /// Where we are in the scenario. Updated by the host as
    /// chunks execute.
    pub scenario_progress: ChunkIndex,
    /// Bounded FIFO log of recent atomics. Trimmed to
    /// [`MAX_RECENT_ATOMICS`] (20) on overflow.
    pub recent_atomics: VecDeque<AtomicRecord>,
    /// Cached selector resolutions. Invalidated on
    /// `selector_miss` (per ADR 0001).
    pub selector_cache: HashMap<String, ResolvedTarget>,
    /// Bounded FIFO of `ForegroundApp` transitions. Trimmed to
    /// [`MAX_FOREGROUND_HISTORY`] (10).
    pub foreground_app_history: VecDeque<ForegroundApp>,
    /// Hard session deadline in **milliseconds since epoch** (NOT
    /// `Instant`, which is not serialisable). After this,
    /// `is_expired()` returns `true` and the session aborts.
    /// Set on construction; updated only via `extend_deadline`.
    pub deadline_ms_since_epoch: u64,
    /// Internal: clock anchor for `ts_ms` in `AtomicRecord`. Not
    /// JSON-serialised.
    #[serde(skip)]
    session_start: Instant,
    /// Internal: process start time anchor for `deadline_ms_since_epoch`
    /// translation. Captured at process start; not serialised.
    #[serde(skip)]
    epoch_anchor: Instant,
}

impl Default for SessionState {
    fn default() -> Self {
        Self::new(TaskSpec {
            description: String::new(),
            timeout: None,
            tags: Vec::new(),
        })
    }
}

impl SessionState {
    /// Create a fresh session with the default deadline (5 min).
    #[must_use]
    pub fn new(task: TaskSpec) -> Self {
        let now = Instant::now();
        let deadline = now + task.timeout.unwrap_or(DEFAULT_DEADLINE);
        Self {
            task,
            scenario_progress: ChunkIndex::new(0, 0),
            recent_atomics: VecDeque::with_capacity(MAX_RECENT_ATOMICS),
            selector_cache: HashMap::new(),
            foreground_app_history: VecDeque::with_capacity(MAX_FOREGROUND_HISTORY),
            deadline_ms_since_epoch: (deadline - now).as_millis() as u64,
            session_start: now,
            epoch_anchor: now,
        }
    }

    /// Create a session with an explicit deadline.
    #[must_use]
    pub fn with_deadline(task: TaskSpec, deadline: Instant) -> Self {
        let now = Instant::now();
        Self {
            task,
            scenario_progress: ChunkIndex::new(0, 0),
            recent_atomics: VecDeque::with_capacity(MAX_RECENT_ATOMICS),
            selector_cache: HashMap::new(),
            foreground_app_history: VecDeque::with_capacity(MAX_FOREGROUND_HISTORY),
            deadline_ms_since_epoch: deadline
                .checked_duration_since(now)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
            session_start: now,
            epoch_anchor: now,
        }
    }

    /// Record one atomic execution. Trims `recent_atomics` to
    /// [`MAX_RECENT_ATOMICS`] on overflow (FIFO drop-oldest).
    pub fn record_atomic(&mut self, id: ActionId, atomic: Action, result: ActionResult) {
        let record = AtomicRecord {
            id,
            atomic,
            result,
            ts_ms: self.session_start.elapsed().as_millis() as u64,
        };
        if self.recent_atomics.len() >= MAX_RECENT_ATOMICS {
            self.recent_atomics.pop_front();
        }
        self.recent_atomics.push_back(record);
    }

    /// Cache a selector resolution. Overwrites any existing entry.
    pub fn cache_selector(&mut self, selector: String, target: ResolvedTarget) {
        self.selector_cache.insert(selector, target);
    }

    /// Invalidate one selector (called on `selector_miss`).
    /// Returns `true` if the cache had the entry.
    pub fn invalidate_selector(&mut self, selector: &str) -> bool {
        self.selector_cache.remove(selector).is_some()
    }

    /// Look up a cached selector.
    #[must_use]
    pub fn lookup_selector(&self, selector: &str) -> Option<&ResolvedTarget> {
        self.selector_cache.get(selector)
    }

    /// Record a foreground app transition. Trims
    /// `foreground_app_history` to [`MAX_FOREGROUND_HISTORY`].
    pub fn record_foreground_app(&mut self, app: ForegroundApp) {
        if self.foreground_app_history.len() >= MAX_FOREGROUND_HISTORY {
            self.foreground_app_history.pop_front();
        }
        self.foreground_app_history.push_back(app);
    }

    /// True if the session has run past its deadline.
    #[must_use]
    pub fn is_expired(&self) -> bool {
        Instant::now() > self.epoch_anchor + Duration::from_millis(self.deadline_ms_since_epoch)
    }

    /// Remaining time before deadline. `Duration::ZERO` if expired.
    #[must_use]
    pub fn remaining(&self) -> Duration {
        let deadline = self.epoch_anchor + Duration::from_millis(self.deadline_ms_since_epoch);
        deadline.saturating_duration_since(Instant::now())
    }

    /// Update the scenario progress (called by the host as
    /// chunks execute).
    pub fn advance_chunk(&mut self) {
        self.scenario_progress.current = self.scenario_progress.current.saturating_add(1);
    }

    /// Mark the total scenario size (called by the host after
    /// first plan emission).
    pub fn set_total_chunks(&mut self, total: u32) {
        self.scenario_progress.total = total;
    }

    /// True if any atomic result was a structured failure
    /// (`landed == false` + `code.is_some()`).
    /// Used by the LLM to decide "do I need to re-plan?".
    #[must_use]
    pub fn has_structured_failures(&self) -> bool {
        self.recent_atomics
            .iter()
            .any(|r| r.result.is_structured_failure())
    }

    /// Count of `selector_miss` errors in the recent atomics.
    /// Used by ADR 0008 to decide on cloud escalation
    /// (selector_miss N=2 → escalate).
    #[must_use]
    pub fn selector_miss_count(&self) -> u32 {
        self.recent_atomics
            .iter()
            .filter(|r| matches!(r.result.code, Some(AtomicErrorCode::SelectorMiss)))
            .count() as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::{Action, ActionResult, GroundTruth};

    fn dummy_task() -> TaskSpec {
        TaskSpec {
            description: "test".into(),
            timeout: None,
            tags: vec![],
        }
    }

    fn dummy_atomic_result() -> ActionResult {
        ActionResult {
            id: ActionId(0),
            landed: true,
            ground_truth: GroundTruth::default(),
            elapsed_ms: 1,
            code: None,
            retryable: false,
        }
    }

    #[test]
    fn new_uses_default_deadline_5min() {
        let s = SessionState::new(dummy_task());
        assert!(!s.is_expired());
        let remaining = s.remaining();
        assert!(
            remaining > Duration::from_secs(4 * 60),
            "remaining < 4min: {remaining:?}"
        );
        assert!(remaining <= Duration::from_secs(5 * 60));
    }

    #[test]
    fn record_atomic_bounded_to_20() {
        let mut s = SessionState::new(dummy_task());
        for i in 0..25 {
            s.record_atomic(
                ActionId(i),
                Action::Tap {
                    x: 0,
                    y: 0,
                    deadline_ms: 100,
                },
                dummy_atomic_result(),
            );
        }
        assert_eq!(
            s.recent_atomics.len(),
            20,
            "must be bounded at MAX_RECENT_ATOMICS=20"
        );
        // FIFO: first 5 dropped, last 20 retained (ids 5..24)
        let first_id = s.recent_atomics.front().unwrap().id;
        let last_id = s.recent_atomics.back().unwrap().id;
        assert_eq!(first_id, ActionId(5));
        assert_eq!(last_id, ActionId(24));
    }

    #[test]
    fn record_atomic_fifo_trim() {
        let mut s = SessionState::new(dummy_task());
        for i in 0..30 {
            s.record_atomic(
                ActionId(i),
                Action::Tap {
                    x: 0,
                    y: 0,
                    deadline_ms: 100,
                },
                dummy_atomic_result(),
            );
        }
        assert_eq!(s.recent_atomics.len(), 20);
        // The oldest in the deque is now id 10
        assert_eq!(s.recent_atomics.front().unwrap().id, ActionId(10));
    }

    #[test]
    fn selector_cache_invalidated_on_miss() {
        let mut s = SessionState::new(dummy_task());
        s.cache_selector(
            "Button[id=login]".into(),
            ResolvedTarget {
                selector: "Button[id=login]".into(),
                node_id: 42,
                bounds: (100, 200, 300, 400),
            },
        );
        assert!(s.lookup_selector("Button[id=login]").is_some());
        let had = s.invalidate_selector("Button[id=login]");
        assert!(
            had,
            "invalidate_selector should return true for cached entry"
        );
        assert!(s.lookup_selector("Button[id=login]").is_none());
    }

    #[test]
    fn selector_cache_hit_returns_resolved() {
        let mut s = SessionState::new(dummy_task());
        let target = ResolvedTarget {
            selector: "X".into(),
            node_id: 7,
            bounds: (0, 0, 100, 200),
        };
        s.cache_selector("X".into(), target.clone());
        let looked = s.lookup_selector("X").unwrap();
        assert_eq!(looked, &target);
    }

    #[test]
    fn is_expired_after_deadline() {
        let mut s = SessionState::new(dummy_task());
        // Force deadline into the past
        s.deadline_ms_since_epoch = 0;
        assert!(s.is_expired());
    }

    #[test]
    fn is_not_expired_before_deadline() {
        let s = SessionState::new(dummy_task());
        assert!(!s.is_expired());
    }

    #[test]
    fn advance_chunk_increments() {
        let mut s = SessionState::new(dummy_task());
        s.set_total_chunks(5);
        assert_eq!(s.scenario_progress, ChunkIndex::new(0, 5));
        s.advance_chunk();
        s.advance_chunk();
        assert_eq!(s.scenario_progress.current, 2);
        assert!(!s.scenario_progress.is_exhausted());
        for _ in 0..5 {
            s.advance_chunk();
        }
        assert!(s.scenario_progress.is_exhausted());
    }

    #[test]
    fn record_foreground_app_bounded_to_10() {
        let mut s = SessionState::new(dummy_task());
        for i in 0..15 {
            s.record_foreground_app(ForegroundApp {
                package: format!("com.app{i}"),
                activity: ".Main".into(),
                focused: true,
                since_ms: 0,
            });
        }
        assert_eq!(s.foreground_app_history.len(), 10);
    }

    #[test]
    fn selector_miss_count_returns_only_selector_miss() {
        let mut s = SessionState::new(dummy_task());
        for i in 0..3 {
            s.record_atomic(
                ActionId(i),
                Action::Tap {
                    x: 0,
                    y: 0,
                    deadline_ms: 100,
                },
                ActionResult {
                    code: Some(AtomicErrorCode::SelectorMiss),
                    retryable: true,
                    ..dummy_atomic_result()
                },
            );
        }
        s.record_atomic(
            ActionId(99),
            Action::Tap {
                x: 0,
                y: 0,
                deadline_ms: 100,
            },
            ActionResult {
                code: Some(AtomicErrorCode::Timeout),
                retryable: true,
                ..dummy_atomic_result()
            },
        );
        assert_eq!(s.selector_miss_count(), 3);
    }

    #[test]
    fn has_structured_failures_true_iff_any_fail() {
        let mut s = SessionState::new(dummy_task());
        assert!(!s.has_structured_failures());
        s.record_atomic(
            ActionId(0),
            Action::Tap {
                x: 0,
                y: 0,
                deadline_ms: 100,
            },
            ActionResult {
                landed: false,
                code: Some(AtomicErrorCode::SelectorMiss),
                ..dummy_atomic_result()
            },
        );
        assert!(s.has_structured_failures());
    }
}
