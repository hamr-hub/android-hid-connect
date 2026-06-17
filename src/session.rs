//! High-level `HidSession` facade for AI / agent control of an Android
//! device via UHID.
//!
//! `HidSession` opens a keyboard and/or gamepad in one call, exposes
//! intent-style helpers (`type_text`, `tap`, `swipe`, `set_stick`,
//! `set_button`), and guarantees that all `UHID_CREATE` messages are
//! paired with `UHID_DESTROY` on drop — even if the caller panics.
//!
//! **Note**: scrcpy's UHID mouse is a *relative* device. For absolute
//! screen-coordinate taps / swipes the session uses scrcpy's
//! `INJECT_TOUCH_EVENT` message, which does not require a UHID device
//! to be open. Pass [`OpenRequest::mouse`] only if you want to drive
//! the relative UHID mouse yourself via [`HidSession::mouse`].

use std::time::Duration;

use crate::coalesce::CoalescingWriter;
use crate::control::message::{ControlMessage, InjectTouchEvent};
use crate::error::{Error, Result, TransportWrite};
use crate::hid::gamepad::GamepadHid;
use crate::hid::keyboard::KeyboardHid;
use crate::hid::mouse::MouseHid;
use crate::hid::HidDevice;
use crate::types::{GamepadAxis, GamepadButton, Modifiers, Scancode, HID_ID_GAMEPAD_FIRST};

/// Which HID devices the session should open. Touch events are always
/// available (no UHID device needed) — they ride on the same control
/// socket. Use [`OpenRequest::all`], [`OpenRequest::none`], or any
/// combination of `kbd`, `mouse`, `gamepad`.
///
/// `coalesce` (default `true`) wraps the transport in a
/// [`CoalescingWriter`] so that bursty `UhidInput` traffic (e.g. 1 kHz
/// gamepad stick jitter) is batched into a single `write_all` per
/// 1 ms window. Set to `false` for power users who want every message
/// to hit the wire immediately.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct OpenRequest {
    pub kbd: bool,
    pub mouse: bool,
    pub gamepad: bool,
    pub coalesce: bool,
}

impl OpenRequest {
    pub const fn none() -> Self {
        Self {
            kbd: false,
            mouse: false,
            gamepad: false,
            coalesce: true,
        }
    }
    pub const fn all() -> Self {
        Self {
            kbd: true,
            mouse: true,
            gamepad: true,
            coalesce: true,
        }
    }
    pub const fn kbd_only() -> Self {
        Self {
            kbd: true,
            mouse: false,
            gamepad: false,
            coalesce: true,
        }
    }
    pub const fn mouse_only() -> Self {
        Self {
            kbd: false,
            mouse: true,
            gamepad: false,
            coalesce: true,
        }
    }
    pub const fn gamepad_only() -> Self {
        Self {
            kbd: false,
            mouse: false,
            gamepad: true,
            coalesce: true,
        }
    }
}

/// Owned UHID session: opens kbd/mouse/gamepad together and tracks
/// lifetime. `T` is the underlying transport (e.g.
/// `std::net::TcpStream`, `Vec<u8>`, [`crate::transport::MockTransport`]).
/// The session takes ownership and returns the transport from
/// [`HidSession::close`].
///
/// When `OpenRequest::coalesce` is `true` (the default), the transport
/// is wrapped in a [`CoalescingWriter`] that batches `UhidInput` writes
/// within a 1 ms window. Critical messages (`UhidCreate` / `UhidDestroy`)
/// bypass the buffer.
#[derive(Debug)]
pub struct HidSession<T: TransportWrite> {
    transport: CoalescingWriter<T>,
    kbd: Option<KeyboardHid>,
    mouse: Option<MouseHid>,
    gamepad: Option<GamepadHid>,
    closed: bool,
    /// Screen dimensions, used to populate `INJECT_TOUCH_EVENT` payloads.
    screen_w: u16,
    screen_h: u16,
}

// `HidSession` is `Send` whenever the transport is, which is the
// property AI agent runtimes (tokio, threaded LLM loops) rely on.
// Documented as a compile-time check in `tests/session_lifecycle.rs`.
unsafe impl<T: TransportWrite + Send> Send for HidSession<T> {}

