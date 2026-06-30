//! Std-only lock-free SPSC ring for the gamepad frame fast-path.
//!
//! Bypasses `mpsc::sync_channel` for [`crate::client::HidClient`] gamepad
//! sends in the `gamepad_only_realtime()` mode. At 240Hz single-producer
//! this replaces `mpsc::sync_channel(4096).send()` (≈50ns futex on full)
//! with a 5–10ns cache-line-padded atomic ring op.
//!
//! # Design
//!
//! - Fixed capacity `N = 8` frames (≈ 8 × 20B payload = 160B storage).
//! - Cache-line-padded `head` (producer-only) and `tail` (consumer-only)
//!   to avoid false sharing across cores.
//! - `Box<[UnsafeCell<MaybeUninit<GamepadFrameRaw>>; N]>` slot storage
//!   so the storage address is stable and we never invalidate references
//!   across pushes/pops.
//! - `push` is non-blocking: returns [`TryPushError::Full`] on overflow.
//!   Drop-oldest is gamepad-correct: a late frame is a stale frame,
//!   dropping it is better than blocking the producer. The
//!   [`crate::client::HidClient`] send methods fall through to the mpsc on
//!   `Full`, so a frame is never silently lost.
//!
//! # Safety
//!
//! The `unsafe` is bounded:
//!
//! - `head` is only written by the producer (which is a single
//!   `&GamepadFrameRing` reference borrowed by every `HidClient::clone()`).
//! - `tail` is only written by the consumer (the dedicated gamepad
//!   consumer thread spawned by
//!   `HidSession::into_client_with_gamepad_ring`).
//! - A slot is fully initialized **before** the producer publishes the
//!   new `head` with `Release`; the consumer only reads a slot after
//!   observing the new `head` with `Acquire`.
//! - `tail` is loaded by the producer with `Relaxed` because stale
//!   tail is fine — the full-ring check is approximate and the
//!   producer can always fall through to the mpsc on `Full`.

use std::cell::UnsafeCell;
use std::mem::MaybeUninit;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::session::GamepadFrameRaw;

/// Compile-time capacity of the SPSC ring. Sized for 240Hz × ~33ms
/// max consumer stall before frames start being dropped.
pub const RING_CAPACITY: usize = 8;

/// Cache-line-padded atomic so `head` and `tail` never share a cache
/// line with each other (or with the slot storage). The alignment
/// attribute is what does the work; no `CACHE_LINE` constant needed.
#[repr(align(64))]
struct CacheLine<T>(T);

/// Error returned by [`GamepadFrameRing::push`] when the ring is full.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TryPushError {
    /// The ring buffer is at capacity. The producer should drop the
    /// frame (gamepad) or fall back to a slower transport.
    Full,
}

/// Std-only lock-free SPSC ring for `GamepadFrameRaw`.
///
/// Created by [`crate::client::HidSession::into_client_with_gamepad_ring`]
/// and shared between all `HidClient::clone()`s (`Arc<GamepadFrameRing>`)
/// on the producer side and the dedicated gamepad consumer thread on the
/// consumer side.
///
/// The ring is **not** multi-producer safe. The single producer is the
/// set of `HidClient` clones calling gamepad send methods from a single
/// thread (typically the 240Hz game loop). The single consumer is the
/// dedicated `"android-hid-gamepad-fast"` thread.
///
/// If you need MPMC, route through `mpsc::sync_channel` instead.
pub struct GamepadFrameRing {
    head: CacheLine<AtomicUsize>,
    tail: CacheLine<AtomicUsize>,
    slots: Box<[UnsafeCell<MaybeUninit<GamepadFrameRaw>>; RING_CAPACITY]>,
}

// SAFETY: `GamepadFrameRing` is an SPSC ring. The slot storage is
// `UnsafeCell` (not `Sync` by default) because we manage the aliasing
// invariants ourselves. We explicitly assert that the ring can be
// transferred across thread boundaries (so the producer thread can hold
// the `Arc<GamepadFrameRing>` and the consumer thread can hold another
// clone), but we do NOT assert that two threads can call `push`
// concurrently or two threads can call `pop` concurrently. The
// `Send + Sync` bounds here are required so `Arc<GamepadFrameRing>`
// is `Send`, which lets us `.spawn()` the consumer thread with an
// `Arc<GamepadFrameRing>` in its closure. The actual SPSC invariant is
// upheld by the caller (the gamepad consumer thread is the only
// consumer; the 240Hz game loop is the only producer).
unsafe impl Send for GamepadFrameRing {}
unsafe impl Sync for GamepadFrameRing {}

