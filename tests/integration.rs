//! End-to-end integration test: drive a real local TCP socket pair and
//! verify the bytes that come out match the scrcpy protocol layout the
//! server-side `ControlMessageReader` would parse.

use std::io::Read;
use std::net::Shutdown;
use std::thread;

use android_hid_connect::control::message::{ControlMessage, UhidCreate, UhidDestroy, UhidInput};
use android_hid_connect::transport::{send_batch, send_one, MockTransport};
use android_hid_connect::types::{GamepadAxis, GamepadButton, Modifiers, MouseButton};
use android_hid_connect::{GamepadHid, HidDevice, KeyboardHid, MouseHid};

/// Read `n` bytes from a stream or fail the test.
fn read_exact(stream: &mut std::net::TcpStream, n: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(n);
    while out.len() < n {
        let mut buf = [0u8; 256];
        let k = stream.read(&mut buf).expect("read");
        if k == 0 {
            break;
        }
        out.extend_from_slice(&buf[..k]);
    }
    out
}

#[test]
fn keyboard_open_input_destroy_wire_format() {
    // Bind a local TCP listener, accept on a thread, and read the bytes
    // the library writes.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        // Open (small) + Input + Destroy
        let bytes = read_exact(&mut sock, 1024);
        sock.shutdown(Shutdown::Both).ok();
        bytes
    });

    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    let mut kbd = KeyboardHid::new();

    let open = kbd.open_message(None).unwrap();
    let down = kbd.key_event(0x04, true, Modifiers::LSHIFT).unwrap();
    let up = kbd.key_event(0x04, false, Modifiers::empty()).unwrap();
    let close = kbd.close_message().unwrap();

    send_batch(&mut stream, &[open, down, up, close]).unwrap();
    drop(stream);

    let bytes = handle.join().unwrap();

    // First byte of the open message must be 12 (UHID_CREATE).
    let open_len = {
        let mut idx = 0;
        assert_eq!(bytes[idx], 12, "expected UHID_CREATE type");
        idx += 1;
        let id = u16::from_be_bytes([bytes[idx], bytes[idx + 1]]);
        assert_eq!(id, 1);
        idx += 2;
        idx += 2 + 2; // vendor + product
        let name_len = bytes[idx] as usize;
        idx += 1 + name_len;
        let rd_len = u16::from_be_bytes([bytes[idx], bytes[idx + 1]]) as usize;
        idx += 2 + rd_len;
        idx
    };

    // UHID_INPUT (13) + id(2) + size(2) + 8 bytes data = 13 bytes
    let input_start = open_len;
    assert_eq!(bytes[input_start], 13);
    let id = u16::from_be_bytes([bytes[input_start + 1], bytes[input_start + 2]]);
    assert_eq!(id, 1);
    let sz = u16::from_be_bytes([bytes[input_start + 3], bytes[input_start + 4]]);
    assert_eq!(sz, 8);
    assert_eq!(bytes[input_start + 5], 0x02); // LSHIFT
    assert_eq!(bytes[input_start + 7], 0x04); // A

    // Second UHID_INPUT (key release)
    let input2_start = input_start + 5 + 8;
    assert_eq!(bytes[input2_start], 13);
    let sz2 = u16::from_be_bytes([bytes[input2_start + 3], bytes[input2_start + 4]]);
    assert_eq!(sz2, 8);
    // Released: no LSHIFT, no key in slot 2.
    assert_eq!(bytes[input2_start + 5], 0x00);
    assert_eq!(bytes[input2_start + 7], 0x00);

    // UHID_DESTROY (14) + id(2) — comes after the second input.
    let destroy_start = input2_start + 5 + 8;
    assert_eq!(bytes[destroy_start], 14);
    assert_eq!(
        u16::from_be_bytes([bytes[destroy_start + 1], bytes[destroy_start + 2]]),
        1
    );
    let destroy_end = destroy_start + 3;
    assert_eq!(destroy_end, bytes.len());
}

#[test]
fn mouse_5_byte_reports() {
    let mut t = MockTransport::new();
    let m = MouseHid::new();
    let open = m.open_message(None).unwrap();
    let click = m.click_message(MouseButton::state(&[MouseButton::Left, MouseButton::X1]));
    let motion = m.motion_message(10, -5, MouseButton::state(&[MouseButton::Left]));
    let close = m.close_message().unwrap();
    send_batch(&mut t, &[open, click, motion, close]).unwrap();

    let bytes = t.into_bytes();
    // The last 3 bytes must be UHID_DESTROY (14) + id=2.
    assert_eq!(&bytes[bytes.len() - 3..], &[14, 0x00, 0x02]);
    // The very first byte must be UHID_CREATE (12).
    assert_eq!(bytes[0], 12);
    // The report-desc size embedded in the open message must equal the
    // length of the descriptor used in the mouse driver.
    let open_len = {
        // type(1) + id(2) + vid(2) + pid(2) = 7; name_len is at byte 7.
        let mut idx = 7;
        let name_len = bytes[idx] as usize;
        idx += 1 + name_len;
        let rd_len = u16::from_be_bytes([bytes[idx], bytes[idx + 1]]) as usize;
        idx += 2 + rd_len;
        idx
    };
    // 2 UHID_INPUT messages of 5 bytes each = 22 bytes after the open.
    let expected_after_open = 5 + 5 + 5 + 5 + 3; // click(5+5) + motion(5+5) + destroy(3)
    assert_eq!(bytes.len(), open_len + expected_after_open);
}

