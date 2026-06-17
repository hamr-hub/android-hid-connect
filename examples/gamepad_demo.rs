//! Demo: open a virtual Xbox 360 gamepad, hold A, and tilt the right
//! stick for two seconds. Mirrors `scrcpy`'s built-in gamepad support.
//!
//! Run with:
//!
//! ```text
//! adb forward tcp:27183 localabstract:scrcpy
//! cargo run --example gamepad_demo
//! ```

use std::thread;
use std::time::Duration;

use android_hid_connect::transport::{open_tcp, send_batch};
use android_hid_connect::{GamepadAxis, GamepadButton, GamepadHid};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut stream = open_tcp("127.0.0.1", 27183)
        .expect("connect to scrcpy-server at 127.0.0.1:27183 (forgot `adb forward`?)");

    let mut gp = GamepadHid::new();
    let (_hid_id, open_msg) = gp.open(0xCAFE_BABE, Some("DemoPad")).unwrap();
    send_batch(&mut stream, std::slice::from_ref(&open_msg))?;

    // Hold A.
    send_batch(
        &mut stream,
        &[gp.button_event(0xCAFE_BABE, GamepadButton::South, true).unwrap()],
    )?;
    thread::sleep(Duration::from_millis(500));

    // Tilt right stick fully to the right (max positive X = 32767).
    for _ in 0..20 {
        send_batch(
            &mut stream,
            &[gp.axis_event(0xCAFE_BABE, GamepadAxis::RightX, 32767).unwrap()],
        )?;
        thread::sleep(Duration::from_millis(50));
    }

    // Release A and recentre the stick.
    send_batch(
        &mut stream,
        &[
            gp.button_event(0xCAFE_BABE, GamepadButton::South, false).unwrap(),
            gp.axis_event(0xCAFE_BABE, GamepadAxis::RightX, 0).unwrap(),
        ],
    )?;
    thread::sleep(Duration::from_millis(100));

    send_batch(&mut stream, &[gp.close(0xCAFE_BABE).unwrap()])?;
    Ok(())
}
