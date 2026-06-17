//! Control message types and serialization to the scrcpy wire format.
//!
//! The on-wire format is documented in scrcpy's
//! `app/src/control_msg.c` (`sc_control_msg_serialize`). The byte layout
//! for the three UHID messages is:
//!
//! ```text
//! UHID_CREATE (type = 12)
//!   u8  type = 12
//!   u16 id                  (big-endian)
//!   u16 vendor_id           (big-endian)
//!   u16 product_id          (big-endian)
//!   u8  name_len            (1 byte length prefix; max 127)
//!   [name bytes]
//!   u16 report_desc_size    (big-endian)
//!   [report_desc bytes]
//!
//! UHID_INPUT (type = 13)
//!   u8  type = 13
//!   u16 id                  (big-endian)
//!   u16 size                (big-endian, max 15)
//!   [data bytes]
//!
//! UHID_DESTROY (type = 14)
//!   u8  type = 14
//!   u16 id                  (big-endian)
//! ```
//!
//! We also expose the other control message types from scrcpy for
//! completeness, so the same connection can be used to issue touch /
//! clipboard / set-display-power / etc. instructions alongside UHID.

use crate::error::{Error, Result};
use crate::types::HID_MAX_SIZE;

/// Hard cap on a single serialized control message (matches
/// `SC_CONTROL_MSG_MAX_SIZE` = 256 KiB on the scrcpy side).
pub const CONTROL_MSG_MAX_SIZE: usize = 1 << 18;

/// Maximum length of an injected text payload.
pub const INJECT_TEXT_MAX_LENGTH: usize = 300;

