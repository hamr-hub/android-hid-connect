//! HID mouse driver — Rust port of `sc_mouse_uhid` + `sc_hid_mouse`.
//!
//! 5-byte reports:
//!
//! ```text
//!   byte 0: button bitmap
//!   byte 1: relative X motion (signed, clamped to -127..=127)
//!   byte 2: relative Y motion
//!   byte 3: vertical wheel (signed, clamped to -127..=127)
//!   byte 4: AC Pan / horizontal wheel
//! ```
//!
//! The mouse also tracks residual scroll so that fractional scroll
//! deltas (e.g. a slow trackpad) accumulate into a discrete byte only
//! once they cross an integer boundary — see
//! [`MouseHid::generate_input_from_scroll`].

use crate::control::message::{ControlMessage, UhidCreate, UhidDestroy, UhidInput};
use crate::error::Result;
use crate::hid::descriptor::MOUSE_REPORT_DESC;
use crate::hid::{HidDevice, HidReport};
use crate::types::{HID_ID_MOUSE, HID_MAX_SIZE};

/// Total size of a mouse HID input report.
const MOUSE_REPORT_SIZE: usize = 5;

/// Mouse button bit positions in byte 0 of the report. Values match
/// `sc_hid_buttons_from_buttons_state` in scrcpy.
const BTN_LEFT:   u8 = 1 << 0;
const BTN_RIGHT:  u8 = 1 << 1;
const BTN_MIDDLE: u8 = 1 << 2;
const BTN_X1:     u8 = 1 << 3;
const BTN_X2:     u8 = 1 << 4;

/// Pack a `sc_mouse_button`-style bitmap into byte 0 of the report.
fn buttons_byte(buttons_state: u8) -> u8 {
    // The scrcpy `sc_mouse_button` enum is encoded as a bitmask with
    // bits at positions 1, 2, 3, 16, 17 (from `SDL_BUTTON_MASK`).
    //   SC_MOUSE_BUTTON_LEFT   = SDL_BUTTON_LMASK   = 1 << 0  → bit 0
    //   SC_MOUSE_BUTTON_RIGHT  = SDL_BUTTON_RMASK   = 1 << 1  → bit 1
    //   SC_MOUSE_BUTTON_MIDDLE = SDL_BUTTON_MMASK   = 1 << 2  → bit 2
    //   SC_MOUSE_BUTTON_X1     = SDL_BUTTON_X1MASK  = 1 << 3
    //   SC_MOUSE_BUTTON_X2     = SDL_BUTTON_X2MASK  = 1 << 4
    // Because the input `buttons_state` is already a `u8` bitmask at
    // these bit positions, we can copy the 5 low bits directly.
    let mut c = 0u8;
    if buttons_state & BTN_LEFT   != 0 { c |= BTN_LEFT; }
    if buttons_state & BTN_RIGHT  != 0 { c |= BTN_RIGHT; }
    if buttons_state & BTN_MIDDLE != 0 { c |= BTN_MIDDLE; }
    if buttons_state & BTN_X1     != 0 { c |= BTN_X1; }
    if buttons_state & BTN_X2     != 0 { c |= BTN_X2; }
    c
}

/// Clamp a 32-bit signed integer to the signed-byte range used by
/// relative motion fields in the HID report.
#[inline]
fn clamp_i8(v: i32) -> i8 {
    if v < -127 { -127 } else if v > 127 { 127 } else { v as i8 }
}

/// Consume the integer portion of a float, leaving the fractional part
/// in `*scroll`. Returns the consumed byte.
fn consume_scroll_integer(scroll: &mut f32) -> i8 {
    let value = scroll.clamp(-127.0, 127.0);
    let consume: i8 = value.trunc() as i8;
    let residual = value - consume as f32;
    *scroll = residual;
    consume
}

/// HID mouse device.
#[derive(Debug, Clone)]
pub struct MouseHid {
    /// Fractional horizontal scroll left over from the previous event.
    pub residual_hscroll: f32,
    /// Fractional vertical scroll left over from the previous event.
    pub residual_vscroll: f32,
}

impl Default for MouseHid {
    fn default() -> Self { Self::new() }
}

impl MouseHid {
    pub fn new() -> Self {
        Self {
            residual_hscroll: 0.0,
            residual_vscroll: 0.0,
        }
    }

    /// Convert a relative motion event into a 5-byte input report.
    ///
    /// `buttons_state` is a bitwise-OR of [`crate::types::MouseButton`]
    /// values.
    pub fn generate_input_from_motion(&self, xrel: i32, yrel: i32,
                                       buttons_state: u8) -> HidReport {
        let mut data = [0u8; MOUSE_REPORT_SIZE];
        data[0] = buttons_byte(buttons_state);
        data[1] = clamp_i8(xrel) as u8;
        data[2] = clamp_i8(yrel) as u8;
        // bytes 3-4 (wheel / AC pan) stay 0 for pure motion.
        HidReport::new(HID_ID_MOUSE, data.to_vec())
    }

    /// Convert a click event (down or up) into a 5-byte input report.
    pub fn generate_input_from_click(&self, buttons_state: u8) -> HidReport {
        let mut data = [0u8; MOUSE_REPORT_SIZE];
        data[0] = buttons_byte(buttons_state);
        // Bytes 1-4 (motion + wheel) stay 0 for a pure click.
        HidReport::new(HID_ID_MOUSE, data.to_vec())
    }