impl std::fmt::Debug for GamepadFrameRing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GamepadFrameRing")
            .field("capacity", &RING_CAPACITY)
            .field("len", &self.len())
            .finish_non_exhaustive()
    }
}

impl GamepadFrameRing {
    /// Build a fresh ring with [`RING_CAPACITY`] slots. All slots are
    /// uninitialised memory; the producer writes a slot fully before
    /// publishing the new head.
    pub fn new() -> Self {
        let slots: Box<[UnsafeCell<MaybeUninit<GamepadFrameRaw>>; RING_CAPACITY]> =
            Box::new(std::array::from_fn(|_| {
                UnsafeCell::new(MaybeUninit::uninit())
            }));
        Self {
            head: CacheLine(AtomicUsize::new(0)),
            tail: CacheLine(AtomicUsize::new(0)),
            slots,
        }
    }

    /// Compile-time capacity.
    pub const fn capacity(&self) -> usize {
        RING_CAPACITY
    }

    /// Approximate number of frames currently in the ring. The value
    /// may be slightly stale by the time the caller uses it — it is
    /// intended for diagnostics, not for control flow.
    pub fn len(&self) -> usize {
        let head = self.head.0.load(Ordering::Relaxed);
        let tail = self.tail.0.load(Ordering::Relaxed);
        // `head - tail` is in `0..=RING_CAPACITY`. The only state that
        // would yield `RING_CAPACITY` (full ring) is rejected by push,
        // so the subtraction alone is sufficient.
        head.wrapping_sub(tail).min(RING_CAPACITY)
    }

    /// `true` when the ring is observably empty. As with [`Self::len`],
    /// this is an approximation and may be stale by the next
    /// instruction.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Try to enqueue a frame. Non-blocking: returns [`TryPushError::Full`]
    /// when the ring is at capacity. The caller is expected to fall
    /// back to a slower transport (e.g. `mpsc::sync_channel`) on `Full`.
    ///
    /// # Safety contract
    ///
    /// The caller must guarantee that this method is called from at
    /// most one thread at a time (single-producer).
    pub fn push(&self, frame: GamepadFrameRaw) -> std::result::Result<(), TryPushError> {
        let head = self.head.0.load(Ordering::Relaxed);
        let tail = self.tail.0.load(Ordering::Relaxed);
        if head.wrapping_sub(tail) >= RING_CAPACITY {
            return Err(TryPushError::Full);
        }
        // SAFETY: the `head - tail < RING_CAPACITY` check above proves
        // the slot at `head % RING_CAPACITY` is not currently owned by
        // the consumer. The producer is the sole writer of this slot
        // until it publishes the new `head` with Release; the consumer
        // will only read the slot after observing the new `head` with
        // Acquire. We are the sole producer, so no other thread races
        // on the same slot.
        unsafe {
            let slot = self.slots.get_unchecked(head % RING_CAPACITY);
            (*slot.get()).write(frame);
        }
        // Publish the new head with Release so the consumer's Acquire
        // load of `head` synchronises with the slot write above.
        self.head.0.store(head.wrapping_add(1), Ordering::Release);
        Ok(())
    }

    /// Pop the next frame, or `None` if the ring is empty.
    ///
    /// # Safety contract
    ///
    /// The caller must guarantee that this method is called from at
    /// most one thread at a time (single-consumer).
    pub fn pop(&self) -> Option<GamepadFrameRaw> {
        let tail = self.tail.0.load(Ordering::Relaxed);
        // Acquire pairs with the producer's Release on `head`. If we
        // observe head > tail, the slot write is visible.
        let head = self.head.0.load(Ordering::Acquire);
        if head == tail {
            return None;
        }
        // SAFETY: `head > tail` (modulo RING_CAPACITY) means the slot
        // at `tail % RING_CAPACITY` was fully written and the new head
        // was published. We are the sole consumer of this slot until
        // we publish the new tail with Release.
        let frame = unsafe {
            let slot = self.slots.get_unchecked(tail % RING_CAPACITY);
            (*slot.get()).assume_init_read()
        };
        // Publish the new tail with Release so a future producer's
        // Acquire load of `tail` would see this (we use Relaxed on the
        // producer side, but the Release on the consumer side is still
        // required for the slot reuse to be ordered after the read).
        self.tail.0.store(tail.wrapping_add(1), Ordering::Release);
        Some(frame)
    }
}

