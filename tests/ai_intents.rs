//! Integration tests for AI/agent intent methods on `HidSession`.
//! Each intent is verified by inspecting the wire bytes on a
//! `MockTransport` to confirm the correct underlying `ControlMessage`
//! (or sequence thereof) was emitted.

use android_hid_connect::session::{HidSession, OpenRequest};
use android_hid_connect::transport::MockTransport;
use android_hid_connect::{
    AndroidKeyAction, AndroidKeycode, ClipboardCopyKey, AI_FLAG_FEATURES, AI_FLAG_KEYFRAMES,
    AI_FLAG_MOTION, AI_FLAG_OBJECTS, AI_FLAG_TEXT,
};

const TAG_TOUCH: u8 = 2;
const TAG_UHID_DESTROY: u8 = 14;
const TOUCH_PAYLOAD_SIZE: usize = 31;

fn find_msg_with_tag(bytes: &[u8], tag: u8) -> Option<&[u8]> {
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == tag {
            let len = match tag {
                12 => {
                    // UHID_CREATE: tag + id(2) + vid(2) + pid(2) + name_len(1) + name + rd_size(2) + rd
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
                    8 + name_len + 2 + rd_size
                }
                13 => {
                    // UHID_INPUT: tag + id(2) + size(2) + data
                    if i + 5 > bytes.len() {
                        break;
                    }
                    let size = u16::from_be_bytes([bytes[i + 3], bytes[i + 4]]) as usize;
                    5 + size
                }
                14 => 3,         // UHID_DESTROY: tag + id(2)
                TAG_TOUCH => 32, // INJECT_TOUCH_EVENT
                9 => {
                    // SET_CLIPBOARD: tag + sequence(8) + paste(1) + text_len(4) + text
                    if i + 14 > bytes.len() {
                        break;
                    }
                    let text_len = u32::from_be_bytes([
                        bytes[i + 10],
                        bytes[i + 11],
                        bytes[i + 12],
                        bytes[i + 13],
                    ]) as usize;
                    14 + text_len
                }
                16 => {
                    // START_APP: tag + name_len(1) + name
                    if i + 2 > bytes.len() {
                        break;
                    }
                    let name_len = bytes[i + 1] as usize;
                    2 + name_len
                }
                // Tag-only (1-byte) messages
                5 | 6 | 7 | 11 | 15 | 17 | 18 | 19 | 20 => 1,
                3 => 21, // INJECT_SCROLL_EVENT
                4 => 2,  // BACK_OR_SCREEN_ON: tag + action
                8 => 2,  // GET_CLIPBOARD: tag + copy_key
                10 => 2, // SET_DISPLAY_POWER: tag + on(1)
                21 => 5, // RESIZE_DISPLAY: tag + width(2) + height(2)
                22 => 6, // AI_CONFIG: tag + flags(1) + sample_interval(2) + feature_dim(2)
                23 => 9, // AI_QUERY: tag + since_timestamp_ms(8)
                24 => 1, // AI_PAUSE: tag only
                _ => break,
            };
            if i + len <= bytes.len() {
                return Some(&bytes[i..i + len]);
            }
            break;
        }
        i += 1;
    }
    None
}

fn count_touch_events(bytes: &[u8]) -> usize {
    let mut count = 0;
    let mut i = 0;
    while i + TOUCH_PAYLOAD_SIZE <= bytes.len() {
        if bytes[i] == TAG_TOUCH
            && (bytes[i + 1] <= 3)
            && bytes[i + 2..i + 9].iter().all(|b| *b == 0)
            && bytes[i + 9] < 10
        {
            count += 1;
            i += 1 + TOUCH_PAYLOAD_SIZE;
        } else {
            i += 1;
        }
    }
    count
}

fn setup() -> HidSession<MockTransport> {
    HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap()
}

fn run_one<F: FnOnce(&mut HidSession<MockTransport>)>(f: F) -> Vec<u8> {
    let mut s = setup();
    f(&mut s);
    s.close().unwrap();
    s.into_inner().into_bytes()
}

fn inject_key_events(bytes: &[u8]) -> Vec<(u8, u32, u32, u32)> {
    let mut found = Vec::new();
    let mut i = 0;
    while i + 14 <= bytes.len() {
        if bytes[i] == 0 {
            found.push((
                bytes[i + 1],
                u32::from_be_bytes([bytes[i + 2], bytes[i + 3], bytes[i + 4], bytes[i + 5]]),
                u32::from_be_bytes([bytes[i + 6], bytes[i + 7], bytes[i + 8], bytes[i + 9]]),
                u32::from_be_bytes([bytes[i + 10], bytes[i + 11], bytes[i + 12], bytes[i + 13]]),
            ));
            i += 14;
        } else {
            i += 1;
        }
    }
    found
}

