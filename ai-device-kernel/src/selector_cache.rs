//! `SelectorCache` — thin wrapper that wires selector-miss
//! detection into `SessionState::invalidate_selector`.
//!
//! Per grill ADR 0001 (selector miss → re-plan) + PRD-A AC-6.
//! LLM step loop calls `record_miss(selector)` on every
//! `AtomicErrorCode::SelectorMiss`; the cache auto-invalidates
//! the entry and the next LLM call will re-dump the UI tree for
//! that selector.

use crate::action::ActionResult;
use crate::session_state::SessionState;

/// Thin wrapper around `SessionState` for selector-miss bookkeeping.
pub struct SelectorCache<'a> {
    session: &'a mut SessionState,
}

impl std::fmt::Debug for SelectorCache<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SelectorCache").finish_non_exhaustive()
    }
}

impl<'a> SelectorCache<'a> {
    /// Borrow a `SessionState` as a `SelectorCache`.
    pub fn new(session: &'a mut SessionState) -> Self {
        Self { session }
    }

    /// Record that a selector succeeded. No-op for cache state
    /// (just bumps the recent_atomics via the session).
    pub fn record_hit(&mut self, _selector: &str) {
        // Hits don't invalidate; nothing to do here. The session's
        // selector_cache entries remain.
    }

    /// Record that a selector missed. If the cache had the entry,
    /// returns `true` (i.e. the entry was invalidated). If the
    /// selector wasn't cached, returns `false` (no-op).
    ///
    /// Convenience over `ActionResult`-level integration: also
    /// accepts a pre-checked `ActionResult` and only invalidates
    /// when `code == Some(AtomicErrorCode::SelectorMiss)`.
    pub fn record_miss_from_result(&mut self, selector: &str, result: &ActionResult) -> bool {
        use crate::action::AtomicErrorCode;
        if matches!(result.code, Some(AtomicErrorCode::SelectorMiss)) {
            self.session.invalidate_selector(selector)
        } else {
            false
        }
    }

    /// Lower-level: directly invalidate a selector (no result check).
    pub fn record_miss(&mut self, selector: &str) -> bool {
        self.session.invalidate_selector(selector)
    }

    /// Look up a cached selector (read-only borrow into the session).
    pub fn lookup(&self, selector: &str) -> Option<&crate::session_state::ResolvedTarget> {
        self.session.lookup_selector(selector)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::{ActionResult, AtomicErrorCode, GroundTruth};
    use crate::ids::ActionId;
    use crate::session_state::{ResolvedTarget, TaskSpec};

    fn sample_session() -> SessionState {
        SessionState::new(TaskSpec {
            description: "test".into(),
            timeout: None,
            tags: vec![],
        })
    }

    fn success_result() -> ActionResult {
        ActionResult {
            id: ActionId(0),
            landed: true,
            ground_truth: GroundTruth::default(),
            elapsed_ms: 1,
            code: None,
            retryable: false,
        }
    }

    fn miss_result() -> ActionResult {
        ActionResult {
            id: ActionId(0),
            landed: false,
            ground_truth: GroundTruth::default(),
            elapsed_ms: 1,
            code: Some(AtomicErrorCode::SelectorMiss),
            retryable: true,
        }
    }

    fn other_failure_result() -> ActionResult {
        ActionResult {
            id: ActionId(0),
            landed: false,
            ground_truth: GroundTruth::default(),
            elapsed_ms: 1,
            code: Some(AtomicErrorCode::Timeout),
            retryable: true,
        }
    }

    fn cached_target() -> ResolvedTarget {
        ResolvedTarget {
            selector: "Button[id=login]".into(),
            node_id: 42,
            bounds: (100, 200, 300, 400),
        }
    }

    #[test]
    fn record_miss_invalidates_cached_entry() {
        let mut session = sample_session();
        session.cache_selector("Button[id=login]".into(), cached_target());
        assert!(session.lookup_selector("Button[id=login]").is_some());

        let mut cache = SelectorCache::new(&mut session);
        let had = cache.record_miss("Button[id=login]");
        assert!(had, "miss on cached selector must return true");
        assert!(cache.lookup("Button[id=login]").is_none());
    }

    #[test]
    fn record_miss_on_uncached_returns_false() {
        let mut session = sample_session();
        let mut cache = SelectorCache::new(&mut session);
        let had = cache.record_miss("never-cached");
        assert!(!had);
    }

    #[test]
    fn record_miss_from_result_only_invalidates_on_selector_miss() {
        let mut session = sample_session();
        session.cache_selector("X".into(), cached_target());

        let mut cache = SelectorCache::new(&mut session);
        // Success → no invalidation
        assert!(!cache.record_miss_from_result("X", &success_result()));
        assert!(cache.lookup("X").is_some());
        // Other failure (not selector_miss) → no invalidation
        assert!(!cache.record_miss_from_result("X", &other_failure_result()));
        assert!(cache.lookup("X").is_some());
        // SelectorMiss → invalidation
        assert!(cache.record_miss_from_result("X", &miss_result()));
        assert!(cache.lookup("X").is_none());
    }

    #[test]
    fn record_hit_is_noop() {
        let mut session = sample_session();
        session.cache_selector("X".into(), cached_target());
        let mut cache = SelectorCache::new(&mut session);
        cache.record_hit("X");
        assert!(cache.lookup("X").is_some());
    }
}