/// scrcpy `sc_control_msg_type` enum values. Only the UHID-related and
/// the most common non-UHID types are listed; everything else can be
/// added as needed.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ControlMsgType {
    InjectKeycode = 0,
    InjectText = 1,
    InjectTouchEvent = 2,
    InjectScrollEvent = 3,
    BackOrScreenOn = 4,
    ExpandNotification = 5,
    ExpandSettings = 6,
    CollapsePanels = 7,
    GetClipboard = 8,
    SetClipboard = 9,
    SetDisplayPower = 10,
    RotateDevice = 11,
    UhidCreate = 12,
    UhidInput = 13,
    UhidDestroy = 14,
    OpenHardKbSettings = 15,
    StartApp = 16,
    ResetVideo = 17,
    CameraSetTorch = 18,
    CameraZoomIn = 19,
    CameraZoomOut = 20,
    ResizeDisplay = 21,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UhidCreate {
    pub id: u16,
    pub vendor_id: u16,
    pub product_id: u16,
    pub name: Option<String>,
    pub report_desc: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UhidInput {
    pub id: u16,
    pub size: u16,
    pub data: [u8; HID_MAX_SIZE],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UhidDestroy {
    pub id: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InjectKeycode {
    pub action: u8,   // AKEY_EVENT_ACTION_DOWN / UP
    pub keycode: u32, // AKEYCODE_*
    pub repeat: u32,
    pub metastate: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InjectText {
    pub text: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct InjectTouchEvent {
    pub action: u8,
    pub pointer_id: u64,
    pub x: i32,
    pub y: i32,
    pub screen_w: u16,
    pub screen_h: u16,
    pub pressure: f32,
    pub action_button: u32,
    pub buttons: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct InjectScrollEvent {
    pub x: i32,
    pub y: i32,
    pub screen_w: u16,
    pub screen_h: u16,
    pub hscroll: f32,
    pub vscroll: f32,
    pub buttons: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackOrScreenOn {
    pub action: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GetClipboard {
    pub copy_key: u8, // 0=none, 1=copy, 2=cut
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetClipboard {
    pub sequence: u64,
    pub paste: bool,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetDisplayPower {
    pub on: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartApp {
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CameraSetTorch {
    pub on: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResizeDisplay {
    pub width: u16,
    pub height: u16,
}

/// The top-level control message enum.
#[derive(Debug, Clone, PartialEq)]
pub enum ControlMessage {
    UhidCreate(UhidCreate),
    UhidInput(UhidInput),
    UhidDestroy(UhidDestroy),
    InjectKeycode(InjectKeycode),
    InjectText(InjectText),
    InjectTouchEvent(InjectTouchEvent),
    InjectScrollEvent(InjectScrollEvent),
    BackOrScreenOn(BackOrScreenOn),
    GetClipboard(GetClipboard),
    SetClipboard(SetClipboard),
    SetDisplayPower(SetDisplayPower),
    StartApp(StartApp),
    CameraSetTorch(CameraSetTorch),
    ResizeDisplay(ResizeDisplay),
    // Tag-only (no payload) messages
    ExpandNotificationPanel,
    ExpandSettingsPanel,
    CollapsePanels,
    RotateDevice,
    OpenHardKeyboardSettings,
    ResetVideo,
    CameraZoomIn,
    CameraZoomOut,
}

impl ControlMessage {
    /// Type tag of this message.
    pub fn msg_type(&self) -> ControlMsgType {
        match self {
            Self::InjectKeycode(_) => ControlMsgType::InjectKeycode,
            Self::InjectText(_) => ControlMsgType::InjectText,
            Self::InjectTouchEvent(_) => ControlMsgType::InjectTouchEvent,
            Self::InjectScrollEvent(_) => ControlMsgType::InjectScrollEvent,
            Self::BackOrScreenOn(_) => ControlMsgType::BackOrScreenOn,
            Self::ExpandNotificationPanel => ControlMsgType::ExpandNotification,
            Self::ExpandSettingsPanel => ControlMsgType::ExpandSettings,
            Self::CollapsePanels => ControlMsgType::CollapsePanels,
            Self::GetClipboard(_) => ControlMsgType::GetClipboard,
            Self::SetClipboard(_) => ControlMsgType::SetClipboard,
            Self::SetDisplayPower(_) => ControlMsgType::SetDisplayPower,
            Self::RotateDevice => ControlMsgType::RotateDevice,
            Self::UhidCreate(_) => ControlMsgType::UhidCreate,
            Self::UhidInput(_) => ControlMsgType::UhidInput,
            Self::UhidDestroy(_) => ControlMsgType::UhidDestroy,
            Self::OpenHardKeyboardSettings => ControlMsgType::OpenHardKbSettings,
            Self::StartApp(_) => ControlMsgType::StartApp,
            Self::ResetVideo => ControlMsgType::ResetVideo,
            Self::CameraSetTorch(_) => ControlMsgType::CameraSetTorch,
            Self::CameraZoomIn => ControlMsgType::CameraZoomIn,
            Self::CameraZoomOut => ControlMsgType::CameraZoomOut,
            Self::ResizeDisplay(_) => ControlMsgType::ResizeDisplay,
        }
    }

    /// Whether this message is one of the two non-droppable messages per
    /// scrcpy's `sc_control_msg_is_droppable` (`UHID_CREATE`,
    /// `UHID_DESTROY`).
    pub fn is_critical(&self) -> bool {
        matches!(self, Self::UhidCreate(_) | Self::UhidDestroy(_))
    }

    /// Serialize this message into a freshly allocated `Vec<u8>`.
    pub fn serialize(&self) -> Result<Vec<u8>> {
        let mut buf = Vec::with_capacity(64);
        self.serialize_into(&mut buf)?;
        Ok(buf)
    }

    /// Serialize into a caller-provided buffer. Returns the number of
    /// bytes written.
    pub fn serialize_into(&self, buf: &mut Vec<u8>) -> Result<usize> {
        let start = buf.len();
        buf.push(self.msg_type() as u8);
        match self {
            Self::UhidCreate(c) => serialize_uhid_create(c, buf)?,
            Self::UhidInput(i) => serialize_uhid_input(i, buf)?,
            Self::UhidDestroy(d) => serialize_uhid_destroy(d, buf),
            Self::InjectKeycode(k) => serialize_inject_keycode(k, buf),
            Self::InjectText(t) => serialize_inject_text(t, buf),
            Self::InjectTouchEvent(t) => serialize_inject_touch(t, buf),
            Self::InjectScrollEvent(s) => serialize_inject_scroll(s, buf),
            Self::BackOrScreenOn(b) => {
                buf.push(b.action);
            }
            Self::GetClipboard(g) => {
                buf.push(g.copy_key);
            }
            Self::SetClipboard(s) => serialize_set_clipboard(s, buf),
            Self::SetDisplayPower(s) => {
                buf.push(s.on as u8);
            }
            Self::StartApp(s) => serialize_start_app(s, buf)?,
            Self::CameraSetTorch(t) => {
                buf.push(t.on as u8);
            }
            Self::ResizeDisplay(r) => {
                buf.extend_from_slice(&r.width.to_be_bytes());
                buf.extend_from_slice(&r.height.to_be_bytes());
            }
            Self::ExpandNotificationPanel
            | Self::ExpandSettingsPanel
            | Self::CollapsePanels
            | Self::RotateDevice
            | Self::OpenHardKeyboardSettings
            | Self::ResetVideo
            | Self::CameraZoomIn
            | Self::CameraZoomOut => { /* no payload */ }
        }
        let written = buf.len() - start;
        if written > CONTROL_MSG_MAX_SIZE {
            return Err(Error::ControlMessageTooLarge {
                size: written,
                max: CONTROL_MSG_MAX_SIZE,
            });
        }
        Ok(written)
    }
}

fn write_u16_be(buf: &mut Vec<u8>, v: u16) {
    buf.extend_from_slice(&v.to_be_bytes());
}
fn write_u32_be(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_be_bytes());
}
fn write_u64_be(buf: &mut Vec<u8>, v: u64) {
    buf.extend_from_slice(&v.to_be_bytes());
}

fn write_tiny_string(buf: &mut Vec<u8>, s: &str, max_len: usize) -> Result<()> {
    assert!(max_len <= 0xFF);
    let bytes = s.as_bytes();
    if bytes.len() > max_len {
        return Err(Error::NameTooLong { size: bytes.len() });
    }
    buf.push(bytes.len() as u8);
    buf.extend_from_slice(bytes);
    Ok(())
}

fn write_string(buf: &mut Vec<u8>, s: &str) {
    let bytes = s.as_bytes();
    buf.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    buf.extend_from_slice(bytes);
}

fn write_position(buf: &mut Vec<u8>, x: i32, y: i32, w: u16, h: u16) {
    write_u32_be(buf, x as u32);
    write_u32_be(buf, y as u32);
    write_u16_be(buf, w);
    write_u16_be(buf, h);
}

fn serialize_uhid_create(c: &UhidCreate, buf: &mut Vec<u8>) -> Result<()> {
    write_u16_be(buf, c.id);
    write_u16_be(buf, c.vendor_id);
    write_u16_be(buf, c.product_id);
    match &c.name {
        Some(n) => write_tiny_string(buf, n, 127)?,
        None => {
            buf.push(0);
        }
    }
    if c.report_desc.len() > u16::MAX as usize {
        return Err(Error::ReportDescTooLong {
            size: c.report_desc.len(),
        });
    }
    write_u16_be(buf, c.report_desc.len() as u16);
    buf.extend_from_slice(&c.report_desc);
    Ok(())
}

fn serialize_uhid_input(i: &UhidInput, buf: &mut Vec<u8>) -> Result<()> {
    write_u16_be(buf, i.id);
    if i.size as usize > HID_MAX_SIZE {
        return Err(Error::ControlMessageTooLarge {
            size: i.size as usize,
            max: HID_MSG_BUDGET,
        });
    }
    write_u16_be(buf, i.size);
    buf.extend_from_slice(&i.data[..i.size as usize]);
    Ok(())
}

fn serialize_uhid_destroy(d: &UhidDestroy, buf: &mut Vec<u8>) {
    write_u16_be(buf, d.id);
}

fn serialize_inject_keycode(k: &InjectKeycode, buf: &mut Vec<u8>) {
    buf.push(k.action);
    write_u32_be(buf, k.keycode);
    write_u32_be(buf, k.repeat);
    write_u32_be(buf, k.metastate);
}

fn serialize_inject_text(t: &InjectText, buf: &mut Vec<u8>) {
    let truncated = if t.text.len() > INJECT_TEXT_MAX_LENGTH {
        &t.text[..INJECT_TEXT_MAX_LENGTH]
    } else {
        &t.text[..]
    };
    write_string(buf, truncated);
}

fn serialize_inject_touch(t: &InjectTouchEvent, buf: &mut Vec<u8>) {
    buf.push(t.action);
    write_u64_be(buf, t.pointer_id);
    write_position(buf, t.x, t.y, t.screen_w, t.screen_h);
    let pressure = (t.pressure.clamp(0.0, 1.0) * 65536.0) as u16;
    write_u16_be(buf, pressure);
    write_u32_be(buf, t.action_button);
    write_u32_be(buf, t.buttons);
}

fn serialize_inject_scroll(s: &InjectScrollEvent, buf: &mut Vec<u8>) {
    write_position(buf, s.x, s.y, s.screen_w, s.screen_h);
    let hscroll_norm = (s.hscroll / 16.0).clamp(-1.0, 1.0);
    let vscroll_norm = (s.vscroll / 16.0).clamp(-1.0, 1.0);
    let hscroll = (hscroll_norm * 32768.0) as i16 as u16;
    let vscroll = (vscroll_norm * 32768.0) as i16 as u16;
    write_u16_be(buf, hscroll);
    write_u16_be(buf, vscroll);
    write_u32_be(buf, s.buttons);
}

fn serialize_set_clipboard(s: &SetClipboard, buf: &mut Vec<u8>) {
    write_u64_be(buf, s.sequence);
    buf.push(s.paste as u8);
    let max = CONTROL_MSG_MAX_SIZE - 14;
    let truncated = if s.text.len() > max {
        &s.text[..max]
    } else {
        &s.text[..]
    };
    write_string(buf, truncated);
}

fn serialize_start_app(s: &StartApp, buf: &mut Vec<u8>) -> Result<()> {
    write_tiny_string(buf, &s.name, 255)
}

/// Used in `serialize_uhid_input` to give a clearer error message.
const HID_MSG_BUDGET: usize = 5 + HID_MAX_SIZE;

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(msg: ControlMessage) -> Vec<u8> {
        let v = msg.serialize().unwrap();
        // Spot check: first byte is the type tag.
        assert_eq!(v[0], msg.msg_type() as u8);
        v
    }

    #[test]
    fn uhid_create_serialize_layout() {
        let msg = ControlMessage::UhidCreate(UhidCreate {
            id: 1,
            vendor_id: 0x045e,
            product_id: 0x028e,
            name: Some("Pad".to_string()),
            report_desc: vec![0x05, 0x01, 0x09, 0x06],
        });
        let v = msg.serialize().unwrap();
        // type(1) | id(2) | vid(2) | pid(2) | name_len(1) | name(3) | rd_size(2) | rd(4) = 17
        assert_eq!(v.len(), 17);
        assert_eq!(v[0], 12);
        assert_eq!(&v[1..3], &[0x00, 0x01]);
        assert_eq!(&v[3..5], &[0x04, 0x5e]);
        assert_eq!(&v[5..7], &[0x02, 0x8e]);
        assert_eq!(v[7], 3);
        assert_eq!(&v[8..11], b"Pad");
        assert_eq!(&v[11..13], &[0x00, 0x04]);
        assert_eq!(&v[13..17], &[0x05, 0x01, 0x09, 0x06]);
    }

    #[test]
    fn uhid_input_serialize_layout() {
        let mut data = [0u8; HID_MAX_SIZE];
        data[0] = 0x02;
        data[1] = 0x00;
        data[2] = 0x04; // LSHIFT + A
        let msg = ControlMessage::UhidInput(UhidInput {
            id: 1,
            size: 8,
            data,
        });
        let v = msg.serialize().unwrap();
        // type(1) | id(2) | size(2) | data(8) = 13
        assert_eq!(v.len(), 13);
        assert_eq!(v[0], 13);
        assert_eq!(&v[1..3], &[0x00, 0x01]);
        assert_eq!(&v[3..5], &[0x00, 0x08]);
        assert_eq!(&v[5..13], &[0x02, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00]);
    }

    #[test]
    fn uhid_destroy_serialize_layout() {
        let msg = ControlMessage::UhidDestroy(UhidDestroy { id: 3 });
        let v = msg.serialize().unwrap();
        assert_eq!(v, vec![14, 0x00, 0x03]);
    }

    #[test]
    fn critical_flag_matches_scrcpy() {
        assert!(ControlMessage::UhidCreate(UhidCreate {
            id: 1,
            vendor_id: 0,
            product_id: 0,
            name: None,
            report_desc: vec![],
        })
        .is_critical());
        assert!(ControlMessage::UhidDestroy(UhidDestroy { id: 1 }).is_critical());
        assert!(!ControlMessage::UhidInput(UhidInput {
            id: 1,
            size: 0,
            data: [0; HID_MAX_SIZE],
        })
        .is_critical());
    }

    #[test]
    fn name_too_long_rejected() {
        let s = "a".repeat(128);
        let r = ControlMessage::UhidCreate(UhidCreate {
            id: 1,
            vendor_id: 0,
            product_id: 0,
            name: Some(s),
            report_desc: vec![],
        })
        .serialize();
        assert!(matches!(r, Err(Error::NameTooLong { size: 128 })));
    }

    #[test]
    fn tag_only_messages_serialize_to_one_byte() {
        for msg in [
            ControlMessage::ExpandNotificationPanel,
            ControlMessage::ExpandSettingsPanel,
            ControlMessage::CollapsePanels,
            ControlMessage::RotateDevice,
            ControlMessage::OpenHardKeyboardSettings,
            ControlMessage::ResetVideo,
            ControlMessage::CameraZoomIn,
            ControlMessage::CameraZoomOut,
        ] {
            let v = msg.serialize().unwrap();
            assert_eq!(v.len(), 1, "{:?} should be 1 byte", msg);
        }
    }

    #[test]
    fn inject_keycode_layout() {
        let msg = ControlMessage::InjectKeycode(InjectKeycode {
            action: 0,
            keycode: 29,
            repeat: 0,
            metastate: 0,
        });
        let v = msg.serialize().unwrap();
        // type(1) + action(1) + keycode(4) + repeat(4) + metastate(4) = 14
        assert_eq!(v.len(), 14);
        assert_eq!(v[0], 0);
        assert_eq!(v[1], 0);
        assert_eq!(&v[2..6], &29u32.to_be_bytes());
    }

    #[test]
    fn inject_scroll_normalises_clamped() {
        // hscroll=200 (raw 200/16 = 12.5 → clamp to 1.0 → 0x7FFF)
        let msg = ControlMessage::InjectScrollEvent(InjectScrollEvent {
            x: 0,
            y: 0,
            screen_w: 1080,
            screen_h: 1920,
            hscroll: 200.0,
            vscroll: 0.0,
            buttons: 0,
        });
        let v = msg.serialize().unwrap();
        // type(1) + position(12) + hscroll(2) + vscroll(2) + buttons(4) = 21
        assert_eq!(v.len(), 21);
        assert_eq!(&v[13..15], &0x7FFFu16.to_be_bytes());
    }

    #[test]
    fn roundtrip_helper_works() {
        let _ = roundtrip(ControlMessage::UhidDestroy(UhidDestroy { id: 1 }));
    }
}
