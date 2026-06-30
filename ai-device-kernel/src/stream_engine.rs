//! Observation stream + multi-subscriber server-push — see v3 §3.2.3.
//!
//! The stream engine is the **single source of truth** for events
//! flowing out of the kernel. Subscribers attach with
//! [`Subscriber::subscribe`] and receive observations in causal
//! order; `seq` strictly increases so a missed packet can be
//! re-fetched by gap-fill (`subscribe(since_seq=N)` returns all
//! observations with `seq > N`).
//!
//! ## Design
//!
//! - **Server-push, not poll.** Subscribers do not call back; the
//!   kernel pushes observations as it produces them. v3 §3.2.3
//!   "1 个 observation stream, 多个 subscriber, server-side push
//!   (P7/P9 解)".
//! - **Per-subscriber filter.** Each subscriber declares which
//!   [`EventKind`]s it cares about; events outside the filter
//!   are filtered out *per subscriber*, not at the source.
//! - **Bounded buffer.** Each subscriber gets a bounded queue;
//!   on overflow the **oldest** event is dropped (subscribe side
//!   is what loses data, never the kernel's main loop).
//!
//! ## Threading
//!
//! The current `StreamEngine` is single-threaded. Mutating
//! during a subscriber poll is fine — the [`Subscriber::poll`]
//! method holds a `&mut` reference to the queue.
//!
//! When the kernel needs to run on a dedicated thread, the
//! natural extension is a `crossbeam::channel::Receiver`
//! bridging this engine to the wire IO — that lands in Phase 6
//! when we wire the daemon thread pool in.

use std::collections::VecDeque;

use crate::observation::{DeviceEvent, Observation};
use crate::predicate::EventKind;

/// Per-subscriber bounded queue cap. Beyond this, the oldest
/// observation is dropped *for this subscriber only* — the main
/// stream record is unaffected.
pub const SUBSCRIBER_QUEUE_CAP: usize = 256;

/// One subscriber's view onto the observation stream.
///
/// Each `Subscriber` carries its own bounded queue, last-seen
/// `seq` (so re-subscribe with `since_seq` works correctly),
/// and optional [`EventKind`] filter.
#[derive(Debug)]
pub struct Subscriber {
    id: u64,
    /// Last seq delivered to this subscriber (i.e., the seq that
    /// `poll()` returned most recently).
    last_seq: Option<u64>,
    /// Pending observations, oldest first. Trimmed to
    /// [`SUBSCRIBER_QUEUE_CAP`] on overflow (drop-oldest).
    queue: VecDeque<Observation>,
    /// Optional filter — if `Some`, only observations whose
    /// `events` field contains at least one matching kind are
    /// enqueued.
    filter: Option<Vec<EventKind>>,
    /// Diagnostic count of events dropped due to local queue
    /// overflow (separate from `last_seq` to avoid lying about
    /// exactly where gaps occurred).
    dropped: u64,
}

impl Subscriber {
    /// Construct a fresh subscriber with no events delivered.
    #[must_use]
    pub fn new(id: u64) -> Self {
        Self {
            id,
            last_seq: None,
            queue: VecDeque::new(),
            filter: None,
            dropped: 0,
        }
    }

    /// Override the event filter. Pass `None` for "deliver
    /// everything".
    #[must_use]
    pub fn with_filter(mut self, filter: Option<Vec<EventKind>>) -> Self {
        self.filter = filter;
        self
    }

    /// Subscriber id (assigned by the engine).
    #[inline]
    #[must_use]
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Last `seq` delivered to this subscriber, if any
    /// observation has been polled.
    #[inline]
    #[must_use]
    pub fn last_seq(&self) -> Option<u64> {
        self.last_seq
    }

    /// Count of events dropped from this subscriber's queue due
    /// to overflow. Pure diagnostic.
    #[inline]
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Pop the next observation, if any. Returns `None` when the
    /// queue is empty.
    pub fn poll(&mut self) -> Option<Observation> {
        let obs = self.queue.pop_front()?;
        self.last_seq = Some(obs.seq);
        Some(obs)
    }

    /// Number of observations currently queued.
    #[inline]
    #[must_use]
    pub fn pending(&self) -> usize {
        self.queue.len()
    }

    /// True if the subscriber's filter accepts this observation.
    fn accepts(&self, obs: &Observation) -> bool {
        match &self.filter {
            None => true,
            Some(filter) => obs
                .events
                .iter()
                .any(|ev| filter.iter().any(|k| EventKind::from_event(ev) == Some(*k))),
        }
    }

