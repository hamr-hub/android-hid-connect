//! Gamepad 240Hz SPSC ring sanity — v3 §3.4 + AC-V3-5.1.
//!
//! Validates that the existing `GamepadFrameRing` (SPSC lock-free,
//! capacity = 8) is correct under single-threaded stress: every
//! pushed frame is consumable in FIFO order; once the ring is
//! full, push returns `Err(Full)` and never silently overwrites.
//!
//! AC-V3-5.1 ("240Hz gamepad 30s drop count = 0") is the
//! production-rate assertion. The SPSC ring itself is proven
//! lossless in this test (single-threaded round-trip). The
//! multi-threaded 240 Hz × 30 s stress is exercised in
//! `benches/uhid_throughput.rs` (criterion-backed) and
//! `tests/protocol_tcp_round_trip.rs` (TCP integration).
//!
//! ## Run
//!
//! ```bash
//! cargo test --test gamepad_240hz_bench -- --nocapture
//! ```

use android_hid_connect::gamepad_ring::{GamepadFrameRing, RING_CAPACITY};
use android_hid_connect::session::GamepadFrameRaw;

/// Build a synthetic gamepad frame filled with `seq` so we can
/// verify order-preservation in FIFO.
fn frame(seq: u64) -> GamepadFrameRaw {
    let s = seq as u32;
    GamepadFrameRaw {
        buttons: s,
        left_x: seq as i16,
        left_y: (seq.wrapping_mul(3)) as i16,
        right_x: (seq.wrapping_mul(5)) as i16,
        right_y: (seq.wrapping_mul(7)) as i16,
        left_trigger: (seq.wrapping_mul(11)) as i16,
        right_trigger: (seq.wrapping_mul(13)) as i16,
    }
}

#[test]
fn gamepad_ring_capacity_is_8() {
    // Sanity-check the constant v3 §3.4 cites.
    assert_eq!(RING_CAPACITY, 8);
    let ring = GamepadFrameRing::new();
    for i in 0..RING_CAPACITY {
        ring.push(frame(i as u64)).expect("push within capacity");
    }
    assert!(ring.push(frame(RING_CAPACITY as u64)).is_err());
}

#[test]
fn gamepad_ring_fifo_in_order_at_capacity() {
    // Verify AC-V3-5.1's underlying guarantee (zero drops at the
    // data-structure level): every pushed frame is consumable
    // once, in FIFO order, across the full ring capacity.
    let ring = GamepadFrameRing::new();
    for i in 0..RING_CAPACITY as u64 {
        ring.push(frame(i as u64)).expect("within capacity");
    }
    // Drain — every seq appears exactly once, in order.
    for expected in 0..RING_CAPACITY as u64 {
        let popped = ring.pop().expect("queue not yet drained");
        assert_eq!(popped.buttons, expected as u32, "FIFO order");
    }
    assert!(ring.is_empty());
    assert!(ring.pop().is_none());
    // After draining we should be able to push RING_CAPACITY
    // more frames.
    for i in 0..RING_CAPACITY as u64 {
        ring.push(frame(100 + i)).expect("refilled after drain");
    }
    let last = ring.pop().unwrap();
    assert_eq!(last.buttons, 100);
}

#[test]
fn gamepad_ring_overflow_preserves_oldest() {
    // Over-capacity pushes return `Err(Full)` — the producer
    // must retry; the ring never overwrites the oldest frame.
    let ring = GamepadFrameRing::new();
    for i in 0..RING_CAPACITY as u64 {
        ring.push(frame(i)).expect("push within capacity");
    }
    // Pushing extra frames must fail without side-effects.
    for _ in 0..5 {
        assert!(ring.push(frame(u32::MAX as u64)).is_err());
    }
    // The first RING_CAPACITY frames must still be there.
    for i in 0..RING_CAPACITY as u64 {
        let popped = ring.pop().expect("preserved");
        assert_eq!(popped.buttons, i as u32, "FIFO order preserved under back-pressure");
    }
}

#[test]
fn gamepad_ring_wraparound_round_trip() {
    // SPSC ring pointer wrap-around: push / pop / push / pop
    // interleaved across many cycles, holding the index space
    // past one full wrap.
    let ring = GamepadFrameRing::new();
    for cycle in 0..(RING_CAPACITY as u64 + 1) * 10 {
        let seq = cycle;
        ring.push(frame(seq)).expect("push always succeeds after drain");
        let popped = ring.pop().expect("pop should yield");
        assert_eq!(popped.buttons, seq as u32);
    }
    assert!(ring.is_empty());
}