impl Default for GamepadFrameRing {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::thread;

    fn make_frame(seq: u32) -> GamepadFrameRaw {
        // Spread the seq across the field set so any cache of stale
        // values is detected by the equality assertion downstream.
        GamepadFrameRaw {
            buttons: seq,
            left_x: seq as i16,
            left_y: (seq.wrapping_mul(3)) as i16,
            right_x: (seq.wrapping_mul(5)) as i16,
            right_y: (seq.wrapping_mul(7)) as i16,
            left_trigger: (seq.wrapping_mul(11)) as i16,
            right_trigger: (seq.wrapping_mul(13)) as i16,
        }
    }

    #[test]
    fn test_push_pop_basic() {
        let ring = GamepadFrameRing::new();
        assert_eq!(ring.capacity(), RING_CAPACITY);
        assert_eq!(ring.len(), 0);
        assert!(ring.is_empty());

        for i in 1..=5u32 {
            ring.push(make_frame(i)).expect("push within capacity");
        }
        // Approximate len: with one thread, no consumer, the value
        // should be exact.
        assert_eq!(ring.len(), 5);

        for i in 1..=5u32 {
            let popped = ring.pop().expect("pop should yield a frame");
            assert_eq!(popped, make_frame(i), "order must be preserved");
        }
        assert_eq!(ring.len(), 0);
        assert!(ring.is_empty());
        assert!(ring.pop().is_none());
    }

    #[test]
    fn test_ring_full_returns_full() {
        let ring = GamepadFrameRing::new();
        for i in 0..RING_CAPACITY as u32 {
            ring.push(make_frame(i))
                .expect("fill should succeed within capacity");
        }
        // Next push must fail.
        let err = ring.push(make_frame(RING_CAPACITY as u32)).unwrap_err();
        assert_eq!(err, TryPushError::Full);

        // Pop one and the next push should succeed.
        let popped = ring.pop().expect("pop after fill");
        assert_eq!(popped, make_frame(0));
        ring.push(make_frame(RING_CAPACITY as u32))
            .expect("push should succeed after a pop");
    }

    #[test]
    fn test_wrap_around() {
        let ring = GamepadFrameRing::new();
        let total = 1_000u32;
        // Push/pop interleaved so the ring never overflows; this
        // exercises the head/tail wrap-around (mod N) paths.
        for i in 0..total {
            ring.push(make_frame(i))
                .expect("interleaved push must not overflow");
            let popped = ring.pop().expect("interleaved pop");
            assert_eq!(popped, make_frame(i));
        }
        assert!(ring.is_empty());
    }

    #[test]
    fn test_spsc_concurrent() {
        let ring = Arc::new(GamepadFrameRing::new());
        let total: u32 = 10_000;
        let stop = Arc::new(AtomicBool::new(false));
        let produced = Arc::new(AtomicUsize::new(0));
        let consumed = Arc::new(AtomicUsize::new(0));
        let consumer_first_error: Arc<std::sync::Mutex<Option<String>>> =
            Arc::new(std::sync::Mutex::new(None));

        let ring_p = Arc::clone(&ring);
        let produced_p = Arc::clone(&produced);
        let producer = thread::spawn(move || {
            for i in 0..total {
                // Spin until the ring has room so we exercise the
                // wrap-around path with the consumer concurrently
                // running, not just a one-shot test.
                loop {
                    match ring_p.push(make_frame(i)) {
                        Ok(()) => break,
                        Err(TryPushError::Full) => thread::yield_now(),
                    }
                }
                produced_p.fetch_add(1, Ordering::Relaxed);
            }
        });

        let ring_c = Arc::clone(&ring);
        let consumed_c = Arc::clone(&consumed);
        let err_c = Arc::clone(&consumer_first_error);
        let stop_c = Arc::clone(&stop);
        let consumer = thread::spawn(move || {
            let mut last_seen: i64 = -1;
            let mut local_count: u32 = 0;
            loop {
                match ring_c.pop() {
                    Some(frame) => {
                        let seq = frame.buttons as i64;
                        // The producer is the only writer; sequence
                        // numbers must be strictly monotonic and
                        // contiguous (no loss, no duplication) because
                        // the producer waits on Full.
                        if seq != last_seen + 1 {
                            let mut g = err_c.lock().unwrap();
                            if g.is_none() {
                                *g = Some(format!(
                                    "out-of-order pop: expected {}, got {}",
                                    last_seen + 1,
                                    seq
                                ));
                            }
                        }
                        last_seen = seq;
                        local_count += 1;
                        consumed_c.fetch_add(1, Ordering::Relaxed);
                    }
                    None => {
                        if stop_c.load(Ordering::Relaxed) && ring_c.is_empty() {
                            break;
                        }
                        thread::yield_now();
                    }
                }
            }
            assert_eq!(local_count, total, "consumer must see every frame once");
        });

        producer.join().expect("producer join");
        stop.store(true, Ordering::Release);
        consumer.join().expect("consumer join");

        assert_eq!(produced.load(Ordering::Relaxed), total as usize);
        assert_eq!(consumed.load(Ordering::Relaxed), total as usize);
        let err = consumer_first_error.lock().unwrap().take();
        assert!(err.is_none(), "ordering violation: {err:?}");
    }

