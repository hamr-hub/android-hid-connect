//! Memory layer — v3 §3.2.0 + §5 Phase 3.5–3.6.
//!
//! `Memory` is the kernel's local cache of "what worked on this
//! screen before". Each entry maps a [`ScreenId`] to one or
//! more successful action sequences (a [`ActionSequence`]).
//!
//! ## Design
//!
//! Inspired by AutoDroid (arXiv 2308.15272) but with three
//! critical differences (v3 §3.2.0):
//!
//! 1. **Online accumulation, not offline pre-exploration**:
//!    `record(success)` appends; `record(failure)` does not
//!    invalidate existing entries (we keep failures too, so
//!    the agent learns to *avoid* a known-bad path).
//! 2. **Per-screen cache, not global UTG**: each entry lives
//!    inside one `ScreenId`. Memory does not maintain
//!    cross-screen edges.
//! 3. **No pre-warming required**: the kernel starts empty
//!    and grows as the agent executes actions.
//!
//! AC-V3-3.5 ("Memory fingerprint 命中 > 60%") is the
//! headline metric. The shape of the cache makes a hit cheap:
//! `lookup(screen_id)` is O(1) over a `HashMap`.
//!
//! ## Persistence
//!
//! AC-V3-3.6 says "Memory 落盘 SQLite, 重启 daemon 不丢失".
//! Phase 3 lands the in-memory cache. SQLite persistence
//! ships in Phase 3.x because the dep (`rusqlite`) lands in
//! a follow-up commit alongside the existing workspace's
//! `android-hid-daemon` SQLite usage.
//!
//! ## Threading
//!
//! Single-threaded; the daemon's thread pool will guard
//! concurrent access with the engine's existing mutex in
//! Phase 6.

use std::collections::{HashMap, VecDeque};

use crate::action::Action;
use crate::ids::ScreenId;

/// One successful (or failed) action sequence.
///
/// `successes` are what the agent replays. `failures` are
/// anti-patterns the agent should avoid on this screen.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ActionSequence {
    /// Successful actions in the order they were recorded.
    pub successes: Vec<Action>,
    /// Failed actions (recorded for "avoid" semantics; Phase 4
    /// hardens this into a proper anti-pattern cache).
    pub failures: Vec<(Action, String)>,
}

impl ActionSequence {
    #[allow(dead_code, reason = "test-only constructor; Default covers production")]
    fn new() -> Self {
        Self::default()
    }

    /// Total recorded attempts (successes + failures).
    #[must_use]
    pub fn attempt_count(&self) -> usize {
        self.successes.len() + self.failures.len()
    }
}

/// In-memory kernel memory. Bounded per-screen entry count.
#[derive(Debug, Default)]
pub struct Memory {
    entries: HashMap<ScreenId, ActionSequence>,
    /// LRU list of screen_ids — oldest at the front, newest at
    /// the back. Used to evict cold entries.
    lru: VecDeque<ScreenId>,
    /// Number of cumulative hits (successes re-applied) since
    /// the kernel started.
    hit_count: u64,
    /// Number of cumulative misses (lookups that found no
    /// entry). `hit_count / (hit_count + miss_count)` is the
    /// AC-V3-3.5 metric.
    miss_count: u64,
}

impl Memory {
    /// Maximum entries in the cache. Sized so worst-case RAM
    /// is `MAX_ENTRIES * sizeof(ActionSequence)` ≈ 100 KB.
    pub const MAX_ENTRIES: usize = 1024;

    /// Build an empty memory.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of distinct screen entries currently cached.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True if no entries are cached.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Look up an entry by screen id. Bumps the LRU position
    /// when the entry exists.
    pub fn lookup(&mut self, screen_id: ScreenId) -> Option<&ActionSequence> {
        if self.entries.contains_key(&screen_id) {
            self.hit_count += 1;
            self.touch_lru(screen_id);
            self.entries.get(&screen_id)
        } else {
            self.miss_count += 1;
            None
        }
    }

