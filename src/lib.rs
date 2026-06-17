//! `android-hid-connect` — Rust port of scrcpy's UHID control surface.
//!
//! scrcpy (and any compatible `scrcpy-server` running on an Android
//! device) accepts control messages over a TCP socket. The three most
//! interesting messages for synthesising real input are
//! `UHID_CREATE` / `UHID_INPUT` / `UHID_DESTROY`: they make the device
//! believe a USB keyboard, mouse or gamepad was just plugged in, and
//! then feed it HID reports that the Android input subsystem delivers to
//! the focused app as if they came from real hardware.
//!
//! This crate provides:
//!
//! * a pure-Rust implementation of the three HID device drivers
//!   ([`hid::KeyboardHid`], [`hid::MouseHid`], [`hid::GamepadHid`]) ported
//!   from `scrcpy/app/src/hid/` and `scrcpy/app/src/uhid/`. Each one
//!   exposes a `build_*` / `inject_*` API that returns a typed
//!   [`control::ControlMessage`];
//! * a [`control`] module that knows how to serialize any
//!   [`control::ControlMessage`] into the scrcpy wire format (the same
//!   bytes that scrcpy-server's `ControlMessageReader` consumes);
//! * a tiny [`transport`] module with helpers to open a TCP socket
//!   (typically `127.0.0.1:27183` after `adb forward tcp:27183
//!   localabstract:scrcpy`) and a `MockTransport` for unit tests.
//!
//! ```no_run
//! use android_hid_connect::{KeyboardHid, HidDevice, Modifiers};
//! use android_hid_connect::transport::{open_tcp, send_one};
//!
//! let mut sock = open_tcp("127.0.0.1", 27183).unwrap();
//! let mut kbd = KeyboardHid::new();
//! send_one(&mut sock, &kbd.open_message(None).unwrap()).unwrap();
//! send_one(
//!     &mut sock,
//!     &kbd.key_event(0x04, true, Modifiers::LSHIFT).unwrap(),
//! ).unwrap();
//! // release
//! send_one(
//!     &mut sock,
//!     &kbd.key_event(0x04, false, Modifiers::empty()).unwrap(),
//! ).unwrap();
//! send_one(&mut sock, &kbd.close_message().unwrap()).unwrap();
//! ```
//!
//! See `README.md` for the full protocol layout and a `adb forward`
//! recipe.

#![deny(missing_debug_implementations)]
#![warn(rust_2018_idioms)]

pub mod control;
pub mod error;
pub mod hid;
pub mod session;
pub mod transport;
pub mod types;

pub use error::{Error, Result, TransportWrite};
pub use hid::gamepad::GamepadHid;
pub use hid::keyboard::KeyboardHid;
pub use hid::mouse::MouseHid;
pub use hid::{HidDevice, HidReport};
pub use types::{
    GamepadAxis, GamepadButton, Modifiers, MouseButton, Scancode,
    HID_ID_KEYBOARD, HID_ID_MOUSE,
};
