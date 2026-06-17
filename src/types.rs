//! Common enums and constants used across HID device drivers.
//!
//! The numeric values of [`Scancode`], [`MouseButton`], [`GamepadAxis`] and
//! [`GamepadButton`] deliberately match the USB HID Usage Tables (the
//! physical scancodes), so report bytes can be written with a single
//! `as u8` cast.

use crate::error::{Error, Result};

/// Maximum size of a single HID input report (matches `SC_HID_MAX_SIZE` in
/// scrcpy). The gamepad state report is the largest consumer at 15 bytes.
pub const HID_MAX_SIZE: usize = 15;

/// Maximum number of concurrent gamepads (matches `SC_MAX_GAMEPADS` in scrcpy).
pub const MAX_GAMEPADS: usize = 8;

/// Reserved gamepad id meaning "no gamepad bound to this slot" (matches
/// `SC_GAMEPAD_ID_INVALID = 0`).
pub const GAMEPAD_ID_INVALID: u32 = 0;

/// First HID id used for gamepads. Keyboard = 1, mouse = 2, so gamepads
/// start at 3. With 8 slots, valid gamepad ids are 3..=10.
pub const HID_ID_KEYBOARD: u16 = 1;
pub const HID_ID_MOUSE: u16 = 2;
pub const HID_ID_GAMEPAD_FIRST: u16 = 3;
pub const HID_ID_GAMEPAD_LAST: u16 = HID_ID_GAMEPAD_FIRST + MAX_GAMEPADS as u16 - 1;

/// Keyboard scancodes (USB HID Usage IDs, Keyboard/Keypad Page 0x07).
///
/// This covers the standard AT-101 / USB HID keyboard scancode range
/// (0x04..=0x65) plus the modifier keys 0xE0..=0xE7. Values above 0x65 are
/// rejected by [`crate::hid::KeyboardHid`] unless they are modifiers.
#[repr(u16)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Scancode {
    // Letters
    A = 0x04, B = 0x05, C = 0x06, D = 0x07, E = 0x08, F = 0x09, G = 0x0A,
    H = 0x0B, I = 0x0C, J = 0x0D, K = 0x0E, L = 0x0F, M = 0x10, N = 0x11,
    O = 0x12, P = 0x13, Q = 0x14, R = 0x15, S = 0x16, T = 0x17, U = 0x18,
    V = 0x19, W = 0x1A, X = 0x1B, Y = 0x1C, Z = 0x1D,
    // Top row digits
    D1 = 0x1E, D2 = 0x1F, D3 = 0x20, D4 = 0x21, D5 = 0x22, D6 = 0x23,
    D7 = 0x24, D8 = 0x25, D9 = 0x26, D0 = 0x27,
    // Control keys
    Enter      = 0x28,
    Escape     = 0x29,
    Backspace  = 0x2A,
    Tab        = 0x2B,
    Space      = 0x2C,
    Minus      = 0x2D,
    Equals     = 0x2E,
    LeftBrace  = 0x2F,
    RightBrace = 0x30,
    Backslash  = 0x31,
    NonUsHash  = 0x32,
    Semicolon  = 0x33,
    Apostrophe = 0x34,
    Grave      = 0x35,
    Comma      = 0x36,
    Period     = 0x37,
    Slash      = 0x38,
    CapsLock   = 0x39,
    // Function keys
    F1  = 0x3A, F2  = 0x3B, F3  = 0x3C, F4  = 0x3D,
    F5  = 0x3E, F6  = 0x3F, F7  = 0x40, F8  = 0x41,
    F9  = 0x42, F10 = 0x43, F11 = 0x44, F12 = 0x45,
    PrintScreen = 0x46,
    ScrollLock  = 0x47,
    Pause       = 0x48,
    Insert      = 0x49,
    Home        = 0x4A,
    PageUp      = 0x4B,
    Delete      = 0x4C,
    End         = 0x4D,
    PageDown    = 0x4E,
    Right       = 0x4F,
    Left        = 0x50,
    Down        = 0x51,
    Up          = 0x52,
    // Numpad
    NumLockClear = 0x53,
    KpDivide     = 0x54,
    KpMultiply   = 0x55,
    KpMinus      = 0x56,
    KpPlus       = 0x57,
    KpEnter      = 0x58,
    Kp1 = 0x59, Kp2 = 0x5A, Kp3 = 0x5B, Kp4 = 0x5C, Kp5 = 0x5D,
    Kp6 = 0x5E, Kp7 = 0x5F, Kp8 = 0x60, Kp9 = 0x61, Kp0 = 0x62,
    KpPeriod    = 0x63,
    /// Reserved HID usage 0x64 — not named in scrcpy's enum but
    /// constructible via [`Scancode::from_u16_unchecked`] for raw
    /// byte-passthrough use cases.
    #[allow(non_camel_case_types)]
    Reserved64  = 0x64,
    /// Reserved HID usage 0x65 — same role as [`Self::Reserved64`].
    #[allow(non_camel_case_types)]
    Reserved65  = 0x65,
    // Modifiers (USB HID modifier byte bit positions)
    LeftCtrl   = 0xE0,
    LeftShift  = 0xE1,
    LeftAlt    = 0xE2,
    LeftGui    = 0xE3,
    RightCtrl  = 0xE4,
    RightShift = 0xE5,
    RightAlt   = 0xE6,
    RightGui   = 0xE7,
}

