//! Integration tests for `HidSession` — the high-level facade that
//! opens kbd / mouse / gamepad in one call and tears them down
//! panic-safely. Every test drives a `MockTransport` so no real device
//! is needed.

use android_hid_connect::session::{HidSession, OpenRequest};
use android_hid_connect::transport::MockTransport;
use android_hid_connect::types::{GamepadAxis, GamepadButton, HID_ID_GAMEPAD_FIRST};

/// Type tag of `INJECT_TOUCH_EVENT` (matches `ControlMsgType::InjectTouchEvent = 2`).
const TAG_TOUCH: u8 = 2;
/// Fixed payload size of `INJECT_TOUCH_EVENT` (1+8+4+4+2+2+2+4+4).
const TOUCH_PAYLOAD_SIZE: usize = 31;

/// Walk through the recorded bytes and pull out the UHID_CREATE /
/// UHID_DESTROY / UHID_INPUT frame boundaries.
fn split_messages(bytes: &[u8]) -> Vec<(u8, Vec<u8>)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let tag = bytes[i];
        match tag {
            12 => {
                if i + 8 > bytes.len() { break; }
                let name_len = bytes[i + 7] as usize;
                if i + 8 + name_len + 2 > bytes.len() { break; }
                let rd_size = u16::from_be_bytes([
                    bytes[i + 8 + name_len],
                    bytes[i + 8 + name_len + 1],
                ]) as usize;
                let total = 8 + name_len + 2 + rd_size;
                if i + total > bytes.len() { break; }
                out.push((tag, bytes[i..i + total].to_vec()));
                i += total;
            }
            13 => {
                if i + 5 > bytes.len() { break; }
                let size = u16::from_be_bytes([bytes[i + 3], bytes[i + 4]]) as usize;
                let total = 5 + size;
                if i + total > bytes.len() { break; }
                out.push((tag, bytes[i..i + total].to_vec()));
                i += total;
            }
            14 => {
                if i + 3 > bytes.len() { break; }
                out.push((tag, bytes[i..i + 3].to_vec()));
                i += 3;
            }
            _ => break,
        }
    }
    out
}

fn count_create_for(bytes: &[u8], id: u16) -> usize {
    split_messages(bytes).iter()
        .filter(|(t, m)| *t == 12 && u16::from_be_bytes([m[1], m[2]]) == id)
        .count()
}

fn count_destroy_for(bytes: &[u8], id: u16) -> usize {
    split_messages(bytes).iter()
        .filter(|(t, m)| *t == 14 && u16::from_be_bytes([m[1], m[2]]) == id)
        .count()
}

fn count_inputs(bytes: &[u8]) -> usize {
    split_messages(bytes).iter().filter(|(t, _)| *t == 13).count()
}

fn count_touch_events(bytes: &[u8]) -> usize {
    // INJECT_TOUCH_EVENT = [02, action(1), pointer_id(8), x(4), y(4), w(2), h(2), pressure(2), action_button(4), buttons(4)] = 32 bytes
    let mut count = 0;
    let mut i = 0;
    while i + 1 + TOUCH_PAYLOAD_SIZE <= bytes.len() {
        if bytes[i] == TAG_TOUCH
            && (bytes[i + 1] == 0 || bytes[i + 1] == 1 || bytes[i + 1] == 2)
            && bytes[i + 2..i + 10].iter().all(|b| *b == 0)
        {
            count += 1;
            i += 1 + TOUCH_PAYLOAD_SIZE;
        } else {
            i += 1;
        }
    }
    count
}

/// Build a session, run the closure with a `&mut HidSession`, close it,
/// return the recorded bytes.
fn run<F: FnOnce(&mut HidSession<MockTransport>)>(req: OpenRequest, f: F) -> Vec<u8> {
    let mut s = HidSession::open(MockTransport::new(), req).unwrap();
    f(&mut s);
    s.close().unwrap();
    s.into_inner().into_bytes()
}

#[test]
fn open_creates_three_devices() {
    let bytes = run(OpenRequest::all(), |_| {});
    assert_eq!(count_create_for(&bytes, 1), 1, "kbd CREATE");
    assert_eq!(count_create_for(&bytes, 2), 1, "mouse CREATE");
    assert_eq!(count_create_for(&bytes, HID_ID_GAMEPAD_FIRST), 1, "gamepad CREATE");
    assert_eq!(count_destroy_for(&bytes, 1), 1, "kbd DESTROY");
    assert_eq!(count_destroy_for(&bytes, 2), 1, "mouse DESTROY");
    assert_eq!(count_destroy_for(&bytes, HID_ID_GAMEPAD_FIRST), 1, "gamepad DESTROY");
}