    /// Convert a scroll event into a 5-byte input report, accumulating
    /// fractional deltas. Returns `None` if neither axis has accumulated
    /// at least ±1 since the last emitted report.
    pub fn generate_input_from_scroll(&mut self, hscroll: f32, vscroll: f32)
        -> Option<HidReport>
    {
        self.residual_hscroll += hscroll;
        self.residual_vscroll += vscroll;
        let h = consume_scroll_integer(&mut self.residual_hscroll);
        let v = consume_scroll_integer(&mut self.residual_vscroll);
        if h == 0 && v == 0 { return None; }

        let mut data = [0u8; MOUSE_REPORT_SIZE];
        data[0] = 0;
        data[3] = v as u8;
        data[4] = h as u8;
        Some(HidReport::new(HID_ID_MOUSE, data.to_vec()))
    }

    /// Convenience: motion + click in a single shot. Equivalent to
    /// [`Self::generate_input_from_motion`] but with explicit
    /// `xrel=0, yrel=0`.
    pub fn click_message(&self, buttons_state: u8) -> ControlMessage {
        let report = self.generate_input_from_click(buttons_state);
        self.to_input_message(&report)
    }

    pub fn motion_message(&self, xrel: i32, yrel: i32,
                          buttons_state: u8) -> ControlMessage {
        let report = self.generate_input_from_motion(xrel, yrel, buttons_state);
        self.to_input_message(&report)
    }

    /// Convert a [`HidReport`] into a UHID_INPUT [`ControlMessage`].
    pub fn to_input_message(&self, report: &HidReport) -> ControlMessage {
        assert_eq!(report.hid_id, HID_ID_MOUSE);
        let mut data = [0u8; HID_MAX_SIZE];
        let n = report.data.len().min(HID_MAX_SIZE);
        data[..n].copy_from_slice(&report.data[..n]);
        ControlMessage::UhidInput(UhidInput {
            id: report.hid_id,
            size: n as u16,
            data,
        })
    }
}

impl HidDevice for MouseHid {
    fn hid_id(&self) -> u16 { HID_ID_MOUSE }

    fn open_message(&self, _name: Option<&str>) -> Result<ControlMessage> {
        Ok(ControlMessage::UhidCreate(UhidCreate {
            id: HID_ID_MOUSE,
            vendor_id: 0,
            product_id: 0,
            name: None,
            report_desc: MOUSE_REPORT_DESC.to_vec(),
        }))
    }

    fn close_message(&self) -> Result<ControlMessage> {
        Ok(ControlMessage::UhidDestroy(UhidDestroy { id: HID_ID_MOUSE }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::MouseButton;

    #[test]
    fn motion_clamps_to_signed_byte() {
        let m = MouseHid::new();
        let r = m.generate_input_from_motion(5, -4, 0);
        assert_eq!(r.data, vec![0x00, 0x05, 0xFC, 0x00, 0x00]); // -4 = 0xFC
    }

    #[test]
    fn motion_clamps_large_delta() {
        let m = MouseHid::new();
        let r = m.generate_input_from_motion(500, -500, 0);
        assert_eq!(r.data[1], 127);
        assert_eq!(r.data[2] as i8, -127);
    }

    #[test]
    fn click_carries_button_state() {
        let m = MouseHid::new();
        let r = m.generate_input_from_click(
            MouseButton::state(&[MouseButton::Left]));
        assert_eq!(r.data, vec![0x01, 0x00, 0x00, 0x00, 0x00]);
    }

    #[test]
    fn scroll_emit_only_after_integer_accumulated() {
        let mut m = MouseHid::new();
        assert!(m.generate_input_from_scroll(0.5, 0.0).is_none());
        assert!(m.generate_input_from_scroll(0.5, 0.0).is_some());
    }

    #[test]
    fn scroll_consumes_integer_part() {
        let mut m = MouseHid::new();
        let r = m.generate_input_from_scroll(2.7, -1.3).unwrap();
        // v = -1, h = 2 (truncation toward zero)
        assert_eq!(r.data[3] as i8, -1);
        assert_eq!(r.data[4] as i8,  2);
        // residual kept for next event
        assert!((m.residual_hscroll - 0.7).abs() < 1e-6);
        assert!((m.residual_vscroll + 0.3).abs() < 1e-6);
    }

    #[test]
    fn click_message_to_control() {
        let m = MouseHid::new();
        let msg = m.click_message(MouseButton::state(&[MouseButton::Right]));
        match msg {
            ControlMessage::UhidInput(i) => {
                assert_eq!(i.id, 2);
                assert_eq!(i.size, 5);
                assert_eq!(i.data[0], 0x02);
            }
            _ => panic!("expected UhidInput"),
        }
    }

    #[test]
    fn open_close_roundtrip() {
        let m = MouseHid::new();
        match m.open_message(None).unwrap() {
            ControlMessage::UhidCreate(c) => {
                assert_eq!(c.id, 2);
                assert_eq!(c.vendor_id, 0);
                assert!(c.name.is_none());
            }
            _ => panic!(),
        }
        match m.close_message().unwrap() {
            ControlMessage::UhidDestroy(d) => assert_eq!(d.id, 2),
            _ => panic!(),
        }
    }

    #[test]
    fn buttons_byte_packs_three_buttons() {
        let s = MouseButton::state(&[MouseButton::Left, MouseButton::Middle, MouseButton::X1]);
        let m = MouseHid::new();
        let r = m.generate_input_from_click(s);
        assert_eq!(r.data[0], 0b00001101);
    }
}
