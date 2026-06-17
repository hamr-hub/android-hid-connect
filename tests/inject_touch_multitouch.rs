//! Tests that exercise 10-point multi-touch via the low-level
//! `HidSession::inject_touch` API. The high-level `MultitouchHandle`
//! ergonomics are out of scope for this iteration (linter deferred),
//! but the underlying capability is verified here.

use android_hid_connect::session::{HidSession, OpenRequest};
use android_hid_connect::transport::MockTransport;

/// Type tag of `INJECT_TOUCH_EVENT` (= 2). Matches `ControlMsgType::InjectTouchEvent`.
const TAG_TOUCH: u8 = 2;
const TAG_UHID_CREATE: u8 = 12;
const TAG_UHID_DESTROY: u8 = 14;

/// Frame-aware parser for our `MockTransport` byte stream.
/// Walks the wire format strictly, so it is not fooled by patterns
/// inside HID descriptors or other payload bytes.
#[derive(Debug, Default, Clone, Copy)]
struct TouchFrame {
    action: u8,
    pointer_id: u64,
    x: i32,
    y: i32,
}

fn parse_touch_frames(bytes: &[u8]) -> Vec<TouchFrame> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let tag = bytes[i];
        match tag {
            TAG_UHID_CREATE => {
                // 12 | id(2) | vid(2) | pid(2) | name_len(1) | name | rd_size(2) | rd
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
                i += total;
            }
            13 => {
                // UHID_INPUT | id(2) | size(2) | data(size)
                if i + 5 > bytes.len() {
                    break;
                }
                let size = u16::from_be_bytes([bytes[i + 3], bytes[i + 4]]) as usize;
                let total = 5 + size;
                if i + total > bytes.len() {
                    break;
                }
                i += total;
            }
            TAG_UHID_DESTROY => {
                if i + 3 > bytes.len() {
                    break;
                }
                i += 3;
            }
            TAG_TOUCH => {
                // 2 | action(1) | pointer_id(8) | x(4) | y(4) | w(2) | h(2) | pressure(2) | ab(4) | buttons(4) = 32 bytes
                if i + 32 > bytes.len() {
                    break;
                }
                let action = bytes[i + 1];
                let pointer_id = u64::from_be_bytes([
                    bytes[i + 2],
                    bytes[i + 3],
                    bytes[i + 4],
                    bytes[i + 5],
                    bytes[i + 6],
                    bytes[i + 7],
                    bytes[i + 8],
                    bytes[i + 9],
                ]);
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
                out.push(TouchFrame {
                    action,
                    pointer_id,
                    x,
                    y,
                });
                i += 32;
            }
            _ => break,
        }
    }
    out
}

#[test]
fn inject_touch_carries_pointer_id() {
    let mut s = HidSession::open(MockTransport::new(), OpenRequest::kbd_only()).unwrap();
    s.set_screen_size(1080, 2400);
    s.inject_touch(0, 7, 100, 200, 1.0).unwrap();
    s.inject_touch(1, 7, 100, 200, 0.0).unwrap();
    s.close().unwrap();
    let bytes = s.into_inner().into_bytes();
    let frames = parse_touch_frames(&bytes);
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0].action, 0); // DOWN
    assert_eq!(frames[1].action, 1); // UP
    assert!(frames.iter().all(|f| f.pointer_id == 7));
}

#[test]
fn ten_point_lifecycle_via_inject_touch() {
    let mut s = HidSession::open(MockTransport::new(), OpenRequest::kbd_only()).unwrap();
    s.set_screen_size(1080, 2400);
    for id in 0..10u64 {
        s.inject_touch(0, id, (id as i32) * 100, 200, 1.0).unwrap();
    }
    for id in 0..10u64 {
        s.inject_touch(2, id, (id as i32) * 100, 300, 1.0).unwrap();
    }
    for id in 0..10u64 {
        s.inject_touch(1, id, 0, 0, 0.0).unwrap();
    }
    s.close().unwrap();
    let bytes = s.into_inner().into_bytes();
    let frames = parse_touch_frames(&bytes);
    assert_eq!(
        frames.len(),
        30,
        "expected 30 touch events, got {}",
        frames.len()
    );
    for id in 0..10u64 {
        let n = frames.iter().filter(|f| f.pointer_id == id).count();
        assert_eq!(n, 3, "pointer {id} should appear 3 times (down+move+up)");
    }
    // First 10 frames are DOWN (action 0), next 10 are MOVE (action 2), last 10 are UP (action 1).
    for (i, f) in frames.iter().take(10).enumerate() {
        assert_eq!(f.action, 0, "frame {i} should be DOWN");
    }
    for (i, f) in frames.iter().skip(10).take(10).enumerate() {
        assert_eq!(f.action, 2, "frame {} should be MOVE", i + 10);
    }
    for (i, f) in frames.iter().skip(20).take(10).enumerate() {
        assert_eq!(f.action, 1, "frame {} should be UP", i + 20);
    }
}

#[test]
fn inject_touch_serializes_pointer_id_big_endian() {
    let mut s = HidSession::open(MockTransport::new(), OpenRequest::kbd_only()).unwrap();
    s.set_screen_size(1080, 2400);
    s.inject_touch(0, 0x0102030405060708, 0, 0, 1.0).unwrap();
    s.close().unwrap();
    let bytes = s.into_inner().into_bytes();
    let frames = parse_touch_frames(&bytes);
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].pointer_id, 0x0102030405060708);
}

#[test]
fn inject_touch_works_without_any_uhid_device() {
    let mut s = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    s.set_screen_size(1080, 2400);
    s.inject_touch(0, 0, 540, 1200, 1.0).unwrap();
    s.close().unwrap();
    let bytes = s.into_inner().into_bytes();
    let frames = parse_touch_frames(&bytes);
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].pointer_id, 0);
    assert_eq!(frames[0].x, 540);
    assert_eq!(frames[0].y, 1200);
}