// === single-message intents ===

#[test]
fn set_screen_power_emits_set_display_power() {
    let bytes = run_one(|s| {
        s.set_screen_power(false).unwrap();
    });
    // Tag 10 (SetDisplayPower), payload = 1 byte (on as u8)
    assert_eq!(bytes[0], 10);
    assert_eq!(bytes[1], 0); // off
}

#[test]
fn press_keys_emit_inject_keycode() {
    let bytes = run_one(|s| {
        s.press_home().unwrap();
        s.press_back().unwrap();
        s.open_recents().unwrap();
    });
    // 3 INJECT_KEYCODE messages (tag 0). Count them.
    let count = bytes.iter().filter(|b| **b == 0).count();
    // CREATE? No, OpenRequest::none() → no CREATE. DESTROY at end?
    // Yes, tag 14. So tag 0 should be exactly 3.
    // Actually tag 14 is 3 bytes. tag 0 (InjectKeycode) is 1+1+4+4+4 = 14 bytes.
    // We sent 3 inject_keycode calls = 3 × 14 bytes = 42 bytes tagged 0.
    assert!(count >= 3, "expected ≥ 3 tag-0 frames, got {count}");
    // Verify keycodes (3, 4, 187) appear in big-endian u32 at offset 2-5 of each
    // tag-0 frame.
    let keycodes = [3u32, 4, 187];
    let mut found = 0;
    let mut i = 0;
    while i + 14 <= bytes.len() {
        if bytes[i] == 0 {
            let kc = u32::from_be_bytes([bytes[i + 2], bytes[i + 3], bytes[i + 4], bytes[i + 5]]);
            if keycodes.contains(&kc) {
                found += 1;
            }
            i += 14;
        } else {
            i += 1;
        }
    }
    assert_eq!(found, 3);
}

#[test]
fn volume_keys_use_correct_keycodes() {
    let bytes = run_one(|s| {
        s.volume_up().unwrap();
        s.volume_down().unwrap();
        s.volume_mute().unwrap();
    });
    let keycodes = [24u32, 25, 164];
    let mut found = 0;
    let mut i = 0;
    while i + 14 <= bytes.len() {
        if bytes[i] == 0 {
            let kc = u32::from_be_bytes([bytes[i + 2], bytes[i + 3], bytes[i + 4], bytes[i + 5]]);
            if keycodes.contains(&kc) {
                found += 1;
            }
            i += 14;
        } else {
            i += 1;
        }
    }
    assert_eq!(found, 3);
}

#[test]
fn typed_android_keycodes_emit_inject_keycode() {
    let bytes = run_one(|s| {
        s.press_android_key(AndroidKeycode::POWER).unwrap();
        s.inject_android_key_event(AndroidKeyAction::UP, AndroidKeycode::ENTER, 2, 3)
            .unwrap();
        s.release_android_key(AndroidKeycode::MENU).unwrap();
    });
    let expected = [(0u8, 26u32, 0u32, 0u32), (1, 66, 2, 3), (1, 82, 0, 0)];
    assert_eq!(inject_key_events(&bytes), expected);
}

#[test]
fn tap_android_key_emits_down_then_up() {
    let bytes = run_one(|s| {
        s.tap_android_key_with_metastate(AndroidKeycode::ENTER, 3)
            .unwrap();
        s.tap_android_keycode(82, 0).unwrap();
    });

    assert_eq!(
        inject_key_events(&bytes),
        vec![(0, 66, 0, 3), (1, 66, 0, 3), (0, 82, 0, 0), (1, 82, 0, 0)]
    );
}

#[test]
fn back_or_screen_on_emits_action_payload() {
    let bytes = run_one(|s| {
        s.back_or_screen_on(AndroidKeyAction::UP).unwrap();
    });
    assert_eq!(find_msg_with_tag(&bytes, 4), Some(&[4, 1][..]));
}

#[test]
fn panel_intents_emit_tag_only() {
    let bytes = run_one(|s| {
        s.show_notifications().unwrap();
        s.show_quick_settings().unwrap();
        s.collapse_panels().unwrap();
    });
    // Tag 5, 6, 7 — all 1-byte messages.
    assert!(bytes.contains(&5));
    assert!(bytes.contains(&6));
    assert!(bytes.contains(&7));
}

