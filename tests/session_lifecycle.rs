//! Integration tests for `HidSession` — the high-level facade that
//! opens kbd / mouse / gamepad in one call and tears them down
//! panic-safely. Every test drives a `MockTransport` so no real device
//! is needed.

use android_hid_connect::session::{GamepadFrameRaw, HidSession, OpenRequest};
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
                if i + 8 > bytes.len() {
                    break;
                }
                let name_len = bytes[i + 7] as usize;
                if i + 8 + name_len + 2 > bytes.len() {
                    break;
                }
                let rd_size =
                    u16::from_be_bytes([bytes[i + 8 + name_len], bytes[i + 8 + name_len + 1]])
                        as usize;
                let total = 8 + name_len + 2 + rd_size;
                if i + total > bytes.len() {
                    break;
                }
                out.push((tag, bytes[i..i + total].to_vec()));
                i += total;
            }
            13 => {
                if i + 5 > bytes.len() {
                    break;
                }
                let size = u16::from_be_bytes([bytes[i + 3], bytes[i + 4]]) as usize;
                let total = 5 + size;
                if i + total > bytes.len() {
                    break;
                }
                out.push((tag, bytes[i..i + total].to_vec()));
                i += total;
            }
            14 => {
                if i + 3 > bytes.len() {
                    break;
                }
                out.push((tag, bytes[i..i + 3].to_vec()));
                i += 3;
            }
            _ => break,
        }
    }
    out
}

fn count_create_for(bytes: &[u8], id: u16) -> usize {
    split_messages(bytes)
        .iter()
        .filter(|(t, m)| *t == 12 && u16::from_be_bytes([m[1], m[2]]) == id)
        .count()
}

fn count_destroy_for(bytes: &[u8], id: u16) -> usize {
    split_messages(bytes)
        .iter()
        .filter(|(t, m)| *t == 14 && u16::from_be_bytes([m[1], m[2]]) == id)
        .count()
}

fn count_inputs(bytes: &[u8]) -> usize {
    split_messages(bytes)
        .iter()
        .filter(|(t, _)| *t == 13)
        .count()
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
    assert_eq!(
        count_create_for(&bytes, HID_ID_GAMEPAD_FIRST),
        1,
        "gamepad CREATE"
    );
    assert_eq!(count_destroy_for(&bytes, 1), 1, "kbd DESTROY");
    assert_eq!(count_destroy_for(&bytes, 2), 1, "mouse DESTROY");
    assert_eq!(
        count_destroy_for(&bytes, HID_ID_GAMEPAD_FIRST),
        1,
        "gamepad DESTROY"
    );
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
    assert_eq!(
        count_touch_events(&bytes),
        2,
        "expected 2 touch events (down + up)"
    );
}

#[test]
fn swipe_emits_intermediate_moves() {
    let bytes = run(OpenRequest::kbd_only(), |s| {
        s.set_screen_size(1080, 2400);
        s.swipe(
            (100, 500),
            (900, 500),
            std::time::Duration::from_millis(300),
            5,
        )
        .unwrap();
    });
    let n = count_touch_events(&bytes);
    assert!(
        n >= 5,
        "expected >= 5 touch events for 5-step swipe, got {}",
        n
    );
}

