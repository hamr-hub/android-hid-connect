//! HID gamepad driver — Rust port of `sc_gamepad_uhid` + `sc_hid_gamepad`.
//!
//! Each open gamepad takes a slot out of the 8 available
//! ([`crate::types::MAX_GAMEPADS`]). The driver tracks the full 32-bit
//! button bitmap (16 bits transmitted as-is + 16 dpad bits transformed
//! into a 4-bit hat switch value) and the four 16-bit analog axes plus
//! two 16-bit triggers.
//!
//! 15-byte report layout:
//!
//! ```text
//!   bytes 0-1:   left stick X (u16, 0..=65535, little-endian)
//!   bytes 2-3:   left stick Y
//!   bytes 4-5:   right stick X
//!   bytes 6-7:   right stick Y
//!   bytes 8-9:   left trigger (u16, 0..=32767)
//!   bytes 10-11: right trigger (u16, 0..=32767)
//!   bytes 12-13: 16 button bits (little-endian, bit 0 = South / A)
//!   byte 14:     hat switch position (1..=8, 0 = centered)
//! ```

use crate::control::message::{ControlMessage, UhidCreate, UhidDestroy, UhidInput};
use crate::error::{Error, Result};
use crate::hid::descriptor::GAMEPAD_REPORT_DESC;
use crate::hid::{HidDevice, HidReport};
use crate::types::{
    dpad_hat_value, GamepadAxis, GamepadButton, GAMEPAD_ID_INVALID, HID_ID_GAMEPAD_FIRST,
    HID_ID_GAMEPAD_LAST, HID_MAX_SIZE, MAX_GAMEPADS,
};

/// Total size of a gamepad HID input report.
const GAMEPAD_REPORT_SIZE: usize = 15;

/// Mask of the 16 "transmit as-is" button bits (the lower 16 of the
/// internal 32-bit button bitmap).
const BUTTONS_MASK: u32 = 0xFFFF;

/// Bit set when a gamepad slot is free. The first 16 bits of the button
/// state stay at 0, but the dpad bits must be cleared so a freed slot
/// does not bleed dpad state into the next opened gamepad.
const SLOT_FREE_BUTTONS: u32 = 0;

const SC_GAMEPAD_AXIS_LEFT_TRIGGER: i16 = GamepadAxis::LeftTrigger as i16;
const SC_GAMEPAD_AXIS_RIGHT_TRIGGER: i16 = GamepadAxis::RightTrigger as i16;

const SC_GAMEPAD_AXIS_LEFTX: i16 = GamepadAxis::LeftX as i16;
const SC_GAMEPAD_AXIS_LEFTY: i16 = GamepadAxis::LeftY as i16;
const SC_GAMEPAD_AXIS_RIGHTX: i16 = GamepadAxis::RightX as i16;
const SC_GAMEPAD_AXIS_RIGHTY: i16 = GamepadAxis::RightY as i16;

/// Per-gamepad state tracked by [`GamepadHid`].
#[derive(Debug, Clone, Copy, Default)]
struct GamepadSlot {
    /// Bound gamepad id (0 = free slot, matches `SC_GAMEPAD_ID_INVALID`).
    gamepad_id: u32,
    /// Full 32-bit button bitmap (lower 16 + dpad).
    buttons: u32,
    /// Four analog axes, mapped from i16 (`-32768..=32767`) to
    /// `u16` (`0..=65535`) using the scrcpy `AXIS_RESCALE` macro.
    axis_left_x: u16,
    axis_left_y: u16,
    axis_right_x: u16,
    axis_right_y: u16,
    /// Two analog triggers. Spec allows 0..=32767.
    axis_left_trigger: u16,
    axis_right_trigger: u16,
}

/// HID gamepad device with up to [`MAX_GAMEPADS`] concurrent slots.
#[derive(Debug, Clone)]
pub struct GamepadHid {
    slots: [GamepadSlot; MAX_GAMEPADS],
}

impl Default for GamepadHid {
    fn default() -> Self {
        Self::new()
    }
}

impl GamepadHid {
    pub fn new() -> Self {
        let mut slots = [GamepadSlot::default(); MAX_GAMEPADS];
        for s in slots.iter_mut() {
            s.gamepad_id = GAMEPAD_ID_INVALID;
        }
        Self { slots }
    }

    /// HID id assigned to a given slot index.
    fn slot_hid_id(slot_idx: usize) -> u16 {
        debug_assert!(slot_idx < MAX_GAMEPADS);
        HID_ID_GAMEPAD_FIRST + slot_idx as u16
    }