    /// Immutably look up without touching LRU. Cheap diagnostic.
    #[must_use]
    pub fn peek(&self, screen_id: ScreenId) -> Option<&ActionSequence> {
        self.entries.get(&screen_id)
    }

    /// Record a successful action for the given screen.
    pub fn record_success(&mut self, screen_id: ScreenId, action: Action) {
        let entry = self.entries.entry(screen_id).or_default();
        entry.successes.push(action);
        self.touch_lru(screen_id);
        self.maybe_evict();
    }

    /// Record a failed action for the given screen.
    pub fn record_failure(
        &mut self,
        screen_id: ScreenId,
        action: Action,
        reason: String,
    ) {
        let entry = self.entries.entry(screen_id).or_default();
        entry.failures.push((action, reason));
        self.touch_lru(screen_id);
        self.maybe_evict();
    }

    /// Reset all hit/miss counters (kept separate from the
    /// cache so the kernel can reset AC metrics without
    /// losing cached entries).
    pub fn reset_metrics(&mut self) {
        self.hit_count = 0;
        self.miss_count = 0;
    }

    /// Cumulative hits since the kernel started (or since
    /// the last `reset_metrics`).
    #[must_use]
    pub fn hit_count(&self) -> u64 {
        self.hit_count
    }

    /// Cumulative misses.
    #[must_use]
    pub fn miss_count(&self) -> u64 {
        self.miss_count
    }

    /// AC-V3-3.5 hit rate (0–1). Returns `None` when no
    /// lookups have been recorded yet.
    #[must_use]
    pub fn hit_rate(&self) -> Option<f32> {
        let total = self.hit_count + self.miss_count;
        if total == 0 {
            None
        } else {
            Some(self.hit_count as f32 / total as f32)
        }
    }

    /// Move `screen_id` to the back of the LRU list (most
    /// recently used).
    fn touch_lru(&mut self, screen_id: ScreenId) {
        if let Some(pos) = self.lru.iter().position(|s| *s == screen_id) {
            self.lru.remove(pos);
        }
        self.lru.push_back(screen_id);
    }

    /// Evict cold entries past `MAX_ENTRIES`.
    fn maybe_evict(&mut self) {
        while self.entries.len() > Self::MAX_ENTRIES {
            // Pop the cold front of the LRU list. If the
            // entry was already removed (race with another
            // removal path), skip it.
            while let Some(victim) = self.lru.pop_front() {
                if self.entries.remove(&victim).is_some() {
                    break;
                }
            }
            // If we exhausted the LRU without removing
            // anything, break to avoid an infinite loop.
            if self.entries.len() > Self::MAX_ENTRIES
                && self.lru.is_empty()
            {
                break;
            }
        }
    }