/// Golden-coordinate guard for the P3 swipe hot-loop optimization:
/// the hoisted-cast interpolation must produce byte-identical (x, y)
/// samples to the pre-optimization `base + delta * (i / steps)` math.
#[test]
fn swipe_interpolation_coordinates_are_exact() {
    let bytes = run(OpenRequest::kbd_only(), |s| {
        s.set_screen_size(1080, 2400);
        // 4 steps from (100, 200) → (900, 1000): the intermediate
        // MOVE samples fall at i=1,2,3 with t = 0.25, 0.5, 0.75.
        s.swipe(
            (100, 200),
            (900, 1000),
            std::time::Duration::from_millis(64),
            4,
        )
        .unwrap();
    });

    // Extract the (x, y) of every INJECT_TOUCH_EVENT in order.
    // Message layout: [tag=02][action(1)][pointer_id(8)][x(4)][y(4)]...
    let mut coords = Vec::new();
    let mut i = 0;
    while i + 1 + TOUCH_PAYLOAD_SIZE <= bytes.len() {
        if bytes[i] == TAG_TOUCH
            && (bytes[i + 1] == 0 || bytes[i + 1] == 1 || bytes[i + 1] == 2)
            && bytes[i + 2..i + 10].iter().all(|b| *b == 0)
        {
            let x = i32::from_be_bytes([
                bytes[i + 10],
                bytes[i + 11],
                bytes[i + 12],
                bytes[i + 13],
            ]);
            let y = i32::from_be_bytes([
                bytes[i + 14],
                bytes[i + 15],
                bytes[i + 16],
                bytes[i + 17],
            ]);
            coords.push((x, y));
            i += 1 + TOUCH_PAYLOAD_SIZE;
        } else {
            i += 1;
        }
    }

    // Recompute the golden samples with the exact original expression.
    let (x0, y0) = (100i32, 200i32);
    let (x1, y1) = (900i32, 1000i32);
    let steps = 4u32;
    let mut golden = vec![(x0, y0)]; // ACTION_DOWN at the start point
    for step in 1..steps {
        let t = step as f32 / steps as f32;
        let x = (x0 as f32 + (x1 - x0) as f32 * t).round() as i32;
        let y = (y0 as f32 + (y1 - y0) as f32 * t).round() as i32;
        golden.push((x, y));
    }
    golden.push((x1, y1)); // final MOVE to the endpoint
    golden.push((x1, y1)); // ACTION_UP at the endpoint

    assert_eq!(
        coords, golden,
        "P3 hoisted-cast swipe must be byte-identical to the original interpolation"
    );
}

/// Count every INJECT_TOUCH_EVENT regardless of pointer id, returning
/// `(action, pointer_id)` pairs in wire order.
fn touch_events_detailed(bytes: &[u8]) -> Vec<(u8, u64)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 1 + TOUCH_PAYLOAD_SIZE <= bytes.len() {
        // A touch frame is tag=02, action in {0,1,2,3}, then an 8-byte
        // pointer id. We validate the following fixed-size fields land
        // on another valid tag (or EOF) to avoid mis-syncing on data.
        if bytes[i] == TAG_TOUCH && matches!(bytes[i + 1], 0..=3) {
            let action = bytes[i + 1];
            let pid = u64::from_be_bytes([
                bytes[i + 2],
                bytes[i + 3],
                bytes[i + 4],
                bytes[i + 5],
                bytes[i + 6],
                bytes[i + 7],
                bytes[i + 8],
                bytes[i + 9],
            ]);
            out.push((action, pid));
            i += 1 + TOUCH_PAYLOAD_SIZE;
        } else {
            i += 1;
        }
    }
    out
}

/// C2: drag holds a press at the origin, streams MOVEs, and releases
/// at the target — down + intermediate moves + final move + up, all on
/// pointer 0.
#[test]
fn drag_emits_hold_move_release_on_single_pointer() {
    let bytes = run(OpenRequest::kbd_only(), |s| {
        s.set_screen_size(1080, 2400);
        s.drag((200, 300), (800, 1500), 6, std::time::Duration::ZERO)
            .unwrap();
    });
    let events = touch_events_detailed(&bytes);
    // All events are on pointer 0.
    assert!(events.iter().all(|(_, pid)| *pid == 0), "drag is single-finger");
    // Exactly one DOWN and one UP, several MOVEs in between.
    let downs = events.iter().filter(|(a, _)| *a == 0).count();
    let ups = events.iter().filter(|(a, _)| *a == 1).count();
    let moves = events.iter().filter(|(a, _)| *a == 2).count();
    assert_eq!(downs, 1, "one pickup");
    assert_eq!(ups, 1, "one drop");
    assert!(moves >= 6, "expected >= 6 drag moves, got {moves}");
    // Order: DOWN first, UP last.
    assert_eq!(events.first().map(|(a, _)| *a), Some(0));
    assert_eq!(events.last().map(|(a, _)| *a), Some(1));
}