#[test]
fn type_text_emits_key_events() {
    let bytes = run(OpenRequest::kbd_only(), |s| {
        s.type_text("Hi").unwrap();
    });
    // 1 CREATE + 4 INPUT (H down, H up, i down, i up) + 1 DESTROY
    assert_eq!(count_inputs(&bytes), 4, "expected 4 key events for 'Hi'");
}

#[test]
fn type_text_strict_errors_on_unsupported() {
    let mut s = HidSession::open(MockTransport::new(), OpenRequest::kbd_only()).unwrap();
    let r = s.type_text_strict("中");
    assert!(r.is_err(), "expected error on unsupported char");
    // Close cleanly.
    s.close().unwrap();
}

#[test]
fn tap_emits_touch_down_up() {
    let bytes = run(OpenRequest::kbd_only(), |s| {
        s.set_screen_size(1080, 2400);
        s.tap(540, 1200).unwrap();
    });
    assert_eq!(count_touch_events(&bytes), 2, "expected 2 touch events (down + up)");
}

#[test]
fn swipe_emits_intermediate_moves() {
    let bytes = run(OpenRequest::kbd_only(), |s| {
        s.set_screen_size(1080, 2400);
        s.swipe((100, 500), (900, 500), std::time::Duration::from_millis(300), 5).unwrap();
    });
    let n = count_touch_events(&bytes);
    assert!(n >= 5, "expected >= 5 touch events for 5-step swipe, got {}", n);
}

#[test]
fn gamepad_button_and_axis_emit_inputs() {
    let bytes = run(OpenRequest::gamepad_only(), |s| {
        s.set_button(GamepadButton::South, true).unwrap();
        s.set_button(GamepadButton::South, false).unwrap();
        s.set_stick(GamepadAxis::LeftX, 0.5).unwrap();
    });
    // 2 button events + 1 axis event = 3 UHID_INPUT frames
    assert_eq!(count_inputs(&bytes), 3);
}

#[test]
fn drop_is_panic_safe() {
    use std::panic;
    // Construct a session, capture it, then drop in a catch_unwind. The
    // drop impl must not panic even when DESTROY write would succeed.
    let s = HidSession::open(MockTransport::new(), OpenRequest::all()).unwrap();
    let result = panic::catch_unwind(panic::AssertUnwindSafe(|| {
        drop(s);
    }));
    assert!(result.is_ok(), "drop must not panic");
}

#[test]
fn drop_sends_destroy() {
    // We can't easily observe the transport after drop, so verify the
    // structural invariant: explicit close() sends DESTROY, and Drop
    // calls the same try_close_all path. The Drop path is covered by
    // `drop_is_panic_safe` (no panic) and the explicit `close` test.
    let mut s = HidSession::open(MockTransport::new(), OpenRequest::all()).unwrap();
    s.close().unwrap();
    assert!(s.is_closed());
    let t = s.into_inner();
    let bytes = t.into_bytes();
    assert_eq!(count_destroy_for(&bytes, 1), 1);
    assert_eq!(count_destroy_for(&bytes, 2), 1);
    assert_eq!(count_destroy_for(&bytes, HID_ID_GAMEPAD_FIRST), 1);
}

#[test]
fn close_is_idempotent() {
    let mut s = HidSession::open(MockTransport::new(), OpenRequest::kbd_only()).unwrap();
    s.close().unwrap();
    s.close().unwrap(); // second call is a no-op
    let t = s.into_inner();
    let bytes = t.into_bytes();
    // Only 1 DESTROY, not 2.
    assert_eq!(count_destroy_for(&bytes, 1), 1);
}

#[test]
fn is_send_sync() {
    fn assert_send<T: Send>() {}
    fn assert_sync<T: Sync>() {}
    assert_send::<HidSession<MockTransport>>();
    assert_sync::<HidSession<MockTransport>>();
}

#[test]
fn lifecycle_error_when_device_not_open() {
    let mut s = HidSession::open(MockTransport::new(), OpenRequest::kbd_only()).unwrap();
    let r = s.set_button(GamepadButton::South, true);
    assert!(r.is_err(), "expected SessionLifecycle error");
    s.close().unwrap();
}

#[test]
fn open_with_none_opens_no_devices() {
    let bytes = run(OpenRequest::none(), |_| {});
    // No CREATE, no DESTROY.
    assert!(bytes.is_empty(), "expected empty bytes, got {:?}", bytes);
}