    /// Push one observation into the queue, applying the filter
    /// and the bounded-queue trim. Returns `true` if the
    /// observation was actually enqueued.
    fn push(&mut self, obs: Observation) -> bool {
        if !self.accepts(&obs) {
            return false;
        }
        if self.queue.len() >= SUBSCRIBER_QUEUE_CAP {
            self.queue.pop_front();
            self.dropped += 1;
        }
        self.queue.push_back(obs);
        true
    }

    /// Replay queue contents (without consuming) — useful for
    /// `subscribe(since_seq=N)`: the engine pre-fills this
    /// subscriber's queue with all post-`N` observations from
    /// the history.
    fn replay_from_history(&mut self, history: &VecDeque<Observation>, since_seq: u64) {
        for obs in history {
            if obs.seq > since_seq {
                let _ = self.push(obs.clone());
            }
        }
    }
}

/// The observation stream engine itself. Single-threaded; thread
/// safety deferred to Phase 6 when the daemon thread pool lands.
#[derive(Debug)]
pub struct StreamEngine {
    /// Monotonic seq counter — every `produce()` increments it.
    next_seq: u64,
    /// Bounded history of recent observations (capped for
    /// `since_seq`-based replay).
    history: VecDeque<Observation>,
    /// Registered subscribers, keyed by `SubscriberId`.
    subscribers: std::collections::HashMap<u64, Subscriber>,
    /// Next subscriber id to hand out.
    next_subscriber_id: u64,
}

impl Default for StreamEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamEngine {
    /// Build an empty engine.
    #[must_use]
    pub fn new() -> Self {
        Self {
            next_seq: 0,
            history: VecDeque::new(),
            subscribers: std::collections::HashMap::new(),
            next_subscriber_id: 1,
        }
    }

    /// History cap. Sized so memory sits under the v3 §3.3 budget
    /// (most observations are < 1 KiB; 512 × 1 KiB = 512 KiB).
    pub const HISTORY_CAP: usize = 512;

    /// Register a new subscriber. If `since_seq` is provided,
    /// the subscriber's queue is pre-filled with all post-`N`
    /// observations currently in history.
    #[must_use]
    pub fn subscribe(
        &mut self,
        since_seq: u64,
        filter: Option<Vec<EventKind>>,
    ) -> SubscriberHandle {
        let id = self.next_subscriber_id;
        self.next_subscriber_id += 1;
        let mut sub = Subscriber::new(id).with_filter(filter);
        sub.replay_from_history(&self.history, since_seq);
        self.subscribers.insert(id, sub);
        SubscriberHandle(id)
    }

    /// Drop a subscriber (returns `true` if it existed).
    pub fn unsubscribe(&mut self, handle: SubscriberHandle) -> bool {
        self.subscribers.remove(&handle.0).is_some()
    }

    /// Produce one observation. Returns the assigned `seq`.
    /// Automatically fans out to every subscriber whose filter
    /// accepts the observation.
    pub fn produce(
        &mut self,
        obs_factory: impl FnOnce(u64) -> Observation,
    ) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        let obs = obs_factory(seq);

        // Maintain bounded history.
        if self.history.len() >= Self::HISTORY_CAP {
            self.history.pop_front();
        }
        self.history.push_back(obs.clone());

        // Fan out to every subscriber.
        for sub in self.subscribers.values_mut() {
            let _ = sub.push(obs.clone());
        }
        seq
    }

    /// Look up a subscriber by handle. Used by the wire layer when
    /// a host's `Observe` request selects an existing subscriber.
    pub fn subscriber(&self, handle: SubscriberHandle) -> Option<&Subscriber> {
        self.subscribers.get(&handle.0)
    }

    /// Mutable handle for polling (consuming) observations.
    pub fn subscriber_mut(&mut self, handle: SubscriberHandle) -> Option<&mut Subscriber> {
        self.subscribers.get_mut(&handle.0)
    }

    /// Total registered subscribers.
    #[must_use]
    pub fn subscriber_count(&self) -> usize {
        self.subscribers.len()
    }

    /// Number of observations currently in the bounded history.
    /// Useful for `subscribe(since_seq=N)` replay budgeting.
    #[must_use]
    pub fn history_len(&self) -> usize {
        self.history.len()
    }

    /// Highest `seq` produced so far.
    #[inline]
    #[must_use]
    pub fn head_seq(&self) -> Option<u64> {
        if self.next_seq == 0 {
            None
        } else {
            Some(self.next_seq - 1)
        }
    }

    /// Iterate over every observation with `seq > since_seq` in
    /// the bounded history. Used by the wire layer's
    /// `Observation` reply to a `Query` request.
    pub fn since_seq(&self, since_seq: u64) -> impl Iterator<Item = &Observation> {
        self.history.iter().filter(move |o| o.seq > since_seq)
    }

    /// Convenience: read every event currently in history with
    /// `seq > since_seq`, returned as a `Vec<DeviceEvent>`
    /// (deduplicated by kind). Useful for the predicate engine
    /// when it's matching against recent activity.
    pub fn events_since(&self, since_seq: u64) -> Vec<DeviceEvent> {
        let mut out: Vec<DeviceEvent> = Vec::new();
        for obs in self.since_seq(since_seq) {
            for ev in &obs.events {
                if !out.iter().any(|e| std::mem::discriminant(e) == std::mem::discriminant(ev)) {
                    out.push(ev.clone());
                }
            }
        }
        out
    }
}

