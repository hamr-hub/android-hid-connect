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
use crate::types::HID_MAX_SIZE;

/// Result of building an input report: fixed-size bytes to feed into a
/// UHID_INPUT control message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HidReport {
    pub hid_id: u16,
    pub size: u16,
    pub data: [u8; HID_MAX_SIZE],
}

impl HidReport {
    pub fn new(hid_id: u16, data: &[u8]) -> Self {
        let mut buf = [0u8; HID_MAX_SIZE];
        let n = data.len().min(HID_MAX_SIZE);
        buf[..n].copy_from_slice(&data[..n]);
        Self {
            hid_id,
            size: n as u16,
            data: buf,
        }
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
