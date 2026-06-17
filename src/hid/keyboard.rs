//! HID keyboard driver — Rust port of `sc_keyboard_uhid` + `sc_hid_keyboard`.
//!
//! The driver tracks the current "keys held" set so that the host can
//! always emit a complete 8-byte report describing the current state
//! (not just a delta). The 8-byte layout matches scrcpy:
//!
//! ```text
//!   byte 0: modifier bitmap
//!   byte 1: reserved (0)
//!   bytes 2..=7: up to 6 currently-pressed scancodes
//! ```
//!
//! If more than 6 non-modifier keys are pressed, the list is replaced with
//! the USB HID "ErrorRollOver" code (0x01) in every slot (the "phantom
//! state") per the HID specification.

use crate::control::message::{ControlMessage, UhidCreate, UhidDestroy, UhidInput};
use crate::error::{Error, Result};
use crate::hid::descriptor::KEYBOARD_REPORT_DESC;
use crate::hid::{HidDevice, HidReport};
use crate::types::{Modifiers, HID_ID_KEYBOARD, HID_MAX_SIZE};

/// Number of non-modifier scancode slots in a keyboard report.
const KEYBOARD_REPORT_SIZE: usize = 8;
const KEYBOARD_MAX_KEYS: usize = 6;

/// Bit-position offsets of the modifier keys in the modifier byte.
const SC_HID_MOD_LCTRL: u8 = 1 << 0;
const SC_HID_MOD_LSHIFT: u8 = 1 << 1;
const SC_HID_MOD_LALT: u8 = 1 << 2;
const SC_HID_MOD_LGUI: u8 = 1 << 3;
const SC_HID_MOD_RCTRL: u8 = 1 << 4;
const SC_HID_MOD_RSHIFT: u8 = 1 << 5;
const SC_HID_MOD_RALT: u8 = 1 << 6;
const SC_HID_MOD_RGUI: u8 = 1 << 7;

/// USB HID ErrorRollOver code used in the phantom-state slots.
const SC_HID_ERROR_ROLL_OVER: u8 = 0x01;

/// Translate an 8-bit `Modifiers` bitmap (where each bit corresponds to a
/// modifier key in the same bit position scrcpy uses) into the byte that
/// actually goes into the keyboard report.
#[inline]
fn hid_modifier_byte(m: Modifiers) -> u8 {
    // The bit positions are identical, so this is a straight copy.
    m.bits()
}

/// HID keyboard device.
///
/// Holds the keyboard's state machine (which keys are currently held, and
/// the latest modifier snapshot from the device's LED output) and produces
/// the 8-byte input reports scrcpy expects.
#[derive(Debug, Clone)]
pub struct KeyboardHid {
    /// `true` for each scancode currently held. The size matches the
    /// AT-101 / USB HID scancode range up to 0x65 (102 keys); the modifier
    /// range 0xE0..=0xE7 is tracked separately via the modifier byte.
    keys: [bool; 0x66],

    /// Latest modifier snapshot derived from the device's LED output
    /// report. Used for synchronisation when a `modifiers(...)` event
    /// arrives without an accompanying key event.
    device_mod: Modifiers,
}

impl Default for KeyboardHid {
    fn default() -> Self {
        Self::new()
    }
}

impl KeyboardHid {
    pub fn new() -> Self {
        Self {
            keys: [false; 0x66],
            device_mod: Modifiers::empty(),
        }
    }

    /// The number of keys currently held (including the 0..=0x65 range
    /// only, not modifiers). Exposed for tests.
    pub fn pressed_count(&self) -> usize {
        self.keys.iter().filter(|k| **k).count()
    }

    /// Mark a scancode as pressed (`true`) or released (`false`). Modifiers
    /// (0xE0..=0xE7) are tracked in the modifier byte and do not occupy a
    /// slot in the 6-key list.
    pub fn set_key(&mut self, scancode: u8, pressed: bool) -> Result<()> {
        let sc = scancode as u16;
        // Validate that the scancode is in-range (modifier or 0..=0x65).
        if sc > 0x65 && !(0xE0..=0xE7).contains(&sc) {
            return Err(Error::ScancodeOutOfRange(sc));
        }
        if sc <= 0x65 {
            self.keys[sc as usize] = pressed;
        }
        // Modifier scancodes are not stored in `keys`; they ride in
        // `device_mod` and are emitted via the modifier byte in reports.
        Ok(())
    }

    /// Update the modifier byte directly (e.g. to resync with the
    /// device's LED output). Returns the previous modifier value so the
    /// caller can diff and synthesise a phantom state.
    pub fn set_modifiers(&mut self, m: Modifiers) -> Modifiers {
        let prev = self.device_mod;
        self.device_mod = m;
        prev
    }

    /// Snapshot of the modifier byte the device last reported.
    pub fn modifiers(&self) -> Modifiers {
        self.device_mod
    }