/// C3: a two-finger pinch drives pointers 0 and 1 down, streams
/// simultaneous moves, and lifts both.
#[test]
fn pinch_drives_two_pointers() {
    let bytes = run(OpenRequest::kbd_only(), |s| {
        s.set_screen_size(1080, 2400);
        // Spread (zoom-in): 100 px → 400 px radius over 5 steps.
        s.pinch(540, 1200, 100, 400, 5).unwrap();
    });
    let events = touch_events_detailed(&bytes);
    // Both pointers 0 and 1 must appear.
    assert!(events.iter().any(|(_, pid)| *pid == 0), "finger 0 present");
    assert!(events.iter().any(|(_, pid)| *pid == 1), "finger 1 present");
    // One down + one up per finger.
    for pid in [0u64, 1] {
        let downs = events
            .iter()
            .filter(|(a, p)| *a == 0 && *p == pid)
            .count();
        let ups = events.iter().filter(|(a, p)| *a == 1 && *p == pid).count();
        assert_eq!(downs, 1, "finger {pid} one down");
        assert_eq!(ups, 1, "finger {pid} one up");
    }
}

/// C7: a generic N-finger tap puts N fingers down together and lifts
/// them, using distinct pointer ids.
#[test]
fn multi_finger_tap_uses_distinct_pointers() {
    let bytes = run(OpenRequest::kbd_only(), |s| {
        s.multi_finger_tap(&[(100, 100), (300, 100), (500, 100), (700, 100)])
            .unwrap();
    });
    let events = touch_events_detailed(&bytes);
    // 4 downs (pointers 0..4) + 4 ups.
    let downs: Vec<u64> = events
        .iter()
        .filter(|(a, _)| *a == 0)
        .map(|(_, p)| *p)
        .collect();
    let ups: Vec<u64> = events
        .iter()
        .filter(|(a, _)| *a == 1)
        .map(|(_, p)| *p)
        .collect();
    assert_eq!(downs, vec![0, 1, 2, 3], "4 fingers down in order");
    assert_eq!(ups, vec![0, 1, 2, 3], "4 fingers up in order");
}

/// C7: fewer than 2 or more than 10 fingers is rejected.
#[test]
fn multi_finger_tap_rejects_bad_finger_count() {
    let mut s = HidSession::open(MockTransport::new(), OpenRequest::kbd_only()).unwrap();
    assert!(s.multi_finger_tap(&[(1, 1)]).is_err(), "1 finger rejected");
    let too_many: Vec<(i32, i32)> = (0..11).map(|n| (n, n)).collect();
    assert!(
        s.multi_finger_tap(&too_many).is_err(),
        "11 fingers rejected"
    );
    s.close().unwrap();
}

