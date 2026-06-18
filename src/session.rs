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

use crate::coalesce::{
    CoalescingWriter, DEFAULT_HARD_LIMIT, DEFAULT_WINDOW, DIRECT_GAMEPAD_BATCH_FRAMES,
};
use crate::control::message::{ControlMessage, InjectTouchEvent};
use crate::error::{Error, Result, TransportWrite};
use crate::hid::gamepad::GamepadHid;
use crate::hid::keyboard::KeyboardHid;
use crate::hid::mouse::MouseHid;
use crate::hid::HidDevice;
use crate::types::{
    dpad_hat_value, GamepadAxis, GamepadButton, Modifiers, Scancode, HID_ID_GAMEPAD_FIRST,
};

/// Which HID devices the session should open. Touch events are always
/// available (no UHID device needed) — they ride on the same control
/// socket. Use [`OpenRequest::all`], [`OpenRequest::none`], or any
/// combination of `kbd`, `mouse`, `gamepad`.
///
/// `coalesce` (default `true`) wraps the transport in a
/// [`CoalescingWriter`] so that bursty `UhidInput` traffic (e.g. 1 kHz
/// gamepad stick jitter) is batched into a single `write_all` per
/// 1 ms window. Set to `false` for power users who want every
/// message to skip batching and write immediately. For the lowest
/// latency gamepad loop, use [`OpenRequest::gamepad_only_realtime`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct OpenRequest {
    pub kbd: bool,
    pub mouse: bool,
    pub gamepad: bool,
    pub coalesce: bool,
    pub coalesce_window: Duration,
    pub coalesce_hard_limit: usize,
}

impl OpenRequest {
    pub const fn none() -> Self {
        Self {
            kbd: false,
            mouse: false,
            gamepad: false,
            coalesce: true,
            coalesce_window: DEFAULT_WINDOW,
            coalesce_hard_limit: DEFAULT_HARD_LIMIT,
        }
    }
    pub const fn all() -> Self {
        Self {
            kbd: true,
            mouse: true,
            gamepad: true,
            coalesce: true,
            coalesce_window: DEFAULT_WINDOW,
            coalesce_hard_limit: DEFAULT_HARD_LIMIT,
        }
    }
    pub const fn kbd_only() -> Self {
        Self {
            kbd: true,
            mouse: false,
            gamepad: false,
            coalesce: true,
            coalesce_window: DEFAULT_WINDOW,
            coalesce_hard_limit: DEFAULT_HARD_LIMIT,
        }
    }
    pub const fn mouse_only() -> Self {
        Self {
            kbd: false,
            mouse: true,
            gamepad: false,
            coalesce: true,
            coalesce_window: DEFAULT_WINDOW,
            coalesce_hard_limit: DEFAULT_HARD_LIMIT,
        }
    }
    pub const fn gamepad_only() -> Self {
        Self {
            kbd: false,
            mouse: false,
            gamepad: true,
            coalesce: true,
            coalesce_window: DEFAULT_WINDOW,
            coalesce_hard_limit: DEFAULT_HARD_LIMIT,
        }
    }
    /// Open only a gamepad with immediate writes (no coalescing), tuned
    /// for the lowest-latency control loops.
    pub const fn gamepad_only_realtime() -> Self {
        Self {
            kbd: false,
            mouse: false,
            gamepad: true,
            coalesce: false,
            coalesce_window: Duration::from_millis(0),
            coalesce_hard_limit: 0,
        }
    }

    /// Configure the same device set, but with coalescing disabled.
    /// Useful for ultra-low-latency control loops (e.g. fighting-game
    /// style gamepad control).
    pub const fn with_coalesce(mut self, coalesce: bool) -> Self {
        self.coalesce = coalesce;
        self
    }