#[test]
fn misc_intents() {
    let bytes = run_one(|s| {
        s.rotate_device().unwrap();
        s.resize_display(720, 1280).unwrap();
        s.set_torch(true).unwrap();
        s.camera_zoom_in().unwrap();
        s.camera_zoom_out().unwrap();
        s.open_hard_keyboard_settings().unwrap();
        s.reset_video().unwrap();
        s.launch_app("com.android.settings").unwrap();
    });
    // rotate_device: tag 11 (1 byte)
    // resize_display: tag 21, payload = 2+2 = 4 bytes (w, h)
    // set_torch: tag 18, payload = 1 byte
    // camera_zoom_in: tag 19 (1 byte)
    // camera_zoom_out: tag 20 (1 byte)
    // open_hard_kb_settings: tag 15 (1 byte)
    // reset_video: tag 17 (1 byte)
    // launch_app: tag 16, payload = 1B len + N bytes
    assert!(bytes.contains(&11));
    assert!(bytes.contains(&18));
    assert!(bytes.contains(&19));
    assert!(bytes.contains(&20));
    assert!(bytes.contains(&15));
    assert!(bytes.contains(&17));
    // resize_display: tag 21 + 4 bytes payload
    let resize = find_msg_with_tag(&bytes, 21).expect("resize_display frame");
    assert_eq!(resize.len(), 5);
    assert_eq!(u16::from_be_bytes([resize[1], resize[2]]), 720);
    assert_eq!(u16::from_be_bytes([resize[3], resize[4]]), 1280);
    // launch_app: tag 16, name_len=20 + "com.android.settings"
    let launch = find_msg_with_tag(&bytes, 16).expect("launch_app frame");
    assert_eq!(launch[0], 16);
    assert_eq!(launch[1], 20);
    assert_eq!(&launch[2..], b"com.android.settings");
}

#[test]
fn ai_extension_helpers_emit_config_query_pause() {
    let flags =
        AI_FLAG_KEYFRAMES | AI_FLAG_FEATURES | AI_FLAG_MOTION | AI_FLAG_OBJECTS | AI_FLAG_TEXT;
    let bytes = run_one(|s| {
        s.configure_ai(flags, 16, 64).unwrap();
        s.query_ai(0x0102_0304_0506_0708).unwrap();
        s.pause_ai().unwrap();
    });

    let config = find_msg_with_tag(&bytes, 22).expect("AI_CONFIG frame");
    assert_eq!(config, &[22, flags, 0, 16, 0, 64]);
    let query = find_msg_with_tag(&bytes, 23).expect("AI_QUERY frame");
    assert_eq!(query[0], 23);
    assert_eq!(
        u64::from_be_bytes(query[1..9].try_into().unwrap()),
        0x0102_0304_0506_0708
    );
    assert_eq!(find_msg_with_tag(&bytes, 24), Some(&[24][..]));
}

#[test]
fn set_clipboard_emits_set_clipboard() {
    let bytes = run_one(|s| {
        s.set_clipboard("hello world", true).unwrap();
    });
    // tag 9 (SetClipboard): 1 + 8 (sequence) + 1 (paste) + 4 (text len) + N
    let frame = find_msg_with_tag(&bytes, 9).expect("set_clipboard frame");
    assert_eq!(frame[0], 9);
    // sequence is 0 → first 8 bytes after tag are 0
    assert_eq!(&frame[1..9], &[0; 8]);
    // paste = true → byte 9 (offset 9 from frame start) is 1
    assert_eq!(frame[9], 1);
    // text_len = 11 → bytes 10-13 are [0, 0, 0, 11]
    assert_eq!(&frame[10..14], &[0, 0, 0, 11]);
    // text = "hello world"
    assert_eq!(&frame[14..], b"hello world");
}

#[test]
fn get_clipboard_emits_get_clipboard() {
    let bytes = run_one(|s| {
        s.get_clipboard().unwrap();
    });
    assert_eq!(bytes[0], 8); // tag 8 (GetClipboard)
    assert_eq!(bytes[1], 0); // copy_key = 0
}

#[test]
fn request_clipboard_uses_requested_copy_key() {
    let bytes = run_one(|s| {
        s.request_clipboard(2).unwrap();
    });
    assert_eq!(bytes[0], 8); // tag 8 (GetClipboard)
    assert_eq!(bytes[1], 2); // copy_key = cut
}