/// AKEY_EVENT_ACTION_DOWN / UP / MOVE (mirrors Android's
/// `MotionEvent.ACTION_*` constants used by scrcpy).
const ACTION_DOWN: u8 = 0;
const ACTION_UP: u8 = 1;
const ACTION_MOVE: u8 = 2;

impl<T: TransportWrite> HidSession<T> {
    /// Open the requested devices on `transport`, sending one
    /// `UHID_CREATE` per enabled device. If any `UHID_CREATE` fails,
    /// every already-opened device is `UHID_DESTROY`d and the original
    /// error is returned.
    pub fn open(transport: T, req: OpenRequest) -> Result<Self> {
        let transport = if req.coalesce {
            CoalescingWriter::new(transport)
        } else {
            // Bypass coalescing: same CoalescingWriter type, but with
            // a tiny window + large hard limit so the message goes
            // out on the very next push.
            CoalescingWriter::with_limits(transport, Duration::from_micros(1), usize::MAX)
        };
        let mut s = HidSession {
            transport,
            kbd: None,
            mouse: None,
            gamepad: None,
            closed: false,
            screen_w: 1080,
            screen_h: 1920,
        };
        if req.kbd {
            let k = KeyboardHid::new();
            let msg = k.open_message(None)?;
            s.send(&msg)?;
            s.kbd = Some(k);
        }
        if req.mouse {
            let m = MouseHid::new();
            let msg = m.open_message(None)?;
            s.send(&msg)?;
            s.mouse = Some(m);
        }
        if req.gamepad {
            let mut g = GamepadHid::new();
            // Allocate the first slot (id 3 = HID_ID_GAMEPAD_FIRST).
            // `GamepadHid::open` returns the assigned hid_id; we use the
            // public `create_message` helper to build the CREATE payload
            // with the correct id.
            let (hid_id, _msg) =
                g.open(HID_ID_GAMEPAD_FIRST as u32, Some("Microsoft X-Box 360 Pad"))?;
            let create = GamepadHid::create_message(hid_id, Some("Microsoft X-Box 360 Pad"));
            s.send(&create)?;
            s.gamepad = Some(g);
        }
        Ok(s)
    }

    /// Override the screen size used for touch events (default 1080x1920).
    /// Most call sites should set this from the device's actual display
    /// size so the server-side `INJECT_TOUCH_EVENT` is well-formed.
    pub fn set_screen_size(&mut self, w: u16, h: u16) {
        self.screen_w = w;
        self.screen_h = h;
    }

    /// Type a string into the focused app by sending key down/up events.
    ///
    /// Supports the ASCII subset covered by [`Scancode::try_from_char`]
    /// (letters, digits, space, Enter, Tab, Backspace, Escape, and the
    /// common US-Layout shifted symbols). Chars outside the supported
    /// set are skipped — call [`Self::type_text_strict`] if you want
    /// unsupported chars to be an error.
    pub fn type_text(&mut self, s: &str) -> Result<()> {
        for ch in s.chars() {
            let mut mods = Modifiers::empty();
            let Some(sc) = Scancode::try_from_char(ch, &mut mods) else {
                continue;
            };
            self.key(sc.to_u8(), true, mods)?;
            self.key(sc.to_u8(), false, mods)?;
        }
        Ok(())
    }

    /// Like [`Self::type_text`] but returns an error on the first
    /// unsupported character.
    pub fn type_text_strict(&mut self, s: &str) -> Result<()> {
        for ch in s.chars() {
            let mut mods = Modifiers::empty();
            let sc = Scancode::try_from_char(ch, &mut mods).ok_or(Error::SessionLifecycle(
                "unsupported char in type_text_strict",
            ))?;
            self.key(sc.to_u8(), true, mods)?;
            self.key(sc.to_u8(), false, mods)?;
        }
        Ok(())
    }