    /// Find a free slot, if any.
    fn free_slot(&self) -> Option<usize> {
        self.slots
            .iter()
            .position(|s| s.gamepad_id == GAMEPAD_ID_INVALID)
    }

    /// Find the slot bound to a given gamepad id.
    fn find_slot(&self, gamepad_id: u32) -> Option<usize> {
        self.slots.iter().position(|s| s.gamepad_id == gamepad_id)
    }

    /// Allocate a slot for `gamepad_id` and return the HID id assigned
    /// to it. The slot's axes and button state are reset.
    pub fn open(&mut self, gamepad_id: u32, name: Option<&str>) -> Result<(u16, ControlMessage)> {
        if gamepad_id == GAMEPAD_ID_INVALID {
            return Err(Error::UnknownGamepad(gamepad_id));
        }
        if self.find_slot(gamepad_id).is_some() {
            // Already open — treat as a no-op and re-emit CREATE. scrcpy
            // also does not fail in this case, but we re-use the same
            // slot to keep things idempotent.
            let idx = self.find_slot(gamepad_id).unwrap();
            return Ok((
                Self::slot_hid_id(idx),
                self.build_create(Self::slot_hid_id(idx), name)?,
            ));
        }
        let slot_idx = self.free_slot().ok_or(Error::NoGamepadSlot)?;
        let hid_id = Self::slot_hid_id(slot_idx);
        let slot = GamepadSlot {
            gamepad_id,
            buttons: SLOT_FREE_BUTTONS,
            axis_left_x: axis_rescale(0),
            axis_left_y: axis_rescale(0),
            axis_right_x: axis_rescale(0),
            axis_right_y: axis_rescale(0),
            axis_left_trigger: 0,
            axis_right_trigger: 0,
        };
        self.slots[slot_idx] = slot;
        Ok((hid_id, self.build_create(hid_id, name)?))
    }

    /// Release the slot bound to `gamepad_id`.
    pub fn close(&mut self, gamepad_id: u32) -> Result<ControlMessage> {
        if gamepad_id == GAMEPAD_ID_INVALID {
            return Err(Error::UnknownGamepad(gamepad_id));
        }
        let slot_idx = self
            .find_slot(gamepad_id)
            .ok_or(Error::UnknownGamepad(gamepad_id))?;
        self.slots[slot_idx].gamepad_id = GAMEPAD_ID_INVALID;
        Ok(ControlMessage::UhidDestroy(UhidDestroy {
            id: Self::slot_hid_id(slot_idx),
        }))
    }

    fn build_create(&self, hid_id: u16, name: Option<&str>) -> Result<ControlMessage> {
        // Xbox 360 is the default identity scrcpy uses.
        Ok(ControlMessage::UhidCreate(UhidCreate {
            id: hid_id,
            vendor_id: 0x045e,
            product_id: 0x028e,
            name: name.map(|s| s.to_string()),
            report_desc: GAMEPAD_REPORT_DESC.to_vec(),
        }))
    }

    /// Apply a button event and return the resulting UHID_INPUT message.
    pub fn button_event(
        &mut self,
        gamepad_id: u32,
        button: GamepadButton,
        pressed: bool,
    ) -> Result<ControlMessage> {
        let slot_idx = self
            .find_slot(gamepad_id)
            .ok_or(Error::UnknownGamepad(gamepad_id))?;
        let slot = &mut self.slots[slot_idx];
        let bit = button as u32;
        if pressed {
            slot.buttons |= bit;
        } else {
            slot.buttons &= !bit;
        }
        let hid_id = Self::slot_hid_id(slot_idx);
        let data = slot_report_bytes(hid_id, slot);
        Ok(to_input_message(hid_id, &data))
    }

    /// Apply an axis event. Triggers are always positive; sticks are
    /// signed. Out-of-range axis values are clamped.
    pub fn axis_event(
        &mut self,
        gamepad_id: u32,
        axis: GamepadAxis,
        value: i16,
    ) -> Result<ControlMessage> {
        let slot_idx = self
            .find_slot(gamepad_id)
            .ok_or(Error::UnknownGamepad(gamepad_id))?;
        let slot = &mut self.slots[slot_idx];
        match axis as i16 {
            x if x == SC_GAMEPAD_AXIS_LEFTX => slot.axis_left_x = axis_rescale(value),
            x if x == SC_GAMEPAD_AXIS_LEFTY => slot.axis_left_y = axis_rescale(value),
            x if x == SC_GAMEPAD_AXIS_RIGHTX => slot.axis_right_x = axis_rescale(value),
            x if x == SC_GAMEPAD_AXIS_RIGHTY => slot.axis_right_y = axis_rescale(value),
            x if x == SC_GAMEPAD_AXIS_LEFT_TRIGGER => {
                slot.axis_left_trigger = (value.max(0) as u16).min(0x7FFF);
            }
            x if x == SC_GAMEPAD_AXIS_RIGHT_TRIGGER => {
                slot.axis_right_trigger = (value.max(0) as u16).min(0x7FFF);
            }
            _ => return Err(Error::UnknownGamepad(0)), // unknown axis
        }
        let hid_id = Self::slot_hid_id(slot_idx);
        let data = slot_report_bytes(hid_id, slot);
        Ok(to_input_message(hid_id, &data))
    }