impl Scancode {
    /// Raw numeric value as it appears in the HID keyboard report.
    #[inline]
    pub const fn to_u8(self) -> u8 {
        self as u8
    }

    /// True if this scancode is one of the 8 USB HID modifiers
    /// (0xE0..=0xE7).
    #[inline]
    pub fn is_modifier(self) -> bool {
        let v = self as u16;
        (0xE0..=0xE7).contains(&v)
    }

    /// Construct from a raw byte. Returns `None` for unknown scancodes.
    ///
    /// Only the well-known AT-101 + modifier range is enumerated; raw values
    /// outside `Some(known)` can still be passed as a custom byte via
    /// [`Scancode::from_u16_unchecked`].
    #[inline]
    pub fn from_u8(b: u8) -> Option<Self> {
        Some(match b {
            0x04 => Self::A, 0x05 => Self::B, 0x06 => Self::C, 0x07 => Self::D,
            0x08 => Self::E, 0x09 => Self::F, 0x0A => Self::G, 0x0B => Self::H,
            0x0C => Self::I, 0x0D => Self::J, 0x0E => Self::K, 0x0F => Self::L,
            0x10 => Self::M, 0x11 => Self::N, 0x12 => Self::O, 0x13 => Self::P,
            0x14 => Self::Q, 0x15 => Self::R, 0x16 => Self::S, 0x17 => Self::T,
            0x18 => Self::U, 0x19 => Self::V, 0x1A => Self::W, 0x1B => Self::X,
            0x1C => Self::Y, 0x1D => Self::Z,
            0x1E => Self::D1, 0x1F => Self::D2, 0x20 => Self::D3, 0x21 => Self::D4,
            0x22 => Self::D5, 0x23 => Self::D6, 0x24 => Self::D7, 0x25 => Self::D8,
            0x26 => Self::D9, 0x27 => Self::D0,
            0x28 => Self::Enter, 0x29 => Self::Escape, 0x2A => Self::Backspace,
            0x2B => Self::Tab, 0x2C => Self::Space, 0x2D => Self::Minus,
            0x2E => Self::Equals, 0x2F => Self::LeftBrace, 0x30 => Self::RightBrace,
            0x31 => Self::Backslash, 0x32 => Self::NonUsHash, 0x33 => Self::Semicolon,
            0x34 => Self::Apostrophe, 0x35 => Self::Grave, 0x36 => Self::Comma,
            0x37 => Self::Period, 0x38 => Self::Slash, 0x39 => Self::CapsLock,
            0x3A => Self::F1, 0x3B => Self::F2, 0x3C => Self::F3, 0x3D => Self::F4,
            0x3E => Self::F5, 0x3F => Self::F6, 0x40 => Self::F7, 0x41 => Self::F8,
            0x42 => Self::F9, 0x43 => Self::F10, 0x44 => Self::F11, 0x45 => Self::F12,
            0x46 => Self::PrintScreen, 0x47 => Self::ScrollLock, 0x48 => Self::Pause,
            0x49 => Self::Insert, 0x4A => Self::Home, 0x4B => Self::PageUp,
            0x4C => Self::Delete, 0x4D => Self::End, 0x4E => Self::PageDown,
            0x4F => Self::Right, 0x50 => Self::Left, 0x51 => Self::Down, 0x52 => Self::Up,
            0x53 => Self::NumLockClear, 0x54 => Self::KpDivide, 0x55 => Self::KpMultiply,
            0x56 => Self::KpMinus, 0x57 => Self::KpPlus, 0x58 => Self::KpEnter,
            0x59 => Self::Kp1, 0x5A => Self::Kp2, 0x5B => Self::Kp3, 0x5C => Self::Kp4,
            0x5D => Self::Kp5, 0x5E => Self::Kp6, 0x5F => Self::Kp7, 0x60 => Self::Kp8,
            0x61 => Self::Kp9, 0x62 => Self::Kp0, 0x63 => Self::KpPeriod,
            0xE0 => Self::LeftCtrl, 0xE1 => Self::LeftShift, 0xE2 => Self::LeftAlt,
            0xE3 => Self::LeftGui, 0xE4 => Self::RightCtrl, 0xE5 => Self::RightShift,
            0xE6 => Self::RightAlt, 0xE7 => Self::RightGui,
            _ => return None,
        })
    }