    /// Inject a single key down (`pressed = true`) or up (`pressed = false`).
    pub fn key(&mut self, scancode: u8, pressed: bool, mods: Modifiers) -> Result<()> {
        let kbd = self
            .kbd
            .as_mut()
            .ok_or(Error::SessionLifecycle("keyboard not open"))?;
        let msg = kbd.key_event(scancode, pressed, mods)?;
        self.send(&msg)
    }

    /// Press and release the LEFT mouse button at the absolute screen
    /// coordinate `(x, y)`. Implemented via `INJECT_TOUCH_EVENT` — no
    /// UHID mouse device needs to be open.
    pub fn tap(&mut self, x: i32, y: i32) -> Result<()> {
        let down = self.touch_msg(ACTION_DOWN, 0, x, y, 1.0);
        self.send(&down)?;
        let up = self.touch_msg(ACTION_UP, 0, x, y, 0.0);
        self.send(&up)?;
        Ok(())
    }

    /// Linear-interpolate a swipe from `from` to `to` over `steps` (≥ 2)
    /// intermediate samples. The button is held down throughout. The
    /// `dur` value is recorded for caller-visible timing — the session
    /// is synchronous and does not sleep between events (the caller is
    /// responsible for pacing if needed).
    pub fn swipe(
        &mut self,
        from: (i32, i32),
        to: (i32, i32),
        _dur: Duration,
        steps: u32,
    ) -> Result<()> {
        let steps = steps.max(2);
        let (x0, y0) = from;
        let (x1, y1) = to;
        self.send(&self.touch_msg(ACTION_DOWN, 0, x0, y0, 1.0))?;
        for i in 1..steps {
            let t = i as f32 / steps as f32;
            let x = (x0 as f32 + (x1 - x0) as f32 * t).round() as i32;
            let y = (y0 as f32 + (y1 - y0) as f32 * t).round() as i32;
            self.send(&self.touch_msg(ACTION_MOVE, 0, x, y, 1.0))?;
        }
        self.send(&self.touch_msg(ACTION_MOVE, 0, x1, y1, 1.0))?;
        self.send(&self.touch_msg(ACTION_UP, 0, x1, y1, 0.0))?;
        Ok(())
    }

    /// Set a single gamepad stick/trigger axis to `value` in `[-1.0, 1.0]`
    /// (triggers are clamped to `[0, 1]`). Writes one `UHID_INPUT`.
    pub fn set_stick(&mut self, axis: GamepadAxis, value: f32) -> Result<()> {
        let gp = self
            .gamepad
            .as_mut()
            .ok_or(Error::SessionLifecycle("gamepad not open"))?;
        let raw = (value.clamp(-1.0, 1.0) * 32767.0) as i16;
        let msg = gp.axis_event(HID_ID_GAMEPAD_FIRST as u32, axis, raw)?;
        self.send(&msg)
    }

    /// Set a single gamepad button to `pressed`. Writes one `UHID_INPUT`.
    pub fn set_button(&mut self, btn: GamepadButton, pressed: bool) -> Result<()> {
        let gp = self
            .gamepad
            .as_mut()
            .ok_or(Error::SessionLifecycle("gamepad not open"))?;
        let msg = gp.button_event(HID_ID_GAMEPAD_FIRST as u32, btn, pressed)?;
        self.send(&msg)
    }

    /// Access the underlying keyboard driver (for advanced key-level
    /// events not covered by the high-level helpers). Panics if the
    /// keyboard was not opened.
    pub fn keyboard(&mut self) -> &mut KeyboardHid {
        self.kbd
            .as_mut()
            .expect("keyboard requested but not opened")
    }
    /// Access the underlying mouse driver. Panics if not opened.
    pub fn mouse(&mut self) -> &mut MouseHid {
        self.mouse.as_mut().expect("mouse requested but not opened")
    }
    /// Access the underlying gamepad driver. Panics if not opened.
    pub fn gamepad(&mut self) -> &mut GamepadHid {
        self.gamepad
            .as_mut()
            .expect("gamepad requested but not opened")
    }

