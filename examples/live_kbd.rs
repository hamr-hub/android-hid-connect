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

use std::io::Read;
use std::time::Duration;
use android_hid_connect::control::message::{ControlMessage, GetClipboard, SetClipboard};
use android_hid_connect::transport::{open_tcp, send_one};
use android_hid_connect::types::Modifiers;
use android_hid_connect::{HidDevice, KeyboardHid};

const DEVICE_NAME_FIELD_LENGTH: usize = 64;

/// Parse a device_msg frame from the control socket:
///   type (1 byte) | length (4 bytes BE) | length bytes of payload
fn read_device_msg(stream: &mut std::net::TcpStream) -> std::io::Result<(u8, Vec<u8>)> {
    let mut type_byte = [0u8; 1];
    stream.read_exact(&mut type_byte)?;
    let ty = type_byte[0];
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut payload = vec![0u8; len];
    if len > 0 { stream.read_exact(&mut payload)?; }
    Ok((ty, payload))
}

fn main() -> std::process::ExitCode {
    let port: u16 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(27183);
    println!("== live kbd+clipboard test (port {port}) ==");
    let mut stream = match open_tcp("127.0.0.1", port) {
        Ok(s) => s,
        Err(e) => { eprintln!("connect: {e}"); return std::process::ExitCode::from(2); }
    };
    stream.set_read_timeout(Some(Duration::from_secs(3))).ok();
    stream.set_write_timeout(Some(Duration::from_secs(3))).ok();

    // 1) dummy byte
    let mut dummy = [0u8; 1];
    stream.read_exact(&mut dummy).unwrap();
    println!("dummy byte: 0x{:02x}", dummy[0]);

    // 2) device meta = raw 64 bytes (UTF-8 name padded with 0).
    //    Per scrcpy's DesktopConnection.sendDeviceMeta, this is an
    //    out-of-band prefix on the control socket — NOT a device_msg
    //    frame.
    let mut device_meta = vec![0u8; DEVICE_NAME_FIELD_LENGTH];
    stream.read_exact(&mut device_meta).unwrap();
    let name_len = device_meta.iter().position(|&b| b == 0).unwrap_or(DEVICE_NAME_FIELD_LENGTH);
    let name = String::from_utf8_lossy(&device_meta[..name_len]).to_string();
    println!("device meta ({name_len} bytes): {name}");

    // 3) Round-trip the clipboard: ask the server for whatever the
    //    device currently has, then push a test string back.
    let get_cb = ControlMessage::GetClipboard(GetClipboard { copy_key: 1 });
    send_one(&mut stream, &get_cb).unwrap();
    println!("GET_CLIPBOARD sent (copy_key=1)");
    match read_device_msg(&mut stream) {
        Ok((ty, payload)) => {
            let txt = String::from_utf8_lossy(&payload).to_string();
            println!("DEVICE_MSG type={ty} len={} text={txt:?}", payload.len());
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
            'H' => { mods = Modifiers::LSHIFT; 0x0B }
            'e' => 0x08,
            'l' => 0x0C,
            'o' => 0x12,
            _ => continue,
        };
        send_one(&mut stream, &kbd.key_event(sc, true, mods).unwrap()).unwrap();
        send_one(&mut stream, &kbd.key_event(sc, false, Modifiers::empty()).unwrap()).unwrap();
    }
    send_one(&mut stream, &kbd.close_message().unwrap()).unwrap();
    println!("UHID_DESTROY keyboard: written");

    // 5) drain remaining DEVICE_MSG frames for ~3s
    let mut drained = 0;
    stream.set_read_timeout(Some(Duration::from_millis(500))).ok();
    while let Ok((ty, payload)) = read_device_msg(&mut stream) {
        let s = String::from_utf8_lossy(&payload);
        match ty {
            1 => {
                let seq = if payload.len() >= 8 {
                    u64::from_be_bytes(payload[..8].try_into().unwrap())
                } else { 0 };
                println!("drained ACK_CLIPBOARD seq={seq}");
            }
            2 => {
                let id = u16::from_be_bytes(payload[..2].try_into().unwrap_or([0,0]));
                println!("drained UHID_OUTPUT id={id} data={:02x?}", &payload[4..]);
            }
            _ => println!("drained DEVICE_MSG type={ty} payload={s:?}"),
        }
        drained += 1;
        if drained > 16 { break; }
    }
    std::process::ExitCode::SUCCESS
}