    /// Construct from a raw `u16` scancode. The caller is responsible
    /// for ensuring the value is a valid HID usage; values outside
    /// `0x00..=0x65` and `0xE0..=0xE7` may construct but using them as
    /// enum variants is undefined behaviour (Rust's enum validity
    /// rule). To safely hold an arbitrary `u16` for transport, use
    /// the raw byte directly with [`crate::hid::KeyboardHid::set_key`]
    /// after [`crate::types::validate_scancode`].
    #[inline]
    pub const fn from_u16_unchecked(v: u16) -> Self {
        // SAFETY: caller asserts `v` is one of the legal scancode
        // variants (0..=0x65 or 0xE0..=0xE7). `Scancode` is `repr(u16)`
        // so the bit pattern matches.
        unsafe { std::mem::transmute::<u16, Scancode>(v) }
    }

    /// Simple ASCII char → scancode mapping used by
    /// [`crate::session::HidSession::type_text`]. Returns `None` for chars
    /// outside the supported set. The required modifier (e.g. `LSHIFT` for
    /// uppercase letters and shifted symbols) is written into `*mods`,
    /// which is reset to `Modifiers::empty()` on every call.
    pub fn try_from_char(c: char, mods: &mut Modifiers) -> Option<Self> {
        *mods = Modifiers::empty();
        match c {
            'a'..='z' => Scancode::from_u8((c as u8) - b'a' + 0x04),
            'A'..='Z' => {
                *mods = Modifiers::LSHIFT;
                Scancode::from_u8((c as u8) - b'A' + 0x04)
            }
            '1'..='9' => Scancode::from_u8((c as u8) - b'1' + 0x1E),
            '0' => Some(Scancode::D0),
            ' '  => Some(Scancode::Space),
            '\n' => Some(Scancode::Enter),
            '\t' => Some(Scancode::Tab),
            '\u{0008}' => Some(Scancode::Backspace),
            '\u{001b}' => Some(Scancode::Escape),
            '!' => { *mods = Modifiers::LSHIFT; Some(Scancode::D1) }
            '@' => { *mods = Modifiers::LSHIFT; Some(Scancode::D2) }
            '#' => { *mods = Modifiers::LSHIFT; Some(Scancode::D3) }
            '$' => { *mods = Modifiers::LSHIFT; Some(Scancode::D4) }
            '%' => { *mods = Modifiers::LSHIFT; Some(Scancode::D5) }
            '^' => { *mods = Modifiers::LSHIFT; Some(Scancode::D6) }
            '&' => { *mods = Modifiers::LSHIFT; Some(Scancode::D7) }
            '*' => { *mods = Modifiers::LSHIFT; Some(Scancode::D8) }
            '(' => { *mods = Modifiers::LSHIFT; Some(Scancode::D9) }
            ')' => { *mods = Modifiers::LSHIFT; Some(Scancode::D0) }
            '-' => Some(Scancode::Minus),
            '_' => { *mods = Modifiers::LSHIFT; Some(Scancode::Minus) }
            '=' => Some(Scancode::Equals),
            '+' => { *mods = Modifiers::LSHIFT; Some(Scancode::Equals) }
            '[' => Some(Scancode::LeftBrace),
            '{' => { *mods = Modifiers::LSHIFT; Some(Scancode::LeftBrace) }
            ']' => Some(Scancode::RightBrace),
            '}' => { *mods = Modifiers::LSHIFT; Some(Scancode::RightBrace) }
            '\\' => Some(Scancode::Backslash),
            '|' => { *mods = Modifiers::LSHIFT; Some(Scancode::Backslash) }
            ';' => Some(Scancode::Semicolon),
            ':' => { *mods = Modifiers::LSHIFT; Some(Scancode::Semicolon) }
            '\'' => Some(Scancode::Apostrophe),
            '"' => { *mods = Modifiers::LSHIFT; Some(Scancode::Apostrophe) }
            '`' => Some(Scancode::Grave),
            '~' => { *mods = Modifiers::LSHIFT; Some(Scancode::Grave) }
            ',' => Some(Scancode::Comma),
            '<' => { *mods = Modifiers::LSHIFT; Some(Scancode::Comma) }
            '.' => Some(Scancode::Period),
            '>' => { *mods = Modifiers::LSHIFT; Some(Scancode::Period) }
            '/' => Some(Scancode::Slash),
            '?' => { *mods = Modifiers::LSHIFT; Some(Scancode::Slash) }
            _ => None,
        }
    }
}