    /// Send a control message over the owned transport. When the
    /// session is in coalescing mode, the message is buffered and sent
    /// on the next flush (1 ms window, hard limit, or explicit
    /// [`Self::flush_now`] call). Critical messages bypass the buffer.
    pub fn send(&mut self, msg: &ControlMessage) -> Result<()> {
        let _reason = self.transport.push(msg)?;
        Ok(())
    }

    /// Force any buffered messages to the transport. Returns the
    /// number of bytes flushed. Always call this before reading the
    /// transport (e.g. in tests using `MockTransport`) to ensure no
    /// bytes are still in the coalescing buffer.
    pub fn flush_now(&mut self) -> Result<usize> {
        self.transport.flush_now()
    }

    /// Statistics from the underlying coalescing writer: total
    /// messages pushed, total bytes written, and pending bytes
    /// currently buffered.
    pub fn stats(&self) -> (u64, u64, usize) {
        (
            self.transport.pushed(),
            self.transport.written(),
            self.transport.pending_bytes(),
        )
    }

    /// Consume the session, sending `UHID_DESTROY` for every device
    /// that was opened. Idempotent — calling it twice is a no-op.
    /// Use [`Self::into_inner`] to recover the underlying transport.
    pub fn close(&mut self) -> Result<()> {
        self.try_close_all()
    }

    /// Recover the underlying transport after the session is closed.
    /// Does not run close again — the caller is responsible for having
    /// called [`Self::close`] (or for letting the `Drop` impl run).
    /// Panics if the session is not yet closed.
    pub fn into_inner(self) -> T {
        assert!(
            self.closed,
            "HidSession::into_inner called before close(); leak risk"
        );
        // SAFETY: `closed == true` means `Drop` is a no-op. We move out
        // the coalescing-wrapped transport and `forget(self)` to
        // disable the destructor.
        let cw = unsafe { std::ptr::read(&self.transport) };
        std::mem::forget(self);
        // The CoalescingWriter's Drop is a no-op because we just took
        // ownership. Extract the inner transport.
        cw.into_inner().expect("into_inner after close()")
    }

    /// `true` if the session is already closed (or close was called).
    pub fn is_closed(&self) -> bool {
        self.closed
    }

    // === AI intent methods ===
    //
    // Each intent is 1-3 underlying ControlMessages. Naming follows
    // the LLM function-call convention (press_home not
    // inject_keycode_home, launch_app not start_app).

    /// Set the screen on or off (`SetDisplayPower`).
    pub fn set_screen_power(&mut self, on: bool) -> Result<()> {
        self.send(&ControlMessage::SetDisplayPower(
            crate::control::message::SetDisplayPower { on },
        ))
    }

    /// Inject a raw keycode (Android `KeyEvent.KEYCODE_*`).
    /// `keycode` and `metastate` follow `InjectKeycode`.
    pub fn inject_keycode(&mut self, action: u8, keycode: u32,
                          repeat: u32, metastate: u32) -> Result<()> {
        self.send(&ControlMessage::InjectKeycode(
            crate::control::message::InjectKeycode { action, keycode, repeat, metastate },
        ))
    }

    /// Press the Home key.
    pub fn press_home(&mut self) -> Result<()> {
        self.inject_keycode(0, 3, 0, 0)  // KEYCODE_HOME = 3
    }
    /// Press the Back key.
    pub fn press_back(&mut self) -> Result<()> {
        self.inject_keycode(0, 4, 0, 0)  // KEYCODE_BACK = 4
    }
    /// Open the recents / app-switcher.
    pub fn open_recents(&mut self) -> Result<()> {
        self.inject_keycode(0, 187, 0, 0) // KEYCODE_APP_SWITCH = 187
    }
    /// Volume up.
    pub fn volume_up(&mut self) -> Result<()> {
        self.inject_keycode(0, 24, 0, 0) // KEYCODE_VOLUME_UP = 24
    }
    /// Volume down.
    pub fn volume_down(&mut self) -> Result<()> {
        self.inject_keycode(0, 25, 0, 0) // KEYCODE_VOLUME_DOWN = 25
    }
    /// Volume mute.
    pub fn volume_mute(&mut self) -> Result<()> {
        self.inject_keycode(0, 164, 0, 0) // KEYCODE_VOLUME_MUTE = 164
    }

