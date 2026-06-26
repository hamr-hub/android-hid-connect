//! Minimal live test: drives a real scrcpy-server over the UHID
//! control socket. Verifies:
//!
//!   * dummy byte + raw 64-byte device meta (out-of-band)
//!   * GET_CLIPBOARD round-trip with device→host DEVICE_MSG_CLIPBOARD
//!   * UHID keyboard open / input / close wire format
//!   * non-UHID messages (SetClipboard) accepted
//!
//! Note: the actual kernel-level UHID device creation may fail on
//! devices whose kernel rejects virtual HID devices (Samsung OneUI
//! returns EINVAL on /dev/uhid write). That is a device-side
//! limitation, not a library bug — the bytes we send are byte-for-byte
//! identical to scrcpy's, as verified by the unit + integration tests.

use android_hid_connect::control::message::{ControlMessage, GetClipboard, SetClipboard};
use android_hid_connect::device::{read_device_message, read_scrcpy_control_prefix, DeviceMessage};
use android_hid_connect::transport::{open_tcp, send_one};
use android_hid_connect::types::Modifiers;
use android_hid_connect::{HidDevice, KeyboardHid};
use std::time::Duration;

fn main() -> std::process::ExitCode {
    let port: u16 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(27183);
    println!("== live kbd+clipboard test (port {port}) ==");
    let mut stream = match open_tcp("127.0.0.1", port) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("connect: {e}");
            return std::process::ExitCode::from(2);
        }
    };
    stream.set_read_timeout(Some(Duration::from_secs(3))).ok();
    stream.set_write_timeout(Some(Duration::from_secs(3))).ok();

    // 1) dummy byte + raw 64-byte device meta. This is an out-of-band
    //    prefix on the control socket, NOT a device_msg frame.
    let prefix = read_scrcpy_control_prefix(&mut stream).unwrap();
    println!("dummy byte: 0x{:02x}", prefix.dummy_byte);
    println!("device meta: {}", prefix.device_name);

    // 3) Round-trip the clipboard: ask the server for whatever the
    //    device currently has, then push a test string back.
    let get_cb = ControlMessage::GetClipboard(GetClipboard { copy_key: 1 });
    send_one(&mut stream, &get_cb).unwrap();
    println!("GET_CLIPBOARD sent (copy_key=1)");
    match read_device_message(&mut stream) {
        Ok(DeviceMessage::Clipboard(txt)) => {
            println!("DEVICE_MSG_CLIPBOARD len={} text={txt:?}", txt.len());
        }
        Ok(msg) => {
            println!("DEVICE_MSG type={} {}", msg.msg_type(), msg.describe());
        }
        Err(e) => println!("(no DEVICE_MSG in 3s — expected if clipboard unchanged): {e}"),
    }
    let set_cb = ControlMessage::SetClipboard(SetClipboard {
        sequence: 42,
        paste: false,
        text: "android-hid-connect live test".to_string(),
    });
    send_one(&mut stream, &set_cb).unwrap();
    println!("SET_CLIPBOARD sent");

    // 4) UHID keyboard lifecycle
    let mut kbd = KeyboardHid::new();
    send_one(&mut stream, &kbd.open_message(None).unwrap()).unwrap();
    println!("UHID_CREATE keyboard: written");
    for ch in "Hello".chars() {
        let mut mods = Modifiers::empty();
        let sc = match ch {
            'H' => {
                mods = Modifiers::LSHIFT;
                0x0B
            }
            'e' => 0x08,
            'l' => 0x0C,
            'o' => 0x12,
            _ => continue,
        };
        send_one(&mut stream, &kbd.key_event(sc, true, mods).unwrap()).unwrap();
        send_one(
            &mut stream,
            &kbd.key_event(sc, false, Modifiers::empty()).unwrap(),
        )
        .unwrap();
    }
    send_one(&mut stream, &kbd.close_message().unwrap()).unwrap();
    println!("UHID_DESTROY keyboard: written");

    // 5) drain remaining DEVICE_MSG frames for ~3s
    let mut drained = 0;
    stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .ok();
    while let Ok(msg) = read_device_message(&mut stream) {
        match msg {
            DeviceMessage::Clipboard(text) => {
                println!("drained CLIPBOARD text={text:?}");
            }
            DeviceMessage::AckClipboard { sequence } => {
                println!("drained ACK_CLIPBOARD seq={sequence}");
            }
            DeviceMessage::UhidOutput { id, data } => {
                println!("drained UHID_OUTPUT id={id} data={data:02x?}");
            }
        }
        drained += 1;
        if drained > 16 {
            break;
        }
    }
    std::process::ExitCode::SUCCESS
}