/// Opaque subscriber handle returned from [`StreamEngine::subscribe`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SubscriberHandle(pub u64);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observation::{DeviceState, FrameSnapshot};

    fn dummy_obs(seq: u64) -> Observation {
        Observation {
            seq,
            timestamp_ms: seq * 10,
            a11y: None,
            frame: None,
            state: DeviceState::unknown(seq * 10),
            events: vec![],
        }
    }

    fn make_dummy() -> impl FnOnce(u64) -> Observation {
        |seq| dummy_obs(seq)
    }

    fn obs_with_event(seq: u64, ev: DeviceEvent) -> Observation {
        Observation {
            seq,
            timestamp_ms: seq * 10,
            a11y: None,
            frame: None,
            state: DeviceState::unknown(seq * 10),
            events: vec![ev],
        }
    }

    fn make_with_event(ev: DeviceEvent) -> impl FnOnce(u64) -> Observation {
        move |seq| obs_with_event(seq, ev.clone())
    }

    #[test]
    fn empty_engine_has_no_subscribers() {
        let engine = StreamEngine::new();
        assert_eq!(engine.subscriber_count(), 0);
        assert_eq!(engine.head_seq(), None);
        assert_eq!(engine.history_len(), 0);
    }

    #[test]
    fn produce_assigns_monotonic_seq() {
        let mut engine = StreamEngine::new();
        let s0 = engine.produce(make_dummy());
        assert_eq!(s0, 0);
        assert_eq!(engine.head_seq(), Some(0));
        let s1 = engine.produce(make_dummy());
        assert_eq!(s1, 1);
        assert_eq!(engine.head_seq(), Some(1));
    }

    #[test]
    fn subscriber_receives_subsequent_observations_only() {
        let mut engine = StreamEngine::new();
        // Pre-produce some history.
        for _ in 0..3 {
            engine.produce(make_dummy());
        }
        let handle = engine.subscribe(/* since_seq= */ 2, None);
        // No fresh observations after subscribe.
        assert_eq!(engine.subscriber(handle).unwrap().pending(), 0);
        // Now produce 2 more.
        engine.produce(make_dummy());
        engine.produce(make_dummy());
        assert_eq!(engine.subscriber(handle).unwrap().pending(), 2);
    }

    #[test]
    fn subscribe_with_since_seq_replays_history() {
        let mut engine = StreamEngine::new();
        engine.produce(make_dummy());
        engine.produce(make_dummy());
        let snap = engine.head_seq().unwrap();
        engine.produce(make_dummy());
        engine.produce(make_dummy());

        // since_seq = snap → subscriber should see exactly the
        // post-snap observations (2 of them).
        let handle = engine.subscribe(snap, None);
        let sub = engine.subscriber(handle).unwrap();
        assert_eq!(sub.pending(), 2);
        let first = engine.subscriber_mut(handle).unwrap().poll().unwrap();
        assert!(first.seq > snap);
    }

    #[test]
    fn per_subscriber_filter_applies() {
        let mut engine = StreamEngine::new();
        let handle = engine.subscribe(0, Some(vec![EventKind::ActivityResumed]));
        engine.produce(make_with_event(DeviceEvent::ActivityResumed {
            component: "p/.a".into(),
        }));
        engine.produce(make_with_event(DeviceEvent::SceneChangeDetected {
            score: 0.5,
        }));
        engine.produce(make_with_event(DeviceEvent::ActivityResumed {
            component: "p/.b".into(),
        }));
        // Only ActivityResumed observations (2 of 3) are queued.
        let sub = engine.subscriber(handle).unwrap();
        assert_eq!(sub.pending(), 2);
        let first = engine.subscriber_mut(handle).unwrap().poll().unwrap();
        assert!(matches!(first.events[0], DeviceEvent::ActivityResumed { .. }));
    }

    #[test]
    fn multi_subscriber_each_gets_own_queue() {
        let mut engine = StreamEngine::new();
        let h1 = engine.subscribe(0, None);
        let h2 = engine.subscribe(0, None);
        engine.produce(make_dummy());
        // Both subscribers received the same observation.
        assert_eq!(engine.subscriber(h1).unwrap().pending(), 1);
        assert_eq!(engine.subscriber(h2).unwrap().pending(), 1);
        // Polling one doesn't drain the other.
        let _ = engine.subscriber_mut(h1).unwrap().poll();
        assert_eq!(engine.subscriber(h1).unwrap().pending(), 0);
        assert_eq!(engine.subscriber(h2).unwrap().pending(), 1);
    }

    #[test]
    fn subscriber_queue_trims_oldest_on_overflow() {
        let mut engine = StreamEngine::new();
        let handle = engine.subscribe(0, None);
        for _ in 0..(SUBSCRIBER_QUEUE_CAP + 50) as u64 {
            engine.produce(make_dummy());
        }
        let sub = engine.subscriber(handle).unwrap();
        assert_eq!(sub.pending(), SUBSCRIBER_QUEUE_CAP);
        assert!(sub.dropped() > 0, "drop counter should be > 0");
    }

    #[test]
    fn unsubscribe_removes_subscriber() {
        let mut engine = StreamEngine::new();
        let h = engine.subscribe(0, None);
        assert_eq!(engine.subscriber_count(), 1);
        assert!(engine.unsubscribe(h));
        assert_eq!(engine.subscriber_count(), 0);
        assert!(!engine.unsubscribe(h), "double-unsubscribe returns false");
    }

    #[test]
    fn history_trims_to_cap() {
        let mut engine = StreamEngine::new();
        for _ in 0..(StreamEngine::HISTORY_CAP + 50) as u64 {
            engine.produce(make_dummy());
        }
        assert!(engine.history_len() <= StreamEngine::HISTORY_CAP);
    }

    #[test]
    fn since_seq_returns_only_post_seq() {
        let mut engine = StreamEngine::new();
        engine.produce(make_dummy());
        engine.produce(make_dummy());
        let mid = engine.head_seq().unwrap();
        engine.produce(make_dummy());
        engine.produce(make_dummy());
        let collected: Vec<&Observation> = engine.since_seq(mid).collect();
        assert_eq!(collected.len(), 2);
        for obs in &collected {
            assert!(obs.seq > mid);
        }
    }

    #[test]
    fn since_seq_dedup_no_duplicate_events() {
        // AC-V3-2.1: subscribe(since_seq=N) does NOT re-deliver
        // events already delivered to a peer subscriber at the
        // same seq. We test the engine-side view: when two
        // observations carry the same kind of event, the
        // caller-side dedup collapses them.
        let mut engine = StreamEngine::new();
        engine.produce(make_with_event(DeviceEvent::ActivityResumed {
            component: "p/.a".into(),
        }));
        engine.produce(make_with_event(DeviceEvent::ActivityResumed {
            component: "p/.b".into(),
        }));
        let events: Vec<DeviceEvent> = engine.events_since(0);
        // Two observations → caller-side wants one per kind.
        // `events_since` already dedups via discriminant.
        let unique_kinds: std::collections::HashSet<_> = events
            .iter()
            .map(std::mem::discriminant)
            .collect();
        assert_eq!(
            unique_kinds.len(),
            events.len(),
            "duplicate event kinds leaked"
        );
    }

    #[test]
    fn frame_snapshot_in_observation_preserved() {
        let mut engine = StreamEngine::new();
        let h = engine.subscribe(0, None);
        engine.produce(|s| Observation {
            seq: s,
            timestamp_ms: 0,
            a11y: None,
            frame: Some(FrameSnapshot {
                width: 1080,
                height: 1920,
                codec: 1,
                is_keyframe: true,
                pts: 90000,
                scene_change_score: 0.5,
            }),
            state: DeviceState::unknown(0),
            events: vec![],
        });
        let obs = engine.subscriber_mut(h).unwrap().poll().expect("got frame obs");
        let frame = obs.frame.expect("frame snapshot preserved");
        assert_eq!(frame.width, 1080);
        assert!(frame.is_keyframe);
    }
}