    /// Expand the notification panel.
    pub fn show_notifications(&mut self) -> Result<()> {
        self.send(&ControlMessage::ExpandNotificationPanel)
    }
    /// Expand the quick-settings panel.
    pub fn show_quick_settings(&mut self) -> Result<()> {
        self.send(&ControlMessage::ExpandSettingsPanel)
    }
    /// Collapse notification + quick-settings panels.
    pub fn collapse_panels(&mut self) -> Result<()> {
        self.send(&ControlMessage::CollapsePanels)
    }

    /// Rotate the device display.
    pub fn rotate_device(&mut self) -> Result<()> {
        self.send(&ControlMessage::RotateDevice)
    }
    /// Resize the virtual display (developer mode).
    pub fn resize_display(&mut self, w: u16, h: u16) -> Result<()> {
        self.send(&ControlMessage::ResizeDisplay(
            crate::control::message::ResizeDisplay { width: w, height: h },
        ))
    }
    /// Toggle the camera torch.
    pub fn set_torch(&mut self, on: bool) -> Result<()> {
        self.send(&ControlMessage::CameraSetTorch(
            crate::control::message::CameraSetTorch { on },
        ))
    }
    /// Camera zoom in.
    pub fn camera_zoom_in(&mut self) -> Result<()> {
        self.send(&ControlMessage::CameraZoomIn)
    }
    /// Camera zoom out.
    pub fn camera_zoom_out(&mut self) -> Result<()> {
        self.send(&ControlMessage::CameraZoomOut)
    }
    /// Open the physical-keyboard settings activity.
    pub fn open_hard_keyboard_settings(&mut self) -> Result<()> {
        self.send(&ControlMessage::OpenHardKeyboardSettings)
    }
    /// Reset the scrcpy video stream.
    pub fn reset_video(&mut self) -> Result<()> {
        self.send(&ControlMessage::ResetVideo)
    }
    /// Launch an app by package name.
    pub fn launch_app(&mut self, name: &str) -> Result<()> {
        self.send(&ControlMessage::StartApp(
            crate::control::message::StartApp { name: name.to_string() },
        ))
    }
    /// Write text to the device clipboard.
    pub fn set_clipboard(&mut self, text: &str, paste: bool) -> Result<()> {
        self.send(&ControlMessage::SetClipboard(
            crate::control::message::SetClipboard {
                sequence: 0, paste, text: text.to_string(),
            },
        ))
    }
    /// Request a clipboard read. **Phase 1 stub**: returns `Ok(())`
    /// (the read itself is fire-and-forget; the dispatcher will
    /// reply with an empty string for now). True server-reply
    /// forwarding is a follow-up run.
    pub fn get_clipboard(&mut self) -> Result<()> {
        self.send(&ControlMessage::GetClipboard(
            crate::control::message::GetClipboard { copy_key: 0 },
        ))
    }

    /// Two quick taps at the same coordinate. Sends 4 touch events
    /// (down / up / down / up).
    pub fn double_tap(&mut self, x: i32, y: i32) -> Result<()> {
        self.touch_msg_pub(crate::multitouch::ACTION_DOWN, 0, x, y, 1.0)?;
        self.touch_msg_pub(crate::multitouch::ACTION_UP, 0, x, y, 0.0)?;
        self.touch_msg_pub(crate::multitouch::ACTION_DOWN, 0, x, y, 1.0)?;
        self.touch_msg_pub(crate::multitouch::ACTION_UP, 0, x, y, 0.0)
    }
    /// Press, hold for `dur`, then release. Blocks the calling
    /// thread for `dur` (callers that need non-blocking should
    /// wrap in `tokio::task::spawn_blocking`).
    pub fn long_press(&mut self, x: i32, y: i32, dur: std::time::Duration) -> Result<()> {
        self.touch_msg_pub(crate::multitouch::ACTION_DOWN, 0, x, y, 1.0)?;
        std::thread::sleep(dur);
        self.touch_msg_pub(crate::multitouch::ACTION_UP, 0, x, y, 0.0)
    }
    /// Three-finger swipe down (Android screenshot gesture).
    pub fn three_finger_screenshot(&mut self) -> Result<()> {
        let w = self.screen_w as i32;
        let h = self.screen_h as i32;
        for id in 0u64..3 {
            self.touch_msg_pub(crate::multitouch::ACTION_DOWN, id, w / 4 * (id as i32 + 1), h / 4, 1.0)?;
        }
        for step in 1..=10 {
            for id in 0u64..3 {
                self.touch_msg_pub(
                    crate::multitouch::ACTION_MOVE, id,
                    w / 4 * (id as i32 + 1),
                    h / 4 + (h / 2 * step / 10),
                    1.0,
                )?;
            }
        }
        for id in 0u64..3 {
            self.touch_msg_pub(crate::multitouch::ACTION_UP, id, w / 4 * (id as i32 + 1), h * 3 / 4, 0.0)?;
        }
        Ok(())
    }