#[test]
fn gamepad_15_byte_reports() {
    let mut t = MockTransport::new();
    let mut g = GamepadHid::new();
    let (hid_id, open) = g.open(7, Some("Xbox 360")).unwrap();
    let btn_a = g.button_event(7, GamepadButton::South, true).unwrap();
    let stick = g.axis_event(7, GamepadAxis::LeftX, 16384).unwrap();
    let close = g.close(7).unwrap();
    send_batch(&mut t, &[open, btn_a, stick, close]).unwrap();

    let bytes = t.into_bytes();
    // Spot check: open must be UHID_CREATE (12)
    assert_eq!(bytes[0], 12);
    // The 2nd message must be UHID_INPUT (13) of size 15
    let open_len = {
        // type(1) + id(2) + vid(2) + pid(2) = 7; name_len at byte 7.
        let mut idx = 7;
        let name_len = bytes[idx] as usize;
        idx += 1 + name_len;
        let rd_len = u16::from_be_bytes([bytes[idx], bytes[idx + 1]]) as usize;
        idx += 2 + rd_len;
        idx
    };
    // The first UHID_INPUT (btn_a) occupies 20 bytes after the open.
    // The second UHID_INPUT (stick) starts at open_len + 20.
    let input2_start = open_len + 20;
    assert_eq!(bytes[input2_start], 13);
    assert_eq!(
        u16::from_be_bytes([bytes[input2_start + 1], bytes[input2_start + 2]]),
        hid_id
    );
    assert_eq!(
        u16::from_be_bytes([bytes[input2_start + 3], bytes[input2_start + 4]]),
        15
    );
    // Byte 12 of the input (offset input2_start + 5 + 12) must be 0x01
    // (bit 0 = South / A on — this is the cumulative button state).
    assert_eq!(bytes[input2_start + 5 + 12], 0x01);
    // Stick X = 16384 + 32768 = 49152 → 0xC000 LE → [0x00, 0xC0]
    assert_eq!(bytes[input2_start + 5], 0x00);
    assert_eq!(bytes[input2_start + 6], 0xC0);
}

#[test]
fn dpad_hat_in_byte_14() {
    let mut t = MockTransport::new();
    let mut g = GamepadHid::new();
    g.open(1, None).unwrap();
    g.button_event(1, GamepadButton::DpadUp, true).unwrap();
    g.button_event(1, GamepadButton::DpadRight, true).unwrap();
    let msg = g.button_event(1, GamepadButton::South, true).unwrap();
    send_one(&mut t, &msg).unwrap();
    let bytes = t.into_bytes();
    // type(1) + id(2) + size(2) + data(15) = 20
    assert_eq!(bytes.len(), 20);
    // Byte 14 of the report (offset 5 + 14 = 19) must be 2 (up+right).
    assert_eq!(bytes[19], 2);
}

#[test]
fn name_too_long_rejected() {
    let s = "a".repeat(128);
    let msg = ControlMessage::UhidCreate(UhidCreate {
        id: 1,
        vendor_id: 0,
        product_id: 0,
        name: Some(s),
        report_desc: vec![],
    });
    let r = msg.serialize();
    assert!(r.is_err());
}

#[test]
fn droppable_vs_critical() {
    // UHID_INPUT and friends are droppable; UHID_CREATE/DESTROY are not.
    let input = ControlMessage::UhidInput(UhidInput {
        id: 1,
        size: 0,
        data: [0; 15],
    });
    let create = ControlMessage::UhidCreate(UhidCreate {
        id: 1,
        vendor_id: 0,
        product_id: 0,
        name: None,
        report_desc: vec![],
    });
    let destroy = ControlMessage::UhidDestroy(UhidDestroy { id: 1 });
    assert!(!input.is_critical());
    assert!(create.is_critical());
    assert!(destroy.is_critical());
}

#[test]
fn phantom_state_on_overflow() {
    let mut t = MockTransport::new();
    let mut k = KeyboardHid::new();
    let _ = k.open_message(None).unwrap();
    // Press 7 keys.
    for sc in 0x04u8..0x0B {
        let msg = k.key_event(sc, true, Modifiers::empty()).unwrap();
        send_one(&mut t, &msg).unwrap();
    }
    // Find the last UHID_INPUT (after the 7th key was pressed) and
    // assert that bytes 2..=7 of the data are all 0x01 (ErrorRollOver).
    let bytes = t.into_bytes();
    // The last type=13 byte starts the 7th input message.
    let input_off = bytes
        .windows(8)
        .rposition(|w| w[0] == 13 && w[1] == 0x00 && w[2] == 0x01)
        .expect("must contain at least one UHID_INPUT for id=1");
    // type(1) + id(2) + size(2) + data(8) = 13
    let size = u16::from_be_bytes([bytes[input_off + 3], bytes[input_off + 4]]);
    assert_eq!(size, 8);
    let data_start = input_off + 5;
    for i in 0..6 {
        assert_eq!(
            bytes[data_start + 2 + i],
            0x01,
            "slot {} should be ErrorRollOver, got {:#x}",
            i,
            bytes[data_start + 2 + i]
        );
    }
}