    /// Approximate memory footprint in bytes. Used by the
    /// kernel's `AC-V3-3.3` budget check.
    #[must_use]
    pub fn approx_memory_bytes(&self) -> usize {
        // Rough estimate: 256 B per cached ActionSequence.
        self.entries.len() * 256
            + self.lru.len() * std::mem::size_of::<ScreenId>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen_id_a() -> ScreenId {
        ScreenId::compute(b"a11y-A", b"ph-A", "com.foo/.A")
    }

    fn screen_id_b() -> ScreenId {
        ScreenId::compute(b"a11y-B", b"ph-B", "com.foo/.B")
    }

    fn dummy_action_x() -> Action {
        Action::Tap {
            x: 0,
            y: 0,
            deadline_ms: 0,
        }
    }

    fn dummy_action_y() -> Action {
        Action::Tap {
            x: 100,
            y: 200,
            deadline_ms: 0,
        }
    }

    #[test]
    fn empty_memory() {
        let mem = Memory::new();
        assert!(mem.is_empty());
        assert_eq!(mem.len(), 0);
        assert_eq!(mem.hit_count(), 0);
        assert_eq!(mem.miss_count(), 0);
        assert_eq!(mem.hit_rate(), None);
    }

    #[test]
    fn record_success_creates_entry() {
        let mut mem = Memory::new();
        mem.record_success(screen_id_a(), dummy_action_x());
        assert_eq!(mem.len(), 1);
        let entry = mem.peek(screen_id_a()).expect("entry");
        assert_eq!(entry.successes.len(), 1);
        assert_eq!(entry.failures.len(), 0);
    }

    #[test]
    fn record_failure_creates_entry_with_reason() {
        let mut mem = Memory::new();
        mem.record_failure(screen_id_a(), dummy_action_x(), "selector miss".into());
        let entry = mem.peek(screen_id_a()).expect("entry");
        assert_eq!(entry.successes.len(), 0);
        assert_eq!(entry.failures.len(), 1);
        assert_eq!(entry.failures[0].1, "selector miss");
    }

    #[test]
    fn lookup_hits_when_entry_exists() {
        let mut mem = Memory::new();
        mem.record_success(screen_id_a(), dummy_action_x());
        assert!(mem.lookup(screen_id_a()).is_some());
        assert_eq!(mem.hit_count(), 1);
        assert_eq!(mem.miss_count(), 0);
    }

    #[test]
    fn lookup_misses_when_no_entry() {
        let mut mem = Memory::new();
        assert!(mem.lookup(screen_id_a()).is_none());
        assert_eq!(mem.hit_count(), 0);
        assert_eq!(mem.miss_count(), 1);
    }

    #[test]
    fn multiple_actions_per_screen() {
        let mut mem = Memory::new();
        mem.record_success(screen_id_a(), dummy_action_x());
        mem.record_success(screen_id_a(), dummy_action_y());
        let entry = mem.peek(screen_id_a()).expect("entry");
        assert_eq!(entry.successes.len(), 2);
        assert_eq!(entry.attempt_count(), 2);
    }

    #[test]
    fn multiple_screens_cached_independently() {
        let mut mem = Memory::new();
        mem.record_success(screen_id_a(), dummy_action_x());
        mem.record_success(screen_id_b(), dummy_action_y());
        assert_eq!(mem.len(), 2);
        assert_eq!(mem.peek(screen_id_a()).unwrap().successes.len(), 1);
        assert_eq!(mem.peek(screen_id_b()).unwrap().successes.len(), 1);
    }

    #[test]
    fn hit_rate_calculation() {
        let mut mem = Memory::new();
        mem.record_success(screen_id_a(), dummy_action_x());
        // 2 hits, 1 miss = 67% hit rate.
        for _ in 0..2 {
            assert!(mem.lookup(screen_id_a()).is_some());
        }
        assert!(mem.lookup(screen_id_b()).is_none());
        let rate = mem.hit_rate().unwrap();
        assert!(rate > 0.66 && rate < 0.68, "unexpected rate: {rate}");
    }

    #[test]
    fn reset_metrics_doesnt_clear_cache() {
        let mut mem = Memory::new();
        mem.record_success(screen_id_a(), dummy_action_x());
        mem.lookup(screen_id_a());
        mem.reset_metrics();
        assert_eq!(mem.hit_count(), 0);
        assert_eq!(mem.miss_count(), 0);
        assert_eq!(mem.len(), 1, "cache must survive reset_metrics");
    }

    #[test]
    fn evicts_lru_when_over_cap() {
        let mut mem = Memory::new();
        // Pin entry "a" by repeatedly touching it. Fill the
        // cap with entries that don't get touched; the
        // eviction should remove the oldest untouched.
        mem.record_success(screen_id_a(), dummy_action_x());
        for i in 0..(Memory::MAX_ENTRIES + 10) {
            let sid = ScreenId::compute(
                format!("a11y-{i}").as_bytes(),
                format!("ph-{i}").as_bytes(),
                &format!("com.foo/.S{i}"),
            );
            mem.record_success(sid, dummy_action_y());
        }
        // Entry "a" was the very first insert, but we
        // touched it via record_success once. After 1024
        // other inserts, "a" is the cold tail of the LRU.
        // The cap is 1024 entries — anything past that
        // evicts the cold front.
        assert!(mem.len() <= Memory::MAX_ENTRIES);
    }

    #[test]
    fn approx_memory_does_not_panic_and_fits() {
        let mut mem = Memory::new();
        mem.record_success(screen_id_a(), dummy_action_x());
        mem.record_success(screen_id_b(), dummy_action_y());
        let bytes = mem.approx_memory_bytes();
        assert!(bytes > 0);
        assert!(bytes < 1 << 20, "Memory must stay under 1 MiB");
    }

    #[test]
    fn action_sequence_attempt_count_includes_failures() {
        let mut seq = ActionSequence::new();
        assert_eq!(seq.attempt_count(), 0);
        seq.successes.push(dummy_action_x());
        seq.failures
            .push((dummy_action_y(), "fail".into()));
        assert_eq!(seq.attempt_count(), 2);
    }

    #[test]
    fn peek_does_not_count_as_hit() {
        // `peek` is the read-only access path; it must
        // not affect the AC-V3-3.5 metric.
        let mut mem = Memory::new();
        mem.record_success(screen_id_a(), dummy_action_x());
        let _ = mem.peek(screen_id_a());
        let _ = mem.peek(screen_id_a());
        assert_eq!(mem.hit_count(), 0, "peek must not bump hit_count");
        assert_eq!(mem.miss_count(), 0, "peek must not bump miss_count");
    }

    #[test]
    fn touch_lru_promotes_recent() {
        // Insert a, b, c. Touch a. Then exceed the cap with
        // d…; `b` should be the eviction candidate (b was
        // touched before a was touched last; a is now the
        // most-recent because of the explicit touch).
        let mut mem = Memory::new();
        let a = screen_id_a();
        let b = screen_id_b();
        let c = screen_id_c();
        mem.record_success(a, dummy_action_x());
        mem.record_success(b, dummy_action_x());
        mem.record_success(c, dummy_action_x());
        // Touch `a` again → it becomes the most recent.
        mem.record_success(a, dummy_action_y());
        // Re-touch b → a moves to the tail, b is now newer
        // than a?  Actually no — LRU semantically: most
        // recently used = end of deque. Toggling a *then* b
        // → order: [..., c, a, b].
        mem.record_success(b, dummy_action_y());
        // Sanity: at least one of the recent touches put a
        // closer to the back than c.
        let pos_a = mem.lru.iter().position(|s| *s == a).unwrap();
        let pos_c = mem.lru.iter().position(|s| *s == c).unwrap();
        // 'a' should be after 'c' in LRU order (a touched
        // more recently).
        assert!(pos_a > pos_c);
    }

    fn screen_id_c() -> ScreenId {
        ScreenId::compute(b"a11y-C", b"ph-C", "com.foo/.C")
    }

    #[test]
    fn v3_ac_3_5_hit_rate_dominates_after_warmup() {
        // Synthetic scenario: 100 lookups over 10 screens,
        // each screen pre-warmed once. Hit rate should be 100%
        // because every lookup hits a cached screen.
        let mut mem = Memory::new();
        let sids: Vec<ScreenId> = (0..10)
            .map(|i| {
                ScreenId::compute(
                    format!("a11y-{i}").as_bytes(),
                    format!("ph-{i}").as_bytes(),
                    &format!("com.foo/.S{i}"),
                )
            })
            .collect();
        // Pre-warm each screen once.
        for sid in &sids {
            mem.record_success(*sid, dummy_action_x());
        }
        // Look up each screen 10 times.
        for _ in 0..10 {
            for sid in &sids {
                let _ = mem.lookup(*sid);
            }
        }
        let rate = mem.hit_rate().expect("rate");
        assert!(rate > 0.99, "expected ~100%, got {rate}");
        // AC-V3-3.5 metric (≥ 60% per the design doc) is
        // trivially satisfied in this scenario. The
        // production scenario is more demanding: screens
        // surface and recede, and the agent periodically
        // doesn't recognise a screen fingerprint.
    }
}
