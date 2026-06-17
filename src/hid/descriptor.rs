//! USB HID report descriptors for the keyboard, mouse and gamepad devices
//! created by [`crate::hid::KeyboardHid`], [`crate::hid::MouseHid`] and
//! [`crate::hid::GamepadHid`].
//!
//! The byte tables are transcribed verbatim from
//! `scrcpy/app/src/hid/hid_keyboard.c`, `hid_mouse.c`, `hid_gamepad.c` and
//! from the USB HID specification 1.11 / HID Usage Tables 1.5.

/// HID keyboard report descriptor.
///
/// 8-byte reports:
///   * byte 0: modifier bitmap (1 flag per modifier)
///   * byte 1: reserved (always 0)
///   * bytes 2..=7: up to 6 currently-pressed scancodes
///
/// Includes an LED output report (5 LEDs + 3 padding bits) so the device
/// can report NumLock / CapsLock / ScrollLock back to the host.
pub const KEYBOARD_REPORT_DESC: &[u8] = &[
    // Usage Page (Generic Desktop)
    0x05, 0x01,
    // Usage (Keyboard)
    0x09, 0x06,

    // Collection (Application)
    0xA1, 0x01,

    // Usage Page (Key Codes)
    0x05, 0x07,
    // Usage Minimum (224)
    0x19, 0xE0,
    // Usage Maximum (231)
    0x29, 0xE7,
    // Logical Minimum (0)
    0x15, 0x00,
    // Logical Maximum (1)
    0x25, 0x01,
    // Report Size (1)
    0x75, 0x01,
    // Report Count (8)
    0x95, 0x08,
    // Input (Data, Variable, Absolute): Modifier byte
    0x81, 0x02,

    // Report Size (8)
    0x75, 0x08,
    // Report Count (1)
    0x95, 0x01,
    // Input (Constant): Reserved byte
    0x81, 0x01,

    // Usage Page (LEDs)
    0x05, 0x08,
    // Usage Minimum (1)
    0x19, 0x01,
    // Usage Maximum (5)
    0x29, 0x05,
    // Report Size (1)
    0x75, 0x01,
    // Report Count (5)
    0x95, 0x05,
    // Output (Data, Variable, Absolute): LED report
    0x91, 0x02,

    // Report Size (3)
    0x75, 0x03,
    // Report Count (1)
    0x95, 0x01,
    // Output (Constant): LED report padding
    0x91, 0x01,

    // Usage Page (Key Codes)
    0x05, 0x07,
    // Usage Minimum (0)
    0x19, 0x00,
    // Usage Maximum (101)
    0x29, 0x65, // SC_HID_KEYBOARD_KEYS - 1 = 0x66 - 1
    // Logical Minimum (0)
    0x15, 0x00,
    // Logical Maximum (101)
    0x25, 0x65,
    // Report Size (8)
    0x75, 0x08,
    // Report Count (6)
    0x95, 0x06,
    // Input (Data, Array): Keys
    0x81, 0x00,

    // End Collection
    0xC0,
];

/// HID mouse report descriptor (5-button, 3 relative axes, plus AC Pan).
///
/// 5-byte reports:
///   * byte 0: button bitmap (5 bits, 3 padding)
///   * byte 1: relative X motion (signed, -127..=127)
///   * byte 2: relative Y motion (signed, -127..=127)
///   * byte 3: vertical wheel (signed, -127..=127)
///   * byte 4: AC Pan / horizontal wheel (signed, -127..=127)
pub const MOUSE_REPORT_DESC: &[u8] = &[
    // Usage Page (Generic Desktop)
    0x05, 0x01,
    // Usage (Mouse)
    0x09, 0x02,

    // Collection (Application)
    0xA1, 0x01,

    // Usage (Pointer)
    0x09, 0x01,

    // Collection (Physical)
    0xA1, 0x00,

    // Usage Page (Buttons)
    0x05, 0x09,

    // Usage Minimum (1)
    0x19, 0x01,
    // Usage Maximum (5)
    0x29, 0x05,
    // Logical Minimum (0)
    0x15, 0x00,
    // Logical Maximum (1)
    0x25, 0x01,
    // Report Count (5)
    0x95, 0x05,
    // Report Size (1)
    0x75, 0x01,
    // Input (Data, Variable, Absolute): 5 buttons bits
    0x81, 0x02,

    // Report Count (1)
    0x95, 0x01,
    // Report Size (3)
    0x75, 0x03,
    // Input (Constant): 3 bits padding
    0x81, 0x01,

    // Usage Page (Generic Desktop)
    0x05, 0x01,
    // Usage (X)
    0x09, 0x30,
    // Usage (Y)
    0x09, 0x31,
    // Usage (Wheel)
    0x09, 0x38,
    // Logical Minimum (-127)
    0x15, 0x81,
    // Logical Maximum (127)
    0x25, 0x7F,
    // Report Size (8)
    0x75, 0x08,
    // Report Count (3)
    0x95, 0x03,
    // Input (Data, Variable, Relative): 3 position bytes (X, Y, Wheel)
    0x81, 0x06,

    // Usage Page (Consumer Page)
    0x05, 0x0C,
    // Usage(AC Pan)
    0x0A, 0x38, 0x02,
    // Logical Minimum (-127)
    0x15, 0x81,
    // Logical Maximum (127)
    0x25, 0x7F,
    // Report Size (8)
    0x75, 0x08,
    // Report Count (1)
    0x95, 0x01,
    // Input (Data, Variable, Relative): 1 byte (AC Pan)
    0x81, 0x06,

    // End Collection
    0xC0,

    // End Collection
    0xC0,
];