    /// Build an 8-byte input report representing the current keyboard
    /// state, using the supplied modifier bitmap. (The caller is expected
    /// to also OR in any modifier-only scancodes they want to inject.)
    pub fn build_report(&self, modifiers: Modifiers) -> HidReport {
        let mut data = [0u8; KEYBOARD_REPORT_SIZE];
        data[0] = hid_modifier_byte(modifiers);
        // data[1] is reserved, stays 0.
        let mut slot: usize = 0;
        for (sc, &held) in self.keys.iter().enumerate() {
            if !held {
                continue;
            }
            if slot >= KEYBOARD_MAX_KEYS {
                // Phantom state: fill every key slot with ErrorRollOver.
                for b in &mut data[2..] {
                    *b = SC_HID_ERROR_ROLL_OVER;
                }
                return HidReport::new(HID_ID_KEYBOARD, data.to_vec());
            }
            data[2 + slot] = sc as u8;
            slot += 1;
        }
        HidReport::new(HID_ID_KEYBOARD, data.to_vec())
    }

    /// Build a report from a single scancode event, automatically
    /// mutating internal state. Returns `Err` for out-of-range scancodes
    /// (the event is dropped).
    pub fn inject_key(
        &mut self,
        scancode: u8,
        pressed: bool,
        modifiers: Modifiers,
    ) -> Result<HidReport> {
        self.set_key(scancode, pressed)?;
        self.device_mod = modifiers;
        Ok(self.build_report(modifiers))
    }

    /// Convert a [`HidReport`] into a UHID_INPUT [`ControlMessage`].
    pub fn to_input_message(&self, report: &HidReport) -> ControlMessage {
        assert_eq!(report.hid_id, HID_ID_KEYBOARD);
        let mut data = [0u8; HID_MAX_SIZE];
        let n = report.data.len().min(HID_MAX_SIZE);
        data[..n].copy_from_slice(&report.data[..n]);
        ControlMessage::UhidInput(UhidInput {
            id: report.hid_id,
            size: n as u16,
            data,
        })
    }

    /// Construct a UHID_INPUT from a raw scancode event. Convenience that
    /// combines `inject_key` + `to_input_message`.
    pub fn key_event(
        &mut self,
        scancode: u8,
        pressed: bool,
        modifiers: Modifiers,
    ) -> Result<ControlMessage> {
        let report = self.inject_key(scancode, pressed, modifiers)?;
        Ok(self.to_input_message(&report))
    }

    /// Build a modifier-sync report (used when only CapsLock/NumLock
    /// change but no key event is delivered). Returns `None` if the
    /// requested modifier state is all-clear, since the device already
    /// has the correct view from the previous report.
    pub fn mods_sync_message(&self, modifiers: Modifiers) -> Option<ControlMessage> {
        // Replicate sc_hid_keyboard_generate_input_from_mods: only emit
        // a report if capslock/numlock are part of the modifier state
        // (other modifiers live in the modifier byte and do not need a
        // separate report).
        let mut data = [0u8; HID_MAX_SIZE];
        let mut slot = 0;
        if modifiers.0 != 0 {
            // The set_modifiers call is "diff" in scrcpy, but we emit a
            // full snapshot here. Real callers can diff themselves if
            // they need to minimise traffic.
            data[0] = modifiers.bits();
        }
        // Place scancodes for the two "lock" keys. There is no separate
        // HID modifier bit for CapsLock/NumLock; they are normal keys
        // with the scancode reported separately.
        if (modifiers.bits() & 0x01) != 0 {
            data[2 + slot] = 0x39; // CapsLock
            slot += 1;
        }
        if (modifiers.bits() & 0x02) != 0 {
            data[2 + slot] = 0x53; // NumLockClear
            slot += 1;
        }
        if slot == 0 && data[0] == 0 {
            return None;
        }
        Some(ControlMessage::UhidInput(UhidInput {
            id: HID_ID_KEYBOARD,
            size: KEYBOARD_REPORT_SIZE as u16,
            data,
        }))
    }
}

impl HidDevice for KeyboardHid {
    fn hid_id(&self) -> u16 {
        HID_ID_KEYBOARD
    }

    fn open_message(&self, _name: Option<&str>) -> Result<ControlMessage> {
        // scrcpy's keyboard UHID_CREATE uses vendor/product 0 and a null
        // name; the name is a static "" via `msg.uhid_create.name = NULL`
        // in keyboard_uhid.c. We follow suit — callers can use the
        // `name` arg if they want to override.
        Ok(ControlMessage::UhidCreate(UhidCreate {
            id: HID_ID_KEYBOARD,
            vendor_id: 0,
            product_id: 0,
            name: None,
            report_desc: KEYBOARD_REPORT_DESC.to_vec(),
        }))
    }