/// Bitmask of the 8 USB HID modifier keys (byte 0 of the keyboard report).
///
/// Values match the scrcpy `SC_MOD_*` constants. The 5 high bits are
/// reserved for non-modifier flags (NumLock / CapsLock sync), and are NOT
/// part of the byte sent to the device.
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Modifiers(pub u8);

impl Modifiers {
    pub const NONE: Self = Self(0);
    pub const LCTRL:  Self = Self(1 << 0);
    pub const LSHIFT: Self = Self(1 << 1);
    pub const LALT:   Self = Self(1 << 2);
    pub const LGUI:   Self = Self(1 << 3);
    pub const RCTRL:  Self = Self(1 << 4);
    pub const RSHIFT: Self = Self(1 << 5);
    pub const RALT:   Self = Self(1 << 6);
    pub const RGUI:   Self = Self(1 << 7);

    #[inline]
    pub const fn empty() -> Self { Self(0) }

    #[inline]
    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }

    #[inline]
    pub const fn bits(self) -> u8 { self.0 }
}

impl std::ops::BitOr for Modifiers {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self { Self(self.0 | rhs.0) }
}

impl std::ops::BitOrAssign for Modifiers {
    fn bitor_assign(&mut self, rhs: Self) { self.0 |= rhs.0; }
}

impl std::ops::BitAnd for Modifiers {
    type Output = Self;
    fn bitand(self, rhs: Self) -> Self { Self(self.0 & rhs.0) }
}

impl std::ops::BitXor for Modifiers {
    type Output = Self;
    fn bitxor(self, rhs: Self) -> Self { Self(self.0 ^ rhs.0) }
}

/// Mouse buttons. Values match the bit positions in byte 0 of the mouse
/// HID report.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MouseButton {
    Left   = 1 << 0,
    Right  = 1 << 1,
    Middle = 1 << 2,
    X1     = 1 << 3,
    X2     = 1 << 4,
}

impl MouseButton {
    /// Compose a button-state byte from a slice of buttons.
    pub fn state(buttons: &[MouseButton]) -> u8 {
        let mut s = 0u8;
        for b in buttons {
            s |= *b as u8;
        }
        s
    }
}

