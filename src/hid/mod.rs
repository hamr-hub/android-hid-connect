//! USB HID device drivers for keyboard, mouse and gamepad.
//!
//! Each driver implements the [`HidDevice`] trait: it owns a `report_desc`,
//! the HID id assigned to the device by scrcpy, and a `build_report`
//! method that turns logical input events into the byte stream consumed by
//! the control socket.

pub mod descriptor;
pub mod gamepad;
pub mod keyboard;
pub mod mouse;

use crate::control::message::ControlMessage;
use crate::error::Result;

/// Result of building an input report: a tuple of `(hid_id, data)` to feed
/// into a UHID_INPUT control message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HidReport {
    pub hid_id: u16,
    pub data: Vec<u8>,
}

impl HidReport {
    pub fn new(hid_id: u16, data: Vec<u8>) -> Self {
        Self { hid_id, data }
    }
}

/// Trait implemented by all HID devices (keyboard, mouse, gamepad).
///
/// The lifecycle is:
///   1. [`HidDevice::open`] — returns a UHID_CREATE [`ControlMessage`]
///      describing the device (vendor/product/report descriptor).
///   2. The caller pushes that message through a transport.
///   3. Subsequent state changes (key press, mouse motion, etc.) call
///      `build_*` methods that produce UHID_INPUT [`ControlMessage`]s.
///   4. [`HidDevice::close`] returns a UHID_DESTROY message.
pub trait HidDevice {
    /// The HID id reserved for this kind of device.
    fn hid_id(&self) -> u16;

    /// Build a UHID_CREATE message announcing the device to the host
    /// (scrcpy-server).
    fn open_message(&self, name: Option<&str>) -> Result<ControlMessage>;

    /// Build a UHID_DESTROY message removing the device.
    fn close_message(&self) -> Result<ControlMessage>;
}
