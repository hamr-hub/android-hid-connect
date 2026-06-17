//! Integration tests for the `CoalescingWriter` paths exposed by
//! `HidSession`. Verifies that:
//!   1. Default `OpenRequest::all()` enables coalescing.
//!   2. Burst of `UhidInput` is flushed by `HidSession::flush_now`.
//!   3. `HidSession::close()` flushes the buffer (via `into_inner`).
//!   4. `HidSession::stats` reports pushed / written / pending counts.
//!   5. `OpenRequest { coalesce: false, .. }` disables coalescing.

use android_hid_connect::session::{HidSession, OpenRequest};
use android_hid_connect::transport::MockTransport;
use android_hid_connect::types::{GamepadAxis, GamepadButton, HID_ID_GAMEPAD_FIRST};

/// Type tag of `UhidInput` (= 13).
const TAG_UHID_INPUT: u8 = 13;

#[test]
fn default_open_enables_coalescing() {
    // 100 stick-tilt events back-to-back should NOT all hit the wire
    // immediately — the writer should buffer them until flush.
    let mut s = HidSession::open(MockTransport::new(), OpenRequest::gamepad_only()).unwrap();
    for _ in 0..100 {
        s.set_stick(GamepadAxis::LeftX, 0.5).unwrap();
    }
    // pushed = 1 CREATE (critical) + 100 inputs = 101
    let (pushed, written, pending) = s.stats();
    assert_eq!(pushed, 101);
    assert!(
        written < (pushed * 20) || pending > 0,
        "coalescing must batch: pushed={pushed} written={written} pending={pending}"
    );
    s.close().unwrap();
}

#[test]
fn explicit_flush_drains_buffer() {
    let mut s = HidSession::open(MockTransport::new(), OpenRequest::gamepad_only()).unwrap();
    for _ in 0..10 {
        s.set_button(GamepadButton::South, true).unwrap();
    }
    let (_, written_before, _) = s.stats();
    let flushed = s.flush_now().unwrap();
    let (_, written_after, pending) = s.stats();
    assert!(flushed > 0, "expected non-zero flush");
    assert!(written_after > written_before);
    assert_eq!(pending, 0);
    s.close().unwrap();
}

#[test]
fn close_flushes_via_into_inner() {
    let mut s = HidSession::open(MockTransport::new(), OpenRequest::gamepad_only()).unwrap();
    for _ in 0..5 {
        s.set_stick(GamepadAxis::LeftY, -0.3).unwrap();
    }
    s.close().unwrap();
    let t = s.into_inner();
    let bytes = t.into_bytes();
    // 1 CREATE (gamepad, > 15B) + 5 INPUTs (~20B each) + 1 DESTROY (3B)
    let input_count = bytes.iter().filter(|b| **b == TAG_UHID_INPUT).count();
    assert!(
        input_count >= 5,
        "expected ≥ 5 UhidInput frames, got {input_count}"
    );
}

#[test]
fn coalesce_false_disables_batching() {
    // With coalesce=false, each `send` flushes immediately.
    let mut s = HidSession::open(
        MockTransport::new(),
        OpenRequest {
            gamepad: true,
            coalesce: false,
            ..OpenRequest::none()
        },
    )
    .unwrap();
    s.set_stick(GamepadAxis::LeftX, 0.1).unwrap();
    s.set_stick(GamepadAxis::LeftX, 0.2).unwrap();
    s.set_stick(GamepadAxis::LeftX, 0.3).unwrap();
    // pushed = 1 CREATE (critical) + 3 inputs = 4
    let (pushed, written, pending) = s.stats();
    assert_eq!(pushed, 4);
    assert!(
        pending == 0,
        "coalesce=false should not buffer: pending={pending}"
    );
    // written should be at least 3 inputs worth of bytes
    assert!(written > 30, "written={written} too small for 3 inputs");
    s.close().unwrap();
}

#[test]
fn critical_message_passes_through() {
    // 1 buffered input + 1 critical DESTROY should both be on the
    // wire after a single explicit flush (or before close).
    let mut s = HidSession::open(MockTransport::new(), OpenRequest::gamepad_only()).unwrap();
    s.set_button(GamepadButton::South, true).unwrap();
    s.flush_now().unwrap();
    // close() sends DESTROY; bytes should contain 1 INPUT + 1 DESTROY.
    s.close().unwrap();
    let t = s.into_inner();
    let bytes = t.into_bytes();
    // Find the DESTROY for the gamepad HID id (3).
    let gamepad_destroy = bytes
        .windows(3)
        .any(|w| w == [14, 0x00, HID_ID_GAMEPAD_FIRST as u8]);
    assert!(gamepad_destroy, "expected DESTROY for gamepad id 3");
}

#[test]
fn direct_packed_batch_flushes_once() {
    let mut s = HidSession::open(MockTransport::new(), OpenRequest::gamepad_only_realtime()).unwrap();
    let frames = vec![[0u8; 15]; 64];
    s.set_frame_raw_packed_batch(&frames).unwrap();
    // create + one flushed dispatch.
    assert_eq!(s.flushes(), 2);
    assert_eq!(s.stats().2, 0);
    s.close().unwrap();
}

#[test]
fn direct_raw_batch_unchecked_flushes_once() {
    use android_hid_connect::session::GamepadFrameRaw;

    let mut s = HidSession::open(MockTransport::new(), OpenRequest::gamepad_only_realtime()).unwrap();
    let frames = vec![
        GamepadFrameRaw::new(1, 0, 0, 0, 0, 0, 0),
        GamepadFrameRaw::new(2, 1, -1, 2, -2, 100, 200),
    ];
    s.set_frame_raw_batch_unchecked(&frames).unwrap();
    // create + one flushed dispatch.
    assert_eq!(s.flushes(), 2);
    assert_eq!(s.stats().2, 0);
    s.close().unwrap();
}
