//! Failure-injection tests for the RAII rollback guards introduced in
//! iter/12-stable-and-fast-2026-07-09.
//!
//! The guards in `session.rs` (`KbdChordGuard`, the inline cleanup in
//! `multi_finger_tap`, the `TouchGuard` around `tap_pointer`) are
//! load-bearing for stability: if a transport write fails mid-sequence,
//! the device must not be left in a half-pressed state.
//!
//! Tests use the byte-collecting [`MockTransport`] for happy-path
//! byte-level assertions (the guard's `disarm()` must be a no-op when
//! the sequence completes) and a small failure-injecting wrapper for
//! the rollback path (the function must return the first error and
//! must not panic).
//!
//! [`MockTransport`]: android_hid_connect::transport::MockTransport

use std::cell::Cell;
use std::io::{self, Write};

use android_hid_connect::session::HidSession;
use android_hid_connect::transport::MockTransport;
use android_hid_connect::types::{Modifiers, TouchPointerId};
use android_hid_connect::OpenRequest;

/// `Write` that returns `Err` from the Nth `write` call onwards and
/// records every call. Used to inject a failure mid-sequence.
struct FailingTransport {
    bytes: Vec<u8>,
    fail_after: usize,
    writes_seen: Cell<usize>,
}

impl FailingTransport {
    fn new(fail_after: usize) -> Self {
        Self {
            bytes: Vec::new(),
            fail_after,
            writes_seen: Cell::new(0),
        }
    }
    fn writes_seen(&self) -> usize {
        self.writes_seen.get()
    }
}

impl Write for FailingTransport {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.writes_seen.get() + 1;
        self.writes_seen.set(n);
        if n > self.fail_after {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "injected"));
        }
        self.bytes.extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Open a `MockTransport`-backed session, run `f`, close the session,
/// then return the collected bytes. Centralizes the close/extract
/// boilerplate the happy-path tests need.
fn collect_bytes<F: FnOnce(&mut HidSession<MockTransport>)>(f: F) -> Vec<u8> {
    let mut s = HidSession::open(MockTransport::new(), OpenRequest::kbd_only()).expect("open");
    f(&mut s);
    s.close().expect("close");
    s.into_inner().bytes
}

/// Same as `collect_bytes` but with a touch-only session.
fn collect_bytes_touch<F: FnOnce(&mut HidSession<MockTransport>)>(f: F) -> Vec<u8> {
    let mut s = HidSession::open(MockTransport::new(), OpenRequest::none()).expect("open");
    f(&mut s);
    s.close().expect("close");
    s.into_inner().bytes
}

/// Count `INJECT_TOUCH_EVENT` (type=2) actions in a scrcpy control
/// byte stream. Touch payload = type(1) + action(1) + pointer_id(8) +
/// x(4) + y(4) + screen_w(2) + screen_h(2) + pressure(2) +
/// action_button(4) + buttons(4) = 32 bytes.
fn touch_actions(bytes: &[u8]) -> Vec<&'static str> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 32 <= bytes.len() {
        if bytes[i] == 2 {
            let action = bytes[i + 1];
            let label = match action {
                0 => "DOWN",
                1 => "UP",
                2 => "MOVE",
                3 => "CANCEL",
                _ => "?",
            };
            out.push(label);
            i += 32;
        } else {
            i += 1;
        }
    }
    out
}

/// Count `UHID_INPUT` (type=13) messages in a scrcpy control byte
/// stream. Payload = type(1) + id(2) + size(2) + data(size).
fn kbd_input_count(bytes: &[u8]) -> usize {
    let mut count = 0;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 13 && i + 5 <= bytes.len() {
            let size = u16::from_be_bytes([bytes[i + 3], bytes[i + 4]]) as usize;
            if i + 5 + size > bytes.len() {
                break;
            }
            count += 1;
            i += 5 + size;
        } else {
            i += 1;
        }
    }
    count
}

#[test]
fn key_chord_emits_three_kbd_inputs_on_happy_path() {
    let bytes = collect_bytes(|s| {
        s.key_chord(Modifiers::LCTRL, 0x06).expect("chord");
    });
    // Chord = (down+mods, up+mods, release) = 3 UHID_INPUT messages.
    assert_eq!(
        kbd_input_count(&bytes),
        3,
        "key_chord should emit exactly 3 kbd inputs (got bytes: {bytes:?})"
    );
}

#[test]
fn tap_pointer_emits_down_then_up_on_happy_path() {
    let bytes = collect_bytes_touch(|s| {
        s.tap_pointer(TouchPointerId::finger(0), 100, 200)
            .expect("tap");
    });
    assert_eq!(
        touch_actions(&bytes),
        vec!["DOWN", "UP"],
        "tap_pointer should emit exactly DOWN, UP"
    );
}

#[test]
fn multi_finger_tap_emits_balanced_down_up_on_happy_path() {
    // multi_finger_tap presses all fingers then releases them all —
    // output is [DOWN×N, UP×N], not interleaved.
    let bytes = collect_bytes_touch(|s| {
        s.multi_finger_tap(&[(100, 100), (300, 100), (500, 100)])
            .expect("3-finger tap");
    });
    let mut expected: Vec<&str> = (0..3).map(|_| "DOWN").collect();
    expected.extend((0..3).map(|_| "UP"));
    assert_eq!(
        touch_actions(&bytes),
        expected,
        "3-finger tap = 3 DOWNs then 3 UPs (got bytes: {bytes:?})"
    );
}

#[test]
fn key_chord_returns_error_on_mid_sequence_failure() {
    // Disable coalescing so each `send()` writes synchronously to the
    // failing transport — otherwise the CoalescingWriter buffers the
    // chord's three kbd inputs and only flushes at the end-of-chord
    // trailing flush, masking our injected failure.
    let setup_writes = {
        let req = OpenRequest::kbd_only().with_coalesce(false);
        let mut probe = HidSession::open(FailingTransport::new(usize::MAX), req)
            .expect("probe open");
        probe.close().expect("probe close");
        probe.into_inner().writes_seen()
    };
    let req = OpenRequest::kbd_only().with_coalesce(false);
    let mut s = HidSession::open(FailingTransport::new(setup_writes), req).expect("open");
    let result = s.key_chord(Modifiers::LCTRL, 0x06);
    assert!(result.is_err(), "expected chord to fail on first write");
    drop(s);
}

#[test]
fn multi_finger_tap_returns_first_error_on_mid_sequence_failure() {
    let setup_writes = {
        let req = OpenRequest::none().with_coalesce(false);
        let mut probe = HidSession::open(FailingTransport::new(usize::MAX), req)
            .expect("probe open");
        probe.close().expect("probe close");
        probe.into_inner().writes_seen()
    };
    let req = OpenRequest::none().with_coalesce(false);
    let mut s = HidSession::open(
        FailingTransport::new(setup_writes + 3),
        req,
    )
    .expect("open");
    let result = s.multi_finger_tap(&[(100, 100), (300, 100), (500, 100), (700, 100)]);
    assert!(result.is_err(), "expected failure on 4th finger");
}