/// Gamepad axis. Values match SDL's `SDL_GamepadAxis` enumeration so
/// users can pass-through SDL events.
#[repr(i16)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GamepadAxis {
    LeftX        = 0,
    LeftY        = 1,
    RightX       = 2,
    RightY       = 3,
    LeftTrigger  = 4,
    RightTrigger = 5,
}

/// Bit position of each gamepad button in the 16-bit button field
/// (bytes 12-13 of the gamepad HID report). Matches scrcpy's
/// `sc_hid_gamepad_get_button_id` mapping.
///
/// The dpad variants are encoded in the upper bits so that the dpad
/// state can be transformed into a 4-bit hat-switch value separately
/// from the 16 normal button bits.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GamepadButton {
    South         = 0x0001, // A on Xbox
    East          = 0x0002, // B on Xbox
    West          = 0x0008, // X on Xbox
    North         = 0x0010, // Y on Xbox
    Back          = 0x0400,
    Start         = 0x0800,
    Guide         = 0x1000,
    LeftStick     = 0x2000,
    RightStick    = 0x4000,
    LeftShoulder  = 0x0040,
    RightShoulder = 0x0080,
    DpadUp        = 0x0001_0000, // local-only flag, transformed to hat switch
    DpadDown      = 0x0002_0000,
    DpadLeft      = 0x0004_0000,
    DpadRight     = 0x0008_0000,
}

const DPAD_MASK: u32 =
    GamepadButton::DpadUp as u32 |
    GamepadButton::DpadDown as u32 |
    GamepadButton::DpadLeft as u32 |
    GamepadButton::DpadRight as u32;

/// Convert a 32-bit gamepad button bitmap to the 4-bit hat-switch value
/// (0..=8) used in byte 14 of the gamepad HID report.
///
/// 0 means "no direction"; 1..=8 are arranged as
///
/// ```text
///     8 1 2
///     7 0 3
///     6 5 4
/// ```
pub fn dpad_hat_value(buttons: u32) -> u8 {
    let dpad = buttons & DPAD_MASK;
    let up = dpad & GamepadButton::DpadUp as u32 != 0;
    let down = dpad & GamepadButton::DpadDown as u32 != 0;
    let left = dpad & GamepadButton::DpadLeft as u32 != 0;
    let right = dpad & GamepadButton::DpadRight as u32 != 0;
    if up && left  { return 8; }
    if up && right { return 2; }
    if up          { return 1; }
    if down && left  { return 6; }
    if down && right { return 4; }
    if down          { return 5; }
    if left  { return 7; }
    if right { return 3; }
    0
}