    // === end AI intent methods ===

    fn touch_msg_pub(&mut self, action: u8, pointer_id: u64,
                     x: i32, y: i32, pressure: f32) -> Result<()> {
        self.inject_touch(action, pointer_id, x, y, pressure)
    }

    /// Send `UHID_DESTROY` for every device that is still open. Used by
    /// both [`Self::close`] and the panic-safe `Drop` impl.
    fn try_close_all(&mut self) -> Result<()> {
        if self.closed {
            return Ok(());
        }
        if let Some(k) = self.kbd.as_ref() {
            let msg = k.close_message()?;
            self.send(&msg)?;
        }
        if let Some(m) = self.mouse.as_ref() {
            let msg = m.close_message()?;
            self.send(&msg)?;
        }
        if let Some(g) = self.gamepad.as_mut() {
            // Close the default slot we opened (id = HID_ID_GAMEPAD_FIRST).
            // Ignore the "unknown gamepad" error in case the slot was
            // already torn down by a prior close.
            if let Ok(msg) = g.close(HID_ID_GAMEPAD_FIRST as u32) {
                self.send(&msg)?;
            }
        }
        self.closed = true;
        Ok(())
    }

    /// Low-level multi-touch inject. `pointer_id` should be in
    /// `0..crate::multitouch::MAX_POINTERS` (validated by
    /// [`crate::multitouch::MultitouchHandle`]; direct callers are
    /// responsible for the check). `pressure` is clamped to `[0, 1]`
    /// by the wire-format serializer.
    pub fn inject_touch(
        &mut self,
        action: u8,
        pointer_id: u64,
        x: i32,
        y: i32,
        pressure: f32,
    ) -> Result<()> {
        let msg = self.touch_msg(action, pointer_id, x, y, pressure);
        self.send(&msg)
    }

    /// Borrow a multi-touch handle backed by this session. Cannot
    /// coexist with other `&mut self` borrows (keyboard / mouse /
    /// gamepad) — the borrow checker enforces this at compile time.
    pub fn multitouch(&mut self) -> crate::multitouch::MultitouchHandle<'_, T> {
        crate::multitouch::MultitouchHandle::new(self)
    }

    fn touch_msg(
        &self,
        action: u8,
        pointer_id: u64,
        x: i32,
        y: i32,
        pressure: f32,
    ) -> ControlMessage {
        ControlMessage::InjectTouchEvent(InjectTouchEvent {
            action,
            pointer_id,
            x,
            y,
            screen_w: self.screen_w,
            screen_h: self.screen_h,
            pressure,
            action_button: 0,
            buttons: 0,
        })
    }
}

impl<T: TransportWrite> Drop for HidSession<T> {
    /// Panic-safe: even if the caller is unwinding, we still try to send
    /// `UHID_DESTROY` for every open device. A failure during drop is
    /// swallowed (logged to stderr) so we never abort the process by
    /// double-panicking.
    fn drop(&mut self) {
        if self.closed {
            return;
        }
        let result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.try_close_all()));
        if let Err(panic) = result {
            eprintln!("HidSession::drop: close failed during unwind: {:?}", panic);
        }
    }
}