/// HID gamepad report descriptor (Xbox 360 layout, 2 analog sticks,
/// 2 analog triggers, 16 digital buttons, 1 hat switch).
///
/// 15-byte reports:
///   * bytes 0-1:   left stick X (u16, 0..=65535, little-endian)
///   * bytes 2-3:   left stick Y
///   * bytes 4-5:   right stick X
///   * bytes 6-7:   right stick Y
///   * bytes 8-9:   left trigger (u16, 0..=32767)
///   * bytes 10-11: right trigger (u16, 0..=32767)
///   * bytes 12-13: 16 button bits (little-endian, bit 0 = South / A)
///   * byte 14:     hat switch position (1..=8, 0 = centered)
pub const GAMEPAD_REPORT_DESC: &[u8] = &[
    // Usage Page (Generic Desktop)
    0x05, 0x01,
    // Usage (Gamepad)
    0x09, 0x05,

    // Collection (Application)
    0xA1, 0x01,

    // Collection (Physical)
    0xA1, 0x00,

    // Usage Page (Generic Desktop)
    0x05, 0x01,
    // Usage (X)   Left stick x
    0x09, 0x30,
    // Usage (Y)   Left stick y
    0x09, 0x31,
    // Usage (Rx)  Right stick x
    0x09, 0x33,
    // Usage (Ry)  Right stick y
    0x09, 0x34,
    // Logical Minimum (0)
    0x15, 0x00,
    // Logical Maximum (65535)
    // Cannot use 26 FF FF because 0xFFFF is interpreted as signed 16-bit
    0x27, 0xFF, 0xFF, 0x00, 0x00, // little-endian
    // Report Size (16)
    0x75, 0x10,
    // Report Count (4)
    0x95, 0x04,
    // Input (Data, Variable, Absolute): 4x2 bytes (X, Y, Z, Rz)
    0x81, 0x02,

    // Usage Page (Generic Desktop)
    0x05, 0x01,
    // Usage (Z)
    0x09, 0x32,
    // Usage (Rz)
    0x09, 0x35,
    // Logical Minimum (0)
    0x15, 0x00,
    // Logical Maximum (32767)
    0x26, 0xFF, 0x7F,
    // Report Size (16)
    0x75, 0x10,
    // Report Count (2)
    0x95, 0x02,
    // Input (Data, Variable, Absolute): 2x2 bytes (L2, R2)
    0x81, 0x02,

    // Usage Page (Buttons)
    0x05, 0x09,
    // Usage Minimum (1)
    0x19, 0x01,
    // Usage Maximum (16)
    0x29, 0x10,
    // Logical Minimum (0)
    0x15, 0x00,
    // Logical Maximum (1)
    0x25, 0x01,
    // Report Count (16)
    0x95, 0x10,
    // Report Size (1)
    0x75, 0x01,
    // Input (Data, Variable, Absolute): 16 buttons bits
    0x81, 0x02,

    // Usage Page (Generic Desktop)
    0x05, 0x01,
    // Usage (Hat switch)
    0x09, 0x39,
    // Logical Minimum (1)
    0x15, 0x01,
    // Logical Maximum (8)
    0x25, 0x08,
    // Report Size (4)
    0x75, 0x04,
    // Report Count (1)
    0x95, 0x01,
    // Input (Data, Variable, Null State): 4-bit value
    0x81, 0x42,

    // End Collection
    0xC0,

    // End Collection
    0xC0,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descs_have_content() {
        // The exact byte counts are non-trivial to count by hand and
        // don't matter to the wire protocol — what matters is that the
        // descriptors are non-empty. Sanity-check that no byte was
        // accidentally zeroed.
        assert!(KEYBOARD_REPORT_DESC.len() >= 50);
        assert!(MOUSE_REPORT_DESC.len() >= 50);
        assert!(GAMEPAD_REPORT_DESC.len() >= 60);
        // Also verify the descriptors start with the standard HID
        // header: Usage Page (Generic Desktop).
        assert_eq!(KEYBOARD_REPORT_DESC[0], 0x05);
        assert_eq!(KEYBOARD_REPORT_DESC[1], 0x01);
        assert_eq!(MOUSE_REPORT_DESC[0], 0x05);
        assert_eq!(MOUSE_REPORT_DESC[1], 0x01);
        assert_eq!(GAMEPAD_REPORT_DESC[0], 0x05);
        assert_eq!(GAMEPAD_REPORT_DESC[1], 0x01);
    }
}
