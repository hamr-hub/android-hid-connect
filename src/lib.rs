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

pub mod agent;
pub mod ai;
#[cfg(feature = "tokio")]
pub mod async_device;
pub mod client;
pub mod coalesce;
pub mod control;
pub mod device;
pub mod error;
pub mod hid;
pub mod multitouch;
pub mod session;
pub mod transport;
pub mod types;

pub use agent::{
    AgentAction, AgentControlCloseReport, AgentControlClosed, AgentControlCommandCloseReport,
    AgentControlSession, AgentObjectSelector, AgentPlanBoundedPrefix, AgentPlanBoundedPrefixStop,
    AgentPlanSummary, AgentPoint, AgentRect, AgentScrollFrame, AgentTargetSelector,
    AgentTouchFrame, DEFAULT_AGENT_COMMAND_BOUND,
};
#[cfg(feature = "tokio")]
pub use async_device::{
    read_device_event_async, read_device_message_async, read_scrcpy_control_prefix_async,
    spawn_async_device_event_receiver, spawn_async_device_message_receiver,
    spawn_async_latest_frame_summary_receiver, spawn_default_async_device_event_receiver,
    spawn_default_async_device_message_receiver, AsyncDeviceMessagePump,
    AsyncDeviceMessageReceiver, AsyncLatestFrameSummaryReceiver,
};
pub use client::{
    AndroidKeyFrame, AndroidKeyFrameBatcher, GamepadFrameBatcher, KeyboardChordFrame,
    KeyboardFrame, KeyboardFrameBatcher, MouseFrame, MouseFrameBatcher, PackedGamepadFrameBatcher,
    ScrollFrame, ScrollFrameBatcher, TouchFrame, TouchFrameBatcher, ANDROID_KEY_BATCH_FRAMES,
    GAMEPAD_BATCH_FRAMES, KEYBOARD_BATCH_FRAMES, KEYBOARD_CHORD_EDGES, KEYBOARD_CHORD_KEYS,
    MOUSE_BATCH_FRAMES, SCROLL_BATCH_FRAMES, TOUCH_BATCH_FRAMES,
};
pub use control::{
    AiConfig, AiQuery, AI_FLAG_FEATURES, AI_FLAG_KEYFRAMES, AI_FLAG_MOTION, AI_FLAG_OBJECTS,
    AI_FLAG_TEXT,
};
pub use device::{
    read_device_event, read_device_message, read_scrcpy_control_prefix,
    spawn_default_device_event_receiver, spawn_default_device_message_receiver,
    spawn_device_event_receiver, spawn_device_message_receiver,
    spawn_latest_frame_summary_receiver, DeviceEvent, DeviceMessage, DeviceMessagePump,
    DeviceMessageReceiver, DeviceReadError, LatestFrameSummaryBoundary,
    LatestFrameSummaryObservation, LatestFrameSummaryReceiver, LatestFrameSummarySnapshot,
    ScrcpyControlPrefix,
};
pub use error::{Error, Result, TransportWrite};
pub use hid::gamepad::GamepadHid;
pub use hid::keyboard::KeyboardHid;
pub use hid::mouse::MouseHid;
pub use hid::{HidDevice, HidReport};
pub use session::{GamepadFrameRaw, HidSession, OpenRequest};
pub use transport::{open_tcp, send_batch, send_one};
pub use types::{
    AndroidKeyAction, AndroidKeycode, ClipboardCopyKey, GamepadAxis, GamepadButton, Modifiers,
    MouseButton, Scancode, TouchAction, TouchPointerId, HID_ID_KEYBOARD, HID_ID_MOUSE,
    POINTER_ID_GENERIC_FINGER, POINTER_ID_MOUSE, POINTER_ID_VIRTUAL_FINGER,
};