    /// Configure the coalescing window used when `coalesce == true`.
    /// A zero window is treated as fully direct mode (equivalent to
    /// `with_coalesce(false)`), because it removes all timer-based batching
    /// and keeps frame latency at minimum.
    pub const fn with_coalesce_window(mut self, coalesce_window: Duration) -> Self {
        self.coalesce_window = coalesce_window;
        self
    }

    /// Configure the coalescing hard limit used when `coalesce == true`.
    ///
    /// Set to at least `1` for stable batching behavior.
    pub const fn with_coalesce_hard_limit(mut self, coalesce_hard_limit: usize) -> Self {
        self.coalesce_hard_limit = coalesce_hard_limit;
        self
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
    gamepad_slot: Option<usize>,
    gamepad_hid_id: Option<u16>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GamepadFrameRaw {
    pub buttons: u32,
    pub left_x: i16,
    pub left_y: i16,
    pub right_x: i16,
    pub right_y: i16,
    pub left_trigger: i16,
    pub right_trigger: i16,
}

/// Fixed-size gamepad payload used by every packed gamepad fast path.
pub const GAMEPAD_FRAME_BYTES: usize = 15;

impl GamepadFrameRaw {
    pub const fn new(
        buttons: u32,
        left_x: i16,
        left_y: i16,
        right_x: i16,
        right_y: i16,
        left_trigger: i16,
        right_trigger: i16,
    ) -> Self {
        Self {
            buttons,
            left_x,
            left_y,
            right_x,
            right_y,
            left_trigger,
            right_trigger,
        }
    }

    /// Pack a gamepad frame into the 15-byte HID payload expected by
    /// `UhidInput`.
    #[inline]
    pub fn pack(self) -> [u8; GAMEPAD_FRAME_BYTES] {
        let mut data = [0u8; GAMEPAD_FRAME_BYTES];
        let left_x = (self.left_x as i32 + 0x8000) as u16;
        let left_y = (self.left_y as i32 + 0x8000) as u16;
        let right_x = (self.right_x as i32 + 0x8000) as u16;
        let right_y = (self.right_y as i32 + 0x8000) as u16;
        let left_trigger = (self.left_trigger.max(0) as u16).min(0x7FFF);
        let right_trigger = (self.right_trigger.max(0) as u16).min(0x7FFF);

        data[0..2].copy_from_slice(&left_x.to_le_bytes());
        data[2..4].copy_from_slice(&left_y.to_le_bytes());
        data[4..6].copy_from_slice(&right_x.to_le_bytes());
        data[6..8].copy_from_slice(&right_y.to_le_bytes());
        data[8..10].copy_from_slice(&left_trigger.to_le_bytes());
        data[10..12].copy_from_slice(&right_trigger.to_le_bytes());
        data[12..14].copy_from_slice(&(self.buttons as u16).to_le_bytes());
        data[14] = dpad_hat_value(self.buttons);
        data
    }
}

impl<T: TransportWrite> HidSession<T> {
    /// Open the requested devices on `transport`, sending one
    /// `UHID_CREATE` per enabled device. If any `UHID_CREATE` fails,
    /// every already-opened device is `UHID_DESTROY`d and the original
    /// error is returned.
    pub fn open(transport: T, req: OpenRequest) -> Result<Self> {
        let transport = if req.coalesce && !req.coalesce_window.is_zero() {
            let hard_limit = req.coalesce_hard_limit.max(1);
            CoalescingWriter::with_limits(transport, req.coalesce_window, hard_limit)
        } else {
            // No batching; each non-critical message is flushed as soon
            // as it is pushed.
            CoalescingWriter::direct(transport)
        };
        let mut s = HidSession {
            transport,
            kbd: None,
            mouse: None,
            gamepad: None,
            gamepad_slot: None,
            gamepad_hid_id: None,
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
            // `GamepadHid::open` already returns a fully formed `UhidCreate`
            // payload (including the descriptor copy), reuse it directly.
            let (hid_id, create) =
                g.open(HID_ID_GAMEPAD_FIRST as u32, Some("Microsoft X-Box 360 Pad"))?;
            let slot_idx = GamepadHid::slot_from_hid_id(hid_id)
                .ok_or(Error::SessionLifecycle("invalid gamepad id"))?;
            s.send(&create)?;
            s.gamepad = Some(g);
            s.gamepad_slot = Some(slot_idx);
            s.gamepad_hid_id = Some(hid_id);
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
    #[inline]
    pub fn set_stick(&mut self, axis: GamepadAxis, value: f32) -> Result<()> {
        let raw = (value.clamp(-1.0, 1.0) * 32767.0) as i16;
        self.set_stick_raw(axis, raw)
    }

    /// Set a gamepad stick/trigger axis from a raw scrcpy axis value.
    /// Useful for high-frequency callers that already have an i16 control
    /// value and want to skip the `f32 -> i16` conversion in `set_stick`.
    #[inline]
    pub fn set_stick_raw(&mut self, axis: GamepadAxis, raw: i16) -> Result<()> {
        let (slot_idx, gp) = self.gamepad_with_cached_slot()?;
        let msg = gp.axis_event_slot_idx_raw(slot_idx, axis, raw);
        if let Some((hid_id, payload)) = msg {
            self.transport.push_gamepad_input(hid_id, &payload)?;
        }
        Ok(())
    }

    #[inline]
    fn gamepad_with_cached_slot(&mut self) -> Result<(usize, &mut GamepadHid)> {
        if self.closed {
            return Err(Error::SessionLifecycle("session closed"));
        }
        let slot_idx = self
            .gamepad_slot
            .ok_or(Error::SessionLifecycle("gamepad not open"))?;
        let gp = self
            .gamepad
            .as_mut()
            .ok_or(Error::SessionLifecycle("gamepad not open"))?;
        Ok((slot_idx, gp))
    }

    #[inline]
    fn gamepad_hid_id(&self) -> Result<u16> {
        if self.closed {
            return Err(Error::SessionLifecycle("session closed"));
        }
        self.gamepad_hid_id
            .ok_or(Error::SessionLifecycle("gamepad not open"))
    }

    /// Replace all gamepad buttons from a single bitframe.
    ///
    /// This path is faster for AI-style frame loops than emitting one
    /// per-button event.
    #[inline]
    pub fn set_buttons(&mut self, buttons: u32) -> Result<()> {
        let (slot_idx, gp) = self.gamepad_with_cached_slot()?;
        if let Some((hid_id, payload)) = gp.buttons_event_slot_idx_raw(slot_idx, buttons) {
            self.transport.push_gamepad_input(hid_id, &payload)?;
        }
        Ok(())
    }

    /// Replace all gamepad state fields in a single report (buttons +
    /// left/right stick + left/right trigger).
    ///
    /// This is the lowest-latency path for full-frame gamepad updates
    /// (one command + one UHID_INPUT at most).
    #[inline]
    #[allow(clippy::too_many_arguments)]
    pub fn set_frame_raw(
        &mut self,
        buttons: u32,
        left_x: i16,
        left_y: i16,
        right_x: i16,
        right_y: i16,
        left_trigger: i16,
        right_trigger: i16,
    ) -> Result<()> {
        let (slot_idx, gp) = self.gamepad_with_cached_slot()?;
        if let Some((hid_id, payload)) = gp.full_state_event_slot_idx_raw(
            slot_idx,
            buttons,
            left_x,
            left_y,
            right_x,
            right_y,
            left_trigger,
            right_trigger,
        ) {
            self.transport.push_gamepad_input(hid_id, &payload)?;
        }
        Ok(())
    }

    /// Fastest full-frame path from normalized fields (no state diffing).
    ///
    /// This is intended for high-frequency loops where the caller already
    /// owns the current gamepad state and does not need dedupe inside
    /// the library.
    #[inline]
    #[allow(clippy::too_many_arguments)]
    pub fn set_frame_raw_unchecked(
        &mut self,
        buttons: u32,
        left_x: i16,
        left_y: i16,
        right_x: i16,
        right_y: i16,
        left_trigger: i16,
        right_trigger: i16,
    ) -> Result<()> {
        let frame = GamepadFrameRaw {
            buttons,
            left_x,
            left_y,
            right_x,
            right_y,
            left_trigger,
            right_trigger,
        };
        let hid_id = self.gamepad_hid_id()?;
        self.transport.push_gamepad_input_fields(hid_id, &frame)?;
        Ok(())
    }

    /// Fastest full-frame path from a pre-built frame struct (no state
    /// diffing).
    #[inline]
    pub fn set_frame_raw_unchecked_frame(&mut self, frame: GamepadFrameRaw) -> Result<()> {
        let hid_id = self.gamepad_hid_id()?;
        self.transport.push_gamepad_input_fields(hid_id, &frame)?;
        Ok(())
    }

    /// Fast path for already-packed gamepad frames (15-byte HID payload).
    ///
    /// This bypasses `GamepadHid` state diffing and writes the payload as-is.
    /// Use this only when the caller already keeps equivalent input state in
    /// its own loop and intentionally wants every provided frame pushed.
    #[inline]
    pub fn set_frame_raw_packed(&mut self, payload: &[u8; GAMEPAD_FRAME_BYTES]) -> Result<()> {
        let hid_id = self.gamepad_hid_id()?;
        self.transport.push_gamepad_input(hid_id, payload)?;
        Ok(())
    }

    /// Replace multiple full gamepad frames in sequence. This method
    /// does a single slot lookup and sends each changed frame only
    /// if it differs from the last in-memory state.
    #[inline]
    pub fn set_frame_raw_batch(&mut self, frames: &[GamepadFrameRaw]) -> Result<usize> {
        if frames.is_empty() {
            return Ok(0);
        }
        if frames.len() == 1 {
            let frame = frames[0];
            let (slot_idx, gp) = self.gamepad_with_cached_slot()?;
            if let Some((hid_id, payload)) = gp.full_state_event_slot_idx_raw(
                slot_idx,
                frame.buttons,
                frame.left_x,
                frame.left_y,
                frame.right_x,
                frame.right_y,
                frame.left_trigger,
                frame.right_trigger,
            ) {
                self.transport.push_gamepad_input(hid_id, &payload)?;
                return Ok(1);
            }
            return Ok(0);
        }
        if self.closed {
            return Err(Error::SessionLifecycle("session closed"));
        }
        let slot_idx = self
            .gamepad_slot
            .ok_or(Error::SessionLifecycle("gamepad not open"))?;
        let gamepad = self
            .gamepad
            .as_mut()
            .ok_or(Error::SessionLifecycle("gamepad not open"))?;
        let transport = &mut self.transport;
        let mut sent = 0usize;
        let mut batch = [[0u8; GAMEPAD_FRAME_BYTES]; DIRECT_GAMEPAD_BATCH_FRAMES];
        let mut batch_len = 0usize;
        let mut batch_id = 0u16;
        let mut have_batch_id = false;

        for frame in frames {
            if let Some((hid_id, payload)) = gamepad.full_state_event_slot_idx_raw(
                slot_idx,
                frame.buttons,
                frame.left_x,
                frame.left_y,
                frame.right_x,
                frame.right_y,
                frame.left_trigger,
                frame.right_trigger,
            ) {
                if !have_batch_id {
                    batch_id = hid_id;
                    have_batch_id = true;
                }
                batch[batch_len] = payload;
                batch_len += 1;
                sent += 1;

                if batch_len == DIRECT_GAMEPAD_BATCH_FRAMES {
                    transport.push_gamepad_input_batch(batch_id, &batch)?;
                    batch_len = 0;
                }
            }
        }

        if batch_len > 0 && have_batch_id {
            transport.push_gamepad_input_batch(batch_id, &batch[..batch_len])?;
        }
        Ok(sent)
    }

    /// Replace multiple full frames in sequence without any state dedupe.
    ///
    /// Use this when your control loop already owns a complete frame
    /// stream and wants every frame pushed (including duplicates).
    #[inline]
    pub fn set_frame_raw_batch_unchecked(&mut self, frames: &[GamepadFrameRaw]) -> Result<usize> {
        if frames.is_empty() {
            return Ok(0);
        }
        if frames.len() == 1 {
            let frame = frames[0];
            self.set_frame_raw_unchecked(
                frame.buttons,
                frame.left_x,
                frame.left_y,
                frame.right_x,
                frame.right_y,
                frame.left_trigger,
                frame.right_trigger,
            )?;
            return Ok(1);
        }
        let hid_id = self.gamepad_hid_id()?;
        self.transport
            .push_gamepad_input_batch_from_fields(hid_id, frames)?;
        Ok(frames.len())
    }

    /// Fast path for multiple already-packed 15-byte gamepad frames.
    ///
    /// Bypasses state diffing inside [`GamepadHid`] and writes each
    /// payload directly. This is intentionally explicit and is ideal when
    /// your loop already emits normalized HID report bytes.
    #[inline]
    pub fn set_frame_raw_packed_batch(
        &mut self,
        frames: &[[u8; GAMEPAD_FRAME_BYTES]],
    ) -> Result<usize> {
        if frames.is_empty() {
            return Ok(0);
        }
        if frames.len() == 1 {
            self.set_frame_raw_packed(&frames[0])?;
            return Ok(1);
        }
        let hid_id = self.gamepad_hid_id()?;
        self.transport.push_gamepad_input_batch(hid_id, frames)?;
        Ok(frames.len())
    }

    /// Set both left-stick axes in one report (one `UHID_INPUT`), useful
    /// when stick vectors are produced at render-rate.
    #[inline]
    pub fn set_left_stick_raw(&mut self, x: i16, y: i16) -> Result<()> {
        let (slot_idx, gp) = self.gamepad_with_cached_slot()?;
        if let Some((hid_id, payload)) = gp.left_stick_raw_slot_idx_raw(slot_idx, x, y) {
            self.transport.push_gamepad_input(hid_id, &payload)?;
        }
        Ok(())
    }

    /// Set both right-stick axes in one report (one `UHID_INPUT`).
    #[inline]
    pub fn set_right_stick_raw(&mut self, x: i16, y: i16) -> Result<()> {
        let (slot_idx, gp) = self.gamepad_with_cached_slot()?;
        if let Some((hid_id, payload)) = gp.right_stick_raw_slot_idx_raw(slot_idx, x, y) {
            self.transport.push_gamepad_input(hid_id, &payload)?;
        }
        Ok(())
    }

    /// Set both triggers in one report (one `UHID_INPUT`).
    #[inline]
    pub fn set_triggers_raw(&mut self, left: i16, right: i16) -> Result<()> {
        let (slot_idx, gp) = self.gamepad_with_cached_slot()?;
        if let Some((hid_id, payload)) = gp.triggers_raw_slot_idx_raw(slot_idx, left, right) {
            self.transport.push_gamepad_input(hid_id, &payload)?;
        }
        Ok(())
    }

    /// Set both sticks and triggers in one report (one `UHID_INPUT`) when
    /// you already have a full sampled frame.
    #[inline]
    pub fn set_sticks_raw(
        &mut self,
        left_x: i16,
        left_y: i16,
        right_x: i16,
        right_y: i16,
        left_trigger: i16,
        right_trigger: i16,
    ) -> Result<()> {
        let (slot_idx, gp) = self.gamepad_with_cached_slot()?;
        if let Some((hid_id, payload)) = gp.set_sticks_raw_slot_idx_raw(
            slot_idx,
            left_x,
            left_y,
            right_x,
            right_y,
            left_trigger,
            right_trigger,
        ) {
            self.transport.push_gamepad_input(hid_id, &payload)?;
        }
        Ok(())
    }

    /// Set a single gamepad button to `pressed`. Writes one `UHID_INPUT`.
    #[inline]
    pub fn set_button(&mut self, btn: GamepadButton, pressed: bool) -> Result<()> {
        let (slot_idx, gp) = self.gamepad_with_cached_slot()?;
        if let Some((hid_id, payload)) = gp.button_event_slot_idx_raw(slot_idx, btn, pressed) {
            self.transport.push_gamepad_input(hid_id, &payload)?;
        }
        Ok(())
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

    /// Send a control message over the owned transport. In coalescing
    /// mode, non-critical messages are buffered and sent on the next
    /// flush (1 ms window, hard limit, or explicit [`Self::flush_now`]
    /// call). Critical messages bypass the buffer. When coalescing is
    /// disabled, non-critical messages are flushed immediately.
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

    /// Total transport flushes performed by the underlying coalescing
    /// writer. This is useful for high-frequency control tuning.
    pub fn flushes(&self) -> u64 {
        self.transport.flushes()
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
    pub fn inject_keycode(
        &mut self,
        action: u8,
        keycode: u32,
        repeat: u32,
        metastate: u32,
    ) -> Result<()> {
        self.send(&ControlMessage::InjectKeycode(
            crate::control::message::InjectKeycode {
                action,
                keycode,
                repeat,
                metastate,
            },
        ))
    }

    /// Press the Home key.
    pub fn press_home(&mut self) -> Result<()> {
        self.inject_keycode(0, 3, 0, 0) // KEYCODE_HOME = 3
    }
    /// Press the Back key.
    pub fn press_back(&mut self) -> Result<()> {
        self.inject_keycode(0, 4, 0, 0) // KEYCODE_BACK = 4
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
            crate::control::message::ResizeDisplay {
                width: w,
                height: h,
            },
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
            crate::control::message::StartApp {
                name: name.to_string(),
            },
        ))
    }
    /// Write text to the device clipboard.
    pub fn set_clipboard(&mut self, text: &str, paste: bool) -> Result<()> {
        self.send(&ControlMessage::SetClipboard(
            crate::control::message::SetClipboard {
                sequence: 0,
                paste,
                text: text.to_string(),
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
            self.touch_msg_pub(
                crate::multitouch::ACTION_DOWN,
                id,
                w / 4 * (id as i32 + 1),
                h / 4,
                1.0,
            )?;
        }
        for step in 1..=10 {
            for id in 0u64..3 {
                self.touch_msg_pub(
                    crate::multitouch::ACTION_MOVE,
                    id,
                    w / 4 * (id as i32 + 1),
                    h / 4 + (h / 2 * step / 10),
                    1.0,
                )?;
            }
        }
        for id in 0u64..3 {
            self.touch_msg_pub(
                crate::multitouch::ACTION_UP,
                id,
                w / 4 * (id as i32 + 1),
                h * 3 / 4,
                0.0,
            )?;
        }
        Ok(())
    }

    // === end AI intent methods ===

    fn touch_msg_pub(
        &mut self,
        action: u8,
        pointer_id: u64,
        x: i32,
        y: i32,
        pressure: f32,
    ) -> Result<()> {
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
            // Close the cached slot we opened (id = HID_ID_GAMEPAD_FIRST).
            // Ignore the "unknown gamepad" error in case the slot was
            // already torn down by a prior close.
            if let Some(slot_idx) = self.gamepad_slot {
                if let Ok(msg) = g.close_slot_idx(slot_idx) {
                    self.send(&msg)?;
                }
            } else if let Ok(msg) = g.close(HID_ID_GAMEPAD_FIRST as u32) {
                self.send(&msg)?;
            }
            self.gamepad = None;
            self.gamepad_slot = None;
            self.gamepad_hid_id = None;
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