#[test]
fn typed_clipboard_copy_key_emits_get_clipboard() {
    let bytes = run_one(|s| {
        s.request_clipboard_key(ClipboardCopyKey::COPY).unwrap();
    });
    assert_eq!(bytes[0], 8); // tag 8 (GetClipboard)
    assert_eq!(bytes[1], 1); // copy_key = copy
}

#[test]
fn scroll_emits_inject_scroll_event() {
    let bytes = run_one(|s| {
        s.set_screen_size(720, 1280);
        s.scroll(100, 200, 0.0, -16.0).unwrap();
    });
    let frame = find_msg_with_tag(&bytes, 3).expect("scroll frame");
    assert_eq!(frame.len(), 21);
    assert_eq!(i32::from_be_bytes(frame[1..5].try_into().unwrap()), 100);
    assert_eq!(i32::from_be_bytes(frame[5..9].try_into().unwrap()), 200);
    assert_eq!(u16::from_be_bytes(frame[9..11].try_into().unwrap()), 720);
    assert_eq!(u16::from_be_bytes(frame[11..13].try_into().unwrap()), 1280);
    assert_eq!(u16::from_be_bytes(frame[13..15].try_into().unwrap()), 0);
    assert_eq!(
        u16::from_be_bytes(frame[15..17].try_into().unwrap()),
        0x8000
    );
    assert_eq!(u32::from_be_bytes(frame[17..21].try_into().unwrap()), 0);
}

// === composite intents ===

#[test]
fn double_tap_emits_four_touch_events() {
    let mut s = setup();
    s.set_screen_size(1080, 2400);
    s.double_tap(540, 1200).unwrap();
    s.close().unwrap();
    let bytes = s.into_inner().into_bytes();
    assert_eq!(count_touch_events(&bytes), 4, "expected 4 touch events");
}

#[test]
fn long_press_blocks_for_dur() {
    let mut s = setup();
    s.set_screen_size(1080, 2400);
    let start = std::time::Instant::now();
    s.long_press(100, 200, std::time::Duration::from_millis(50))
        .unwrap();
    let elapsed = start.elapsed();
    assert!(
        elapsed >= std::time::Duration::from_millis(50),
        "long_press should block for dur, elapsed {elapsed:?}"
    );
    s.close().unwrap();
    let bytes = s.into_inner().into_bytes();
    assert_eq!(
        count_touch_events(&bytes),
        2,
        "expected 2 touch events (down + up)"
    );
}

#[test]
fn three_finger_screenshot_emits_36_touches() {
    let mut s = setup();
    s.set_screen_size(1080, 2400);
    s.three_finger_screenshot().unwrap();
    s.close().unwrap();
    let bytes = s.into_inner().into_bytes();
    // 3 down + 3*10 move + 3 up = 36 touch events
    assert_eq!(
        count_touch_events(&bytes),
        36,
        "expected 36 touch events (3 down + 30 move + 3 up)"
    );
    // 3 distinct pointer_ids
    let mut ids = std::collections::HashSet::new();
    let mut i = 0;
    while i + TOUCH_PAYLOAD_SIZE <= bytes.len() {
        if bytes[i] == TAG_TOUCH
            && (bytes[i + 1] <= 3)
            && bytes[i + 2..i + 9].iter().all(|b| *b == 0)
            && bytes[i + 9] < 10
        {
            ids.insert(bytes[i + 9]);
            i += TOUCH_PAYLOAD_SIZE + 1;
        } else {
            i += 1;
        }
    }
    assert_eq!(ids.len(), 3, "expected 3 distinct pointer_ids");
    assert!(ids.contains(&0) && ids.contains(&1) && ids.contains(&2));
}

// === no regression on existing intent calls ===

#[test]
fn close_after_intents_emits_destroys() {
    let mut s = HidSession::open(MockTransport::new(), OpenRequest::all()).unwrap();
    s.press_home().unwrap();
    s.set_screen_power(false).unwrap();
    s.close().unwrap();
    let bytes = s.into_inner().into_bytes();
    // 3 UHID_DESTROY (kbd + mouse + gamepad) at end.
    let mut destroys = 0;
    for w in bytes.windows(3) {
        if w[0] == TAG_UHID_DESTROY {
            destroys += 1;
        }
    }
    assert_eq!(destroys, 3, "expected 3 DESTROY frames (kbd+mouse+gamepad)");
}