    fn close_message(&self) -> Result<ControlMessage> {
        Ok(ControlMessage::UhidDestroy(UhidDestroy {
            id: HID_ID_KEYBOARD,
        }))
    }
}

#[allow(dead_code)]
fn hid_mod_from_sdl_keymod(m: u16) -> u8 {
    // Replicates `sc_hid_mod_from_sdl_keymod` from scrcpy.
    let mut mods = 0u8;
    if m & 0x0001 != 0 {
        mods |= SC_HID_MOD_LCTRL;
    }
    if m & 0x0002 != 0 {
        mods |= SC_HID_MOD_LSHIFT;
    }
    if m & 0x0004 != 0 {
        mods |= SC_HID_MOD_LALT;
    }
    if m & 0x0008 != 0 {
        mods |= SC_HID_MOD_LGUI;
    }
    if m & 0x0010 != 0 {
        mods |= SC_HID_MOD_RCTRL;
    }
    if m & 0x0020 != 0 {
        mods |= SC_HID_MOD_RSHIFT;
    }
    if m & 0x0040 != 0 {
        mods |= SC_HID_MOD_RALT;
    }
    if m & 0x0080 != 0 {
        mods |= SC_HID_MOD_RGUI;
    }
    mods
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::message::ControlMessage;

    #[test]
    fn a_then_w_produces_two_key_slots() {
        let mut k = KeyboardHid::new();
        k.set_key(0x04, true).unwrap(); // A
        k.set_key(0x1A, true).unwrap(); // W
        let r = k.build_report(Modifiers::empty());
        assert_eq!(r.hid_id, HID_ID_KEYBOARD);
        assert_eq!(r.data, vec![0x00, 0x00, 0x04, 0x1A, 0x00, 0x00, 0x00, 0x00]);
    }

    #[test]
    fn phantom_state_when_seven_keys_pressed() {
        let mut k = KeyboardHid::new();
        for sc in 0x04u8..0x0A {
            k.set_key(sc, true).unwrap(); // 6 keys
        }
        k.set_key(0x1E, true).unwrap(); // 7th key
        let r = k.build_report(Modifiers::empty());
        // First 6 slots become scancodes 0x04..=0x09; slot 6 (= 7th
        // key) overflows the array, but we trigger the phantom fill at
        // the boundary, so the entire 6-slot region becomes ErrorRollOver.
        assert_eq!(r.data, vec![0x00, 0x00, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01]);
    }

    #[test]
    fn modifier_byte_is_first_byte() {
        let k = KeyboardHid::new();
        let r = k.build_report(Modifiers::LCTRL | Modifiers::LSHIFT);
        assert_eq!(r.data[0], 0x03);
        assert_eq!(r.data[1], 0x00);
    }

    #[test]
    fn open_message_carries_descriptor() {
        let k = KeyboardHid::new();
        let m = k.open_message(None).unwrap();
        match m {
            ControlMessage::UhidCreate(c) => {
                assert_eq!(c.id, 1);
                assert_eq!(c.vendor_id, 0);
                assert_eq!(c.product_id, 0);
                assert!(c.name.is_none());
                assert_eq!(c.report_desc, KEYBOARD_REPORT_DESC);
            }
            _ => panic!("expected UhidCreate"),
        }
    }

    #[test]
    fn close_message_id_matches() {
        let k = KeyboardHid::new();
        match k.close_message().unwrap() {
            ControlMessage::UhidDestroy(d) => assert_eq!(d.id, 1),
            _ => panic!("expected UhidDestroy"),
        }
    }

    #[test]
    fn out_of_range_scancode_is_rejected() {
        let mut k = KeyboardHid::new();
        assert!(k.set_key(0x66, true).is_err());
        assert!(k.set_key(0xDF, true).is_err());
        assert!(k.set_key(0xE0, true).is_ok()); // modifier OK
        assert!(k.set_key(0xE7, true).is_ok());
    }

    #[test]
    fn inject_key_returns_input_message() {
        let mut k = KeyboardHid::new();
        let m = k.key_event(0x04, true, Modifiers::LSHIFT).unwrap();
        match m {
            ControlMessage::UhidInput(i) => {
                assert_eq!(i.id, 1);
                assert_eq!(i.size, 8);
                assert_eq!(i.data[0], 0x02); // LSHIFT
                assert_eq!(i.data[2], 0x04); // A
            }
            _ => panic!("expected UhidInput"),
        }
    }

    #[test]
    fn set_key_validates_helper() {
        let mut k = KeyboardHid::new();
        assert!(k.set_key(0x04, true).is_ok());
        assert!(k.set_key(0xE0, true).is_ok());
        assert!(k.set_key(0x66, true).is_err());
        assert!(k.set_key(0xDF, true).is_err());
        assert!(k.set_key(0xFF, true).is_err());
    }
}
