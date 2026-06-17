//! Type "Hello, world!" into the focused app on a connected Android
//! device using scrcpy's UHID control protocol.
//!
//! Run with:
//!
//! ```text
//! adb forward tcp:27183 localabstract:scrcpy
//! cargo run --example type_keys
//! ```
//!
//! The example assumes scrcpy-server is already running on the device
//! (push it with `adb push scrcpy-server /data/local/tmp/scrcpy-server`
//! and start it with the matching flags).

use std::io::Write;
use std::thread;
use std::time::Duration;

use android_hid_connect::transport::{open_tcp, send_batch};
use android_hid_connect::{HidDevice, KeyboardHid, Modifiers, Scancode};

/// Map an ASCII character to (scancode, requires_shift).
fn char_to_key(c: char) -> Option<(u8, bool)> {
    let (sc, shift) = match c {
        'a'..='z' => (Scancode::from_u8(0x04 + (c as u8 - b'a'))?, false),
        'A'..='Z' => (Scancode::from_u8(0x04 + (c as u8 - b'A'))?, true),
        '1'..='9' => (Scancode::from_u8(0x1E + (c as u8 - b'1'))?, false),
        '0' => (Scancode::D0, false),
        ' ' => (Scancode::Space, false),
        ',' => (Scancode::Comma, false),
        '.' => (Scancode::Period, false),
        '!' => (Scancode::D1, true),
        '?' => (Scancode::Slash, true),
        _ => return None,
    };
    Some((sc as u8, shift))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut stream = open_tcp("127.0.0.1", 27183)
        .expect("connect to scrcpy-server at 127.0.0.1:27183 (forgot `adb forward`?)");

    let mut kbd = KeyboardHid::new();
    send_batch(&mut stream, &[kbd.open_message(None).unwrap()])?;

    let text = "Hello, world!";
    for c in text.chars() {
        let Some((sc, shift)) = char_to_key(c) else {
            continue;
        };
        let mods = if shift {
            Modifiers::LSHIFT
        } else {
            Modifiers::empty()
        };
        // Press
        send_batch(&mut stream, &[kbd.key_event(sc, true, mods).unwrap()])?;
        thread::sleep(Duration::from_millis(20));
        // Release
        send_batch(
            &mut stream,
            &[kbd.key_event(sc, false, Modifiers::empty()).unwrap()],
        )?;
        thread::sleep(Duration::from_millis(20));
    }

    send_batch(&mut stream, &[kbd.close_message().unwrap()])?;
    stream.flush()?;
    Ok(())
}
