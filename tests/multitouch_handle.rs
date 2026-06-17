//! Integration tests for `MultitouchHandle` (high-level 10-point
//! multi-touch facade over `HidSession::inject_touch`).

use android_hid_connect::error::Error;
use android_hid_connect::multitouch::MAX_POINTERS;
use android_hid_connect::session::{HidSession, OpenRequest};
use android_hid_connect::transport::MockTransport;

const TAG_TOUCH: u8 = 2;
const TAG_UHID_DESTROY: u8 = 14;

#[derive(Debug, Default, Clone, Copy)]
struct TouchFrame {
    action: u8,
    pointer_id: u64,
}

fn parse_touch_frames(bytes: &[u8]) -> Vec<TouchFrame> {
    let mut out = Vec::new();
    let mut i = 0;
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
                if i + 32 > bytes.len() {
                    break;
                }
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
                out.push(TouchFrame {
                    action,
                    pointer_id: pid,
                });
                i += 32;
            }
            _ => break,
        }
    }
    out
}

#[test]
fn down_up_emits_correct_pointer_id() {
    let mut s = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    s.set_screen_size(1080, 2400);
    {
        let mut m = s.multitouch();
        m.down(3, 100, 200, 1.0).unwrap();
        m.up(3).unwrap();
    }
    s.close().unwrap();
    let frames = parse_touch_frames(&s.into_inner().into_bytes());
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0].action, 0); // DOWN
    assert_eq!(frames[1].action, 1); // UP
    assert!(frames.iter().all(|f| f.pointer_id == 3));
}

#[test]
fn ten_point_lifecycle() {
    let mut s = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    s.set_screen_size(1080, 2400);
    {
        let mut m = s.multitouch();
        for id in 0..MAX_POINTERS {
            m.down(id, (id as i32) * 100, 200, 1.0).unwrap();
        }
        assert_eq!(m.active_count(), MAX_POINTERS as usize);
        for id in 0..MAX_POINTERS {
            m.move_to(id, (id as i32) * 100, 300, 1.0).unwrap();
        }
        for id in 0..MAX_POINTERS {
            m.up(id).unwrap();
        }
        assert_eq!(m.active_count(), 0);
    }
    s.close().unwrap();
    let frames = parse_touch_frames(&s.into_inner().into_bytes());
    assert_eq!(
        frames.len(),
        30,
        "expected 30 events (10 down + 10 move + 10 up)"
    );
    for id in 0..MAX_POINTERS {
        let n = frames.iter().filter(|f| f.pointer_id == id).count();
        assert_eq!(n, 3, "pointer {id} should appear 3 times");
    }
}

#[test]
fn pinch_emits_alternating_pointers() {
    let mut s = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    s.set_screen_size(1080, 2400);
    {
        let mut m = s.multitouch();
        m.down(0, 100, 100, 1.0).unwrap();
        m.down(1, 200, 100, 1.0).unwrap();
        m.pinch((0, 100, 100, 300, 100), (1, 200, 100, 0, 100), 5)
            .unwrap();
        m.up(0).unwrap();
        m.up(1).unwrap();
    }
    s.close().unwrap();
    let frames = parse_touch_frames(&s.into_inner().into_bytes());
    // 2 down + 5 pinch * 2 pointers + 2 up = 14 events
    assert_eq!(frames.len(), 14);
    assert_eq!(frames.iter().filter(|f| f.pointer_id == 0).count(), 7);
    assert_eq!(frames.iter().filter(|f| f.pointer_id == 1).count(), 7);
}

#[test]
fn out_of_range_pointer_rejected() {
    let mut s = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    s.set_screen_size(1080, 2400);
    {
        let mut m = s.multitouch();
        let r = m.down(MAX_POINTERS, 0, 0, 1.0);
        assert!(matches!(r, Err(Error::PointerIdOutOfRange(_, _))));
    }
    s.close().unwrap();
}

#[test]
fn up_without_down_rejected() {
    let mut s = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    s.set_screen_size(1080, 2400);
    {
        let mut m = s.multitouch();
        let r = m.up(5);
        assert!(matches!(r, Err(Error::PointerNotActive(5))));
    }
    s.close().unwrap();
}

#[test]
fn down_twice_rejected() {
    let mut s = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    s.set_screen_size(1080, 2400);
    {
        let mut m = s.multitouch();
        m.down(0, 0, 0, 1.0).unwrap();
        let r = m.down(0, 0, 0, 1.0);
        assert!(matches!(r, Err(Error::PointerAlreadyDown(0))));
        m.up(0).unwrap();
    }
    s.close().unwrap();
}

#[test]
fn multitouch_borrows_session() {
    // Compile-time: cannot hold a multitouch handle and also use the
    // keyboard driver simultaneously. This test is intentionally
    // trivial — it exists to lock the borrow contract.
    let mut s = HidSession::open(MockTransport::new(), OpenRequest::kbd_only()).unwrap();
    {
        let _m = s.multitouch();
        // Uncommenting the next line would fail at compile time:
        // let _k = s.keyboard();
    }
    s.close().unwrap();
}

#[test]
fn release_all_terminates_active_pointers() {
    let mut s = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    s.set_screen_size(1080, 2400);
    {
        let mut m = s.multitouch();
        for id in 0..5 {
            m.down(id, 0, 0, 1.0).unwrap();
        }
        assert_eq!(m.active_count(), 5);
        m.release_all().unwrap();
        assert_eq!(m.active_count(), 0);
    }
    s.close().unwrap();
}