/// Validate a scancode and return its raw byte.
///
/// Returns `Err(ScancodeOutOfRange)` for raw values above the AT-101 limit
/// (0x65) that are not in the modifier range (0xE0..=0xE7).
pub fn validate_scancode(scancode: u16) -> Result<u8> {
    if scancode > 0xFF {
        return Err(Error::ScancodeOutOfRange(scancode));
    }
    let b = scancode as u8;
    if b > 0x65 && !(0xE0..=0xE7).contains(&b) {
        return Err(Error::ScancodeOutOfRange(scancode));
    }
    Ok(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modifier_bits_match_scrcpy() {
        // Sanity check: matches SC_HID_MOD_* in scrcpy/app/src/hid/hid_keyboard.c
        assert_eq!(Modifiers::LCTRL.bits(),  0x01);
        assert_eq!(Modifiers::LSHIFT.bits(), 0x02);
        assert_eq!(Modifiers::LALT.bits(),   0x04);
        assert_eq!(Modifiers::LGUI.bits(),   0x08);
        assert_eq!(Modifiers::RCTRL.bits(),  0x10);
        assert_eq!(Modifiers::RSHIFT.bits(), 0x20);
        assert_eq!(Modifiers::RALT.bits(),   0x40);
        assert_eq!(Modifiers::RGUI.bits(),   0x80);
    }

    #[test]
    fn mouse_buttons_pack_into_byte() {
        let s = MouseButton::state(&[MouseButton::Left, MouseButton::X2]);
        assert_eq!(s, 0b00010001);
    }

    #[test]
    fn dpad_table() {
        assert_eq!(dpad_hat_value(0), 0);
        assert_eq!(dpad_hat_value(GamepadButton::DpadUp as u32), 1);
        assert_eq!(dpad_hat_value(GamepadButton::DpadRight as u32), 3);
        assert_eq!(dpad_hat_value(
            GamepadButton::DpadUp as u32 | GamepadButton::DpadLeft as u32), 8);
        assert_eq!(dpad_hat_value(
            GamepadButton::DpadDown as u32 | GamepadButton::DpadRight as u32), 4);
    }

    #[test]
    fn scancode_roundtrip() {
        for s in [Scancode::A, Scancode::Z, Scancode::F12, Scancode::LeftShift] {
            assert_eq!(Scancode::from_u8(s.to_u8()), Some(s));
        }
    }

    #[test]
    fn validate_rejects_out_of_range() {
        assert!(validate_scancode(0x66).is_err());
        assert!(validate_scancode(0xDF).is_err());
        assert!(validate_scancode(0xE0).is_ok()); // modifier OK
        assert!(validate_scancode(0xE7).is_ok());
        assert!(validate_scancode(0xFF).is_err());
        assert!(validate_scancode(0x100).is_err());
    }

    #[test]
    fn scancode_unchecked_boundaries() {
        // AC-9: pin the unsafe `from_u16_unchecked` path so the
        // bit-pattern invariant is locked down. Only test values that
        // correspond to a real `Scancode` variant; constructing an
        // invalid discriminant via transmute is UB and trips Rust's
        // enum-validity runtime check in debug builds.
        assert_eq!(Scancode::from_u16_unchecked(0x0004), Scancode::A);
        assert_eq!(Scancode::from_u16_unchecked(0x001D), Scancode::Z);
        assert_eq!(Scancode::from_u16_unchecked(0x001E), Scancode::D1);
        assert_eq!(Scancode::from_u16_unchecked(0x0027), Scancode::D0);
        assert_eq!(Scancode::from_u16_unchecked(0x002C), Scancode::Space);
        assert_eq!(Scancode::from_u16_unchecked(0x003A), Scancode::F1);
        assert_eq!(Scancode::from_u16_unchecked(0x0045), Scancode::F12);
        assert_eq!(Scancode::from_u16_unchecked(0x0063), Scancode::KpPeriod);
        assert_eq!(Scancode::from_u16_unchecked(0x0064), Scancode::Reserved64);
        assert_eq!(Scancode::from_u16_unchecked(0x0065), Scancode::Reserved65);
        assert_eq!(Scancode::from_u16_unchecked(0x00E0), Scancode::LeftCtrl);
        assert_eq!(Scancode::from_u16_unchecked(0x00E1), Scancode::LeftShift);
        assert_eq!(Scancode::from_u16_unchecked(0x00E7), Scancode::RightGui);
        // Round-trip through to_u8 for the legal 0..=0x65 + 0xE0..=0xE7 ranges.
        for raw in [0x04u16, 0x1E, 0x39, 0x4F, 0x65, 0xE0, 0xE7] {
            let s = Scancode::from_u16_unchecked(raw);
            assert_eq!(s as u16, raw);
            assert_eq!(s.to_u8() as u16, raw);
        }
    }

    #[test]
    fn try_from_char_basic() {
        let mut m = Modifiers::empty();
        assert_eq!(Scancode::try_from_char('a', &mut m), Some(Scancode::A));
        assert_eq!(m, Modifiers::empty());
        assert_eq!(Scancode::try_from_char('A', &mut m), Some(Scancode::A));
        assert_eq!(m, Modifiers::LSHIFT);
        assert_eq!(Scancode::try_from_char('5', &mut m), Some(Scancode::D5));
        assert_eq!(m, Modifiers::empty());
        assert_eq!(Scancode::try_from_char(' ', &mut m), Some(Scancode::Space));
        assert_eq!(Scancode::try_from_char('\n', &mut m), Some(Scancode::Enter));
        assert_eq!(Scancode::try_from_char('!', &mut m), Some(Scancode::D1));
        assert_eq!(m, Modifiers::LSHIFT);
        // Unsupported char
        assert_eq!(Scancode::try_from_char('\u{4e2d}', &mut m), None);
    }
}