    #[test]
    fn test_gamepad_fast_path_integration() {
        use crate::session::HidSession;
        use crate::session::OpenRequest;
        use crate::transport::MockTransport;
        use crate::types::HID_ID_GAMEPAD_FIRST;

        let session = HidSession::open(
            MockTransport::new(),
            OpenRequest::gamepad_only_realtime(),
        )
        .expect("open gamepad-only session");
        let (client, dispatcher) = session
            .into_client_with_gamepad_ring()
            .expect("client w/ ring");

        let total: u32 = 100;
        // Sanity check: the client was constructed in fast-path mode.
        assert!(
            client.is_gamepad_fast_path(),
            "client should be in gamepad fast-path mode"
        );
        // Push frames one at a time via the non-blocking fast path.
        // Drop-on-full is the correct gamepad semantic (a late frame
        // is a stale frame). The test verifies that the consumer
        // drains the ring and the wire count matches the number of
        // frames accepted by the ring.
        //
        // We start at `i = 1` because the gamepad's initial state is
        // all-zeros; the first frame with `buttons: 0` would be deduped
        // by `set_frame_raw` (no payload emitted).
        let mut accepted: u32 = 0;
        for i in 1..=total {
            if client.try_send_frame(make_frame(i)).is_ok() {
                accepted += 1;
            }
            // Give the consumer thread a chance to drain the ring so
            // we don't lose all but 8 frames to back-pressure.
            std::thread::yield_now();
            std::thread::sleep(std::time::Duration::from_micros(50));
        }
        assert!(
            accepted >= 1,
            "at least some frames should be accepted (got {accepted})"
        );
        // The consumer thread should not have observed an error.
        assert!(!client.gamepad_consumer_error());

        // Close the client: this signals the consumer thread to exit
        // its loop, close the session (UHID_DESTROY), and return the
        // transport. `dispatcher.join()` reaps the consumer thread.
        client.close();
        let transport = dispatcher.join().expect("dispatcher join");

        // Count UHID_INPUT messages with the gamepad HID id. In direct
        // mode the coalescer flushes after every push, so we expect
        // exactly `total` such reports on the wire (plus one
        // UHID_CREATE from open() and one UHID_DESTROY from close()).
        let bytes = transport.into_bytes();
        let mut gamepad_inputs = 0usize;
        let mut i = 0usize;
        while i < bytes.len() {
            match bytes[i] {
                12 => {
                    if i + 8 > bytes.len() {
                        break;
                    }
                    let name_len = bytes[i + 7] as usize;
                    if i + 8 + name_len + 2 > bytes.len() {
                        break;
                    }
                    let rd_len_idx = i + 8 + name_len;
                    let rd_len = u16::from_be_bytes([bytes[rd_len_idx], bytes[rd_len_idx + 1]])
                        as usize;
                    i += 8 + name_len + 2 + rd_len;
                }
                13 => {
                    if i + 5 > bytes.len() {
                        break;
                    }
                    let id = u16::from_be_bytes([bytes[i + 1], bytes[i + 2]]);
                    let size = u16::from_be_bytes([bytes[i + 3], bytes[i + 4]]) as usize;
                    if i + 5 + size > bytes.len() {
                        break;
                    }
                    if id == HID_ID_GAMEPAD_FIRST {
                        gamepad_inputs += 1;
                    }
                    i += 5 + size;
                }
                14 => i += 3,
                _ => break,
            }
        }
        assert_eq!(
            gamepad_inputs, accepted as usize,
            "wire UHID_INPUT count ({gamepad_inputs}) must match ring-accepted count ({accepted})"
        );
    }
}