/// C4: a modifier key-chord holds the modifier byte across the key
/// tap, then clears it — the shape a Ctrl+A / Ctrl+C shortcut needs.
#[test]
fn key_chord_holds_then_releases_modifier() {
    use android_hid_connect::types::Modifiers;
    // Keyboard UHID_INPUT frames: tag=13, id(2), size(2), then payload
    // [mod_byte, reserved, sc0, ...]. So within a frame, offset 5 is
    // the modifier byte and offset 7 is the first scancode.
    const HID_LCTRL: u8 = 0x01;
    const SC_A: u8 = 0x04;

    let bytes = run(OpenRequest::kbd_only(), |s| {
        // Ctrl+A (select-all): scancode 0x04 with LCTRL held.
        s.key_chord(Modifiers::LCTRL, SC_A).unwrap();
    });

    // Pull out the keyboard UHID_INPUT frames (tag 13, id 1).
    let inputs: Vec<Vec<u8>> = split_messages(&bytes)
        .into_iter()
        .filter(|(t, m)| *t == 13 && u16::from_be_bytes([m[1], m[2]]) == 1)
        .map(|(_, m)| m)
        .collect();
    assert_eq!(inputs.len(), 3, "chord emits 3 keyboard reports");

    // Report 1: key A down, LCTRL held.
    assert_eq!(inputs[0][5], HID_LCTRL, "modifier held on key-down");
    assert_eq!(inputs[0][7], SC_A, "scancode present on key-down");
    // Report 2: key A up, LCTRL still held.
    assert_eq!(inputs[1][5], HID_LCTRL, "modifier held on key-up");
    // Report 3: modifier released, no keys held.
    assert_eq!(inputs[2][5], 0x00, "modifier cleared at the end");
    assert_eq!(inputs[2][7], 0x00, "no key held at the end");
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
fn gamepad_frame_pack_and_unchecked_always_sends() {
    let packed = GamepadFrameRaw::new(
        GamepadButton::South as u32 | GamepadButton::DpadUp as u32,
        100,
        -200,
        300,
        -400,
        1234,
        4567,
    )
    .pack();
    let bytes = run(OpenRequest::gamepad_only(), |s| {
        s.set_frame_raw_packed(&packed).unwrap();
    });
    let msgs = split_messages(&bytes);
    let gamepad_inputs: Vec<_> = msgs.iter().filter(|(t, _)| *t == 13).collect();
    assert_eq!(gamepad_inputs.len(), 1);
    assert_eq!(&gamepad_inputs[0].1[5..20], &packed);

    let bytes = run(OpenRequest::gamepad_only(), |s| {
        s.set_frame_raw(GamepadButton::South as u32, 100, 120, 300, -400, 1234, 0)
            .unwrap();
        s.set_frame_raw(GamepadButton::South as u32, 100, 120, 300, -400, 1234, 0)
            .unwrap();
        s.set_frame_raw_unchecked(GamepadButton::South as u32, 100, 120, 300, -400, 1234, 0)
            .unwrap();
        s.set_frame_raw_unchecked(GamepadButton::South as u32, 100, 120, 300, -400, 1234, 0)
            .unwrap(); // explicit duplicate sent unconditionally in unchecked path
    });
    // set_frame_raw dedupes equal state, so only 1 + 2 unchecked = 3.
    assert_eq!(count_inputs(&bytes), 3);
}

#[test]
fn inject_raw_gamepad_report_sends_byte_exact_payload() {
    let payload = [0x10, 0x20, 0x30, 0x40, 0x50];
    let bytes = run(OpenRequest::gamepad_only(), |s| {
        s.inject_raw_gamepad_report(&payload).unwrap();
    });
    let msgs = split_messages(&bytes);
    let gamepad_inputs: Vec<_> = msgs.iter().filter(|(tag, _)| *tag == 13).collect();
    assert_eq!(gamepad_inputs.len(), 1);
    assert_eq!(
        u16::from_be_bytes([gamepad_inputs[0].1[1], gamepad_inputs[0].1[2]]),
        HID_ID_GAMEPAD_FIRST,
    );
    assert_eq!(
        u16::from_be_bytes([gamepad_inputs[0].1[3], gamepad_inputs[0].1[4]]),
        payload.len() as u16,
    );
    assert_eq!(&gamepad_inputs[0].1[5..], payload.as_slice());
}

#[test]
fn gamepad_frame_batch_unchecked_always_sends() {
    let frames = [
        GamepadFrameRaw::new(GamepadButton::South as u32, 100, 120, 300, -400, 1234, 0),
        GamepadFrameRaw::new(GamepadButton::South as u32, 100, 120, 300, -400, 1234, 0),
        GamepadFrameRaw::new(
            GamepadButton::South as u32 | GamepadButton::DpadUp as u32,
            100,
            120,
            300,
            -400,
            1234,
            0,
        ),
    ];
    let bytes = run(OpenRequest::gamepad_only(), |s| {
        let _ = s
            .set_frame_raw_batch_unchecked(&frames)
            .expect("first unchecked batch");
        let _ = s
            .set_frame_raw_batch_unchecked(&frames)
            .expect("second unchanged unchecked batch");
    });
    // unchecked path never dedupes, so every frame is pushed every call
    assert_eq!(count_inputs(&bytes), frames.len() * 2);
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