    /// Convert a [`HidReport`] into a UHID_INPUT [`ControlMessage`].
    pub fn to_input_message(&self, report: &HidReport) -> ControlMessage {
        to_input_message(report.hid_id, &report.data)
    }

    /// Number of gamepads currently registered.
    pub fn active_count(&self) -> usize {
        self.slots
            .iter()
            .filter(|s| s.gamepad_id != GAMEPAD_ID_INVALID)
            .count()
    }

    /// Build a UHID_CREATE for a given HID id (after `open` returns one).
    /// This is a thin wrapper that uses the descriptor and a fixed Xbox
    /// 360 vendor/product pair.
    pub fn create_message(hid_id: u16, name: Option<&str>) -> ControlMessage {
        ControlMessage::UhidCreate(UhidCreate {
            id: hid_id,
            vendor_id: 0x045e,
            product_id: 0x028e,
            name: name.map(|s| s.to_string()),
            report_desc: GAMEPAD_REPORT_DESC.to_vec(),
        })
    }

    /// Map an HID id back to a slot index. Returns `None` for ids outside
    /// the gamepad range.
    pub fn slot_from_hid_id(hid_id: u16) -> Option<usize> {
        if (HID_ID_GAMEPAD_FIRST..=HID_ID_GAMEPAD_LAST).contains(&hid_id) {
            Some((hid_id - HID_ID_GAMEPAD_FIRST) as usize)
        } else {
            None
        }
    }
}

impl HidDevice for GamepadHid {
    /// `GamepadHid` is multi-instance; this returns the first gamepad
    /// slot's hid_id, or 0 if no slot is open. Use [`GamepadHid::open`]
    /// instead to allocate slots.
    fn hid_id(&self) -> u16 {
        self.slots
            .iter()
            .find(|s| s.gamepad_id != GAMEPAD_ID_INVALID)
            .map(|_| Self::slot_hid_id(0))
            .unwrap_or(0)
    }

    fn open_message(&self, _name: Option<&str>) -> Result<ControlMessage> {
        // Use a default-allocated slot if one is free.
        Ok(Self::create_message(
            HID_ID_GAMEPAD_FIRST,
            Some("Microsoft X-Box 360 Pad"),
        ))
    }

    fn close_message(&self) -> Result<ControlMessage> {
        Ok(ControlMessage::UhidDestroy(UhidDestroy {
            id: HID_ID_GAMEPAD_FIRST,
        }))
    }
}

/// `[-32768, 32767] -> [0, 65535]` axis rescaling (scrcpy `AXIS_RESCALE`).
#[inline]
fn axis_rescale(v: i16) -> u16 {
    (v as i32 + 0x8000) as u16
}

fn slot_report_bytes(hid_id: u16, slot: &GamepadSlot) -> Vec<u8> {
    let mut data = vec![0u8; GAMEPAD_REPORT_SIZE];
    data[0..2].copy_from_slice(&slot.axis_left_x.to_le_bytes());
    data[2..4].copy_from_slice(&slot.axis_left_y.to_le_bytes());
    data[4..6].copy_from_slice(&slot.axis_right_x.to_le_bytes());
    data[6..8].copy_from_slice(&slot.axis_right_y.to_le_bytes());
    data[8..10].copy_from_slice(&slot.axis_left_trigger.to_le_bytes());
    data[10..12].copy_from_slice(&slot.axis_right_trigger.to_le_bytes());
    let btn16 = (slot.buttons & BUTTONS_MASK) as u16;
    data[12..14].copy_from_slice(&btn16.to_le_bytes());
    data[14] = dpad_hat_value(slot.buttons);
    let _ = hid_id;
    data
}

