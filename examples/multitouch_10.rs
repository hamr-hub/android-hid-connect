//! Demo: drive 10 simultaneous touch pointers via repeated
//! `HidSession::inject_touch` calls. Each pointer gets a
//! `pointer_id` in `0..=9`; Android's `InputDispatcher` accumulates
//! them into a single `MotionEvent` per logical gesture.
//!
//! Run with: `cargo run --example multitouch_10`
//!
//! Prerequisites: `adb forward tcp:27183 localabstract:scrcpy`

use std::time::Duration;

use android_hid_connect::session::{HidSession, OpenRequest};
use android_hid_connect::transport::open_tcp;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let sock = open_tcp("127.0.0.1", 27183)?;
    let mut s = HidSession::open(sock, OpenRequest::none())?;
    s.set_screen_size(1080, 2400);

    // Press 10 fingers in a horizontal row.
    for id in 0..10u64 {
        s.inject_touch(0, id, 100 + (id as i32) * 90, 1200, 1.0)?;
    }
    std::thread::sleep(Duration::from_millis(50));

    // Drag them all down by 200 px.
    for id in 0..10u64 {
        s.inject_touch(2, id, 100 + (id as i32) * 90, 1400, 1.0)?;
    }
    std::thread::sleep(Duration::from_millis(50));

    // Release.
    for id in 0..10u64 {
        s.inject_touch(1, id, 0, 0, 0.0)?;
    }
    s.flush_now()?;

    s.close()?;
    Ok(())
}