fn to_input_message(hid_id: u16, data: &[u8]) -> ControlMessage {
    let mut buf = [0u8; HID_MAX_SIZE];
    let n = data.len().min(HID_MAX_SIZE);
    buf[..n].copy_from_slice(&data[..n]);
    ControlMessage::UhidInput(UhidInput {
        id: hid_id,
        size: n as u16,
        data: buf,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build() -> GamepadHid {
        GamepadHid::new()
    }

    #[test]
    fn open_assigns_first_slot() {
        let mut g = build();
        let (hid_id, msg) = g.open(42, Some("Pad")).unwrap();
        assert_eq!(hid_id, HID_ID_GAMEPAD_FIRST);
        match msg {
            ControlMessage::UhidCreate(c) => {
                assert_eq!(c.id, hid_id);
                assert_eq!(c.vendor_id, 0x045e);
                assert_eq!(c.product_id, 0x028e);
                assert_eq!(c.name.as_deref(), Some("Pad"));
                assert_eq!(c.report_desc, GAMEPAD_REPORT_DESC);
            }
            _ => panic!(),
        }
        assert_eq!(g.active_count(), 1);
    }

    #[test]
    fn open_fills_eight_slots() {
        let mut g = build();
        for id in 1..=MAX_GAMEPADS as u32 {
            g.open(id, None).unwrap();
        }
        assert_eq!(g.active_count(), MAX_GAMEPADS);
        assert!(g.open(99, None).is_err());
    }

    #[test]
    fn open_rejects_invalid_id() {
        let mut g = build();
        assert!(g.open(0, None).is_err());
    }

    #[test]
    fn close_releases_slot() {
        let mut g = build();
        let (hid_id, _) = g.open(7, None).unwrap();
        let msg = g.close(7).unwrap();
        match msg {
            ControlMessage::UhidDestroy(d) => assert_eq!(d.id, hid_id),
            _ => panic!(),
        }
        assert_eq!(g.active_count(), 0);
    }

    #[test]
    fn close_unknown_errors() {
        let mut g = build();
        assert!(g.close(99).is_err());
    }

    #[test]
    fn button_event_toggles_bitmap() {
        let mut g = build();
        g.open(1, None).unwrap();
        // Press A (South).
        g.button_event(1, GamepadButton::South, true).unwrap();
        // Press Dpad up.
        g.button_event(1, GamepadButton::DpadUp, true).unwrap();

        // Re-emit by toggling Dpad off and re-pressing a button so we
        // can inspect the 15-byte report via axis_event(0) round trip.
        let msg = g.button_event(1, GamepadButton::DpadUp, false).unwrap();
        match msg {
            ControlMessage::UhidInput(i) => {
                assert_eq!(i.id, HID_ID_GAMEPAD_FIRST);
                assert_eq!(i.size, 15);
                // byte 12 = LSB of btn16; bit 0 = South = 1
                assert_eq!(i.data[12], 0x01);
                // byte 13 = MSB of btn16 = 0
                assert_eq!(i.data[13], 0x00);
                // byte 14 = hat switch = 0 (centered) since dpad cleared
                assert_eq!(i.data[14], 0);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn axis_event_rescales() {
        let mut g = build();
        g.open(1, None).unwrap();
        let msg = g.axis_event(1, GamepadAxis::LeftX, 0).unwrap();
        match msg {
            ControlMessage::UhidInput(i) => {
                // 0 -> 0x8000 (little-endian: 0x00 0x80)
                assert_eq!(i.data[0..2], [0x00, 0x80]);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn axis_event_trigger_clamped_nonnegative() {
        let mut g = build();
        g.open(1, None).unwrap();
        g.axis_event(1, GamepadAxis::LeftTrigger, -1000).unwrap();
        let msg = g.axis_event(1, GamepadAxis::LeftTrigger, 100).unwrap();
        match msg {
            ControlMessage::UhidInput(i) => {
                // 100 -> 100
                assert_eq!(i.data[8..10], 100u16.to_le_bytes());
            }
            _ => panic!(),
        }
    }

    #[test]
    fn dpad_hat_value_packed_in_byte_14() {
        let mut g = build();
        g.open(1, None).unwrap();
        g.button_event(1, GamepadButton::DpadUp, true).unwrap();
        g.button_event(1, GamepadButton::DpadRight, true).unwrap();
        // Toggle a non-dpad to emit a fresh report.
        let msg = g.button_event(1, GamepadButton::South, true).unwrap();
        match msg {
            ControlMessage::UhidInput(i) => {
                assert_eq!(i.data[14], 2); // up + right
            }
            _ => panic!(),
        }
    }

    #[test]
    fn slot_hid_id_roundtrip() {
        for i in 0..MAX_GAMEPADS {
            let hid = GamepadHid::slot_hid_id(i);
            assert_eq!(GamepadHid::slot_from_hid_id(hid), Some(i));
        }
        assert_eq!(GamepadHid::slot_from_hid_id(99), None);
    }
}
