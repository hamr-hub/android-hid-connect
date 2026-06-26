//! `HidClient` — parallel command submission via `std::sync::mpsc`.
//!
//! The motivation: `HidSession` is `Send` but all its input methods
//! take `&mut self`, so multiple producer threads would otherwise
//! need a `Mutex` — which serializes the 1ms coalescing window we
//! built in `coalesce::CoalescingWriter`. `HidClient` solves this by
//! handing the `HidSession` to a single dispatcher thread and letting
//! other threads push commands over a bounded channel.
//!
//! Pattern:
//!
//! ```no_run
//! use android_hid_connect::session::{HidSession, OpenRequest};
//! use android_hid_connect::client::HidCommand;
//! use android_hid_connect::transport::open_tcp;
//!
//! let sock = open_tcp("127.0.0.1", 27183).unwrap();
//! let s = HidSession::open(sock, OpenRequest::all()).unwrap();
//! let (client, dispatcher) = s.into_client().unwrap();
//!
//! let c = client.clone();
//! std::thread::spawn(move || {
//!     c.send(HidCommand::TypeText("hello".into())).unwrap();
//! });
//!
//! client.send(HidCommand::MultitouchDown { id: 0, x: 540, y: 1200, pressure: 1.0 }).unwrap();
//! client.send(HidCommand::MultitouchUp { id: 0 }).unwrap();
//!
//! client.close();
//! let _sock = dispatcher.join().unwrap();
//! ```

use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::coalesce::DIRECT_GAMEPAD_BATCH_FRAMES;
use crate::control::message::{
    AiConfig, AiQuery, BackOrScreenOn, ControlMessage, SetClipboard, SetDisplayPower, StartApp,
};
use crate::error::{Error, Result, TransportWrite};
use crate::session::{GamepadFrameRaw, HidSession, GAMEPAD_FRAME_BYTES};
use crate::types::{
    AndroidKeyAction, AndroidKeycode, ClipboardCopyKey, GamepadAxis, GamepadButton, Modifiers,
    MouseButton, Scancode, TouchAction, TouchPointerId,
};

/// Default channel bound for `HidClient`. Bounds the back-pressure
/// between producers and the dispatcher. A larger default reduces
/// back-pressure spikes on high-rate gamepad loops while keeping
/// memory usage bounded in normal use.
pub const DEFAULT_CHANNEL_BOUND: usize = 4096;

/// Fixed stack-buffer size for touch batches.
pub const TOUCH_BATCH_FRAMES: usize = 24;

/// Fixed stack-buffer size for low-latency keyboard edge batches.
pub const KEYBOARD_BATCH_FRAMES: usize = 32;

/// Maximum non-modifier keys in one USB HID keyboard chord.
pub const KEYBOARD_CHORD_KEYS: usize = 6;

/// Maximum edge frames emitted by one keyboard chord.
pub const KEYBOARD_CHORD_EDGES: usize = KEYBOARD_CHORD_KEYS * 2;

/// Fixed stack-buffer size for Android framework key-event batches.
pub const ANDROID_KEY_BATCH_FRAMES: usize = 32;

/// Fixed stack-buffer size for low-latency gamepad frame batches.
pub const GAMEPAD_BATCH_FRAMES: usize = DIRECT_GAMEPAD_BATCH_FRAMES;

/// Fixed stack-buffer size for relative UHID mouse batches.
pub const MOUSE_BATCH_FRAMES: usize = 32;

/// Fixed stack-buffer size for Android absolute scroll batches.
pub const SCROLL_BATCH_FRAMES: usize = 32;

/// One raw touch event for batching through [`HidClient`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TouchFrame {
    pub action: u8,
    pub pointer_id: u64,
    pub x: i32,
    pub y: i32,
    pub pressure: f32,
}

impl TouchFrame {
    pub const EMPTY: Self = Self {
        action: 0,
        pointer_id: 0,
        x: 0,
        y: 0,
        pressure: 0.0,
    };

    pub const fn new(action: u8, pointer_id: u64, x: i32, y: i32, pressure: f32) -> Self {
        Self {
            action,
            pointer_id,
            x,
            y,
            pressure,
        }
    }

    pub const fn with_action(
        action: TouchAction,
        pointer_id: u64,
        x: i32,
        y: i32,
        pressure: f32,
    ) -> Self {
        Self::new(action.value(), pointer_id, x, y, pressure)
    }

    pub const fn with_pointer(
        action: TouchAction,
        pointer_id: TouchPointerId,
        x: i32,
        y: i32,
        pressure: f32,
    ) -> Self {
        Self::with_action(action, pointer_id.value(), x, y, pressure)
    }
}

/// One raw UHID keyboard edge for batching through [`HidClient`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyboardFrame {
    pub scancode: u8,
    pub pressed: bool,
    pub mods: Modifiers,
}

impl KeyboardFrame {
    pub const EMPTY: Self = Self {
        scancode: 0,
        pressed: false,
        mods: Modifiers::NONE,
    };

    pub const fn new(scancode: u8, pressed: bool, mods: Modifiers) -> Self {
        Self {
            scancode,
            pressed,
            mods,
        }
    }

    pub const fn scancode(scancode: Scancode, pressed: bool, mods: Modifiers) -> Self {
        Self::new(scancode.to_u8(), pressed, mods)
    }

    pub const fn down(scancode: u8, mods: Modifiers) -> Self {
        Self::new(scancode, true, mods)
    }

    pub const fn up(scancode: u8) -> Self {
        Self::new(scancode, false, Modifiers::NONE)
    }

    pub const fn scancode_down(scancode: Scancode, mods: Modifiers) -> Self {
        Self::down(scancode.to_u8(), mods)
    }

    pub const fn scancode_up(scancode: Scancode) -> Self {
        Self::up(scancode.to_u8())
    }
}

/// One typed USB HID keyboard chord for shortcut injection.
///
/// `mods` represents held modifier bits for the chord, while `keys` contains
/// only non-modifier scancodes. Expansion presses keys in order and releases
/// them in reverse order, clearing modifiers on the final release.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyboardChordFrame {
    pub len: u8,
    pub keys: [u8; KEYBOARD_CHORD_KEYS],
    pub mods: Modifiers,
}

impl KeyboardChordFrame {
    pub const EMPTY: Self = Self {
        len: 0,
        keys: [0; KEYBOARD_CHORD_KEYS],
        mods: Modifiers::NONE,
    };

    pub const fn new(len: u8, keys: [u8; KEYBOARD_CHORD_KEYS], mods: Modifiers) -> Self {
        Self { len, keys, mods }
    }

    pub const fn single(scancode: u8, mods: Modifiers) -> Self {
        let mut keys = [0; KEYBOARD_CHORD_KEYS];
        keys[0] = scancode;
        Self::new(1, keys, mods)
    }

    pub const fn scancode(scancode: Scancode, mods: Modifiers) -> Self {
        Self::single(scancode.to_u8(), mods)
    }

    pub fn try_new(scancodes: &[u8], mods: Modifiers) -> Result<Self> {
        if scancodes.len() > KEYBOARD_CHORD_KEYS {
            return Err(Error::SessionLifecycle("keyboard chord too large"));
        }
        let mut keys = [0; KEYBOARD_CHORD_KEYS];
        for (dst, scancode) in keys.iter_mut().zip(scancodes.iter()) {
            let sc = *scancode as u16;
            if sc > 0x65 {
                return Err(Error::SessionLifecycle(
                    "keyboard chord keys must be non-modifier scancodes",
                ));
            }
            *dst = *scancode;
        }
        Ok(Self::new(scancodes.len() as u8, keys, mods))
    }

    pub fn try_scancodes(scancodes: &[Scancode], mods: Modifiers) -> Result<Self> {
        if scancodes.len() > KEYBOARD_CHORD_KEYS {
            return Err(Error::SessionLifecycle("keyboard chord too large"));
        }
        let mut keys = [0; KEYBOARD_CHORD_KEYS];
        for (dst, scancode) in keys.iter_mut().zip(scancodes.iter()) {
            if scancode.is_modifier() {
                return Err(Error::SessionLifecycle(
                    "keyboard chord keys must be non-modifier scancodes",
                ));
            }
            *dst = scancode.to_u8();
        }
        Ok(Self::new(scancodes.len() as u8, keys, mods))
    }

    pub fn edge_frames(self) -> Result<([KeyboardFrame; KEYBOARD_CHORD_EDGES], usize)> {
        let len = self.len as usize;
        if len > KEYBOARD_CHORD_KEYS {
            return Err(Error::SessionLifecycle("keyboard chord length overflow"));
        }
        let mut frames = [KeyboardFrame::EMPTY; KEYBOARD_CHORD_EDGES];
        for (idx, key) in self.keys.iter().take(len).enumerate() {
            frames[idx] = KeyboardFrame::down(*key, self.mods);
        }
        for release_idx in 0..len {
            let key = self.keys[len - 1 - release_idx];
            let mods = if release_idx + 1 == len {
                Modifiers::empty()
            } else {
                self.mods
            };
            frames[len + release_idx] = KeyboardFrame::new(key, false, mods);
        }
        Ok((frames, len * 2))
    }
}

/// One Android framework key event for batching `INJECT_KEYCODE` messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AndroidKeyFrame {
    pub action: u8,
    pub keycode: u32,
    pub repeat: u32,
    pub metastate: u32,
}

impl AndroidKeyFrame {
    pub const EMPTY: Self = Self {
        action: 0,
        keycode: 0,
        repeat: 0,
        metastate: 0,
    };

    pub const fn new(action: u8, keycode: u32, repeat: u32, metastate: u32) -> Self {
        Self {
            action,
            keycode,
            repeat,
            metastate,
        }
    }

    pub const fn typed(
        action: AndroidKeyAction,
        keycode: AndroidKeycode,
        repeat: u32,
        metastate: u32,
    ) -> Self {
        Self::new(action.value(), keycode.value(), repeat, metastate)
    }

    pub const fn down(keycode: AndroidKeycode, metastate: u32) -> Self {
        Self::typed(AndroidKeyAction::DOWN, keycode, 0, metastate)
    }

    pub const fn up(keycode: AndroidKeycode, metastate: u32) -> Self {
        Self::typed(AndroidKeyAction::UP, keycode, 0, metastate)
    }
}

/// One relative UHID mouse report for fixed-buffer batching.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseFrame {
    pub dx: i32,
    pub dy: i32,
    pub buttons: u8,
}

impl MouseFrame {
    pub const EMPTY: Self = Self {
        dx: 0,
        dy: 0,
        buttons: 0,
    };

    pub const fn motion(dx: i32, dy: i32, buttons: u8) -> Self {
        Self { dx, dy, buttons }
    }

    pub fn motion_buttons(dx: i32, dy: i32, buttons: &[MouseButton]) -> Self {
        Self::motion(dx, dy, MouseButton::state(buttons))
    }

    pub const fn buttons(buttons: u8) -> Self {
        Self {
            dx: 0,
            dy: 0,
            buttons,
        }
    }
}

/// One Android absolute scroll event for fixed-buffer batching.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScrollFrame {
    pub x: i32,
    pub y: i32,
    pub hscroll: f32,
    pub vscroll: f32,
    pub buttons: u32,
}

impl ScrollFrame {
    pub const EMPTY: Self = Self {
        x: 0,
        y: 0,
        hscroll: 0.0,
        vscroll: 0.0,
        buttons: 0,
    };

    pub const fn new(x: i32, y: i32, hscroll: f32, vscroll: f32, buttons: u32) -> Self {
        Self {
            x,
            y,
            hscroll,
            vscroll,
            buttons,
        }
    }

    pub const fn scroll(x: i32, y: i32, hscroll: f32, vscroll: f32) -> Self {
        Self::new(x, y, hscroll, vscroll, 0)
    }
}

/// Every operation the AI can request. Cheap to construct; the
/// dispatcher routes each variant to the right `HidSession` method.
#[derive(Debug, Clone)]
pub enum HidCommand {
    TypeText(String),
    TypeTextStrict(String),
    Key {
        scancode: u8,
        pressed: bool,
        mods: Modifiers,
    },
    KeyTap {
        scancode: u8,
        mods: Modifiers,
    },
    KeyBatchFixed {
        len: u8,
        frames: [KeyboardFrame; KEYBOARD_BATCH_FRAMES],
    },
    MouseMotion {
        dx: i32,
        dy: i32,
        buttons: u8,
    },
    MouseButtons {
        buttons: u8,
    },
    MouseScroll {
        hscroll: f32,
        vscroll: f32,
    },
    MouseBatchFixed {
        len: u8,
        frames: [MouseFrame; MOUSE_BATCH_FRAMES],
    },
    /// Inject an Android `KeyEvent.KEYCODE_*` control message.
    InjectKeycode {
        action: u8,
        keycode: u32,
        repeat: u32,
        metastate: u32,
    },
    /// Tap one Android `KeyEvent.KEYCODE_*` with DOWN then UP.
    AndroidKeyTap {
        keycode: u32,
        metastate: u32,
    },
    /// Inject multiple Android `KeyEvent.KEYCODE_*` events from a fixed stack
    /// buffer.
    AndroidKeyBatchFixed {
        len: u8,
        frames: [AndroidKeyFrame; ANDROID_KEY_BATCH_FRAMES],
    },
    /// scrcpy BACK_OR_SCREEN_ON with Android `KeyEvent.ACTION_*`.
    BackOrScreenOn {
        action: u8,
    },
    MultitouchDown {
        id: u64,
        x: i32,
        y: i32,
        pressure: f32,
    },
    MultitouchMove {
        id: u64,
        x: i32,
        y: i32,
        pressure: f32,
    },
    MultitouchUp {
        id: u64,
    },
    MultitouchCancel {
        id: u64,
    },
    /// Send multiple raw touch events through one dispatcher command.
    TouchBatchFixed {
        len: u8,
        frames: [TouchFrame; TOUCH_BATCH_FRAMES],
    },
    /// Absolute scrcpy scroll event at a screen coordinate.
    InjectScroll {
        x: i32,
        y: i32,
        hscroll: f32,
        vscroll: f32,
        buttons: u32,
    },
    /// Inject multiple absolute scrcpy scroll events from a fixed stack
    /// buffer.
    InjectScrollBatchFixed {
        len: u8,
        frames: [ScrollFrame; SCROLL_BATCH_FRAMES],
    },
    GamepadButton {
        btn: GamepadButton,
        pressed: bool,
    },
    /// Replace the full gamepad button frame in one call. Bit layout
    /// follows `GamepadButton` (including dpad flags).
    GamepadButtons {
        buttons: u32,
    },
    GamepadStick {
        axis: GamepadAxis,
        value: f32,
    },
    GamepadStickRaw {
        axis: GamepadAxis,
        value: i16,
    },
    GamepadLeftStickRaw {
        x: i16,
        y: i16,
    },
    GamepadRightStickRaw {
        x: i16,
        y: i16,
    },
    GamepadTriggersRaw {
        left: i16,
        right: i16,
    },
    GamepadSticksRaw {
        left_x: i16,
        left_y: i16,
        right_x: i16,
        right_y: i16,
        left_trigger: i16,
        right_trigger: i16,
    },
    /// Replace the full gamepad frame (buttons + both sticks + both triggers).
    GamepadFrameRaw {
        buttons: u32,
        left_x: i16,
        left_y: i16,
        right_x: i16,
        right_y: i16,
        left_trigger: i16,
        right_trigger: i16,
    },
    /// Replace a single full gamepad frame without server-side dedupe.
    GamepadFrameRawUnchecked(GamepadFrameRaw),
    /// Replace multiple full frames in one command.
    GamepadFrameRawBatch(Vec<GamepadFrameRaw>),
    /// Replace multiple full gamepad frames in a fixed stack buffer.
    GamepadFrameRawBatchFixed {
        len: u8,
        frames: [GamepadFrameRaw; DIRECT_GAMEPAD_BATCH_FRAMES],
    },
    /// Replace multiple full gamepad frames without server-side dedupe,
    /// using a fixed stack buffer.
    GamepadFrameRawBatchFixedUnchecked {
        len: u8,
        frames: [GamepadFrameRaw; DIRECT_GAMEPAD_BATCH_FRAMES],
    },
    /// Replace multiple full frames without server-side dedupe.
    GamepadFrameRawBatchUnchecked(Vec<GamepadFrameRaw>),
    /// Replace a full frame in one fast-path command with a pre-packed
    /// 15-byte gamepad report.
    GamepadPackedFrame([u8; GAMEPAD_FRAME_BYTES]),
    /// Replace multiple full frames in one fast-path command with
    /// pre-packed 15-byte gamepad reports.
    GamepadPackedFrameBatch(Vec<[u8; GAMEPAD_FRAME_BYTES]>),
    /// Replace multiple packed frames in one fast-path command with
    /// a fixed stack buffer.
    GamepadPackedFrameBatchFixed {
        len: u8,
        frames: [[u8; GAMEPAD_FRAME_BYTES]; DIRECT_GAMEPAD_BATCH_FRAMES],
    },
    /// Update the screen dimensions used by subsequent touch injection.
    SetScreenSize {
        width: u16,
        height: u16,
    },
    SetScreenPower {
        on: bool,
    },
    ShowNotifications,
    ShowQuickSettings,
    CollapsePanels,
    RotateDevice,
    /// Ask the device/server to resize its display; distinct from
    /// [`Self::SetScreenSize`], which only updates local touch metadata.
    ResizeDisplay {
        width: u16,
        height: u16,
    },
    SetTorch {
        on: bool,
    },
    CameraZoomIn,
    CameraZoomOut,
    OpenHardKeyboardSettings,
    ResetVideo,
    /// Configure the AI summary pipeline on an AI-enabled scrcpy server.
    AiConfig {
        flags: u8,
        sample_interval_ms: u16,
        feature_dim: u16,
    },
    /// Query the AI extension for summaries or stats since a timestamp.
    AiQuery {
        since_timestamp_ms: u64,
    },
    /// Pause the AI summary pipeline on an AI-enabled scrcpy server.
    AiPause,
    LaunchApp {
        name: String,
    },
    SetClipboard {
        text: String,
        paste: bool,
    },
    /// Write text to the device clipboard with a caller-provided sequence
    /// number so the receiver can wait for the matching ACK_CLIPBOARD.
    SetClipboardSequenced {
        sequence: u64,
        text: String,
        paste: bool,
    },
    /// Request a clipboard payload from the device.
    ///
    /// The reply is delivered asynchronously on the server→host device-message
    /// stream as [`crate::device::DeviceMessage::Clipboard`].
    GetClipboard {
        copy_key: u8,
    },
    /// Flush pending coalesced writes and acknowledge after the dispatcher has
    /// processed all commands before this barrier.
    FlushAck {
        ack: SyncSender<Result<usize>>,
    },
    Flush,
    Close,
}

/// Producer side of the parallel control channel. `Clone` = additional
/// producer to the same channel. `Send` but not `Sync` (mpsc isn't);
/// use `Arc<HidClient>` if needed.
#[derive(Debug, Clone)]
pub struct HidClient {
    tx: SyncSender<HidCommand>,
}

/// Handle for joining the dispatcher thread and recovering the
/// underlying transport.
pub struct HidDispatcher<T: TransportWrite + Send + 'static> {
    join: Option<JoinHandle<Result<T>>>,
}

impl<T: TransportWrite + Send + 'static> std::fmt::Debug for HidDispatcher<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HidDispatcher").finish_non_exhaustive()
    }
}

impl<T: TransportWrite + Send + 'static> HidDispatcher<T> {
    pub fn join(mut self) -> Result<T> {
        let j = self
            .join
            .take()
            .ok_or(Error::DispatcherDown("already joined"))?;
        match j.join() {
            Ok(r) => r,
            Err(_) => Err(Error::DispatcherDown("thread panicked")),
        }
    }
}

impl<T: TransportWrite + Send + 'static> HidSession<T> {
    /// Move this session into a background dispatcher thread and
    /// return a `HidClient` + `HidDispatcher`. The session's
    /// `CoalescingWriter` keeps batching inside the dispatcher.
    pub fn into_client(self) -> Result<(HidClient, HidDispatcher<T>)> {
        self.into_client_with_bound(DEFAULT_CHANNEL_BOUND)
    }

    pub fn into_client_with_bound(self, bound: usize) -> Result<(HidClient, HidDispatcher<T>)> {
        let (tx, rx) = mpsc::sync_channel::<HidCommand>(bound);
        let join = thread::Builder::new()
            .name("android-hid-dispatcher".into())
            .spawn(move || dispatcher_loop(self, rx))
            .map_err(|e| Error::Transport(format!("dispatcher spawn: {e}")))?;
        Ok((HidClient { tx }, HidDispatcher { join: Some(join) }))
    }
}

impl HidClient {
    pub fn try_send(&self, cmd: HidCommand) -> Result<()> {
        self.tx.try_send(cmd).map_err(|e| match e {
            mpsc::TrySendError::Full(_) => Error::SessionLifecycle("channel full (back-pressure)"),
            mpsc::TrySendError::Disconnected(_) => Error::DispatcherDown("channel disconnected"),
        })
    }

    fn send_frame_batch_owned(
        &self,
        frames: Vec<GamepadFrameRaw>,
        dedupe: bool,
    ) -> std::result::Result<(), (Error, Vec<GamepadFrameRaw>)> {
        let cmd = if dedupe {
            HidCommand::GamepadFrameRawBatch(frames)
        } else {
            HidCommand::GamepadFrameRawBatchUnchecked(frames)
        };
        self.tx.send(cmd).map_err(|e| {
            (
                Error::DispatcherDown("channel disconnected"),
                recover_gamepad_batch(e.0),
            )
        })
    }

    fn try_send_frame_batch_owned(
        &self,
        frames: Vec<GamepadFrameRaw>,
        dedupe: bool,
    ) -> std::result::Result<(), (Error, Vec<GamepadFrameRaw>)> {
        let cmd = if dedupe {
            HidCommand::GamepadFrameRawBatch(frames)
        } else {
            HidCommand::GamepadFrameRawBatchUnchecked(frames)
        };
        self.tx.try_send(cmd).map_err(|e| match e {
            mpsc::TrySendError::Full(cmd) => (
                Error::SessionLifecycle("channel full (back-pressure)"),
                recover_gamepad_batch(cmd),
            ),
            mpsc::TrySendError::Disconnected(cmd) => (
                Error::DispatcherDown("channel disconnected"),
                recover_gamepad_batch(cmd),
            ),
        })
    }

    fn send_packed_frame_batch_owned(
        &self,
        frames: Vec<[u8; GAMEPAD_FRAME_BYTES]>,
    ) -> std::result::Result<(), (Error, Vec<[u8; GAMEPAD_FRAME_BYTES]>)> {
        self.tx
            .send(HidCommand::GamepadPackedFrameBatch(frames))
            .map_err(|e| {
                (
                    Error::DispatcherDown("channel disconnected"),
                    recover_packed_gamepad_batch(e.0),
                )
            })
    }

    fn try_send_packed_frame_batch_owned(
        &self,
        frames: Vec<[u8; GAMEPAD_FRAME_BYTES]>,
    ) -> std::result::Result<(), (Error, Vec<[u8; GAMEPAD_FRAME_BYTES]>)> {
        self.tx
            .try_send(HidCommand::GamepadPackedFrameBatch(frames))
            .map_err(|e| match e {
                mpsc::TrySendError::Full(cmd) => (
                    Error::SessionLifecycle("channel full (back-pressure)"),
                    recover_packed_gamepad_batch(cmd),
                ),
                mpsc::TrySendError::Disconnected(cmd) => (
                    Error::DispatcherDown("channel disconnected"),
                    recover_packed_gamepad_batch(cmd),
                ),
            })
    }

    /// Send one relative UHID mouse motion report.
    pub fn mouse_motion(&self, dx: i32, dy: i32, buttons: u8) -> Result<()> {
        self.send(HidCommand::MouseMotion { dx, dy, buttons })
    }

    /// Send one relative UHID mouse motion report with typed buttons.
    pub fn mouse_motion_buttons(&self, dx: i32, dy: i32, buttons: &[MouseButton]) -> Result<()> {
        self.mouse_motion(dx, dy, MouseButton::state(buttons))
    }

    /// Send one UHID mouse button-state report.
    pub fn mouse_buttons(&self, buttons: u8) -> Result<()> {
        self.send(HidCommand::MouseButtons { buttons })
    }

    /// Send one UHID mouse button-state report with typed buttons.
    pub fn mouse_button_state(&self, buttons: &[MouseButton]) -> Result<()> {
        self.mouse_buttons(MouseButton::state(buttons))
    }

    /// Send one UHID mouse scroll sample. Fractional deltas are accumulated by
    /// the dispatcher-side `MouseHid`; no report is emitted until a whole HID
    /// wheel unit is available.
    pub fn mouse_scroll(&self, hscroll: f32, vscroll: f32) -> Result<()> {
        self.send(HidCommand::MouseScroll { hscroll, vscroll })
    }

    /// Create a fixed-stack relative mouse frame batcher bound to this client.
    pub fn mouse_frame_batcher(&self) -> MouseFrameBatcher<'_> {
        MouseFrameBatcher::new(self)
    }

    /// Send fixed-buffer relative UHID mouse frames through one channel send.
    pub fn send_mouse_batch_fixed(
        &self,
        len: usize,
        frames: [MouseFrame; MOUSE_BATCH_FRAMES],
    ) -> Result<()> {
        if len == 0 {
            return Ok(());
        }
        if len > MOUSE_BATCH_FRAMES {
            return Err(Error::SessionLifecycle("mouse batch length too large"));
        }
        if len == 1 {
            let frame = frames[0];
            return self.mouse_motion(frame.dx, frame.dy, frame.buttons);
        }
        self.send(HidCommand::MouseBatchFixed {
            len: len as u8,
            frames,
        })
    }

    /// Non-blocking relative UHID mouse motion report.
    pub fn try_mouse_motion(&self, dx: i32, dy: i32, buttons: u8) -> Result<()> {
        self.try_send(HidCommand::MouseMotion { dx, dy, buttons })
    }

    /// Non-blocking relative UHID mouse motion report with typed buttons.
    pub fn try_mouse_motion_buttons(
        &self,
        dx: i32,
        dy: i32,
        buttons: &[MouseButton],
    ) -> Result<()> {
        self.try_mouse_motion(dx, dy, MouseButton::state(buttons))
    }

    /// Non-blocking UHID mouse button-state report.
    pub fn try_mouse_buttons(&self, buttons: u8) -> Result<()> {
        self.try_send(HidCommand::MouseButtons { buttons })
    }

    /// Non-blocking UHID mouse button-state report with typed buttons.
    pub fn try_mouse_button_state(&self, buttons: &[MouseButton]) -> Result<()> {
        self.try_mouse_buttons(MouseButton::state(buttons))
    }

    /// Non-blocking UHID mouse scroll sample.
    pub fn try_mouse_scroll(&self, hscroll: f32, vscroll: f32) -> Result<()> {
        self.try_send(HidCommand::MouseScroll { hscroll, vscroll })
    }

    /// Non-blocking fixed-buffer relative UHID mouse batch.
    pub fn try_send_mouse_batch_fixed(
        &self,
        len: usize,
        frames: [MouseFrame; MOUSE_BATCH_FRAMES],
    ) -> Result<()> {
        if len == 0 {
            return Ok(());
        }
        if len > MOUSE_BATCH_FRAMES {
            return Err(Error::SessionLifecycle("mouse batch length too large"));
        }
        if len == 1 {
            let frame = frames[0];
            return self.try_mouse_motion(frame.dx, frame.dy, frame.buttons);
        }
        self.try_send(HidCommand::MouseBatchFixed {
            len: len as u8,
            frames,
        })
    }

    /// Send one gamepad button edge.
    pub fn send_button(&self, btn: GamepadButton, pressed: bool) -> Result<()> {
        self.send(HidCommand::GamepadButton { btn, pressed })
    }

    /// Replace all gamepad button bits in one command.
    pub fn send_buttons(&self, buttons: u32) -> Result<()> {
        self.send(HidCommand::GamepadButtons { buttons })
    }

    /// Send one normalized gamepad axis sample.
    ///
    /// Forwarded to dispatcher as `GamepadStick`, which keeps a single
    /// conversion step in one hot path.
    pub fn send_stick(&self, axis: GamepadAxis, value: f32) -> Result<()> {
        self.send(HidCommand::GamepadStick { axis, value })
    }

    /// Send one raw gamepad axis sample.
    pub fn send_stick_raw(&self, axis: GamepadAxis, value: i16) -> Result<()> {
        self.send(HidCommand::GamepadStickRaw { axis, value })
    }

    /// Send both left-stick axes in one command.
    pub fn send_left_stick_raw(&self, x: i16, y: i16) -> Result<()> {
        self.send(HidCommand::GamepadLeftStickRaw { x, y })
    }

    /// Send both right-stick axes in one command.
    pub fn send_right_stick_raw(&self, x: i16, y: i16) -> Result<()> {
        self.send(HidCommand::GamepadRightStickRaw { x, y })
    }

    /// Send both trigger axes in one command.
    pub fn send_triggers_raw(&self, left: i16, right: i16) -> Result<()> {
        self.send(HidCommand::GamepadTriggersRaw { left, right })
    }

    /// Send both sticks + both triggers in one command.
    pub fn send_sticks_raw(
        &self,
        left_x: i16,
        left_y: i16,
        right_x: i16,
        right_y: i16,
        left_trigger: i16,
        right_trigger: i16,
    ) -> Result<()> {
        self.send(HidCommand::GamepadSticksRaw {
            left_x,
            left_y,
            right_x,
            right_y,
            left_trigger,
            right_trigger,
        })
    }

    pub fn send(&self, cmd: HidCommand) -> Result<()> {
        self.tx
            .send(cmd)
            .map_err(|_| Error::DispatcherDown("channel disconnected"))
    }

    /// Type text into the focused field using the dispatcher thread.
    pub fn type_text(&self, text: impl Into<String>) -> Result<()> {
        self.send(HidCommand::TypeText(text.into()))
    }

    /// Type text into the focused field and fail at the next checked
    /// dispatcher boundary if any character cannot be represented as a USB HID
    /// keyboard scancode.
    pub fn type_text_strict(&self, text: impl Into<String>) -> Result<()> {
        self.send(HidCommand::TypeTextStrict(text.into()))
    }

    /// Non-blocking strict text injection.
    pub fn try_type_text_strict(&self, text: impl Into<String>) -> Result<()> {
        self.try_send(HidCommand::TypeTextStrict(text.into()))
    }

    /// Send one raw USB HID keyboard scancode edge.
    pub fn key(&self, scancode: u8, pressed: bool, mods: Modifiers) -> Result<()> {
        self.send(HidCommand::Key {
            scancode,
            pressed,
            mods,
        })
    }

    /// Send one typed USB HID keyboard scancode edge.
    pub fn key_scancode(&self, scancode: Scancode, pressed: bool, mods: Modifiers) -> Result<()> {
        self.key(scancode.to_u8(), pressed, mods)
    }

    /// Press one raw USB HID keyboard scancode.
    pub fn press_key(&self, scancode: u8, mods: Modifiers) -> Result<()> {
        self.key(scancode, true, mods)
    }

    /// Release one raw USB HID keyboard scancode.
    pub fn release_key(&self, scancode: u8, mods: Modifiers) -> Result<()> {
        self.key(scancode, false, mods)
    }

    /// Press and release one raw USB HID keyboard scancode through one
    /// dispatcher command.
    pub fn tap_key(&self, scancode: u8, mods: Modifiers) -> Result<()> {
        self.send(HidCommand::KeyTap { scancode, mods })
    }

    /// Press and release one typed USB HID keyboard scancode through one
    /// dispatcher command.
    pub fn tap_scancode(&self, scancode: Scancode, mods: Modifiers) -> Result<()> {
        self.tap_key(scancode.to_u8(), mods)
    }

    /// Send one keyboard chord as a fixed-buffer edge batch.
    pub fn key_chord(&self, chord: KeyboardChordFrame) -> Result<()> {
        let (frames, len) = chord.edge_frames()?;
        let mut batch = [KeyboardFrame::EMPTY; KEYBOARD_BATCH_FRAMES];
        batch[..len].copy_from_slice(&frames[..len]);
        self.send_key_batch_fixed(len, batch)
    }

    /// Send one keyboard chord from typed scancodes.
    pub fn scancode_chord(&self, scancodes: &[Scancode], mods: Modifiers) -> Result<()> {
        self.key_chord(KeyboardChordFrame::try_scancodes(scancodes, mods)?)
    }

    /// Non-blocking raw USB HID keyboard scancode edge.
    pub fn try_key(&self, scancode: u8, pressed: bool, mods: Modifiers) -> Result<()> {
        self.try_send(HidCommand::Key {
            scancode,
            pressed,
            mods,
        })
    }

    /// Non-blocking typed USB HID keyboard scancode edge.
    pub fn try_key_scancode(
        &self,
        scancode: Scancode,
        pressed: bool,
        mods: Modifiers,
    ) -> Result<()> {
        self.try_key(scancode.to_u8(), pressed, mods)
    }

    /// Non-blocking raw USB HID key tap.
    pub fn try_tap_key(&self, scancode: u8, mods: Modifiers) -> Result<()> {
        self.try_send(HidCommand::KeyTap { scancode, mods })
    }

    /// Non-blocking typed USB HID key tap.
    pub fn try_tap_scancode(&self, scancode: Scancode, mods: Modifiers) -> Result<()> {
        self.try_tap_key(scancode.to_u8(), mods)
    }

    /// Non-blocking keyboard chord edge batch.
    pub fn try_key_chord(&self, chord: KeyboardChordFrame) -> Result<()> {
        let (frames, len) = chord.edge_frames()?;
        let mut batch = [KeyboardFrame::EMPTY; KEYBOARD_BATCH_FRAMES];
        batch[..len].copy_from_slice(&frames[..len]);
        self.try_send_key_batch_fixed(len, batch)
    }

    /// Non-blocking typed keyboard chord edge batch.
    pub fn try_scancode_chord(&self, scancodes: &[Scancode], mods: Modifiers) -> Result<()> {
        self.try_key_chord(KeyboardChordFrame::try_scancodes(scancodes, mods)?)
    }

    /// Send fixed-buffer UHID keyboard edge frames through one channel send.
    pub fn send_key_batch_fixed(
        &self,
        len: usize,
        frames: [KeyboardFrame; KEYBOARD_BATCH_FRAMES],
    ) -> Result<()> {
        if len == 0 {
            return Ok(());
        }
        if len > KEYBOARD_BATCH_FRAMES {
            return Err(Error::SessionLifecycle("keyboard batch length too large"));
        }
        if len == 1 {
            let frame = frames[0];
            return self.key(frame.scancode, frame.pressed, frame.mods);
        }
        self.send(HidCommand::KeyBatchFixed {
            len: len as u8,
            frames,
        })
    }

    /// Non-blocking fixed-buffer UHID keyboard edge batch.
    pub fn try_send_key_batch_fixed(
        &self,
        len: usize,
        frames: [KeyboardFrame; KEYBOARD_BATCH_FRAMES],
    ) -> Result<()> {
        if len == 0 {
            return Ok(());
        }
        if len > KEYBOARD_BATCH_FRAMES {
            return Err(Error::SessionLifecycle("keyboard batch length too large"));
        }
        if len == 1 {
            let frame = frames[0];
            return self.try_key(frame.scancode, frame.pressed, frame.mods);
        }
        self.try_send(HidCommand::KeyBatchFixed {
            len: len as u8,
            frames,
        })
    }

    /// Create a fixed-stack keyboard edge batcher bound to this client.
    pub fn keyboard_frame_batcher(&self) -> KeyboardFrameBatcher<'_> {
        KeyboardFrameBatcher::new(self)
    }

    /// Inject a raw Android `KeyEvent.KEYCODE_*` control message.
    pub fn inject_keycode(
        &self,
        action: u8,
        keycode: u32,
        repeat: u32,
        metastate: u32,
    ) -> Result<()> {
        self.send(HidCommand::InjectKeycode {
            action,
            keycode,
            repeat,
            metastate,
        })
    }

    /// Non-blocking Android keycode injection.
    pub fn try_inject_keycode(
        &self,
        action: u8,
        keycode: u32,
        repeat: u32,
        metastate: u32,
    ) -> Result<()> {
        self.try_send(HidCommand::InjectKeycode {
            action,
            keycode,
            repeat,
            metastate,
        })
    }

    /// Inject a typed Android `KeyEvent.KEYCODE_*` control message.
    pub fn inject_android_keycode(
        &self,
        action: u8,
        keycode: AndroidKeycode,
        repeat: u32,
        metastate: u32,
    ) -> Result<()> {
        self.inject_keycode(action, keycode.value(), repeat, metastate)
    }

    /// Non-blocking typed Android keycode injection.
    pub fn try_inject_android_keycode(
        &self,
        action: u8,
        keycode: AndroidKeycode,
        repeat: u32,
        metastate: u32,
    ) -> Result<()> {
        self.try_inject_keycode(action, keycode.value(), repeat, metastate)
    }

    /// Inject a fully typed Android key event.
    pub fn inject_android_key_event(
        &self,
        action: AndroidKeyAction,
        keycode: AndroidKeycode,
        repeat: u32,
        metastate: u32,
    ) -> Result<()> {
        self.inject_android_keycode(action.value(), keycode, repeat, metastate)
    }

    /// Non-blocking fully typed Android key event injection.
    pub fn try_inject_android_key_event(
        &self,
        action: AndroidKeyAction,
        keycode: AndroidKeycode,
        repeat: u32,
        metastate: u32,
    ) -> Result<()> {
        self.try_inject_android_keycode(action.value(), keycode, repeat, metastate)
    }

    /// Press one typed Android keycode with action DOWN.
    pub fn press_android_key(&self, keycode: AndroidKeycode) -> Result<()> {
        self.inject_android_key_event(AndroidKeyAction::DOWN, keycode, 0, 0)
    }

    /// Non-blocking press of one typed Android keycode with action DOWN.
    pub fn try_press_android_key(&self, keycode: AndroidKeycode) -> Result<()> {
        self.try_inject_android_key_event(AndroidKeyAction::DOWN, keycode, 0, 0)
    }

    /// Release one typed Android keycode with action UP.
    pub fn release_android_key(&self, keycode: AndroidKeycode) -> Result<()> {
        self.inject_android_key_event(AndroidKeyAction::UP, keycode, 0, 0)
    }

    /// Non-blocking release of one typed Android keycode with action UP.
    pub fn try_release_android_key(&self, keycode: AndroidKeycode) -> Result<()> {
        self.try_inject_android_key_event(AndroidKeyAction::UP, keycode, 0, 0)
    }

    /// Press and release one raw Android `KeyEvent.KEYCODE_*` through one
    /// dispatcher command.
    pub fn tap_android_keycode(&self, keycode: u32, metastate: u32) -> Result<()> {
        self.send(HidCommand::AndroidKeyTap { keycode, metastate })
    }

    /// Non-blocking raw Android key tap.
    pub fn try_tap_android_keycode(&self, keycode: u32, metastate: u32) -> Result<()> {
        self.try_send(HidCommand::AndroidKeyTap { keycode, metastate })
    }

    /// Press and release one typed Android keycode through one dispatcher
    /// command.
    pub fn tap_android_key(&self, keycode: AndroidKeycode) -> Result<()> {
        self.tap_android_keycode(keycode.value(), 0)
    }

    /// Non-blocking typed Android key tap.
    pub fn try_tap_android_key(&self, keycode: AndroidKeycode) -> Result<()> {
        self.try_tap_android_keycode(keycode.value(), 0)
    }

    /// Press and release one typed Android keycode with a metastate.
    pub fn tap_android_key_with_metastate(
        &self,
        keycode: AndroidKeycode,
        metastate: u32,
    ) -> Result<()> {
        self.tap_android_keycode(keycode.value(), metastate)
    }

    /// Non-blocking typed Android key tap with a metastate.
    pub fn try_tap_android_key_with_metastate(
        &self,
        keycode: AndroidKeycode,
        metastate: u32,
    ) -> Result<()> {
        self.try_tap_android_keycode(keycode.value(), metastate)
    }

    /// Send fixed-buffer Android key events through one channel send.
    pub fn send_android_key_batch_fixed(
        &self,
        len: usize,
        frames: [AndroidKeyFrame; ANDROID_KEY_BATCH_FRAMES],
    ) -> Result<()> {
        if len == 0 {
            return Ok(());
        }
        if len > ANDROID_KEY_BATCH_FRAMES {
            return Err(Error::SessionLifecycle(
                "android key batch length too large",
            ));
        }
        if len == 1 {
            let frame = frames[0];
            return self.inject_keycode(frame.action, frame.keycode, frame.repeat, frame.metastate);
        }
        self.send(HidCommand::AndroidKeyBatchFixed {
            len: len as u8,
            frames,
        })
    }

    /// Non-blocking fixed-buffer Android key event batch.
    pub fn try_send_android_key_batch_fixed(
        &self,
        len: usize,
        frames: [AndroidKeyFrame; ANDROID_KEY_BATCH_FRAMES],
    ) -> Result<()> {
        if len == 0 {
            return Ok(());
        }
        if len > ANDROID_KEY_BATCH_FRAMES {
            return Err(Error::SessionLifecycle(
                "android key batch length too large",
            ));
        }
        if len == 1 {
            let frame = frames[0];
            return self.try_inject_keycode(
                frame.action,
                frame.keycode,
                frame.repeat,
                frame.metastate,
            );
        }
        self.try_send(HidCommand::AndroidKeyBatchFixed {
            len: len as u8,
            frames,
        })
    }

    /// Create a fixed-stack Android framework key-event batcher.
    pub fn android_key_frame_batcher(&self) -> AndroidKeyFrameBatcher<'_> {
        AndroidKeyFrameBatcher::new(self)
    }

    /// Send scrcpy BACK_OR_SCREEN_ON. If the screen is off, scrcpy wakes it;
    /// otherwise it behaves like Back for the supplied key action.
    pub fn back_or_screen_on(&self, action: AndroidKeyAction) -> Result<()> {
        self.send(HidCommand::BackOrScreenOn {
            action: action.value(),
        })
    }

    /// Non-blocking BACK_OR_SCREEN_ON.
    pub fn try_back_or_screen_on(&self, action: AndroidKeyAction) -> Result<()> {
        self.try_send(HidCommand::BackOrScreenOn {
            action: action.value(),
        })
    }

    /// Press the Home key.
    pub fn press_home(&self) -> Result<()> {
        self.press_android_key(AndroidKeycode::HOME)
    }

    /// Press the Back key.
    pub fn press_back(&self) -> Result<()> {
        self.press_android_key(AndroidKeycode::BACK)
    }

    /// Open the Android recents / app switcher.
    pub fn open_recents(&self) -> Result<()> {
        self.press_android_key(AndroidKeycode::APP_SWITCH)
    }

    /// Press Volume Up.
    pub fn volume_up(&self) -> Result<()> {
        self.press_android_key(AndroidKeycode::VOLUME_UP)
    }

    /// Press Volume Down.
    pub fn volume_down(&self) -> Result<()> {
        self.press_android_key(AndroidKeycode::VOLUME_DOWN)
    }

    /// Press Volume Mute.
    pub fn volume_mute(&self) -> Result<()> {
        self.press_android_key(AndroidKeycode::VOLUME_MUTE)
    }

    /// Update the screen dimensions used by subsequent touch injection.
    pub fn set_screen_size(&self, width: u16, height: u16) -> Result<()> {
        self.send(HidCommand::SetScreenSize { width, height })
    }

    /// Non-blocking screen-size update.
    pub fn try_set_screen_size(&self, width: u16, height: u16) -> Result<()> {
        self.tx
            .try_send(HidCommand::SetScreenSize { width, height })
            .map_err(|e| match e {
                mpsc::TrySendError::Full(_) => {
                    Error::SessionLifecycle("channel full (back-pressure)")
                }
                mpsc::TrySendError::Disconnected(_) => {
                    Error::DispatcherDown("channel disconnected")
                }
            })?;
        Ok(())
    }

    /// Turn the display on/off through scrcpy control.
    pub fn set_screen_power(&self, on: bool) -> Result<()> {
        self.send(HidCommand::SetScreenPower { on })
    }

    /// Expand the notification panel.
    pub fn show_notifications(&self) -> Result<()> {
        self.send(HidCommand::ShowNotifications)
    }

    /// Expand the quick-settings panel.
    pub fn show_quick_settings(&self) -> Result<()> {
        self.send(HidCommand::ShowQuickSettings)
    }

    /// Collapse notification and quick-settings panels.
    pub fn collapse_panels(&self) -> Result<()> {
        self.send(HidCommand::CollapsePanels)
    }

    /// Rotate the device display.
    pub fn rotate_device(&self) -> Result<()> {
        self.send(HidCommand::RotateDevice)
    }

    /// Ask the device/server to resize the display.
    ///
    /// This emits scrcpy `RESIZE_DISPLAY`. Use [`Self::set_screen_size`] when
    /// you only need to update local touch-coordinate metadata.
    pub fn resize_display(&self, width: u16, height: u16) -> Result<()> {
        self.send(HidCommand::ResizeDisplay { width, height })
    }

    /// Toggle the camera torch.
    pub fn set_torch(&self, on: bool) -> Result<()> {
        self.send(HidCommand::SetTorch { on })
    }

    /// Camera zoom in.
    pub fn camera_zoom_in(&self) -> Result<()> {
        self.send(HidCommand::CameraZoomIn)
    }

    /// Camera zoom out.
    pub fn camera_zoom_out(&self) -> Result<()> {
        self.send(HidCommand::CameraZoomOut)
    }

    /// Open the physical-keyboard settings activity.
    pub fn open_hard_keyboard_settings(&self) -> Result<()> {
        self.send(HidCommand::OpenHardKeyboardSettings)
    }

    /// Reset the scrcpy video stream.
    pub fn reset_video(&self) -> Result<()> {
        self.send(HidCommand::ResetVideo)
    }

    /// Configure the AI summary pipeline on an AI-enabled scrcpy server.
    pub fn configure_ai(&self, flags: u8, sample_interval_ms: u16, feature_dim: u16) -> Result<()> {
        self.send(HidCommand::AiConfig {
            flags,
            sample_interval_ms,
            feature_dim,
        })
    }

    /// Non-blocking AI summary pipeline configuration.
    pub fn try_configure_ai(
        &self,
        flags: u8,
        sample_interval_ms: u16,
        feature_dim: u16,
    ) -> Result<()> {
        self.try_send(HidCommand::AiConfig {
            flags,
            sample_interval_ms,
            feature_dim,
        })
    }

    /// Query the AI extension for summaries or stats since a timestamp.
    pub fn query_ai(&self, since_timestamp_ms: u64) -> Result<()> {
        self.send(HidCommand::AiQuery { since_timestamp_ms })
    }

    /// Non-blocking AI extension query.
    pub fn try_query_ai(&self, since_timestamp_ms: u64) -> Result<()> {
        self.try_send(HidCommand::AiQuery { since_timestamp_ms })
    }

    /// Pause the AI summary pipeline on an AI-enabled scrcpy server.
    pub fn pause_ai(&self) -> Result<()> {
        self.send(HidCommand::AiPause)
    }

    /// Non-blocking AI summary pipeline pause.
    pub fn try_pause_ai(&self) -> Result<()> {
        self.try_send(HidCommand::AiPause)
    }

    /// Launch an app by Android package name.
    pub fn launch_app(&self, name: impl Into<String>) -> Result<()> {
        self.send(HidCommand::LaunchApp { name: name.into() })
    }

    /// Set the device clipboard without waiting for an ACK.
    pub fn set_clipboard(&self, text: impl Into<String>, paste: bool) -> Result<()> {
        self.send(HidCommand::SetClipboard {
            text: text.into(),
            paste,
        })
    }

    /// Set the device clipboard with a caller-provided ACK sequence.
    pub fn set_clipboard_sequenced(
        &self,
        sequence: u64,
        text: impl Into<String>,
        paste: bool,
    ) -> Result<()> {
        self.send(HidCommand::SetClipboardSequenced {
            sequence,
            text: text.into(),
            paste,
        })
    }

    /// Create a fixed-stack touch frame batcher bound to this client.
    pub fn touch_frame_batcher(&self) -> TouchFrameBatcher<'_> {
        TouchFrameBatcher::new(self)
    }

    /// Tap one screen coordinate with one batched DOWN/UP dispatcher command.
    pub fn tap(&self, x: i32, y: i32) -> Result<()> {
        self.tap_pointer(TouchPointerId::finger(0), x, y)
    }

    /// Tap one screen coordinate with a typed scrcpy pointer id.
    pub fn tap_pointer(&self, pointer_id: TouchPointerId, x: i32, y: i32) -> Result<()> {
        {
            let mut batch = self.touch_frame_batcher();
            batch.down_pointer(pointer_id, x, y, 1.0)?;
            batch.up_pointer(pointer_id, x, y)?;
            batch.flush()?;
        }
        self.flush_wait().map(|_| ())
    }

    /// Two quick taps at one coordinate, sent as one fixed touch batch.
    pub fn double_tap(&self, x: i32, y: i32) -> Result<()> {
        self.double_tap_pointer(TouchPointerId::finger(0), x, y)
    }

    /// Two quick taps at one coordinate with a typed scrcpy pointer id.
    pub fn double_tap_pointer(&self, pointer_id: TouchPointerId, x: i32, y: i32) -> Result<()> {
        {
            let mut batch = self.touch_frame_batcher();
            batch.down_pointer(pointer_id, x, y, 1.0)?;
            batch.up_pointer(pointer_id, x, y)?;
            batch.down_pointer(pointer_id, x, y, 1.0)?;
            batch.up_pointer(pointer_id, x, y)?;
            batch.flush()?;
        }
        self.flush_wait().map(|_| ())
    }

    /// Swipe from one coordinate to another in `steps` intermediate samples.
    pub fn swipe(&self, from: (i32, i32), to: (i32, i32), steps: usize) -> Result<()> {
        self.swipe_pointer(TouchPointerId::finger(0), from, to, steps)
    }

    /// Swipe from one coordinate to another with a typed scrcpy pointer id.
    pub fn swipe_pointer(
        &self,
        pointer_id: TouchPointerId,
        from: (i32, i32),
        to: (i32, i32),
        steps: usize,
    ) -> Result<()> {
        let steps = steps.max(1);
        {
            let mut batch = self.touch_frame_batcher();
            batch.down_pointer(pointer_id, from.0, from.1, 1.0)?;
            for i in 1..=steps {
                let t = i as f32 / steps as f32;
                let x = lerp_i32(from.0, to.0, t);
                let y = lerp_i32(from.1, to.1, t);
                batch.move_pointer_to(pointer_id, x, y, 1.0)?;
            }
            batch.up_pointer(pointer_id, to.0, to.1)?;
            batch.flush()?;
        }
        self.flush_wait().map(|_| ())
    }

    /// Press, hold for `dur`, then release.
    ///
    /// The DOWN frame is flushed through an acknowledged dispatcher barrier
    /// before sleeping, so the hold interval is not collapsed into one final
    /// batch on the producer side.
    pub fn long_press(&self, x: i32, y: i32, dur: Duration) -> Result<()> {
        self.long_press_pointer(TouchPointerId::finger(0), x, y, dur)
    }

    /// Press, hold, then release with a typed scrcpy pointer id.
    pub fn long_press_pointer(
        &self,
        pointer_id: TouchPointerId,
        x: i32,
        y: i32,
        dur: Duration,
    ) -> Result<()> {
        {
            let mut batch = self.touch_frame_batcher();
            batch.down_pointer(pointer_id, x, y, 1.0)?;
            batch.flush()?;
        }
        self.flush_wait()?;
        thread::sleep(dur);
        {
            let mut batch = self.touch_frame_batcher();
            batch.up_pointer(pointer_id, x, y)?;
            batch.flush()?;
        }
        self.flush_wait().map(|_| ())
    }

    /// Three-finger swipe down, commonly mapped to Android screenshots.
    ///
    /// The caller supplies the screen size used to plan coordinates. Use
    /// [`Self::set_screen_size`] separately when the device-side touch metadata
    /// also needs updating.
    pub fn three_finger_screenshot(&self, screen_w: u16, screen_h: u16) -> Result<()> {
        let w = screen_w as i32;
        let h = screen_h as i32;
        {
            let mut batch = self.touch_frame_batcher();
            for id in 0u64..3 {
                batch.down(id, w / 4 * (id as i32 + 1), h / 4, 1.0)?;
            }
            for step in 1..=10 {
                for id in 0u64..3 {
                    batch.move_to(
                        id,
                        w / 4 * (id as i32 + 1),
                        h / 4 + (h / 2 * step / 10),
                        1.0,
                    )?;
                }
            }
            for id in 0u64..3 {
                batch.up(id, w / 4 * (id as i32 + 1), h * 3 / 4)?;
            }
            batch.flush()?;
        }
        self.flush_wait().map(|_| ())
    }

    /// Absolute scroll with no pressed mouse buttons.
    pub fn scroll(&self, x: i32, y: i32, hscroll: f32, vscroll: f32) -> Result<()> {
        self.scroll_with_buttons(x, y, hscroll, vscroll, 0)
    }

    /// Absolute scroll with an explicit Android mouse-button bitmask.
    pub fn scroll_with_buttons(
        &self,
        x: i32,
        y: i32,
        hscroll: f32,
        vscroll: f32,
        buttons: u32,
    ) -> Result<()> {
        self.send(HidCommand::InjectScroll {
            x,
            y,
            hscroll,
            vscroll,
            buttons,
        })?;
        self.flush_wait().map(|_| ())
    }

    /// Non-blocking absolute scroll. This queues only; it does not wait for a
    /// dispatcher barrier.
    pub fn try_scroll(&self, x: i32, y: i32, hscroll: f32, vscroll: f32) -> Result<()> {
        self.try_scroll_with_buttons(x, y, hscroll, vscroll, 0)
    }

    /// Non-blocking absolute scroll with an explicit button bitmask.
    pub fn try_scroll_with_buttons(
        &self,
        x: i32,
        y: i32,
        hscroll: f32,
        vscroll: f32,
        buttons: u32,
    ) -> Result<()> {
        self.try_send(HidCommand::InjectScroll {
            x,
            y,
            hscroll,
            vscroll,
            buttons,
        })
    }

    /// Send fixed-buffer Android absolute scroll events through one channel
    /// send. This queues only; callers that need checked completion should use
    /// [`Self::flush_wait`].
    pub fn send_scroll_batch_fixed(
        &self,
        len: usize,
        frames: [ScrollFrame; SCROLL_BATCH_FRAMES],
    ) -> Result<()> {
        if len == 0 {
            return Ok(());
        }
        if len > SCROLL_BATCH_FRAMES {
            return Err(Error::SessionLifecycle("scroll batch length too large"));
        }
        if len == 1 {
            let frame = frames[0];
            return self.send(HidCommand::InjectScroll {
                x: frame.x,
                y: frame.y,
                hscroll: frame.hscroll,
                vscroll: frame.vscroll,
                buttons: frame.buttons,
            });
        }
        self.send(HidCommand::InjectScrollBatchFixed {
            len: len as u8,
            frames,
        })
    }

    /// Non-blocking fixed-buffer Android absolute scroll event batch.
    pub fn try_send_scroll_batch_fixed(
        &self,
        len: usize,
        frames: [ScrollFrame; SCROLL_BATCH_FRAMES],
    ) -> Result<()> {
        if len == 0 {
            return Ok(());
        }
        if len > SCROLL_BATCH_FRAMES {
            return Err(Error::SessionLifecycle("scroll batch length too large"));
        }
        if len == 1 {
            let frame = frames[0];
            return self.try_send(HidCommand::InjectScroll {
                x: frame.x,
                y: frame.y,
                hscroll: frame.hscroll,
                vscroll: frame.vscroll,
                buttons: frame.buttons,
            });
        }
        self.try_send(HidCommand::InjectScrollBatchFixed {
            len: len as u8,
            frames,
        })
    }

    /// Create a fixed-stack Android absolute scroll event batcher.
    pub fn scroll_frame_batcher(&self) -> ScrollFrameBatcher<'_> {
        ScrollFrameBatcher::new(self)
    }

    /// Cancel one active touch pointer through the dispatcher.
    pub fn cancel_touch(&self, pointer_id: u64) -> Result<()> {
        self.send(HidCommand::MultitouchCancel { id: pointer_id })?;
        self.flush_wait().map(|_| ())
    }

    /// Cancel one active typed scrcpy touch pointer through the dispatcher.
    pub fn cancel_touch_pointer(&self, pointer_id: TouchPointerId) -> Result<()> {
        self.cancel_touch(pointer_id.value())
    }

    /// Non-blocking touch pointer cancel.
    pub fn try_cancel_touch(&self, pointer_id: u64) -> Result<()> {
        self.try_send(HidCommand::MultitouchCancel { id: pointer_id })
    }

    /// Non-blocking typed scrcpy touch pointer cancel.
    pub fn try_cancel_touch_pointer(&self, pointer_id: TouchPointerId) -> Result<()> {
        self.try_cancel_touch(pointer_id.value())
    }

    /// Send a fixed-buffer batch of touch events through one channel send.
    pub fn send_touch_batch_fixed(
        &self,
        len: usize,
        frames: [TouchFrame; TOUCH_BATCH_FRAMES],
    ) -> Result<()> {
        if len == 0 {
            return Ok(());
        }
        if len > TOUCH_BATCH_FRAMES {
            return Err(Error::SessionLifecycle("touch batch length too large"));
        }
        self.send(HidCommand::TouchBatchFixed {
            len: len as u8,
            frames,
        })
    }

    /// Non-blocking fixed-buffer touch batch send.
    pub fn try_send_touch_batch_fixed(
        &self,
        len: usize,
        frames: [TouchFrame; TOUCH_BATCH_FRAMES],
    ) -> Result<()> {
        if len == 0 {
            return Ok(());
        }
        if len > TOUCH_BATCH_FRAMES {
            return Err(Error::SessionLifecycle("touch batch length too large"));
        }
        self.tx
            .try_send(HidCommand::TouchBatchFixed {
                len: len as u8,
                frames,
            })
            .map_err(|e| match e {
                mpsc::TrySendError::Full(_) => {
                    Error::SessionLifecycle("channel full (back-pressure)")
                }
                mpsc::TrySendError::Disconnected(_) => {
                    Error::DispatcherDown("channel disconnected")
                }
            })?;
        Ok(())
    }

    /// Request the current device clipboard. `copy_key` follows scrcpy:
    /// `0 = none`, `1 = copy`, `2 = cut`.
    ///
    /// The clipboard payload is returned on the device-message read side, not
    /// on this write-only client channel. Use
    /// [`crate::agent::AgentControlSession::get_clipboard_and_wait`] when you
    /// want the request and wait combined.
    pub fn request_clipboard(&self, copy_key: u8) -> Result<()> {
        self.send(HidCommand::GetClipboard { copy_key })
    }

    /// Request the current device clipboard with a typed scrcpy copy-key.
    pub fn request_clipboard_key(&self, copy_key: ClipboardCopyKey) -> Result<()> {
        self.request_clipboard(copy_key.value())
    }

    /// Non-blocking clipboard request.
    pub fn try_request_clipboard(&self, copy_key: u8) -> Result<()> {
        self.tx
            .try_send(HidCommand::GetClipboard { copy_key })
            .map_err(|e| match e {
                mpsc::TrySendError::Full(_) => {
                    Error::SessionLifecycle("channel full (back-pressure)")
                }
                mpsc::TrySendError::Disconnected(_) => {
                    Error::DispatcherDown("channel disconnected")
                }
            })?;
        Ok(())
    }

    /// Non-blocking typed clipboard request.
    pub fn try_request_clipboard_key(&self, copy_key: ClipboardCopyKey) -> Result<()> {
        self.try_request_clipboard(copy_key.value())
    }

    /// Send a batch of full gamepad frames through one channel send.
    /// Useful when your loop already produced many consecutive
    /// frame samples and you want to reduce dispatch overhead.
    pub fn send_frame_batch(&self, frames: Vec<GamepadFrameRaw>) -> Result<()> {
        if frames.is_empty() {
            return Ok(());
        }
        if frames.len() == 1 {
            return self.send_frame(frames[0]);
        }
        self.send(HidCommand::GamepadFrameRawBatch(frames))
    }

    /// Send one full gamepad frame with server-side dedupe.
    ///
    /// Use this when your loop wants unchanged-frame suppression even
    /// when going through `HidClient`.
    pub fn send_frame(&self, frame: GamepadFrameRaw) -> Result<()> {
        self.send(HidCommand::GamepadFrameRaw {
            buttons: frame.buttons,
            left_x: frame.left_x,
            left_y: frame.left_y,
            right_x: frame.right_x,
            right_y: frame.right_y,
            left_trigger: frame.left_trigger,
            right_trigger: frame.right_trigger,
        })
    }

    fn send_frame_batch_fixed_internal(
        &self,
        len: usize,
        frames: [GamepadFrameRaw; DIRECT_GAMEPAD_BATCH_FRAMES],
        dedupe: bool,
    ) -> Result<()> {
        if len == 0 {
            return Ok(());
        }
        if len > DIRECT_GAMEPAD_BATCH_FRAMES {
            return Err(Error::SessionLifecycle("frame batch fixed length overflow"));
        }
        if len == 1 {
            let frame = frames[0];
            if dedupe {
                return self.send_frame(frame);
            }
            return self.send_frame_unchecked(frame);
        }
        if dedupe {
            self.send(HidCommand::GamepadFrameRawBatchFixed {
                len: len as u8,
                frames,
            })
        } else {
            self.send(HidCommand::GamepadFrameRawBatchFixedUnchecked {
                len: len as u8,
                frames,
            })
        }
    }

    /// Send full gamepad frames with a fixed stack buffer and state dedupe.
    ///
    /// Use when your loop already keeps a `[GamepadFrameRaw; 32]` ring and
    /// wants to avoid `Vec` allocation in the hot path.
    pub fn send_frame_batch_fixed(
        &self,
        len: usize,
        frames: [GamepadFrameRaw; DIRECT_GAMEPAD_BATCH_FRAMES],
    ) -> Result<()> {
        self.send_frame_batch_fixed_internal(len, frames, true)
    }

    /// Send full gamepad frames with a fixed stack buffer and no state dedupe.
    ///
    /// Use this when frame cadence matters and duplicate frames must still be
    /// written to the device.
    pub fn send_frame_batch_fixed_unchecked(
        &self,
        len: usize,
        frames: [GamepadFrameRaw; DIRECT_GAMEPAD_BATCH_FRAMES],
    ) -> Result<()> {
        self.send_frame_batch_fixed_internal(len, frames, false)
    }

    fn try_send_frame_batch_fixed_internal(
        &self,
        len: usize,
        frames: [GamepadFrameRaw; DIRECT_GAMEPAD_BATCH_FRAMES],
        dedupe: bool,
    ) -> Result<()> {
        if len == 0 {
            return Ok(());
        }
        if len > DIRECT_GAMEPAD_BATCH_FRAMES {
            return Err(Error::SessionLifecycle("frame batch fixed length overflow"));
        }
        if len == 1 {
            let frame = frames[0];
            if dedupe {
                return self.try_send_frame(frame);
            }
            return self.try_send_frame_unchecked(frame);
        }
        let cmd = if dedupe {
            HidCommand::GamepadFrameRawBatchFixed {
                len: len as u8,
                frames,
            }
        } else {
            HidCommand::GamepadFrameRawBatchFixedUnchecked {
                len: len as u8,
                frames,
            }
        };
        self.tx.try_send(cmd).map_err(|e| match e {
            mpsc::TrySendError::Full(_) => Error::SessionLifecycle("channel full (back-pressure)"),
            mpsc::TrySendError::Disconnected(_) => Error::DispatcherDown("channel disconnected"),
        })?;
        Ok(())
    }

    /// Send a batch of full gamepad frames without state-dedupe.
    ///
    /// Use this when the caller owns a complete frame stream and wants
    /// every frame to be written, even if duplicates occur.
    pub fn send_frame_batch_unchecked(&self, frames: Vec<GamepadFrameRaw>) -> Result<()> {
        if frames.is_empty() {
            return Ok(());
        }
        if frames.len() == 1 {
            return self.send_frame_unchecked(frames[0]);
        }
        self.send(HidCommand::GamepadFrameRawBatchUnchecked(frames))
    }

    /// Send one full gamepad frame without state-dedupe.
    ///
    /// Use this when your loop already owns the whole frame and wants
    /// every sample on the wire.
    pub fn send_frame_unchecked(&self, frame: GamepadFrameRaw) -> Result<()> {
        self.send(HidCommand::GamepadFrameRawUnchecked(frame))
    }

    /// Non-blocking single-frame unchecked send.
    ///
    /// Drops to `SessionLifecycle` when the internal queue is full.
    pub fn try_send_frame_unchecked(&self, frame: GamepadFrameRaw) -> Result<()> {
        self.tx
            .try_send(HidCommand::GamepadFrameRawUnchecked(frame))
            .map_err(|e| match e {
                mpsc::TrySendError::Full(_) => {
                    Error::SessionLifecycle("channel full (back-pressure)")
                }
                mpsc::TrySendError::Disconnected(_) => {
                    Error::DispatcherDown("channel disconnected")
                }
            })?;
        Ok(())
    }

    /// Non-blocking gamepad button edge.
    pub fn try_send_button(&self, btn: GamepadButton, pressed: bool) -> Result<()> {
        self.tx
            .try_send(HidCommand::GamepadButton { btn, pressed })
            .map_err(|e| match e {
                mpsc::TrySendError::Full(_) => {
                    Error::SessionLifecycle("channel full (back-pressure)")
                }
                mpsc::TrySendError::Disconnected(_) => {
                    Error::DispatcherDown("channel disconnected")
                }
            })?;
        Ok(())
    }

    /// Non-blocking button-bitframe update.
    pub fn try_send_buttons(&self, buttons: u32) -> Result<()> {
        self.tx
            .try_send(HidCommand::GamepadButtons { buttons })
            .map_err(|e| match e {
                mpsc::TrySendError::Full(_) => {
                    Error::SessionLifecycle("channel full (back-pressure)")
                }
                mpsc::TrySendError::Disconnected(_) => {
                    Error::DispatcherDown("channel disconnected")
                }
            })?;
        Ok(())
    }

    /// Non-blocking normalized axis update.
    pub fn try_send_stick(&self, axis: GamepadAxis, value: f32) -> Result<()> {
        self.tx
            .try_send(HidCommand::GamepadStick { axis, value })
            .map_err(|e| match e {
                mpsc::TrySendError::Full(_) => {
                    Error::SessionLifecycle("channel full (back-pressure)")
                }
                mpsc::TrySendError::Disconnected(_) => {
                    Error::DispatcherDown("channel disconnected")
                }
            })?;
        Ok(())
    }

    /// Non-blocking raw axis update.
    pub fn try_send_stick_raw(&self, axis: GamepadAxis, value: i16) -> Result<()> {
        self.tx
            .try_send(HidCommand::GamepadStickRaw { axis, value })
            .map_err(|e| match e {
                mpsc::TrySendError::Full(_) => {
                    Error::SessionLifecycle("channel full (back-pressure)")
                }
                mpsc::TrySendError::Disconnected(_) => {
                    Error::DispatcherDown("channel disconnected")
                }
            })?;
        Ok(())
    }

    /// Non-blocking left-stick update.
    pub fn try_send_left_stick_raw(&self, x: i16, y: i16) -> Result<()> {
        self.tx
            .try_send(HidCommand::GamepadLeftStickRaw { x, y })
            .map_err(|e| match e {
                mpsc::TrySendError::Full(_) => {
                    Error::SessionLifecycle("channel full (back-pressure)")
                }
                mpsc::TrySendError::Disconnected(_) => {
                    Error::DispatcherDown("channel disconnected")
                }
            })?;
        Ok(())
    }

    /// Non-blocking right-stick update.
    pub fn try_send_right_stick_raw(&self, x: i16, y: i16) -> Result<()> {
        self.tx
            .try_send(HidCommand::GamepadRightStickRaw { x, y })
            .map_err(|e| match e {
                mpsc::TrySendError::Full(_) => {
                    Error::SessionLifecycle("channel full (back-pressure)")
                }
                mpsc::TrySendError::Disconnected(_) => {
                    Error::DispatcherDown("channel disconnected")
                }
            })?;
        Ok(())
    }

    /// Non-blocking trigger-pair update.
    pub fn try_send_triggers_raw(&self, left: i16, right: i16) -> Result<()> {
        self.tx
            .try_send(HidCommand::GamepadTriggersRaw { left, right })
            .map_err(|e| match e {
                mpsc::TrySendError::Full(_) => {
                    Error::SessionLifecycle("channel full (back-pressure)")
                }
                mpsc::TrySendError::Disconnected(_) => {
                    Error::DispatcherDown("channel disconnected")
                }
            })?;
        Ok(())
    }

    /// Non-blocking full-axis + trigger update.
    pub fn try_send_sticks_raw(
        &self,
        left_x: i16,
        left_y: i16,
        right_x: i16,
        right_y: i16,
        left_trigger: i16,
        right_trigger: i16,
    ) -> Result<()> {
        self.tx
            .try_send(HidCommand::GamepadSticksRaw {
                left_x,
                left_y,
                right_x,
                right_y,
                left_trigger,
                right_trigger,
            })
            .map_err(|e| match e {
                mpsc::TrySendError::Full(_) => {
                    Error::SessionLifecycle("channel full (back-pressure)")
                }
                mpsc::TrySendError::Disconnected(_) => {
                    Error::DispatcherDown("channel disconnected")
                }
            })?;
        Ok(())
    }

    /// Non-blocking single-frame send with server-side dedupe.
    ///
    /// Drops to `SessionLifecycle` when the internal queue is full.
    pub fn try_send_frame(&self, frame: GamepadFrameRaw) -> Result<()> {
        self.tx
            .try_send(HidCommand::GamepadFrameRaw {
                buttons: frame.buttons,
                left_x: frame.left_x,
                left_y: frame.left_y,
                right_x: frame.right_x,
                right_y: frame.right_y,
                left_trigger: frame.left_trigger,
                right_trigger: frame.right_trigger,
            })
            .map_err(|e| match e {
                mpsc::TrySendError::Full(_) => {
                    Error::SessionLifecycle("channel full (back-pressure)")
                }
                mpsc::TrySendError::Disconnected(_) => {
                    Error::DispatcherDown("channel disconnected")
                }
            })?;
        Ok(())
    }

    /// Send one packed 15-byte gamepad report through one channel send.
    pub fn send_frame_packed(&self, frame: [u8; GAMEPAD_FRAME_BYTES]) -> Result<()> {
        self.send(HidCommand::GamepadPackedFrame(frame))
    }

    /// Send packed 15-byte gamepad frames through one channel send.
    /// This is the lowest-overhead path when the upstream loop already
    /// emits raw HID payloads.
    pub fn send_frame_packed_batch(&self, frames: Vec<[u8; GAMEPAD_FRAME_BYTES]>) -> Result<()> {
        if frames.is_empty() {
            return Ok(());
        }
        if frames.len() == 1 {
            return self.send_frame_packed(frames[0]);
        }
        self.send(HidCommand::GamepadPackedFrameBatch(frames))
    }

    fn send_frame_packed_batch_fixed_internal(
        &self,
        len: usize,
        frames: [[u8; GAMEPAD_FRAME_BYTES]; DIRECT_GAMEPAD_BATCH_FRAMES],
    ) -> Result<()> {
        if len == 0 {
            return Ok(());
        }
        if len > DIRECT_GAMEPAD_BATCH_FRAMES {
            return Err(Error::SessionLifecycle(
                "frame packed batch fixed length overflow",
            ));
        }
        if len == 1 {
            return self.send_frame_packed(frames[0]);
        }
        self.send(HidCommand::GamepadPackedFrameBatchFixed {
            len: len as u8,
            frames,
        })
    }

    /// Send packed 15-byte gamepad frames with a fixed stack buffer.
    ///
    /// Use when your loop already keeps a `[[u8; 15]; 32]` ring and
    /// wants to avoid `Vec` allocation in the hot path.
    pub fn send_frame_packed_batch_fixed(
        &self,
        len: usize,
        frames: [[u8; GAMEPAD_FRAME_BYTES]; DIRECT_GAMEPAD_BATCH_FRAMES],
    ) -> Result<()> {
        self.send_frame_packed_batch_fixed_internal(len, frames)
    }

    /// Non-blocking packed batch send. Drops to
    /// `SessionLifecycle` when the internal queue is full.
    pub fn try_send_frame_packed_batch(
        &self,
        frames: Vec<[u8; GAMEPAD_FRAME_BYTES]>,
    ) -> Result<()> {
        if frames.is_empty() {
            return Ok(());
        }
        if frames.len() == 1 {
            return self.try_send_frame_packed(frames[0]);
        }
        self.tx
            .try_send(HidCommand::GamepadPackedFrameBatch(frames))
            .map_err(|e| match e {
                mpsc::TrySendError::Full(_) => {
                    Error::SessionLifecycle("channel full (back-pressure)")
                }
                mpsc::TrySendError::Disconnected(_) => {
                    Error::DispatcherDown("channel disconnected")
                }
            })?;
        Ok(())
    }

    /// Non-blocking packed fixed-buffer batch send. Drops to
    /// `SessionLifecycle` when the internal queue is full.
    pub fn try_send_frame_packed_batch_fixed(
        &self,
        len: usize,
        frames: [[u8; GAMEPAD_FRAME_BYTES]; DIRECT_GAMEPAD_BATCH_FRAMES],
    ) -> Result<()> {
        if len == 0 {
            return Ok(());
        }
        if len > DIRECT_GAMEPAD_BATCH_FRAMES {
            return Err(Error::SessionLifecycle(
                "frame packed batch fixed length overflow",
            ));
        }
        if len == 1 {
            return self.try_send_frame_packed(frames[0]);
        }
        self.tx
            .try_send(HidCommand::GamepadPackedFrameBatchFixed {
                len: len as u8,
                frames,
            })
            .map_err(|e| match e {
                mpsc::TrySendError::Full(_) => {
                    Error::SessionLifecycle("channel full (back-pressure)")
                }
                mpsc::TrySendError::Disconnected(_) => {
                    Error::DispatcherDown("channel disconnected")
                }
            })?;
        Ok(())
    }

    /// Non-blocking packed frame send.
    pub fn try_send_frame_packed(&self, frame: [u8; GAMEPAD_FRAME_BYTES]) -> Result<()> {
        self.tx
            .try_send(HidCommand::GamepadPackedFrame(frame))
            .map_err(|e| match e {
                mpsc::TrySendError::Full(_) => {
                    Error::SessionLifecycle("channel full (back-pressure)")
                }
                mpsc::TrySendError::Disconnected(_) => {
                    Error::DispatcherDown("channel disconnected")
                }
            })?;
        Ok(())
    }

    /// Non-blocking batch send. Drops to `SessionLifecycle` when the
    /// internal queue is full.
    pub fn try_send_frame_batch(&self, frames: Vec<GamepadFrameRaw>) -> Result<()> {
        if frames.is_empty() {
            return Ok(());
        }
        if frames.len() == 1 {
            return self.try_send_frame(frames[0]);
        }
        self.tx
            .try_send(HidCommand::GamepadFrameRawBatch(frames))
            .map_err(|e| match e {
                mpsc::TrySendError::Full(_) => {
                    Error::SessionLifecycle("channel full (back-pressure)")
                }
                mpsc::TrySendError::Disconnected(_) => {
                    Error::DispatcherDown("channel disconnected")
                }
            })?;
        Ok(())
    }

    /// Non-blocking full-frame batch send without state-dedupe.
    ///
    /// Use this when frame cadence matters more than transport payload
    /// suppression and drops due to queue-full can be handled upstream.
    pub fn try_send_frame_batch_unchecked(&self, frames: Vec<GamepadFrameRaw>) -> Result<()> {
        if frames.is_empty() {
            return Ok(());
        }
        if frames.len() == 1 {
            return self.try_send_frame_unchecked(frames[0]);
        }
        self.tx
            .try_send(HidCommand::GamepadFrameRawBatchUnchecked(frames))
            .map_err(|e| match e {
                mpsc::TrySendError::Full(_) => {
                    Error::SessionLifecycle("channel full (back-pressure)")
                }
                mpsc::TrySendError::Disconnected(_) => {
                    Error::DispatcherDown("channel disconnected")
                }
            })?;
        Ok(())
    }

    /// Non-blocking fixed-size deduped frame batch send. Drops to
    /// `SessionLifecycle` when the internal queue is full.
    pub fn try_send_frame_batch_fixed(
        &self,
        len: usize,
        frames: [GamepadFrameRaw; DIRECT_GAMEPAD_BATCH_FRAMES],
    ) -> Result<()> {
        self.try_send_frame_batch_fixed_internal(len, frames, true)
    }

    /// Non-blocking fixed-size unchecked frame batch send. Drops to
    /// `SessionLifecycle` when the internal queue is full.
    pub fn try_send_frame_batch_fixed_unchecked(
        &self,
        len: usize,
        frames: [GamepadFrameRaw; DIRECT_GAMEPAD_BATCH_FRAMES],
    ) -> Result<()> {
        self.try_send_frame_batch_fixed_internal(len, frames, false)
    }

    pub fn close(&self) {
        let _ = self.tx.send(HidCommand::Close);
    }

    /// Flush all prior work, report any dispatcher-side command error, then
    /// request dispatcher shutdown.
    ///
    /// This is the checked shutdown path for agent runtimes. It preserves the
    /// fire-and-forget behavior of [`Self::close`] for hot paths, while giving
    /// callers a deterministic boundary when they need to know whether queued
    /// commands actually executed.
    pub fn close_wait(&self) -> Result<()> {
        let result = self.flush_wait().map(|_| ());
        self.close();
        result
    }

    /// Flush any pending coalesced UHID_INPUT writes immediately.
    pub fn flush(&self) -> Result<()> {
        self.send(HidCommand::Flush)
    }

    /// Flush pending coalesced writes and wait until the dispatcher has
    /// processed all commands queued before this barrier.
    pub fn flush_wait(&self) -> Result<usize> {
        let (ack_tx, ack_rx) = mpsc::sync_channel(1);
        self.send(HidCommand::FlushAck { ack: ack_tx })?;
        ack_rx
            .recv()
            .map_err(|_| Error::DispatcherDown("flush acknowledgement dropped"))?
    }

    /// Non-blocking enqueue of a checked flush barrier.
    ///
    /// This returns a back-pressure error if the dispatcher command channel is
    /// already full. Once the barrier is accepted, it waits for the dispatcher
    /// acknowledgement and surfaces prior command errors like [`Self::flush_wait`].
    pub fn try_flush_wait(&self) -> Result<usize> {
        let (ack_tx, ack_rx) = mpsc::sync_channel(1);
        self.tx
            .try_send(HidCommand::FlushAck { ack: ack_tx })
            .map_err(|e| match e {
                mpsc::TrySendError::Full(_) => {
                    Error::SessionLifecycle("channel full (back-pressure)")
                }
                mpsc::TrySendError::Disconnected(_) => {
                    Error::DispatcherDown("channel disconnected")
                }
            })?;
        ack_rx
            .recv()
            .map_err(|_| Error::DispatcherDown("flush acknowledgement dropped"))?
    }

    /// Non-blocking flush request for coalesced UHID_INPUT writes.
    pub fn try_flush(&self) -> Result<()> {
        self.tx.try_send(HidCommand::Flush).map_err(|e| match e {
            mpsc::TrySendError::Full(_) => Error::SessionLifecycle("channel full (back-pressure)"),
            mpsc::TrySendError::Disconnected(_) => Error::DispatcherDown("channel disconnected"),
        })?;
        Ok(())
    }
}

/// Batched sender for high-rate touch gesture producers sharing one
/// `HidClient` channel.
///
/// This is useful when an agent planner already produced a whole tap,
/// drag, multi-sample swipe, or synthetic gesture path. `push` calls stay on
/// the caller thread and only cross the dispatcher channel when the fixed
/// stack buffer is full or when [`Self::flush`] is called.
#[derive(Debug)]
pub struct TouchFrameBatcher<'a> {
    client: &'a HidClient,
    frames: [TouchFrame; TOUCH_BATCH_FRAMES],
    len: usize,
}

impl<'a> TouchFrameBatcher<'a> {
    /// Create an empty touch frame batcher.
    pub fn new(client: &'a HidClient) -> Self {
        Self {
            client,
            frames: [TouchFrame::EMPTY; TOUCH_BATCH_FRAMES],
            len: 0,
        }
    }

    /// Number of frames currently buffered on the caller thread.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether there are no pending frames in the local buffer.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Push one touch frame, flushing the local batch when full.
    pub fn push(&mut self, frame: TouchFrame) -> Result<()> {
        if self.len == TOUCH_BATCH_FRAMES {
            self.flush()?;
        }
        self.frames[self.len] = frame;
        self.len += 1;
        Ok(())
    }

    /// Push one touch frame using non-blocking channel sends.
    pub fn try_push(&mut self, frame: TouchFrame) -> Result<()> {
        if self.len == TOUCH_BATCH_FRAMES {
            self.try_flush()?;
        }
        self.frames[self.len] = frame;
        self.len += 1;
        Ok(())
    }

    /// Push several touch frames.
    pub fn push_many(&mut self, frames: impl IntoIterator<Item = TouchFrame>) -> Result<()> {
        for frame in frames {
            self.push(frame)?;
        }
        Ok(())
    }

    /// Push a contiguous slice of touch frames with fewer boundary checks than
    /// per-item iteration.
    pub fn push_many_slice(&mut self, frames: &[TouchFrame]) -> Result<()> {
        let mut idx = 0usize;
        while idx < frames.len() {
            if self.len == TOUCH_BATCH_FRAMES {
                self.flush()?;
            }
            let room = TOUCH_BATCH_FRAMES - self.len;
            let n = (frames.len() - idx).min(room);
            self.frames[self.len..self.len + n].copy_from_slice(&frames[idx..idx + n]);
            self.len += n;
            idx += n;
            if self.len == TOUCH_BATCH_FRAMES {
                self.flush()?;
            }
        }
        Ok(())
    }

    /// Push several touch frames using non-blocking channel sends.
    pub fn try_push_many(&mut self, frames: impl IntoIterator<Item = TouchFrame>) -> Result<()> {
        for frame in frames {
            self.try_push(frame)?;
        }
        Ok(())
    }

    /// Push a contiguous slice using non-blocking channel sends.
    pub fn try_push_many_slice(&mut self, frames: &[TouchFrame]) -> Result<()> {
        let mut idx = 0usize;
        while idx < frames.len() {
            if self.len == TOUCH_BATCH_FRAMES {
                self.try_flush()?;
            }
            let room = TOUCH_BATCH_FRAMES - self.len;
            let n = (frames.len() - idx).min(room);
            self.frames[self.len..self.len + n].copy_from_slice(&frames[idx..idx + n]);
            self.len += n;
            idx += n;
            if self.len == TOUCH_BATCH_FRAMES {
                self.try_flush()?;
            }
        }
        Ok(())
    }

    /// Queue a touch down frame.
    pub fn down(&mut self, pointer_id: u64, x: i32, y: i32, pressure: f32) -> Result<()> {
        self.push(TouchFrame::with_action(
            TouchAction::DOWN,
            pointer_id,
            x,
            y,
            pressure,
        ))
    }

    /// Queue a touch down frame with a typed scrcpy pointer id.
    pub fn down_pointer(
        &mut self,
        pointer_id: TouchPointerId,
        x: i32,
        y: i32,
        pressure: f32,
    ) -> Result<()> {
        self.down(pointer_id.value(), x, y, pressure)
    }

    /// Queue a touch down frame using non-blocking channel sends.
    pub fn try_down(&mut self, pointer_id: u64, x: i32, y: i32, pressure: f32) -> Result<()> {
        self.try_push(TouchFrame::with_action(
            TouchAction::DOWN,
            pointer_id,
            x,
            y,
            pressure,
        ))
    }

    /// Queue a touch down frame with a typed scrcpy pointer id using
    /// non-blocking channel sends.
    pub fn try_down_pointer(
        &mut self,
        pointer_id: TouchPointerId,
        x: i32,
        y: i32,
        pressure: f32,
    ) -> Result<()> {
        self.try_down(pointer_id.value(), x, y, pressure)
    }

    /// Queue a touch move frame.
    pub fn move_to(&mut self, pointer_id: u64, x: i32, y: i32, pressure: f32) -> Result<()> {
        self.push(TouchFrame::with_action(
            TouchAction::MOVE,
            pointer_id,
            x,
            y,
            pressure,
        ))
    }

    /// Queue a touch move frame with a typed scrcpy pointer id.
    pub fn move_pointer_to(
        &mut self,
        pointer_id: TouchPointerId,
        x: i32,
        y: i32,
        pressure: f32,
    ) -> Result<()> {
        self.move_to(pointer_id.value(), x, y, pressure)
    }

    /// Queue a touch move frame using non-blocking channel sends.
    pub fn try_move_to(&mut self, pointer_id: u64, x: i32, y: i32, pressure: f32) -> Result<()> {
        self.try_push(TouchFrame::with_action(
            TouchAction::MOVE,
            pointer_id,
            x,
            y,
            pressure,
        ))
    }

    /// Queue a touch move frame with a typed scrcpy pointer id using
    /// non-blocking channel sends.
    pub fn try_move_pointer_to(
        &mut self,
        pointer_id: TouchPointerId,
        x: i32,
        y: i32,
        pressure: f32,
    ) -> Result<()> {
        self.try_move_to(pointer_id.value(), x, y, pressure)
    }

    /// Queue a touch up frame at the last known coordinate.
    pub fn up(&mut self, pointer_id: u64, x: i32, y: i32) -> Result<()> {
        self.push(TouchFrame::with_action(
            TouchAction::UP,
            pointer_id,
            x,
            y,
            0.0,
        ))
    }

    /// Queue a touch up frame with a typed scrcpy pointer id.
    pub fn up_pointer(&mut self, pointer_id: TouchPointerId, x: i32, y: i32) -> Result<()> {
        self.up(pointer_id.value(), x, y)
    }

    /// Queue a touch up frame using non-blocking channel sends.
    pub fn try_up(&mut self, pointer_id: u64, x: i32, y: i32) -> Result<()> {
        self.try_push(TouchFrame::with_action(
            TouchAction::UP,
            pointer_id,
            x,
            y,
            0.0,
        ))
    }

    /// Queue a touch up frame with a typed scrcpy pointer id using non-blocking
    /// channel sends.
    pub fn try_up_pointer(&mut self, pointer_id: TouchPointerId, x: i32, y: i32) -> Result<()> {
        self.try_up(pointer_id.value(), x, y)
    }

    /// Queue a touch cancel frame.
    pub fn cancel(&mut self, pointer_id: u64) -> Result<()> {
        self.push(TouchFrame::with_action(
            TouchAction::CANCEL,
            pointer_id,
            0,
            0,
            0.0,
        ))
    }

    /// Queue a touch cancel frame with a typed scrcpy pointer id.
    pub fn cancel_pointer(&mut self, pointer_id: TouchPointerId) -> Result<()> {
        self.cancel(pointer_id.value())
    }

    /// Queue a touch cancel frame using non-blocking channel sends.
    pub fn try_cancel(&mut self, pointer_id: u64) -> Result<()> {
        self.try_push(TouchFrame::with_action(
            TouchAction::CANCEL,
            pointer_id,
            0,
            0,
            0.0,
        ))
    }

    /// Queue a touch cancel frame with a typed scrcpy pointer id using
    /// non-blocking channel sends.
    pub fn try_cancel_pointer(&mut self, pointer_id: TouchPointerId) -> Result<()> {
        self.try_cancel(pointer_id.value())
    }

    /// Flush buffered frames through the blocking sender.
    pub fn flush(&mut self) -> Result<()> {
        if self.len == 0 {
            return Ok(());
        }
        let result = self.client.send_touch_batch_fixed(self.len, self.frames);
        if result.is_ok() {
            self.frames = [TouchFrame::EMPTY; TOUCH_BATCH_FRAMES];
            self.len = 0;
        }
        result
    }

    /// Flush buffered frames through the non-blocking sender.
    pub fn try_flush(&mut self) -> Result<()> {
        if self.len == 0 {
            return Ok(());
        }
        let result = self
            .client
            .try_send_touch_batch_fixed(self.len, self.frames);
        if result.is_ok() {
            self.frames = [TouchFrame::EMPTY; TOUCH_BATCH_FRAMES];
            self.len = 0;
        }
        result
    }
}

impl Drop for TouchFrameBatcher<'_> {
    fn drop(&mut self) {
        let _ = self.try_flush();
    }
}

/// Batched sender for high-rate UHID keyboard edge producers sharing one
/// `HidClient` channel.
///
/// This is useful when an agent planner already produced a low-level key edge
/// sequence, hotkey chord, or macro. Edges stay on the caller thread until the
/// fixed stack buffer is full or [`Self::flush`] is called.
#[derive(Debug)]
pub struct KeyboardFrameBatcher<'a> {
    client: &'a HidClient,
    frames: [KeyboardFrame; KEYBOARD_BATCH_FRAMES],
    len: usize,
}

impl<'a> KeyboardFrameBatcher<'a> {
    /// Create an empty keyboard edge batcher.
    pub fn new(client: &'a HidClient) -> Self {
        Self {
            client,
            frames: [KeyboardFrame::EMPTY; KEYBOARD_BATCH_FRAMES],
            len: 0,
        }
    }

    /// Number of frames currently buffered on the caller thread.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether there are no pending frames in the local buffer.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Push one keyboard edge, flushing the local batch when full.
    pub fn push(&mut self, frame: KeyboardFrame) -> Result<()> {
        if self.len == KEYBOARD_BATCH_FRAMES {
            self.flush()?;
        }
        self.frames[self.len] = frame;
        self.len += 1;
        Ok(())
    }

    /// Push one keyboard edge using non-blocking channel sends.
    pub fn try_push(&mut self, frame: KeyboardFrame) -> Result<()> {
        if self.len == KEYBOARD_BATCH_FRAMES {
            self.try_flush()?;
        }
        self.frames[self.len] = frame;
        self.len += 1;
        Ok(())
    }

    /// Push several keyboard edges.
    pub fn push_many(&mut self, frames: impl IntoIterator<Item = KeyboardFrame>) -> Result<()> {
        for frame in frames {
            self.push(frame)?;
        }
        Ok(())
    }

    /// Push a contiguous slice of keyboard edges with fewer boundary checks
    /// than per-item iteration.
    pub fn push_many_slice(&mut self, frames: &[KeyboardFrame]) -> Result<()> {
        let mut idx = 0usize;
        while idx < frames.len() {
            if self.len == KEYBOARD_BATCH_FRAMES {
                self.flush()?;
            }
            let room = KEYBOARD_BATCH_FRAMES - self.len;
            let n = (frames.len() - idx).min(room);
            self.frames[self.len..self.len + n].copy_from_slice(&frames[idx..idx + n]);
            self.len += n;
            idx += n;
            if self.len == KEYBOARD_BATCH_FRAMES {
                self.flush()?;
            }
        }
        Ok(())
    }

    /// Push several keyboard edges using non-blocking channel sends.
    pub fn try_push_many(&mut self, frames: impl IntoIterator<Item = KeyboardFrame>) -> Result<()> {
        for frame in frames {
            self.try_push(frame)?;
        }
        Ok(())
    }

    /// Push a contiguous slice using non-blocking channel sends.
    pub fn try_push_many_slice(&mut self, frames: &[KeyboardFrame]) -> Result<()> {
        let mut idx = 0usize;
        while idx < frames.len() {
            if self.len == KEYBOARD_BATCH_FRAMES {
                self.try_flush()?;
            }
            let room = KEYBOARD_BATCH_FRAMES - self.len;
            let n = (frames.len() - idx).min(room);
            self.frames[self.len..self.len + n].copy_from_slice(&frames[idx..idx + n]);
            self.len += n;
            idx += n;
            if self.len == KEYBOARD_BATCH_FRAMES {
                self.try_flush()?;
            }
        }
        Ok(())
    }

    /// Queue one raw USB HID keyboard scancode edge.
    pub fn key(&mut self, scancode: u8, pressed: bool, mods: Modifiers) -> Result<()> {
        self.push(KeyboardFrame::new(scancode, pressed, mods))
    }

    /// Queue one typed USB HID keyboard scancode edge.
    pub fn key_scancode(
        &mut self,
        scancode: Scancode,
        pressed: bool,
        mods: Modifiers,
    ) -> Result<()> {
        self.push(KeyboardFrame::scancode(scancode, pressed, mods))
    }

    /// Queue one raw USB HID key tap as down + up edges.
    pub fn tap_key(&mut self, scancode: u8, mods: Modifiers) -> Result<()> {
        self.key(scancode, true, mods)?;
        self.key(scancode, false, Modifiers::empty())
    }

    /// Queue one typed USB HID key tap as down + up edges.
    pub fn tap_scancode(&mut self, scancode: Scancode, mods: Modifiers) -> Result<()> {
        self.tap_key(scancode.to_u8(), mods)
    }

    /// Queue one keyboard chord as ordered down/up edges.
    pub fn chord(&mut self, chord: KeyboardChordFrame) -> Result<()> {
        let (frames, len) = chord.edge_frames()?;
        self.push_many_slice(&frames[..len])
    }

    /// Queue one typed keyboard chord as ordered down/up edges.
    pub fn scancode_chord(&mut self, scancodes: &[Scancode], mods: Modifiers) -> Result<()> {
        self.chord(KeyboardChordFrame::try_scancodes(scancodes, mods)?)
    }

    /// Queue one raw USB HID keyboard scancode edge using non-blocking channel
    /// sends.
    pub fn try_key(&mut self, scancode: u8, pressed: bool, mods: Modifiers) -> Result<()> {
        self.try_push(KeyboardFrame::new(scancode, pressed, mods))
    }

    /// Queue one typed USB HID keyboard scancode edge using non-blocking
    /// channel sends.
    pub fn try_key_scancode(
        &mut self,
        scancode: Scancode,
        pressed: bool,
        mods: Modifiers,
    ) -> Result<()> {
        self.try_push(KeyboardFrame::scancode(scancode, pressed, mods))
    }

    /// Queue one raw USB HID key tap as down + up edges using non-blocking
    /// channel sends.
    pub fn try_tap_key(&mut self, scancode: u8, mods: Modifiers) -> Result<()> {
        self.try_key(scancode, true, mods)?;
        self.try_key(scancode, false, Modifiers::empty())
    }

    /// Queue one typed USB HID key tap as down + up edges using non-blocking
    /// channel sends.
    pub fn try_tap_scancode(&mut self, scancode: Scancode, mods: Modifiers) -> Result<()> {
        self.try_tap_key(scancode.to_u8(), mods)
    }

    /// Queue one keyboard chord using non-blocking channel sends.
    pub fn try_chord(&mut self, chord: KeyboardChordFrame) -> Result<()> {
        let (frames, len) = chord.edge_frames()?;
        self.try_push_many_slice(&frames[..len])
    }

    /// Queue one typed keyboard chord using non-blocking channel sends.
    pub fn try_scancode_chord(&mut self, scancodes: &[Scancode], mods: Modifiers) -> Result<()> {
        self.try_chord(KeyboardChordFrame::try_scancodes(scancodes, mods)?)
    }

    /// Flush buffered frames through the blocking sender.
    pub fn flush(&mut self) -> Result<()> {
        if self.len == 0 {
            return Ok(());
        }
        let result = self.client.send_key_batch_fixed(self.len, self.frames);
        if result.is_ok() {
            self.frames = [KeyboardFrame::EMPTY; KEYBOARD_BATCH_FRAMES];
            self.len = 0;
        }
        result
    }

    /// Flush buffered frames through the non-blocking sender.
    pub fn try_flush(&mut self) -> Result<()> {
        if self.len == 0 {
            return Ok(());
        }
        let result = self.client.try_send_key_batch_fixed(self.len, self.frames);
        if result.is_ok() {
            self.frames = [KeyboardFrame::EMPTY; KEYBOARD_BATCH_FRAMES];
            self.len = 0;
        }
        result
    }
}

impl Drop for KeyboardFrameBatcher<'_> {
    fn drop(&mut self) {
        let _ = self.try_flush();
    }
}

/// Batched sender for Android framework key-event producers sharing one
/// `HidClient` channel.
#[derive(Debug)]
pub struct AndroidKeyFrameBatcher<'a> {
    client: &'a HidClient,
    frames: [AndroidKeyFrame; ANDROID_KEY_BATCH_FRAMES],
    len: usize,
}

impl<'a> AndroidKeyFrameBatcher<'a> {
    /// Create an empty Android key-event batcher.
    pub fn new(client: &'a HidClient) -> Self {
        Self {
            client,
            frames: [AndroidKeyFrame::EMPTY; ANDROID_KEY_BATCH_FRAMES],
            len: 0,
        }
    }

    /// Number of frames currently buffered on the caller thread.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether there are no pending frames in the local buffer.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Push one Android key event, flushing the local batch when full.
    pub fn push(&mut self, frame: AndroidKeyFrame) -> Result<()> {
        if self.len == ANDROID_KEY_BATCH_FRAMES {
            self.flush()?;
        }
        self.frames[self.len] = frame;
        self.len += 1;
        Ok(())
    }

    /// Push one Android key event using non-blocking channel sends.
    pub fn try_push(&mut self, frame: AndroidKeyFrame) -> Result<()> {
        if self.len == ANDROID_KEY_BATCH_FRAMES {
            self.try_flush()?;
        }
        self.frames[self.len] = frame;
        self.len += 1;
        Ok(())
    }

    /// Push several Android key events.
    pub fn push_many(&mut self, frames: impl IntoIterator<Item = AndroidKeyFrame>) -> Result<()> {
        for frame in frames {
            self.push(frame)?;
        }
        Ok(())
    }

    /// Push a contiguous slice of Android key events with fewer boundary
    /// checks than per-item iteration.
    pub fn push_many_slice(&mut self, frames: &[AndroidKeyFrame]) -> Result<()> {
        let mut idx = 0usize;
        while idx < frames.len() {
            if self.len == ANDROID_KEY_BATCH_FRAMES {
                self.flush()?;
            }
            let room = ANDROID_KEY_BATCH_FRAMES - self.len;
            let n = (frames.len() - idx).min(room);
            self.frames[self.len..self.len + n].copy_from_slice(&frames[idx..idx + n]);
            self.len += n;
            idx += n;
            if self.len == ANDROID_KEY_BATCH_FRAMES {
                self.flush()?;
            }
        }
        Ok(())
    }

    /// Push several Android key events using non-blocking channel sends.
    pub fn try_push_many(
        &mut self,
        frames: impl IntoIterator<Item = AndroidKeyFrame>,
    ) -> Result<()> {
        for frame in frames {
            self.try_push(frame)?;
        }
        Ok(())
    }

    /// Push a contiguous slice using non-blocking channel sends.
    pub fn try_push_many_slice(&mut self, frames: &[AndroidKeyFrame]) -> Result<()> {
        let mut idx = 0usize;
        while idx < frames.len() {
            if self.len == ANDROID_KEY_BATCH_FRAMES {
                self.try_flush()?;
            }
            let room = ANDROID_KEY_BATCH_FRAMES - self.len;
            let n = (frames.len() - idx).min(room);
            self.frames[self.len..self.len + n].copy_from_slice(&frames[idx..idx + n]);
            self.len += n;
            idx += n;
            if self.len == ANDROID_KEY_BATCH_FRAMES {
                self.try_flush()?;
            }
        }
        Ok(())
    }

    /// Queue one raw Android key event.
    pub fn keycode(&mut self, action: u8, keycode: u32, repeat: u32, metastate: u32) -> Result<()> {
        self.push(AndroidKeyFrame::new(action, keycode, repeat, metastate))
    }

    /// Queue one typed Android key event.
    pub fn key_event(
        &mut self,
        action: AndroidKeyAction,
        keycode: AndroidKeycode,
        repeat: u32,
        metastate: u32,
    ) -> Result<()> {
        self.push(AndroidKeyFrame::typed(action, keycode, repeat, metastate))
    }

    /// Queue one raw Android key tap as DOWN + UP events.
    pub fn tap_keycode(&mut self, keycode: u32, metastate: u32) -> Result<()> {
        self.keycode(AndroidKeyAction::DOWN.value(), keycode, 0, metastate)?;
        self.keycode(AndroidKeyAction::UP.value(), keycode, 0, metastate)
    }

    /// Queue one typed Android key tap as DOWN + UP events.
    pub fn tap_key(&mut self, keycode: AndroidKeycode) -> Result<()> {
        self.tap_key_with_metastate(keycode, 0)
    }

    /// Queue one typed Android key tap with a metastate as DOWN + UP events.
    pub fn tap_key_with_metastate(
        &mut self,
        keycode: AndroidKeycode,
        metastate: u32,
    ) -> Result<()> {
        self.key_event(AndroidKeyAction::DOWN, keycode, 0, metastate)?;
        self.key_event(AndroidKeyAction::UP, keycode, 0, metastate)
    }

    /// Queue one raw Android key event using non-blocking channel sends.
    pub fn try_keycode(
        &mut self,
        action: u8,
        keycode: u32,
        repeat: u32,
        metastate: u32,
    ) -> Result<()> {
        self.try_push(AndroidKeyFrame::new(action, keycode, repeat, metastate))
    }

    /// Queue one typed Android key event using non-blocking channel sends.
    pub fn try_key_event(
        &mut self,
        action: AndroidKeyAction,
        keycode: AndroidKeycode,
        repeat: u32,
        metastate: u32,
    ) -> Result<()> {
        self.try_push(AndroidKeyFrame::typed(action, keycode, repeat, metastate))
    }

    /// Queue one raw Android key tap as DOWN + UP events using non-blocking
    /// channel sends.
    pub fn try_tap_keycode(&mut self, keycode: u32, metastate: u32) -> Result<()> {
        self.try_keycode(AndroidKeyAction::DOWN.value(), keycode, 0, metastate)?;
        self.try_keycode(AndroidKeyAction::UP.value(), keycode, 0, metastate)
    }

    /// Queue one typed Android key tap as DOWN + UP events using non-blocking
    /// channel sends.
    pub fn try_tap_key(&mut self, keycode: AndroidKeycode) -> Result<()> {
        self.try_tap_key_with_metastate(keycode, 0)
    }

    /// Queue one typed Android key tap with a metastate as DOWN + UP events
    /// using non-blocking channel sends.
    pub fn try_tap_key_with_metastate(
        &mut self,
        keycode: AndroidKeycode,
        metastate: u32,
    ) -> Result<()> {
        self.try_key_event(AndroidKeyAction::DOWN, keycode, 0, metastate)?;
        self.try_key_event(AndroidKeyAction::UP, keycode, 0, metastate)
    }

    /// Flush buffered frames through the blocking sender.
    pub fn flush(&mut self) -> Result<()> {
        if self.len == 0 {
            return Ok(());
        }
        let result = self
            .client
            .send_android_key_batch_fixed(self.len, self.frames);
        if result.is_ok() {
            self.frames = [AndroidKeyFrame::EMPTY; ANDROID_KEY_BATCH_FRAMES];
            self.len = 0;
        }
        result
    }

    /// Flush buffered frames through the non-blocking sender.
    pub fn try_flush(&mut self) -> Result<()> {
        if self.len == 0 {
            return Ok(());
        }
        let result = self
            .client
            .try_send_android_key_batch_fixed(self.len, self.frames);
        if result.is_ok() {
            self.frames = [AndroidKeyFrame::EMPTY; ANDROID_KEY_BATCH_FRAMES];
            self.len = 0;
        }
        result
    }
}

impl Drop for AndroidKeyFrameBatcher<'_> {
    fn drop(&mut self) {
        let _ = self.try_flush();
    }
}

/// Batched sender for high-rate relative UHID mouse producers sharing one
/// `HidClient` channel.
///
/// This is useful for pointer-control loops that produce many small relative
/// deltas. `push` calls stay on the caller thread and only cross the dispatcher
/// channel when the fixed stack buffer is full or when [`Self::flush`] is
/// called.
#[derive(Debug)]
pub struct MouseFrameBatcher<'a> {
    client: &'a HidClient,
    frames: [MouseFrame; MOUSE_BATCH_FRAMES],
    len: usize,
}

impl<'a> MouseFrameBatcher<'a> {
    /// Create an empty relative mouse frame batcher.
    pub fn new(client: &'a HidClient) -> Self {
        Self {
            client,
            frames: [MouseFrame::EMPTY; MOUSE_BATCH_FRAMES],
            len: 0,
        }
    }

    /// Number of frames currently buffered on the caller thread.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether there are no pending frames in the local buffer.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Push one relative mouse frame, flushing the local batch when full.
    pub fn push(&mut self, frame: MouseFrame) -> Result<()> {
        if self.len == MOUSE_BATCH_FRAMES {
            self.flush()?;
        }
        self.frames[self.len] = frame;
        self.len += 1;
        Ok(())
    }

    /// Push one relative mouse frame using non-blocking channel sends.
    pub fn try_push(&mut self, frame: MouseFrame) -> Result<()> {
        if self.len == MOUSE_BATCH_FRAMES {
            self.try_flush()?;
        }
        self.frames[self.len] = frame;
        self.len += 1;
        Ok(())
    }

    /// Push several relative mouse frames.
    pub fn push_many(&mut self, frames: impl IntoIterator<Item = MouseFrame>) -> Result<()> {
        for frame in frames {
            self.push(frame)?;
        }
        Ok(())
    }

    /// Push a contiguous slice of relative mouse frames with fewer boundary
    /// checks than per-item iteration.
    pub fn push_many_slice(&mut self, frames: &[MouseFrame]) -> Result<()> {
        let mut idx = 0usize;
        while idx < frames.len() {
            if self.len == MOUSE_BATCH_FRAMES {
                self.flush()?;
            }
            let room = MOUSE_BATCH_FRAMES - self.len;
            let n = (frames.len() - idx).min(room);
            self.frames[self.len..self.len + n].copy_from_slice(&frames[idx..idx + n]);
            self.len += n;
            idx += n;
            if self.len == MOUSE_BATCH_FRAMES {
                self.flush()?;
            }
        }
        Ok(())
    }

    /// Push several relative mouse frames using non-blocking channel sends.
    pub fn try_push_many(&mut self, frames: impl IntoIterator<Item = MouseFrame>) -> Result<()> {
        for frame in frames {
            self.try_push(frame)?;
        }
        Ok(())
    }

    /// Push a contiguous slice using non-blocking channel sends.
    pub fn try_push_many_slice(&mut self, frames: &[MouseFrame]) -> Result<()> {
        let mut idx = 0usize;
        while idx < frames.len() {
            if self.len == MOUSE_BATCH_FRAMES {
                self.try_flush()?;
            }
            let room = MOUSE_BATCH_FRAMES - self.len;
            let n = (frames.len() - idx).min(room);
            self.frames[self.len..self.len + n].copy_from_slice(&frames[idx..idx + n]);
            self.len += n;
            idx += n;
            if self.len == MOUSE_BATCH_FRAMES {
                self.try_flush()?;
            }
        }
        Ok(())
    }

    /// Queue one relative motion frame.
    pub fn motion(&mut self, dx: i32, dy: i32, buttons: u8) -> Result<()> {
        self.push(MouseFrame::motion(dx, dy, buttons))
    }

    /// Queue one relative motion frame with typed buttons.
    pub fn motion_buttons(&mut self, dx: i32, dy: i32, buttons: &[MouseButton]) -> Result<()> {
        self.push(MouseFrame::motion_buttons(dx, dy, buttons))
    }

    /// Queue one button-state frame with no motion.
    pub fn buttons(&mut self, buttons: u8) -> Result<()> {
        self.push(MouseFrame::buttons(buttons))
    }

    /// Queue one typed button-state frame with no motion.
    pub fn button_state(&mut self, buttons: &[MouseButton]) -> Result<()> {
        self.buttons(MouseButton::state(buttons))
    }

    /// Queue one relative motion frame using non-blocking channel sends.
    pub fn try_motion(&mut self, dx: i32, dy: i32, buttons: u8) -> Result<()> {
        self.try_push(MouseFrame::motion(dx, dy, buttons))
    }

    /// Queue one relative motion frame with typed buttons using non-blocking
    /// channel sends.
    pub fn try_motion_buttons(&mut self, dx: i32, dy: i32, buttons: &[MouseButton]) -> Result<()> {
        self.try_push(MouseFrame::motion_buttons(dx, dy, buttons))
    }

    /// Queue one button-state frame with no motion using non-blocking channel
    /// sends.
    pub fn try_buttons(&mut self, buttons: u8) -> Result<()> {
        self.try_push(MouseFrame::buttons(buttons))
    }

    /// Queue one typed button-state frame with no motion using non-blocking
    /// channel sends.
    pub fn try_button_state(&mut self, buttons: &[MouseButton]) -> Result<()> {
        self.try_buttons(MouseButton::state(buttons))
    }

    /// Flush buffered frames through the blocking sender.
    pub fn flush(&mut self) -> Result<()> {
        if self.len == 0 {
            return Ok(());
        }
        let result = self.client.send_mouse_batch_fixed(self.len, self.frames);
        if result.is_ok() {
            self.frames = [MouseFrame::EMPTY; MOUSE_BATCH_FRAMES];
            self.len = 0;
        }
        result
    }

    /// Flush buffered frames through the non-blocking sender.
    pub fn try_flush(&mut self) -> Result<()> {
        if self.len == 0 {
            return Ok(());
        }
        let result = self
            .client
            .try_send_mouse_batch_fixed(self.len, self.frames);
        if result.is_ok() {
            self.frames = [MouseFrame::EMPTY; MOUSE_BATCH_FRAMES];
            self.len = 0;
        }
        result
    }
}

impl Drop for MouseFrameBatcher<'_> {
    fn drop(&mut self) {
        let _ = self.try_flush();
    }
}

/// Batched sender for Android absolute scroll events sharing one `HidClient`
/// channel.
#[derive(Debug)]
pub struct ScrollFrameBatcher<'a> {
    client: &'a HidClient,
    frames: [ScrollFrame; SCROLL_BATCH_FRAMES],
    len: usize,
}

impl<'a> ScrollFrameBatcher<'a> {
    /// Create an empty scroll event batcher.
    pub fn new(client: &'a HidClient) -> Self {
        Self {
            client,
            frames: [ScrollFrame::EMPTY; SCROLL_BATCH_FRAMES],
            len: 0,
        }
    }

    /// Number of frames currently buffered on the caller thread.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether there are no pending frames in the local buffer.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Push one scroll event, flushing the local batch when full.
    pub fn push(&mut self, frame: ScrollFrame) -> Result<()> {
        if self.len == SCROLL_BATCH_FRAMES {
            self.flush()?;
        }
        self.frames[self.len] = frame;
        self.len += 1;
        Ok(())
    }

    /// Push one scroll event using non-blocking channel sends.
    pub fn try_push(&mut self, frame: ScrollFrame) -> Result<()> {
        if self.len == SCROLL_BATCH_FRAMES {
            self.try_flush()?;
        }
        self.frames[self.len] = frame;
        self.len += 1;
        Ok(())
    }

    /// Push several scroll events.
    pub fn push_many(&mut self, frames: impl IntoIterator<Item = ScrollFrame>) -> Result<()> {
        for frame in frames {
            self.push(frame)?;
        }
        Ok(())
    }

    /// Push a contiguous slice of scroll events with fewer boundary checks
    /// than per-item iteration.
    pub fn push_many_slice(&mut self, frames: &[ScrollFrame]) -> Result<()> {
        let mut idx = 0usize;
        while idx < frames.len() {
            if self.len == SCROLL_BATCH_FRAMES {
                self.flush()?;
            }
            let room = SCROLL_BATCH_FRAMES - self.len;
            let n = (frames.len() - idx).min(room);
            self.frames[self.len..self.len + n].copy_from_slice(&frames[idx..idx + n]);
            self.len += n;
            idx += n;
            if self.len == SCROLL_BATCH_FRAMES {
                self.flush()?;
            }
        }
        Ok(())
    }

    /// Push several scroll events using non-blocking channel sends.
    pub fn try_push_many(&mut self, frames: impl IntoIterator<Item = ScrollFrame>) -> Result<()> {
        for frame in frames {
            self.try_push(frame)?;
        }
        Ok(())
    }

    /// Push a contiguous slice using non-blocking channel sends.
    pub fn try_push_many_slice(&mut self, frames: &[ScrollFrame]) -> Result<()> {
        let mut idx = 0usize;
        while idx < frames.len() {
            if self.len == SCROLL_BATCH_FRAMES {
                self.try_flush()?;
            }
            let room = SCROLL_BATCH_FRAMES - self.len;
            let n = (frames.len() - idx).min(room);
            self.frames[self.len..self.len + n].copy_from_slice(&frames[idx..idx + n]);
            self.len += n;
            idx += n;
            if self.len == SCROLL_BATCH_FRAMES {
                self.try_flush()?;
            }
        }
        Ok(())
    }

    /// Queue one scroll event with no pressed mouse buttons.
    pub fn scroll(&mut self, x: i32, y: i32, hscroll: f32, vscroll: f32) -> Result<()> {
        self.push(ScrollFrame::scroll(x, y, hscroll, vscroll))
    }

    /// Queue one scroll event with an explicit Android mouse-button bitmask.
    pub fn scroll_with_buttons(
        &mut self,
        x: i32,
        y: i32,
        hscroll: f32,
        vscroll: f32,
        buttons: u32,
    ) -> Result<()> {
        self.push(ScrollFrame::new(x, y, hscroll, vscroll, buttons))
    }

    /// Queue one scroll event using non-blocking channel sends.
    pub fn try_scroll(&mut self, x: i32, y: i32, hscroll: f32, vscroll: f32) -> Result<()> {
        self.try_push(ScrollFrame::scroll(x, y, hscroll, vscroll))
    }

    /// Queue one scroll event with buttons using non-blocking channel sends.
    pub fn try_scroll_with_buttons(
        &mut self,
        x: i32,
        y: i32,
        hscroll: f32,
        vscroll: f32,
        buttons: u32,
    ) -> Result<()> {
        self.try_push(ScrollFrame::new(x, y, hscroll, vscroll, buttons))
    }

    /// Flush buffered frames through the blocking sender.
    pub fn flush(&mut self) -> Result<()> {
        if self.len == 0 {
            return Ok(());
        }
        let result = self.client.send_scroll_batch_fixed(self.len, self.frames);
        if result.is_ok() {
            self.frames = [ScrollFrame::EMPTY; SCROLL_BATCH_FRAMES];
            self.len = 0;
        }
        result
    }

    /// Flush buffered frames through the non-blocking sender.
    pub fn try_flush(&mut self) -> Result<()> {
        if self.len == 0 {
            return Ok(());
        }
        let result = self
            .client
            .try_send_scroll_batch_fixed(self.len, self.frames);
        if result.is_ok() {
            self.frames = [ScrollFrame::EMPTY; SCROLL_BATCH_FRAMES];
            self.len = 0;
        }
        result
    }
}

impl Drop for ScrollFrameBatcher<'_> {
    fn drop(&mut self) {
        let _ = self.try_flush();
    }
}

#[inline]
fn lerp_i32(a: i32, b: i32, t: f32) -> i32 {
    (a as f32 + (b - a) as f32 * t).round() as i32
}

fn recover_gamepad_batch(cmd: HidCommand) -> Vec<GamepadFrameRaw> {
    match cmd {
        HidCommand::GamepadFrameRawBatch(frames)
        | HidCommand::GamepadFrameRawBatchUnchecked(frames) => frames,
        _ => unreachable!("expected owned gamepad frame batch command"),
    }
}

fn recover_packed_gamepad_batch(cmd: HidCommand) -> Vec<[u8; GAMEPAD_FRAME_BYTES]> {
    match cmd {
        HidCommand::GamepadPackedFrameBatch(frames) => frames,
        _ => unreachable!("expected owned packed gamepad frame batch command"),
    }
}

/// Batched sender for high-rate gamepad frame loops sharing one
/// `HidClient` channel.
///
/// Repeated `push` calls stay on the caller thread and only cross the
/// channel when the local batch is full or you explicitly `flush`es.
/// For loops already emitting one frame each tick, this can drop
/// dispatcher lock contention substantially.
#[derive(Debug)]
pub struct GamepadFrameBatcher<'a> {
    client: &'a HidClient,
    fixed_frames: [GamepadFrameRaw; DIRECT_GAMEPAD_BATCH_FRAMES],
    fixed_len: usize,
    frames: Vec<GamepadFrameRaw>,
    dedupe: bool,
    batch_size: usize,
    use_fixed: bool,
}

impl<'a> GamepadFrameBatcher<'a> {
    /// Create a batcher that sends full-state-dedupe `set_frame_raw_batch`
    /// calls when it flushes.
    pub fn dedupe(client: &'a HidClient, batch_size: usize) -> Self {
        let batch_size = batch_size.max(1);
        let use_fixed = batch_size <= DIRECT_GAMEPAD_BATCH_FRAMES;
        Self {
            client,
            fixed_frames: [GamepadFrameRaw::new(0, 0, 0, 0, 0, 0, 0); DIRECT_GAMEPAD_BATCH_FRAMES],
            fixed_len: 0,
            frames: if use_fixed {
                Vec::new()
            } else {
                Vec::with_capacity(batch_size)
            },
            dedupe: true,
            batch_size,
            use_fixed,
        }
    }

    /// Create a batcher that sends un-deduped `set_frame_raw_batch_unchecked`
    /// calls when it flushes.
    pub fn unchecked(client: &'a HidClient, batch_size: usize) -> Self {
        let batch_size = batch_size.max(1);
        let use_fixed = batch_size <= DIRECT_GAMEPAD_BATCH_FRAMES;
        Self {
            client,
            fixed_frames: [GamepadFrameRaw::new(0, 0, 0, 0, 0, 0, 0); DIRECT_GAMEPAD_BATCH_FRAMES],
            fixed_len: 0,
            frames: if use_fixed {
                Vec::new()
            } else {
                Vec::with_capacity(batch_size)
            },
            dedupe: false,
            batch_size,
            use_fixed,
        }
    }

    /// Push one frame into the local batch.
    ///
    /// Returns only transport errors. When the batch is full this
    /// automatically flushes before returning.
    pub fn push(&mut self, frame: GamepadFrameRaw) -> Result<()> {
        if self.use_fixed {
            if self.fixed_len >= self.batch_size {
                self.flush()?;
                if self.fixed_len >= self.batch_size {
                    return Err(Error::SessionLifecycle(
                        "frame batcher fixed buffer full (flush did not make space)",
                    ));
                }
            }
            self.fixed_frames[self.fixed_len] = frame;
            self.fixed_len += 1;
            if self.fixed_len >= self.batch_size {
                self.flush()?;
            }
            return Ok(());
        }
        self.frames.push(frame);
        if self.frames.len() >= self.batch_size {
            self.flush()?;
        }
        Ok(())
    }

    /// Push one frame into the local batch using non-blocking channel
    /// semantics. On producer backlog, returns `SessionLifecycle`.
    pub fn try_push(&mut self, frame: GamepadFrameRaw) -> Result<()> {
        if self.use_fixed {
            if self.fixed_len >= self.batch_size {
                self.try_flush()?;
                if self.fixed_len >= self.batch_size {
                    return Err(Error::SessionLifecycle(
                        "frame batcher fixed buffer full (flush did not make space)",
                    ));
                }
            }
            self.fixed_frames[self.fixed_len] = frame;
            self.fixed_len += 1;
            if self.fixed_len >= self.batch_size {
                self.try_flush()?;
            }
            return Ok(());
        }
        self.frames.push(frame);
        if self.frames.len() >= self.batch_size {
            self.try_flush()?;
        }
        Ok(())
    }

    /// Push several frames into the local batch in one step.
    pub fn push_many<I>(&mut self, frames: I) -> Result<()>
    where
        I: IntoIterator<Item = GamepadFrameRaw>,
    {
        for frame in frames {
            self.push(frame)?;
        }
        Ok(())
    }

    /// Push a contiguous slice of frames with fewer boundary checks than
    /// per-item iteration.
    pub fn push_many_slice(&mut self, frames: &[GamepadFrameRaw]) -> Result<()> {
        if self.use_fixed {
            let mut idx = 0usize;
            while idx < frames.len() {
                if self.fixed_len >= self.batch_size {
                    self.flush()?;
                    if self.fixed_len >= self.batch_size {
                        return Err(Error::SessionLifecycle(
                            "frame batcher fixed buffer full (flush did not make space)",
                        ));
                    }
                }
                let room = self.batch_size - self.fixed_len;
                let n = (frames.len() - idx).min(room);
                self.fixed_frames[self.fixed_len..self.fixed_len + n]
                    .copy_from_slice(&frames[idx..idx + n]);
                self.fixed_len += n;
                idx += n;
                if self.fixed_len >= self.batch_size {
                    self.flush()?;
                }
            }
            return Ok(());
        }

        let mut idx = 0usize;
        while idx < frames.len() {
            if self.frames.len() >= self.batch_size {
                self.flush()?;
            }
            let room = self.batch_size - self.frames.len();
            let n = (frames.len() - idx).min(room);
            self.frames.extend_from_slice(&frames[idx..idx + n]);
            idx += n;
            if self.frames.len() >= self.batch_size {
                self.flush()?;
            }
        }
        Ok(())
    }

    /// Push several frames into the local batch in one step using
    /// non-blocking channel semantics.
    pub fn try_push_many<I>(&mut self, frames: I) -> Result<()>
    where
        I: IntoIterator<Item = GamepadFrameRaw>,
    {
        for frame in frames {
            self.try_push(frame)?;
        }
        Ok(())
    }

    /// Push a contiguous slice with non-blocking flush semantics.
    pub fn try_push_many_slice(&mut self, frames: &[GamepadFrameRaw]) -> Result<()> {
        if self.use_fixed {
            let mut idx = 0usize;
            while idx < frames.len() {
                if self.fixed_len >= self.batch_size {
                    self.try_flush()?;
                    if self.fixed_len >= self.batch_size {
                        return Err(Error::SessionLifecycle(
                            "frame batcher fixed buffer full (flush did not make space)",
                        ));
                    }
                }
                let room = self.batch_size - self.fixed_len;
                let n = (frames.len() - idx).min(room);
                self.fixed_frames[self.fixed_len..self.fixed_len + n]
                    .copy_from_slice(&frames[idx..idx + n]);
                self.fixed_len += n;
                idx += n;
                if self.fixed_len >= self.batch_size {
                    self.try_flush()?;
                }
            }
            return Ok(());
        }

        let mut idx = 0usize;
        while idx < frames.len() {
            if self.frames.len() >= self.batch_size {
                self.try_flush()?;
            }
            let room = self.batch_size - self.frames.len();
            let n = (frames.len() - idx).min(room);
            self.frames.extend_from_slice(&frames[idx..idx + n]);
            idx += n;
            if self.frames.len() >= self.batch_size {
                self.try_flush()?;
            }
        }
        Ok(())
    }

    /// Send any queued frames now.
    pub fn flush(&mut self) -> Result<()> {
        if self.use_fixed {
            if self.fixed_len == 0 {
                return Ok(());
            }
            if self.fixed_len == 1 {
                let frame = self.fixed_frames[0];
                let result = if self.dedupe {
                    self.client.send_frame(frame)
                } else {
                    self.client.send_frame_unchecked(frame)
                };
                if result.is_ok() {
                    self.fixed_len = 0;
                }
                return result;
            }
            let mut batch =
                [GamepadFrameRaw::new(0, 0, 0, 0, 0, 0, 0); DIRECT_GAMEPAD_BATCH_FRAMES];
            std::mem::swap(&mut batch, &mut self.fixed_frames);
            let len = self.fixed_len;
            let result = self
                .client
                .send_frame_batch_fixed_internal(len, batch, self.dedupe);
            if result.is_err() {
                self.fixed_len = len;
                std::mem::swap(&mut batch, &mut self.fixed_frames);
            } else {
                self.fixed_len = 0;
            }
            return result;
        }

        if self.frames.is_empty() {
            return Ok(());
        }
        if self.frames.len() == 1 {
            let frame = self.frames[0];
            let result = if self.dedupe {
                self.client.send_frame(frame)
            } else {
                self.client.send_frame_unchecked(frame)
            };
            if result.is_ok() {
                self.frames.clear();
            }
            return result;
        }
        let frames = std::mem::take(&mut self.frames);
        if let Err((err, frames)) = self.client.send_frame_batch_owned(frames, self.dedupe) {
            self.frames = frames;
            return Err(err);
        }
        self.frames = Vec::with_capacity(self.batch_size);
        Ok(())
    }

    /// Send any queued frames now using non-blocking channel semantics.
    pub fn try_flush(&mut self) -> Result<()> {
        if self.use_fixed {
            if self.fixed_len == 0 {
                return Ok(());
            }
            if self.fixed_len == 1 {
                let frame = self.fixed_frames[0];
                let result = if self.dedupe {
                    self.client.try_send_frame(frame)
                } else {
                    self.client.try_send_frame_unchecked(frame)
                };
                if result.is_ok() {
                    self.fixed_len = 0;
                }
                return result;
            }
            let mut batch =
                [GamepadFrameRaw::new(0, 0, 0, 0, 0, 0, 0); DIRECT_GAMEPAD_BATCH_FRAMES];
            std::mem::swap(&mut batch, &mut self.fixed_frames);
            let len = self.fixed_len;
            let result = if self.dedupe {
                self.client.try_send_frame_batch_fixed(len, batch)
            } else {
                self.client.try_send_frame_batch_fixed_unchecked(len, batch)
            };
            if result.is_err() {
                self.fixed_len = len;
                std::mem::swap(&mut batch, &mut self.fixed_frames);
            } else {
                self.fixed_len = 0;
            }
            return result;
        }

        if self.frames.is_empty() {
            return Ok(());
        }
        if self.frames.len() == 1 {
            let frame = self.frames[0];
            let result = if self.dedupe {
                self.client.try_send_frame(frame)
            } else {
                self.client.try_send_frame_unchecked(frame)
            };
            if result.is_ok() {
                self.frames.clear();
            }
            return result;
        }
        let frames = std::mem::take(&mut self.frames);
        if let Err((err, frames)) = self.client.try_send_frame_batch_owned(frames, self.dedupe) {
            self.frames = frames;
            return Err(err);
        }
        self.frames = Vec::with_capacity(self.batch_size);
        Ok(())
    }

    /// Current number of buffered frames.
    pub fn len(&self) -> usize {
        if self.use_fixed {
            return self.fixed_len;
        }
        self.frames.len()
    }

    /// Returns `true` when there is no buffered frame.
    pub fn is_empty(&self) -> bool {
        if self.use_fixed {
            return self.fixed_len == 0;
        }
        self.frames.is_empty()
    }
}

impl<'a> Drop for GamepadFrameBatcher<'a> {
    fn drop(&mut self) {
        // Non-blocking flush: dropping the batcher must not deadlock if
        // the dispatcher's channel is already full (which would happen
        // when the producer thread holds the batcher longer than the
        // dispatcher can drain, or in tests where there is no
        // dispatcher at all).
        let _ = self.try_flush();
    }
}

/// Batched sender for high-rate packed 15-byte gamepad frame loops sharing
/// one `HidClient` channel.
///
/// This avoids per-batch `Vec` allocation by using a fixed stack buffer when
/// the batch size is at most `DIRECT_GAMEPAD_BATCH_FRAMES`.
#[derive(Debug)]
pub struct PackedGamepadFrameBatcher<'a> {
    client: &'a HidClient,
    fixed_frames: [[u8; GAMEPAD_FRAME_BYTES]; DIRECT_GAMEPAD_BATCH_FRAMES],
    fixed_len: usize,
    frames: Vec<[u8; GAMEPAD_FRAME_BYTES]>,
    batch_size: usize,
    use_fixed: bool,
}

impl<'a> PackedGamepadFrameBatcher<'a> {
    /// Create a batcher for pre-packed 15-byte gamepad reports.
    pub fn new(client: &'a HidClient, batch_size: usize) -> Self {
        let batch_size = batch_size.max(1);
        let use_fixed = batch_size <= DIRECT_GAMEPAD_BATCH_FRAMES;
        Self {
            client,
            fixed_frames: [[0u8; GAMEPAD_FRAME_BYTES]; DIRECT_GAMEPAD_BATCH_FRAMES],
            fixed_len: 0,
            frames: if use_fixed {
                Vec::new()
            } else {
                Vec::with_capacity(batch_size)
            },
            batch_size,
            use_fixed,
        }
    }

    /// Push one packed report into the local batch.
    ///
    /// Returns only transport errors. When the batch is full this
    /// automatically flushes before returning.
    pub fn push(&mut self, frame: [u8; GAMEPAD_FRAME_BYTES]) -> Result<()> {
        if self.use_fixed {
            if self.fixed_len >= self.batch_size {
                self.flush()?;
                if self.fixed_len >= self.batch_size {
                    return Err(Error::SessionLifecycle(
                        "packed frame batcher fixed buffer full (flush did not make space)",
                    ));
                }
            }
            self.fixed_frames[self.fixed_len] = frame;
            self.fixed_len += 1;
            if self.fixed_len >= self.batch_size {
                self.flush()?;
            }
            return Ok(());
        }
        self.frames.push(frame);
        if self.frames.len() >= self.batch_size {
            self.flush()?;
        }
        Ok(())
    }

    /// Push one packed report into the local batch using non-blocking channel
    /// semantics. On producer backlog, returns `SessionLifecycle`.
    pub fn try_push(&mut self, frame: [u8; GAMEPAD_FRAME_BYTES]) -> Result<()> {
        if self.use_fixed {
            if self.fixed_len >= self.batch_size {
                self.try_flush()?;
                if self.fixed_len >= self.batch_size {
                    return Err(Error::SessionLifecycle(
                        "packed frame batcher fixed buffer full (flush did not make space)",
                    ));
                }
            }
            self.fixed_frames[self.fixed_len] = frame;
            self.fixed_len += 1;
            if self.fixed_len >= self.batch_size {
                self.try_flush()?;
            }
            return Ok(());
        }
        self.frames.push(frame);
        if self.frames.len() >= self.batch_size {
            self.try_flush()?;
        }
        Ok(())
    }

    /// Push several packed reports into the local batch in one step.
    pub fn push_many<I>(&mut self, frames: I) -> Result<()>
    where
        I: IntoIterator<Item = [u8; GAMEPAD_FRAME_BYTES]>,
    {
        for frame in frames {
            self.push(frame)?;
        }
        Ok(())
    }

    /// Push a contiguous slice of packed reports with fewer boundary checks
    /// than per-item iteration.
    pub fn push_many_slice(&mut self, frames: &[[u8; GAMEPAD_FRAME_BYTES]]) -> Result<()> {
        if self.use_fixed {
            let mut idx = 0usize;
            while idx < frames.len() {
                if self.fixed_len >= self.batch_size {
                    self.flush()?;
                    if self.fixed_len >= self.batch_size {
                        return Err(Error::SessionLifecycle(
                            "packed frame batcher fixed buffer full (flush did not make space)",
                        ));
                    }
                }
                let room = self.batch_size - self.fixed_len;
                let n = (frames.len() - idx).min(room);
                self.fixed_frames[self.fixed_len..self.fixed_len + n]
                    .copy_from_slice(&frames[idx..idx + n]);
                self.fixed_len += n;
                idx += n;
                if self.fixed_len >= self.batch_size {
                    self.flush()?;
                }
            }
            return Ok(());
        }

        let mut idx = 0usize;
        while idx < frames.len() {
            if self.frames.len() >= self.batch_size {
                self.flush()?;
            }
            let room = self.batch_size - self.frames.len();
            let n = (frames.len() - idx).min(room);
            self.frames.extend_from_slice(&frames[idx..idx + n]);
            idx += n;
            if self.frames.len() >= self.batch_size {
                self.flush()?;
            }
        }
        Ok(())
    }

    /// Push several packed reports into the local batch in one step using
    /// non-blocking channel semantics.
    pub fn try_push_many<I>(&mut self, frames: I) -> Result<()>
    where
        I: IntoIterator<Item = [u8; GAMEPAD_FRAME_BYTES]>,
    {
        for frame in frames {
            self.try_push(frame)?;
        }
        Ok(())
    }

    /// Push a contiguous slice of packed reports with non-blocking flush
    /// semantics.
    pub fn try_push_many_slice(&mut self, frames: &[[u8; GAMEPAD_FRAME_BYTES]]) -> Result<()> {
        if self.use_fixed {
            let mut idx = 0usize;
            while idx < frames.len() {
                if self.fixed_len >= self.batch_size {
                    self.try_flush()?;
                    if self.fixed_len >= self.batch_size {
                        return Err(Error::SessionLifecycle(
                            "packed frame batcher fixed buffer full (flush did not make space)",
                        ));
                    }
                }
                let room = self.batch_size - self.fixed_len;
                let n = (frames.len() - idx).min(room);
                self.fixed_frames[self.fixed_len..self.fixed_len + n]
                    .copy_from_slice(&frames[idx..idx + n]);
                self.fixed_len += n;
                idx += n;
                if self.fixed_len >= self.batch_size {
                    self.try_flush()?;
                }
            }
            return Ok(());
        }

        let mut idx = 0usize;
        while idx < frames.len() {
            if self.frames.len() >= self.batch_size {
                self.try_flush()?;
            }
            let room = self.batch_size - self.frames.len();
            let n = (frames.len() - idx).min(room);
            self.frames.extend_from_slice(&frames[idx..idx + n]);
            idx += n;
            if self.frames.len() >= self.batch_size {
                self.try_flush()?;
            }
        }
        Ok(())
    }

    /// Send any queued frames now.
    pub fn flush(&mut self) -> Result<()> {
        if self.use_fixed {
            if self.fixed_len == 0 {
                return Ok(());
            }
            if self.fixed_len == 1 {
                let frame = self.fixed_frames[0];
                let result = self.client.send_frame_packed(frame);
                if result.is_ok() {
                    self.fixed_len = 0;
                }
                return result;
            }
            let mut batch = [[0u8; GAMEPAD_FRAME_BYTES]; DIRECT_GAMEPAD_BATCH_FRAMES];
            std::mem::swap(&mut batch, &mut self.fixed_frames);
            let len = self.fixed_len;
            let result = self.client.send_frame_packed_batch_fixed(len, batch);
            if result.is_err() {
                self.fixed_len = len;
                std::mem::swap(&mut batch, &mut self.fixed_frames);
            } else {
                self.fixed_len = 0;
            }
            return result;
        }

        if self.frames.is_empty() {
            return Ok(());
        }
        if self.frames.len() == 1 {
            let frame = self.frames[0];
            let result = self.client.send_frame_packed(frame);
            if result.is_ok() {
                self.frames.clear();
            }
            return result;
        }
        let frames = std::mem::take(&mut self.frames);
        if let Err((err, frames)) = self.client.send_packed_frame_batch_owned(frames) {
            self.frames = frames;
            return Err(err);
        }
        self.frames = Vec::with_capacity(self.batch_size);
        Ok(())
    }

    /// Send any queued frames now using non-blocking channel semantics.
    pub fn try_flush(&mut self) -> Result<()> {
        if self.use_fixed {
            if self.fixed_len == 0 {
                return Ok(());
            }
            if self.fixed_len == 1 {
                let frame = self.fixed_frames[0];
                let result = self.client.try_send_frame_packed(frame);
                if result.is_ok() {
                    self.fixed_len = 0;
                }
                return result;
            }
            let mut batch = [[0u8; GAMEPAD_FRAME_BYTES]; DIRECT_GAMEPAD_BATCH_FRAMES];
            std::mem::swap(&mut batch, &mut self.fixed_frames);
            let len = self.fixed_len;
            let result = self.client.try_send_frame_packed_batch_fixed(len, batch);
            if result.is_err() {
                self.fixed_len = len;
                std::mem::swap(&mut batch, &mut self.fixed_frames);
            } else {
                self.fixed_len = 0;
            }
            return result;
        }

        if self.frames.is_empty() {
            return Ok(());
        }
        if self.frames.len() == 1 {
            let frame = self.frames[0];
            let result = self.client.try_send_frame_packed(frame);
            if result.is_ok() {
                self.frames.clear();
            }
            return result;
        }
        let frames = std::mem::take(&mut self.frames);
        if let Err((err, frames)) = self.client.try_send_packed_frame_batch_owned(frames) {
            self.frames = frames;
            return Err(err);
        }
        self.frames = Vec::with_capacity(self.batch_size);
        Ok(())
    }

    /// Current number of buffered frames.
    pub fn len(&self) -> usize {
        if self.use_fixed {
            self.fixed_len
        } else {
            self.frames.len()
        }
    }

    /// Returns `true` when there is no buffered frame.
    pub fn is_empty(&self) -> bool {
        if self.use_fixed {
            self.fixed_len == 0
        } else {
            self.frames.is_empty()
        }
    }
}

impl<'a> Drop for PackedGamepadFrameBatcher<'a> {
    fn drop(&mut self) {
        // Non-blocking flush: see the matching comment on
        // `GamepadFrameBatcher::drop`. Blocking on Drop would
        // deadlock if the dispatcher channel is full at unwind time.
        let _ = self.try_flush();
    }
}

fn dispatcher_loop<T: TransportWrite + Send>(
    mut session: HidSession<T>,
    rx: Receiver<HidCommand>,
) -> Result<T> {
    let mut pending_error = None;
    loop {
        let first = match rx.recv() {
            Ok(c) => c,
            Err(_) => {
                let _ = session.close();
                return Ok(session.into_inner());
            }
        };
        if dispatch_to_session(first, &mut session, &mut pending_error) {
            return Ok(session.into_inner());
        }
        while let Ok(cmd) = rx.try_recv() {
            if dispatch_to_session(cmd, &mut session, &mut pending_error) {
                return Ok(session.into_inner());
            }
        }
    }
}

fn record_first_error<T>(pending_error: &mut Option<Error>, result: Result<T>) {
    if pending_error.is_none() {
        if let Err(err) = result {
            *pending_error = Some(err);
        }
    }
}

fn dispatch_to_session<T: TransportWrite + Send>(
    cmd: HidCommand,
    session: &mut HidSession<T>,
    pending_error: &mut Option<Error>,
) -> bool {
    match cmd {
        HidCommand::Close => {
            let _ = session.close();
            true
        }
        HidCommand::TypeText(s) => {
            record_first_error(pending_error, session.type_text(&s));
            false
        }
        HidCommand::TypeTextStrict(s) => {
            record_first_error(pending_error, session.type_text_strict(&s));
            false
        }
        HidCommand::Key {
            scancode,
            pressed,
            mods,
        } => {
            record_first_error(pending_error, session.key(scancode, pressed, mods));
            false
        }
        HidCommand::KeyTap { scancode, mods } => {
            record_first_error(
                pending_error,
                session
                    .key(scancode, true, mods)
                    .and_then(|_| session.key(scancode, false, Modifiers::empty())),
            );
            false
        }
        HidCommand::KeyBatchFixed { len, frames } => {
            for frame in &frames[..len as usize] {
                record_first_error(
                    pending_error,
                    session.key(frame.scancode, frame.pressed, frame.mods),
                );
            }
            false
        }
        HidCommand::MouseMotion { dx, dy, buttons } => {
            record_first_error(pending_error, session.mouse_motion(dx, dy, buttons));
            false
        }
        HidCommand::MouseButtons { buttons } => {
            record_first_error(pending_error, session.mouse_buttons(buttons));
            false
        }
        HidCommand::MouseScroll { hscroll, vscroll } => {
            record_first_error(pending_error, session.mouse_scroll(hscroll, vscroll));
            false
        }
        HidCommand::MouseBatchFixed { len, frames } => {
            for frame in &frames[..len as usize] {
                record_first_error(
                    pending_error,
                    session.mouse_motion(frame.dx, frame.dy, frame.buttons),
                );
            }
            false
        }
        HidCommand::InjectKeycode {
            action,
            keycode,
            repeat,
            metastate,
        } => {
            record_first_error(
                pending_error,
                session.inject_keycode(action, keycode, repeat, metastate),
            );
            false
        }
        HidCommand::AndroidKeyTap { keycode, metastate } => {
            record_first_error(
                pending_error,
                session.tap_android_keycode(keycode, metastate),
            );
            false
        }
        HidCommand::AndroidKeyBatchFixed { len, frames } => {
            for frame in &frames[..len as usize] {
                record_first_error(
                    pending_error,
                    session.inject_keycode(
                        frame.action,
                        frame.keycode,
                        frame.repeat,
                        frame.metastate,
                    ),
                );
            }
            false
        }
        HidCommand::BackOrScreenOn { action } => {
            record_first_error(
                pending_error,
                session.send(&ControlMessage::BackOrScreenOn(BackOrScreenOn { action })),
            );
            false
        }
        HidCommand::MultitouchDown { id, x, y, pressure } => {
            record_first_error(
                pending_error,
                session.inject_touch_action(TouchAction::DOWN, id, x, y, pressure),
            );
            false
        }
        HidCommand::MultitouchMove { id, x, y, pressure } => {
            record_first_error(
                pending_error,
                session.inject_touch_action(TouchAction::MOVE, id, x, y, pressure),
            );
            false
        }
        HidCommand::MultitouchUp { id } => {
            record_first_error(
                pending_error,
                session.inject_touch_action(TouchAction::UP, id, 0, 0, 0.0),
            );
            false
        }
        HidCommand::MultitouchCancel { id } => {
            record_first_error(pending_error, session.cancel_touch(id));
            false
        }
        HidCommand::TouchBatchFixed { len, frames } => {
            for frame in &frames[..len as usize] {
                record_first_error(
                    pending_error,
                    session.inject_touch(
                        frame.action,
                        frame.pointer_id,
                        frame.x,
                        frame.y,
                        frame.pressure,
                    ),
                );
            }
            false
        }
        HidCommand::InjectScroll {
            x,
            y,
            hscroll,
            vscroll,
            buttons,
        } => {
            record_first_error(
                pending_error,
                session.inject_scroll(x, y, hscroll, vscroll, buttons),
            );
            false
        }
        HidCommand::InjectScrollBatchFixed { len, frames } => {
            for frame in &frames[..len as usize] {
                record_first_error(
                    pending_error,
                    session.inject_scroll(
                        frame.x,
                        frame.y,
                        frame.hscroll,
                        frame.vscroll,
                        frame.buttons,
                    ),
                );
            }
            false
        }
        HidCommand::GamepadButton { btn, pressed } => {
            record_first_error(pending_error, session.set_button(btn, pressed));
            false
        }
        HidCommand::GamepadButtons { buttons } => {
            record_first_error(pending_error, session.set_buttons(buttons));
            false
        }
        HidCommand::GamepadStick { axis, value } => {
            record_first_error(pending_error, session.set_stick(axis, value));
            false
        }
        HidCommand::GamepadStickRaw { axis, value } => {
            record_first_error(pending_error, session.set_stick_raw(axis, value));
            false
        }
        HidCommand::GamepadLeftStickRaw { x, y } => {
            record_first_error(pending_error, session.set_left_stick_raw(x, y));
            false
        }
        HidCommand::GamepadRightStickRaw { x, y } => {
            record_first_error(pending_error, session.set_right_stick_raw(x, y));
            false
        }
        HidCommand::GamepadTriggersRaw { left, right } => {
            record_first_error(pending_error, session.set_triggers_raw(left, right));
            false
        }
        HidCommand::GamepadSticksRaw {
            left_x,
            left_y,
            right_x,
            right_y,
            left_trigger,
            right_trigger,
        } => {
            record_first_error(
                pending_error,
                session.set_sticks_raw(
                    left_x,
                    left_y,
                    right_x,
                    right_y,
                    left_trigger,
                    right_trigger,
                ),
            );
            false
        }
        HidCommand::GamepadFrameRaw {
            buttons,
            left_x,
            left_y,
            right_x,
            right_y,
            left_trigger,
            right_trigger,
        } => {
            record_first_error(
                pending_error,
                session.set_frame_raw(
                    buttons,
                    left_x,
                    left_y,
                    right_x,
                    right_y,
                    left_trigger,
                    right_trigger,
                ),
            );
            false
        }
        HidCommand::GamepadFrameRawUnchecked(frame) => {
            record_first_error(pending_error, session.set_frame_raw_unchecked_frame(frame));
            false
        }
        HidCommand::GamepadFrameRawBatch(frames) => {
            record_first_error(pending_error, session.set_frame_raw_batch(&frames));
            false
        }
        HidCommand::GamepadFrameRawBatchFixed { len, frames } => {
            record_first_error(
                pending_error,
                session.set_frame_raw_batch(&frames[..len as usize]),
            );
            false
        }
        HidCommand::GamepadFrameRawBatchFixedUnchecked { len, frames } => {
            record_first_error(
                pending_error,
                session.set_frame_raw_batch_unchecked(&frames[..len as usize]),
            );
            false
        }
        HidCommand::GamepadFrameRawBatchUnchecked(frames) => {
            record_first_error(
                pending_error,
                session.set_frame_raw_batch_unchecked(&frames),
            );
            false
        }
        HidCommand::GamepadPackedFrame(frame) => {
            record_first_error(pending_error, session.set_frame_raw_packed(&frame));
            false
        }
        HidCommand::GamepadPackedFrameBatch(frames) => {
            record_first_error(pending_error, session.set_frame_raw_packed_batch(&frames));
            false
        }
        HidCommand::GamepadPackedFrameBatchFixed { len, frames } => {
            record_first_error(
                pending_error,
                session.set_frame_raw_packed_batch(&frames[..len as usize]),
            );
            false
        }
        HidCommand::SetScreenSize { width, height } => {
            session.set_screen_size(width, height);
            false
        }
        HidCommand::SetScreenPower { on } => {
            record_first_error(
                pending_error,
                session.send(&ControlMessage::SetDisplayPower(SetDisplayPower { on })),
            );
            false
        }
        HidCommand::ShowNotifications => {
            record_first_error(pending_error, session.show_notifications());
            false
        }
        HidCommand::ShowQuickSettings => {
            record_first_error(pending_error, session.show_quick_settings());
            false
        }
        HidCommand::CollapsePanels => {
            record_first_error(pending_error, session.collapse_panels());
            false
        }
        HidCommand::RotateDevice => {
            record_first_error(pending_error, session.rotate_device());
            false
        }
        HidCommand::ResizeDisplay { width, height } => {
            record_first_error(pending_error, session.resize_display(width, height));
            false
        }
        HidCommand::SetTorch { on } => {
            record_first_error(pending_error, session.set_torch(on));
            false
        }
        HidCommand::CameraZoomIn => {
            record_first_error(pending_error, session.camera_zoom_in());
            false
        }
        HidCommand::CameraZoomOut => {
            record_first_error(pending_error, session.camera_zoom_out());
            false
        }
        HidCommand::OpenHardKeyboardSettings => {
            record_first_error(pending_error, session.open_hard_keyboard_settings());
            false
        }
        HidCommand::ResetVideo => {
            record_first_error(pending_error, session.reset_video());
            false
        }
        HidCommand::AiConfig {
            flags,
            sample_interval_ms,
            feature_dim,
        } => {
            record_first_error(
                pending_error,
                session.send(&ControlMessage::AiConfig(AiConfig {
                    flags,
                    sample_interval_ms,
                    feature_dim,
                })),
            );
            false
        }
        HidCommand::AiQuery { since_timestamp_ms } => {
            record_first_error(
                pending_error,
                session.send(&ControlMessage::AiQuery(AiQuery { since_timestamp_ms })),
            );
            false
        }
        HidCommand::AiPause => {
            record_first_error(pending_error, session.send(&ControlMessage::AiPause));
            false
        }
        HidCommand::LaunchApp { name } => {
            record_first_error(
                pending_error,
                session.send(&ControlMessage::StartApp(StartApp { name })),
            );
            false
        }
        HidCommand::SetClipboard { text, paste } => {
            record_first_error(
                pending_error,
                session.send(&ControlMessage::SetClipboard(SetClipboard {
                    sequence: 0,
                    paste,
                    text,
                })),
            );
            false
        }
        HidCommand::SetClipboardSequenced {
            sequence,
            text,
            paste,
        } => {
            record_first_error(
                pending_error,
                session.send(&ControlMessage::SetClipboard(SetClipboard {
                    sequence,
                    paste,
                    text,
                })),
            );
            false
        }
        HidCommand::GetClipboard { copy_key } => {
            record_first_error(pending_error, session.request_clipboard(copy_key));
            false
        }
        HidCommand::FlushAck { ack } => {
            let prior_error = pending_error.take();
            let flush_result = session.flush_now();
            let result = match prior_error {
                Some(err) => {
                    if let Err(flush_err) = flush_result {
                        *pending_error = Some(flush_err);
                    }
                    Err(err)
                }
                None => flush_result,
            };
            let _ = ack.send(result);
            false
        }
        HidCommand::Flush => {
            record_first_error(pending_error, session.flush_now());
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::sync_channel;

    use super::*;
    use crate::error::Error;
    use crate::session::{OpenRequest, GAMEPAD_FRAME_BYTES};
    use crate::transport::MockTransport;

    fn count_touch_events(buf: &[u8]) -> usize {
        let mut i = 0usize;
        let mut n = 0usize;
        while i + 32 <= buf.len() {
            if buf[i] == 2 && buf[i + 1] <= crate::multitouch::ACTION_MOVE {
                n += 1;
                i += 32;
            } else {
                i += 1;
            }
        }
        n
    }

    fn first_touch_screen_size(buf: &[u8]) -> Option<(u16, u16)> {
        let mut i = 0usize;
        while i + 32 <= buf.len() {
            if buf[i] == 2 && buf[i + 1] <= crate::multitouch::ACTION_MOVE {
                let width = u16::from_be_bytes([buf[i + 18], buf[i + 19]]);
                let height = u16::from_be_bytes([buf[i + 20], buf[i + 21]]);
                return Some((width, height));
            }
            i += 1;
        }
        None
    }

    fn mouse_input_payloads(buf: &[u8]) -> Vec<[u8; 5]> {
        let mut i = 0usize;
        let mut out = Vec::new();
        while i + 5 <= buf.len() {
            match buf[i] {
                12 => {
                    if i + 8 > buf.len() {
                        break;
                    }
                    let name_len = buf[i + 7] as usize;
                    if i + 8 + name_len + 2 > buf.len() {
                        break;
                    }
                    let rd_len_idx = i + 8 + name_len;
                    let rd_len =
                        u16::from_be_bytes([buf[rd_len_idx], buf[rd_len_idx + 1]]) as usize;
                    i += 8 + name_len + 2 + rd_len;
                }
                13 => {
                    let id = u16::from_be_bytes([buf[i + 1], buf[i + 2]]);
                    let size = u16::from_be_bytes([buf[i + 3], buf[i + 4]]) as usize;
                    if i + 5 + size > buf.len() {
                        break;
                    }
                    if id == crate::types::HID_ID_MOUSE && size == 5 {
                        let mut payload = [0u8; 5];
                        payload.copy_from_slice(&buf[i + 5..i + 10]);
                        out.push(payload);
                    }
                    i += 5 + size;
                }
                14 => i += 3,
                _ => i += 1,
            }
        }
        out
    }

    fn count_keyboard_inputs(buf: &[u8]) -> usize {
        let mut i = 0usize;
        let mut n = 0usize;
        while i < buf.len() {
            match buf[i] {
                12 => {
                    if i + 8 > buf.len() {
                        break;
                    }
                    let name_len = buf[i + 7] as usize;
                    if i + 8 + name_len + 2 > buf.len() {
                        break;
                    }
                    let rd_len_idx = i + 8 + name_len;
                    let rd_len =
                        u16::from_be_bytes([buf[rd_len_idx], buf[rd_len_idx + 1]]) as usize;
                    i += 8 + name_len + 2 + rd_len;
                }
                13 => {
                    if i + 5 > buf.len() {
                        break;
                    }
                    let id = u16::from_be_bytes([buf[i + 1], buf[i + 2]]);
                    let size = u16::from_be_bytes([buf[i + 3], buf[i + 4]]) as usize;
                    if i + 5 + size > buf.len() {
                        break;
                    }
                    if id == crate::types::HID_ID_KEYBOARD {
                        n += 1;
                    }
                    i += 5 + size;
                }
                14 => i += 3,
                _ => break,
            }
        }
        n
    }

    fn keyboard_input_payloads(buf: &[u8]) -> Vec<[u8; 8]> {
        let mut i = 0usize;
        let mut out = Vec::new();
        while i < buf.len() {
            match buf[i] {
                12 => {
                    if i + 8 > buf.len() {
                        break;
                    }
                    let name_len = buf[i + 7] as usize;
                    if i + 8 + name_len + 2 > buf.len() {
                        break;
                    }
                    let rd_len_idx = i + 8 + name_len;
                    let rd_len =
                        u16::from_be_bytes([buf[rd_len_idx], buf[rd_len_idx + 1]]) as usize;
                    i += 8 + name_len + 2 + rd_len;
                }
                13 => {
                    if i + 5 > buf.len() {
                        break;
                    }
                    let id = u16::from_be_bytes([buf[i + 1], buf[i + 2]]);
                    let size = u16::from_be_bytes([buf[i + 3], buf[i + 4]]) as usize;
                    if i + 5 + size > buf.len() {
                        break;
                    }
                    if id == crate::types::HID_ID_KEYBOARD && size == 8 {
                        let mut payload = [0u8; 8];
                        payload.copy_from_slice(&buf[i + 5..i + 13]);
                        out.push(payload);
                    }
                    i += 5 + size;
                }
                14 => i += 3,
                _ => break,
            }
        }
        out
    }

    fn first_inject_keycodes(buf: &[u8], count: usize) -> Vec<u32> {
        let mut keycodes = Vec::with_capacity(count);
        let mut i = 0usize;
        while keycodes.len() < count && i + 14 <= buf.len() {
            assert_eq!(buf[i], 0, "expected INJECT_KEYCODE at offset {i}");
            keycodes.push(u32::from_be_bytes([
                buf[i + 2],
                buf[i + 3],
                buf[i + 4],
                buf[i + 5],
            ]));
            i += 14;
        }
        keycodes
    }

    #[test]
    fn gamepad_frame_batcher_fixed_try_push_stays_safe_when_channel_full() {
        let (tx, _rx) = sync_channel(1);
        let client = HidClient { tx };
        let mut batcher = GamepadFrameBatcher::dedupe(&client, 2);

        assert!(batcher
            .try_push(GamepadFrameRaw::new(1, 0, 0, 0, 0, 0, 0))
            .is_ok());
        assert!(batcher
            .try_push(GamepadFrameRaw::new(1, 0, 0, 0, 0, 0, 0))
            .is_ok());
        assert!(batcher
            .try_push(GamepadFrameRaw::new(1, 0, 0, 0, 0, 0, 0))
            .is_ok());
        assert!(batcher
            .try_push(GamepadFrameRaw::new(1, 0, 0, 0, 0, 0, 0))
            .is_err());

        let overflow_err = batcher
            .try_push(GamepadFrameRaw::new(1, 0, 0, 0, 0, 0, 0))
            .unwrap_err();
        assert!(matches!(overflow_err, Error::SessionLifecycle(_)));
    }

    #[test]
    fn packed_gamepad_frame_batcher_fixed_try_push_stays_safe_when_channel_full() {
        let (tx, _rx) = sync_channel(1);
        let client = HidClient { tx };
        let mut batcher = PackedGamepadFrameBatcher::new(&client, 2);
        let frame = [1u8; GAMEPAD_FRAME_BYTES];

        assert!(batcher.try_push(frame).is_ok());
        assert!(batcher.try_push(frame).is_ok());
        assert!(batcher.try_push(frame).is_ok());
        assert!(batcher.try_push(frame).is_err());

        let overflow_err = batcher.try_push(frame).unwrap_err();
        assert!(matches!(overflow_err, Error::SessionLifecycle(_)));
    }

    /// Regression for the `cargo test --lib` hang observed on
    /// `iter-skill/uhid-optimize-e2e` (2026-06-17): the previous
    /// `Drop` impl called the **blocking** `flush()`. When the
    /// dispatcher's bounded channel was already full at unwind time,
    /// `tx.send(cmd)` blocked forever because the same thread still
    /// needed to drop the sender afterwards. The fix is to call
    /// `try_flush` instead. This test drops the batcher with a full
    /// channel and asserts the drop completes without hanging.
    #[test]
    fn batcher_drop_is_non_blocking_when_channel_full() {
        let (tx, _rx) = sync_channel(1);
        let client = HidClient { tx };

        // GamepadFrameBatcher: fill the channel so Drop's flush sees a
        // full mpsc and would block if it used the blocking path.
        {
            let mut batcher = GamepadFrameBatcher::dedupe(&client, 2);
            // push 1 + push 2 → flush puts one item in the channel.
            assert!(batcher
                .try_push(GamepadFrameRaw::new(1, 0, 0, 0, 0, 0, 0))
                .is_ok());
            assert!(batcher
                .try_push(GamepadFrameRaw::new(1, 0, 0, 0, 0, 0, 0))
                .is_ok());
            // push 3 + push 4 → batcher is full again, channel is full.
            assert!(batcher
                .try_push(GamepadFrameRaw::new(1, 0, 0, 0, 0, 0, 0))
                .is_ok());
            assert!(batcher
                .try_push(GamepadFrameRaw::new(1, 0, 0, 0, 0, 0, 0))
                .is_err());
            // Drop here MUST NOT block. If it does, the test framework
            // will hang the suite. The test process will be killed by
            // the CI timeout and we will see the regression.
        }

        // PackedGamepadFrameBatcher: same scenario, packed variant.
        // The channel is still full from the previous block, so
        // try_flush will return Err — we just want to ensure Drop
        // completes without hanging.
        {
            let mut batcher = PackedGamepadFrameBatcher::new(&client, 2);
            let frame = [1u8; GAMEPAD_FRAME_BYTES];
            // push 1: batch holds the frame, no flush, Ok.
            assert!(batcher.try_push(frame).is_ok());
            // push 2: batch is full → try_flush → channel full → Err,
            // batcher restored to full. We don't care about the Err
            // here, only that Drop on the full batcher doesn't hang.
            let _ = batcher.try_push(frame);
        }
    }

    #[test]
    fn vector_backed_batcher_flush_restores_frames_when_channel_disconnected() {
        let (tx, rx) = sync_channel(1);
        drop(rx);
        let client = HidClient { tx };
        let batch_size = DIRECT_GAMEPAD_BATCH_FRAMES + 8;

        let mut batcher = GamepadFrameBatcher::dedupe(&client, batch_size);
        let raw_frames = [
            GamepadFrameRaw::new(1, 2, 3, 4, 5, 6, 7),
            GamepadFrameRaw::new(2, 3, 4, 5, 6, 7, 8),
        ];
        batcher.push_many_slice(&raw_frames).unwrap();
        let err = batcher.flush().unwrap_err();
        assert!(matches!(err, Error::DispatcherDown("channel disconnected")));
        assert_eq!(batcher.len(), raw_frames.len());

        let mut packed_batcher = PackedGamepadFrameBatcher::new(&client, batch_size);
        let packed_frames = [[1u8; GAMEPAD_FRAME_BYTES], [2u8; GAMEPAD_FRAME_BYTES]];
        packed_batcher.push_many_slice(&packed_frames).unwrap();
        let err = packed_batcher.flush().unwrap_err();
        assert!(matches!(err, Error::DispatcherDown("channel disconnected")));
        assert_eq!(packed_batcher.len(), packed_frames.len());
    }

    #[test]
    fn vector_backed_batcher_try_flush_restores_frames_when_channel_full() {
        let (tx, _rx) = sync_channel(1);
        let client = HidClient { tx };
        client.try_flush().unwrap();
        let batch_size = DIRECT_GAMEPAD_BATCH_FRAMES + 8;

        let mut batcher = GamepadFrameBatcher::unchecked(&client, batch_size);
        let raw_frames = [
            GamepadFrameRaw::new(1, 2, 3, 4, 5, 6, 7),
            GamepadFrameRaw::new(2, 3, 4, 5, 6, 7, 8),
        ];
        batcher.push_many_slice(&raw_frames).unwrap();
        let err = batcher.try_flush().unwrap_err();
        assert!(matches!(err, Error::SessionLifecycle(_)));
        assert_eq!(batcher.len(), raw_frames.len());

        let mut packed_batcher = PackedGamepadFrameBatcher::new(&client, batch_size);
        let packed_frames = [[1u8; GAMEPAD_FRAME_BYTES], [2u8; GAMEPAD_FRAME_BYTES]];
        packed_batcher.push_many_slice(&packed_frames).unwrap();
        let err = packed_batcher.try_flush().unwrap_err();
        assert!(matches!(err, Error::SessionLifecycle(_)));
        assert_eq!(packed_batcher.len(), packed_frames.len());
    }

    #[test]
    fn send_frame_batch_len_one_uses_single_cmd() {
        let (tx, rx) = sync_channel(2);
        let client = HidClient { tx };
        let frame = GamepadFrameRaw::new(0x0001, 10, 20, 30, 40, 50, 60);

        client.send_frame_batch(vec![frame]).unwrap();
        match rx.try_recv().unwrap() {
            HidCommand::GamepadFrameRaw {
                buttons,
                left_x,
                left_y,
                right_x,
                right_y,
                left_trigger,
                right_trigger,
            } => {
                assert_eq!(buttons, frame.buttons);
                assert_eq!(left_x, frame.left_x);
                assert_eq!(left_y, frame.left_y);
                assert_eq!(right_x, frame.right_x);
                assert_eq!(right_y, frame.right_y);
                assert_eq!(left_trigger, frame.left_trigger);
                assert_eq!(right_trigger, frame.right_trigger);
            }
            other => panic!("expected GamepadFrameRaw command, got {other:?}"),
        }
    }

    #[test]
    fn send_frame_batch_unchecked_len_one_uses_single_cmd() {
        let (tx, rx) = sync_channel(2);
        let client = HidClient { tx };
        let frame = GamepadFrameRaw::new(0x0002, 11, 21, 31, 41, 51, 61);

        client.send_frame_batch_unchecked(vec![frame]).unwrap();
        match rx.try_recv().unwrap() {
            HidCommand::GamepadFrameRawUnchecked(cmd_frame) => {
                assert_eq!(cmd_frame, frame);
            }
            other => panic!("expected GamepadFrameRawUnchecked command, got {other:?}"),
        }
    }

    #[test]
    fn send_frame_packed_batch_len_one_uses_single_cmd() {
        let (tx, rx) = sync_channel(2);
        let client = HidClient { tx };
        let frame = [0xABu8; GAMEPAD_FRAME_BYTES];

        client.send_frame_packed_batch(vec![frame]).unwrap();
        match rx.try_recv().unwrap() {
            HidCommand::GamepadPackedFrame(cmd_frame) => assert_eq!(cmd_frame, frame),
            other => panic!("expected GamepadPackedFrame command, got {other:?}"),
        }
    }

    #[test]
    fn try_send_frame_batch_len_one_uses_single_cmd() {
        let (tx, rx) = sync_channel(2);
        let client = HidClient { tx };
        let frame = GamepadFrameRaw::new(0x0003, 12, 22, 32, 42, 52, 62);

        client.try_send_frame_batch(vec![frame]).unwrap();
        match rx.try_recv().unwrap() {
            HidCommand::GamepadFrameRaw {
                buttons,
                left_x,
                left_y,
                right_x,
                right_y,
                left_trigger,
                right_trigger,
            } => {
                assert_eq!(buttons, frame.buttons);
                assert_eq!(left_x, frame.left_x);
                assert_eq!(left_y, frame.left_y);
                assert_eq!(right_x, frame.right_x);
                assert_eq!(right_y, frame.right_y);
                assert_eq!(left_trigger, frame.left_trigger);
                assert_eq!(right_trigger, frame.right_trigger);
            }
            other => panic!("expected GamepadFrameRaw command, got {other:?}"),
        }
    }

    #[test]
    fn try_send_frame_packed_batch_len_one_uses_single_cmd() {
        let (tx, rx) = sync_channel(2);
        let client = HidClient { tx };
        let frame = [0xCDu8; GAMEPAD_FRAME_BYTES];

        client.try_send_frame_packed_batch(vec![frame]).unwrap();
        match rx.try_recv().unwrap() {
            HidCommand::GamepadPackedFrame(cmd_frame) => assert_eq!(cmd_frame, frame),
            other => panic!("expected GamepadPackedFrame command, got {other:?}"),
        }
    }

    #[test]
    fn gamepad_frame_batcher_flush_single_frame_uses_single_cmd() {
        let (tx, rx) = sync_channel(2);
        let client = HidClient { tx };
        let mut batcher = GamepadFrameBatcher::dedupe(&client, 4);
        let frame = GamepadFrameRaw::new(0x0001, 10, 20, 30, 40, 50, 60);

        batcher.push(frame).unwrap();
        batcher.flush().unwrap();

        match rx.try_recv().unwrap() {
            HidCommand::GamepadFrameRaw {
                buttons,
                left_x,
                left_y,
                right_x,
                right_y,
                left_trigger,
                right_trigger,
            } => {
                assert_eq!(buttons, frame.buttons);
                assert_eq!(left_x, frame.left_x);
                assert_eq!(left_y, frame.left_y);
                assert_eq!(right_x, frame.right_x);
                assert_eq!(right_y, frame.right_y);
                assert_eq!(left_trigger, frame.left_trigger);
                assert_eq!(right_trigger, frame.right_trigger);
            }
            other => panic!("expected GamepadFrameRaw command, got {other:?}"),
        }
    }

    #[test]
    fn packed_gamepad_frame_batcher_flush_single_frame_uses_single_cmd() {
        let (tx, rx) = sync_channel(2);
        let client = HidClient { tx };
        let mut batcher = PackedGamepadFrameBatcher::new(&client, 4);
        let frame = [1u8; GAMEPAD_FRAME_BYTES];

        batcher.push(frame).unwrap();
        batcher.flush().unwrap();

        match rx.try_recv().unwrap() {
            HidCommand::GamepadPackedFrame(cmd_frame) => assert_eq!(cmd_frame, frame),
            other => panic!("expected GamepadPackedFrame command, got {other:?}"),
        }
    }

    #[test]
    fn send_gamepad_input_shortcuts_use_expected_commands() {
        let (tx, rx) = sync_channel(4);
        let client = HidClient { tx };

        client.send_button(GamepadButton::South, true).unwrap();
        match rx.try_recv().unwrap() {
            HidCommand::GamepadButton {
                btn: GamepadButton::South,
                pressed: true,
            } => {}
            other => panic!("expected GamepadButton command, got {other:?}"),
        }

        let expected_buttons = GamepadButton::South as u32;
        client.send_buttons(expected_buttons).unwrap();
        match rx.try_recv().unwrap() {
            HidCommand::GamepadButtons {
                buttons: _expected_buttons,
            } => {}
            other => panic!("expected GamepadButtons command, got {other:?}"),
        }

        client.send_stick_raw(GamepadAxis::LeftX, 123).unwrap();
        match rx.try_recv().unwrap() {
            HidCommand::GamepadStickRaw {
                axis: GamepadAxis::LeftX,
                value: 123,
            } => {}
            other => panic!("expected GamepadStickRaw command, got {other:?}"),
        }

        client.send_sticks_raw(1, 2, 3, 4, 5, 6).unwrap();
        match rx.try_recv().unwrap() {
            HidCommand::GamepadSticksRaw {
                left_x: 1,
                left_y: 2,
                right_x: 3,
                right_y: 4,
                left_trigger: 5,
                right_trigger: 6,
            } => {}
            other => panic!("expected GamepadSticksRaw command, got {other:?}"),
        }
    }

    #[test]
    fn try_send_gamepad_input_shortcuts_use_expected_commands() {
        let (tx, rx) = sync_channel(4);
        let client = HidClient { tx };

        client.try_send_left_stick_raw(7, -7).unwrap();
        match rx.try_recv().unwrap() {
            HidCommand::GamepadLeftStickRaw { x: 7, y: -7 } => {}
            other => panic!("expected GamepadLeftStickRaw command, got {other:?}"),
        }

        client.try_send_right_stick_raw(-8, 8).unwrap();
        match rx.try_recv().unwrap() {
            HidCommand::GamepadRightStickRaw { x: -8, y: 8 } => {}
            other => panic!("expected GamepadRightStickRaw command, got {other:?}"),
        }

        client.try_send_stick(GamepadAxis::RightY, 0.25).unwrap();
        match rx.try_recv().unwrap() {
            HidCommand::GamepadStick {
                axis: GamepadAxis::RightY,
                value,
            } => assert_eq!(value, 0.25),
            other => panic!("expected GamepadStick command, got {other:?}"),
        }
    }

    #[test]
    fn strict_text_shortcuts_use_expected_commands() {
        let (tx, rx) = sync_channel(2);
        let client = HidClient { tx };

        client.type_text_strict("ok").unwrap();
        client.try_type_text_strict("still ok").unwrap();

        match rx.try_recv().unwrap() {
            HidCommand::TypeTextStrict(text) => assert_eq!(text, "ok"),
            other => panic!("expected TypeTextStrict command, got {other:?}"),
        }
        match rx.try_recv().unwrap() {
            HidCommand::TypeTextStrict(text) => assert_eq!(text, "still ok"),
            other => panic!("expected TypeTextStrict command, got {other:?}"),
        }
    }

    #[test]
    fn strict_text_surfaces_unsupported_char_at_checked_boundary() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::kbd_only()).unwrap();
        let (client, dispatcher) = session.into_client().unwrap();

        client.type_text_strict("a中b").unwrap();
        let err = client.flush_wait().unwrap_err();
        assert!(matches!(
            err,
            Error::SessionLifecycle("unsupported char in type_text_strict")
        ));

        assert_eq!(
            client.flush_wait().unwrap(),
            0,
            "strict text error should be reported once"
        );
        client.close();
        let transport = dispatcher.join().unwrap();
        assert_eq!(
            count_keyboard_inputs(&transport.into_bytes()),
            2,
            "only the supported prefix before the bad character is injected"
        );
    }

    #[test]
    fn keyboard_shortcuts_use_expected_commands() {
        let (tx, rx) = sync_channel(4);
        let client = HidClient { tx };

        client.key(0x04, true, Modifiers::LSHIFT).unwrap();
        client
            .try_key_scancode(Scancode::B, false, Modifiers::empty())
            .unwrap();
        client
            .tap_scancode(Scancode::Enter, Modifiers::empty())
            .unwrap();
        client.try_tap_key(0x05, Modifiers::LCTRL).unwrap();

        match rx.try_recv().unwrap() {
            HidCommand::Key {
                scancode: 0x04,
                pressed: true,
                mods: Modifiers(0x02),
            } => {}
            other => panic!("expected Key command, got {other:?}"),
        }
        match rx.try_recv().unwrap() {
            HidCommand::Key {
                scancode,
                pressed: false,
                mods,
            } => {
                assert_eq!(scancode, Scancode::B.to_u8());
                assert_eq!(mods, Modifiers::empty());
            }
            other => panic!("expected Key command, got {other:?}"),
        }
        match rx.try_recv().unwrap() {
            HidCommand::KeyTap { scancode, mods } => {
                assert_eq!(scancode, Scancode::Enter.to_u8());
                assert_eq!(mods, Modifiers::empty());
            }
            other => panic!("expected KeyTap command, got {other:?}"),
        }
        match rx.try_recv().unwrap() {
            HidCommand::KeyTap {
                scancode: 0x05,
                mods: Modifiers(0x01),
            } => {}
            other => panic!("expected KeyTap command, got {other:?}"),
        }
    }

    #[test]
    fn keyboard_tap_dispatches_down_up_and_releases_modifiers() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::kbd_only()).unwrap();
        let (client, dispatcher) = session.into_client().unwrap();

        client.tap_scancode(Scancode::A, Modifiers::LSHIFT).unwrap();
        client
            .key_scancode(Scancode::B, true, Modifiers::empty())
            .unwrap();
        client
            .key_scancode(Scancode::B, false, Modifiers::empty())
            .unwrap();
        client.close();

        let payloads = keyboard_input_payloads(&dispatcher.join().unwrap().into_bytes());
        assert_eq!(payloads.len(), 4);
        assert_eq!(
            payloads[0],
            [
                Modifiers::LSHIFT.bits(),
                0,
                Scancode::A.to_u8(),
                0,
                0,
                0,
                0,
                0
            ]
        );
        assert_eq!(payloads[1], [0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(payloads[2], [0, 0, Scancode::B.to_u8(), 0, 0, 0, 0, 0]);
        assert_eq!(payloads[3], [0, 0, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn keyboard_batch_fixed_uses_expected_commands() {
        let (tx, rx) = sync_channel(2);
        let client = HidClient { tx };
        let mut frames = [KeyboardFrame::EMPTY; KEYBOARD_BATCH_FRAMES];
        frames[0] = KeyboardFrame::scancode_down(Scancode::A, Modifiers::LSHIFT);
        frames[1] = KeyboardFrame::scancode_up(Scancode::A);

        client.send_key_batch_fixed(2, frames).unwrap();
        client.try_send_key_batch_fixed(1, frames).unwrap();

        match rx.try_recv().unwrap() {
            HidCommand::KeyBatchFixed { len, frames } => {
                assert_eq!(len, 2);
                assert_eq!(
                    frames[0],
                    KeyboardFrame::scancode_down(Scancode::A, Modifiers::LSHIFT)
                );
                assert_eq!(frames[1], KeyboardFrame::scancode_up(Scancode::A));
            }
            other => panic!("expected KeyBatchFixed command, got {other:?}"),
        }
        match rx.try_recv().unwrap() {
            HidCommand::Key {
                scancode,
                pressed: true,
                mods,
            } => {
                assert_eq!(scancode, Scancode::A.to_u8());
                assert_eq!(mods, Modifiers::LSHIFT);
            }
            other => panic!("expected single Key command, got {other:?}"),
        }
    }

    #[test]
    fn keyboard_chord_frame_expands_to_ordered_edges() {
        let chord = KeyboardChordFrame::try_scancodes(
            &[Scancode::K, Scancode::C],
            Modifiers::LCTRL | Modifiers::LSHIFT,
        )
        .unwrap();

        let (frames, len) = chord.edge_frames().unwrap();

        assert_eq!(len, 4);
        assert_eq!(
            frames[..len],
            [
                KeyboardFrame::scancode_down(Scancode::K, Modifiers::LCTRL | Modifiers::LSHIFT),
                KeyboardFrame::scancode_down(Scancode::C, Modifiers::LCTRL | Modifiers::LSHIFT),
                KeyboardFrame::new(
                    Scancode::C.to_u8(),
                    false,
                    Modifiers::LCTRL | Modifiers::LSHIFT
                ),
                KeyboardFrame::scancode_up(Scancode::K),
            ]
        );
    }

    #[test]
    fn keyboard_chord_helpers_use_expected_command() {
        let (tx, rx) = sync_channel(2);
        let client = HidClient { tx };

        client
            .scancode_chord(&[Scancode::C], Modifiers::LCTRL)
            .unwrap();
        client
            .try_key_chord(KeyboardChordFrame::scancode(Scancode::V, Modifiers::LCTRL))
            .unwrap();

        match rx.try_recv().unwrap() {
            HidCommand::KeyBatchFixed { len, frames } => {
                assert_eq!(len, 2);
                assert_eq!(
                    frames[0],
                    KeyboardFrame::scancode_down(Scancode::C, Modifiers::LCTRL)
                );
                assert_eq!(frames[1], KeyboardFrame::scancode_up(Scancode::C));
            }
            other => panic!("expected KeyBatchFixed command, got {other:?}"),
        }
        match rx.try_recv().unwrap() {
            HidCommand::KeyBatchFixed { len, frames } => {
                assert_eq!(len, 2);
                assert_eq!(
                    frames[0],
                    KeyboardFrame::scancode_down(Scancode::V, Modifiers::LCTRL)
                );
                assert_eq!(frames[1], KeyboardFrame::scancode_up(Scancode::V));
            }
            other => panic!("expected KeyBatchFixed command, got {other:?}"),
        }
    }

    #[test]
    fn keyboard_chord_rejects_malformed_fixed_length() {
        let chord = KeyboardChordFrame::new(
            (KEYBOARD_CHORD_KEYS + 1) as u8,
            [Scancode::A.to_u8(); KEYBOARD_CHORD_KEYS],
            Modifiers::LCTRL,
        );

        assert!(matches!(
            chord.edge_frames(),
            Err(Error::SessionLifecycle("keyboard chord length overflow"))
        ));
        let (tx, _rx) = sync_channel(1);
        let client = HidClient { tx };
        assert!(matches!(
            client.key_chord(chord),
            Err(Error::SessionLifecycle("keyboard chord length overflow"))
        ));
    }

    #[test]
    fn keyboard_frame_batcher_flushes_at_fixed_capacity() {
        let (tx, rx) = sync_channel(2);
        let client = HidClient { tx };
        let mut batcher = KeyboardFrameBatcher::new(&client);

        for _ in 0..KEYBOARD_BATCH_FRAMES {
            batcher
                .key_scancode(Scancode::A, true, Modifiers::empty())
                .unwrap();
        }
        assert_eq!(batcher.len(), KEYBOARD_BATCH_FRAMES);
        assert!(
            rx.try_recv().is_err(),
            "batcher should not flush before capacity is exceeded"
        );

        batcher
            .key_scancode(Scancode::B, true, Modifiers::empty())
            .unwrap();
        assert_eq!(batcher.len(), 1);
        batcher.flush().unwrap();

        match rx.try_recv().unwrap() {
            HidCommand::KeyBatchFixed { len, frames } => {
                assert_eq!(len as usize, KEYBOARD_BATCH_FRAMES);
                assert_eq!(
                    frames[KEYBOARD_BATCH_FRAMES - 1].scancode,
                    Scancode::A.to_u8()
                );
            }
            other => panic!("expected KeyBatchFixed command, got {other:?}"),
        }
        match rx.try_recv().unwrap() {
            HidCommand::Key {
                scancode,
                pressed: true,
                mods,
            } => {
                assert_eq!(scancode, Scancode::B.to_u8());
                assert_eq!(mods, Modifiers::empty());
            }
            other => panic!("expected single Key command, got {other:?}"),
        }
    }

    #[test]
    fn keyboard_frame_batcher_try_push_many_slice_keeps_batch_on_backpressure() {
        let (tx, rx) = sync_channel(1);
        let client = HidClient { tx };
        let mut batcher = KeyboardFrameBatcher::new(&client);
        let frames: Vec<_> = (0..KEYBOARD_BATCH_FRAMES)
            .map(|_| KeyboardFrame::scancode_down(Scancode::A, Modifiers::empty()))
            .collect();

        batcher.try_push_many_slice(&frames).unwrap();
        assert_eq!(batcher.len(), 0);
        let queued = rx.try_recv().unwrap();
        let HidCommand::KeyBatchFixed { len, .. } = queued else {
            panic!("expected first fixed keyboard batch");
        };
        assert_eq!(len as usize, KEYBOARD_BATCH_FRAMES);

        batcher.try_push_many_slice(&frames).unwrap();
        assert_eq!(batcher.len(), 0);
        let err = batcher.try_push_many_slice(&frames).unwrap_err();
        assert!(matches!(
            err,
            Error::SessionLifecycle("channel full (back-pressure)")
        ));
        assert_eq!(batcher.len(), KEYBOARD_BATCH_FRAMES);
    }

    #[test]
    fn keyboard_frame_batcher_dispatches_all_keyboard_frames() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::kbd_only()).unwrap();
        let (client, dispatcher) = session.into_client().unwrap();

        {
            let mut batcher = KeyboardFrameBatcher::new(&client);
            batcher
                .tap_scancode(Scancode::A, Modifiers::LSHIFT)
                .unwrap();
            batcher
                .key_scancode(Scancode::B, true, Modifiers::empty())
                .unwrap();
            batcher
                .key_scancode(Scancode::B, false, Modifiers::empty())
                .unwrap();
            batcher.flush().unwrap();
        }

        client.close();
        let payloads = keyboard_input_payloads(&dispatcher.join().unwrap().into_bytes());
        assert_eq!(payloads.len(), 4);
        assert_eq!(
            payloads[0],
            [
                Modifiers::LSHIFT.bits(),
                0,
                Scancode::A.to_u8(),
                0,
                0,
                0,
                0,
                0
            ]
        );
        assert_eq!(payloads[1], [0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(payloads[2], [0, 0, Scancode::B.to_u8(), 0, 0, 0, 0, 0]);
        assert_eq!(payloads[3], [0, 0, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn keyboard_frame_batcher_chord_dispatches_ordered_reports() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::kbd_only()).unwrap();
        let (client, dispatcher) = session.into_client().unwrap();

        {
            let mut batcher = KeyboardFrameBatcher::new(&client);
            batcher
                .scancode_chord(&[Scancode::K, Scancode::C], Modifiers::LCTRL)
                .unwrap();
            batcher.flush().unwrap();
        }

        client.close();
        let payloads = keyboard_input_payloads(&dispatcher.join().unwrap().into_bytes());
        assert_eq!(payloads.len(), 4);
        assert_eq!(
            payloads[0],
            [
                Modifiers::LCTRL.bits(),
                0,
                Scancode::K.to_u8(),
                0,
                0,
                0,
                0,
                0
            ]
        );
        assert_eq!(
            payloads[1],
            [
                Modifiers::LCTRL.bits(),
                0,
                Scancode::C.to_u8(),
                Scancode::K.to_u8(),
                0,
                0,
                0,
                0
            ]
        );
        assert_eq!(
            payloads[2],
            [
                Modifiers::LCTRL.bits(),
                0,
                Scancode::K.to_u8(),
                0,
                0,
                0,
                0,
                0
            ]
        );
        assert_eq!(payloads[3], [0, 0, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn mouse_shortcuts_use_expected_commands() {
        let (tx, rx) = sync_channel(8);
        let client = HidClient { tx };

        client
            .mouse_motion_buttons(10, -5, &[MouseButton::Left, MouseButton::X1])
            .unwrap();
        client.mouse_button_state(&[MouseButton::Right]).unwrap();
        client.mouse_scroll(0.0, 2.0).unwrap();
        let mut frames = [MouseFrame::EMPTY; MOUSE_BATCH_FRAMES];
        frames[0] = MouseFrame::motion(1, 2, MouseButton::Left as u8);
        frames[1] = MouseFrame::buttons(MouseButton::Middle as u8);
        client.send_mouse_batch_fixed(2, frames).unwrap();

        match rx.try_recv().unwrap() {
            HidCommand::MouseMotion { dx, dy, buttons } => {
                assert_eq!((dx, dy), (10, -5));
                assert_eq!(buttons, MouseButton::Left as u8 | MouseButton::X1 as u8);
            }
            other => panic!("expected MouseMotion command, got {other:?}"),
        }
        match rx.try_recv().unwrap() {
            HidCommand::MouseButtons { buttons } => {
                assert_eq!(buttons, MouseButton::Right as u8);
            }
            other => panic!("expected MouseButtons command, got {other:?}"),
        }
        match rx.try_recv().unwrap() {
            HidCommand::MouseScroll { hscroll, vscroll } => {
                assert_eq!((hscroll, vscroll), (0.0, 2.0));
            }
            other => panic!("expected MouseScroll command, got {other:?}"),
        }
        match rx.try_recv().unwrap() {
            HidCommand::MouseBatchFixed { len, frames } => {
                assert_eq!(len, 2);
                assert_eq!(frames[0].dx, 1);
                assert_eq!(frames[1].buttons, MouseButton::Middle as u8);
            }
            other => panic!("expected MouseBatchFixed command, got {other:?}"),
        }
    }

    #[test]
    fn mouse_helpers_dispatch_to_uhid_reports() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::mouse_only()).unwrap();
        let (client, dispatcher) = session.into_client().unwrap();

        client
            .mouse_motion(200, -200, MouseButton::Left as u8)
            .unwrap();
        client.mouse_buttons(MouseButton::Right as u8).unwrap();
        client.mouse_scroll(0.0, 2.0).unwrap();
        let mut frames = [MouseFrame::EMPTY; MOUSE_BATCH_FRAMES];
        frames[0] = MouseFrame::motion(-3, 4, MouseButton::X1 as u8);
        frames[1] = MouseFrame::motion(5, -6, 0);
        client.send_mouse_batch_fixed(2, frames).unwrap();
        client.close();

        let bytes = dispatcher.join().unwrap().into_bytes();
        let payloads = mouse_input_payloads(&bytes);
        assert_eq!(payloads.len(), 5);
        assert_eq!(payloads[0], [MouseButton::Left as u8, 127u8, 129u8, 0, 0]);
        assert_eq!(payloads[1], [MouseButton::Right as u8, 0, 0, 0, 0]);
        assert_eq!(payloads[2], [0, 0, 0, 2, 0]);
        assert_eq!(payloads[3], [MouseButton::X1 as u8, (-3i8) as u8, 4, 0, 0]);
        assert_eq!(payloads[4], [0, 5, (-6i8) as u8, 0, 0]);
    }

    #[test]
    fn mouse_batch_rejects_oversized_len() {
        let (tx, _rx) = sync_channel(1);
        let client = HidClient { tx };
        let err = client
            .send_mouse_batch_fixed(
                MOUSE_BATCH_FRAMES + 1,
                [MouseFrame::EMPTY; MOUSE_BATCH_FRAMES],
            )
            .unwrap_err();
        assert!(matches!(err, Error::SessionLifecycle(_)));
    }

    #[test]
    fn mouse_frame_batcher_flushes_at_fixed_capacity() {
        let (tx, rx) = sync_channel(2);
        let client = HidClient { tx };
        let mut batcher = MouseFrameBatcher::new(&client);

        for i in 0..MOUSE_BATCH_FRAMES {
            batcher.motion(i as i32, -(i as i32), 0).unwrap();
        }
        assert_eq!(batcher.len(), MOUSE_BATCH_FRAMES);
        assert!(
            rx.try_recv().is_err(),
            "batcher should not flush before capacity is exceeded"
        );

        batcher
            .motion_buttons(
                MOUSE_BATCH_FRAMES as i32,
                1,
                &[MouseButton::Left, MouseButton::Right],
            )
            .unwrap();
        assert_eq!(batcher.len(), 1);
        batcher.flush().unwrap();

        match rx.try_recv().unwrap() {
            HidCommand::MouseBatchFixed { len, frames } => {
                assert_eq!(len as usize, MOUSE_BATCH_FRAMES);
                assert_eq!(
                    frames[MOUSE_BATCH_FRAMES - 1].dx,
                    MOUSE_BATCH_FRAMES as i32 - 1
                );
                assert_eq!(
                    frames[MOUSE_BATCH_FRAMES - 1].dy,
                    -(MOUSE_BATCH_FRAMES as i32 - 1)
                );
            }
            other => panic!("expected MouseBatchFixed command, got {other:?}"),
        }
        match rx.try_recv().unwrap() {
            HidCommand::MouseMotion { dx, dy, buttons } => {
                assert_eq!((dx, dy), (MOUSE_BATCH_FRAMES as i32, 1));
                assert_eq!(buttons, MouseButton::Left as u8 | MouseButton::Right as u8);
            }
            other => panic!("expected single MouseMotion command, got {other:?}"),
        }
    }

    #[test]
    fn mouse_frame_batcher_push_many_slice_splits_at_capacity() {
        let (tx, rx) = sync_channel(2);
        let client = HidClient { tx };
        let mut batcher = MouseFrameBatcher::new(&client);
        let frames: Vec<_> = (0..MOUSE_BATCH_FRAMES + 2)
            .map(|i| MouseFrame::motion(i as i32, 0, 0))
            .collect();

        batcher.push_many_slice(&frames).unwrap();
        assert_eq!(batcher.len(), 2);
        batcher.flush().unwrap();

        match rx.try_recv().unwrap() {
            HidCommand::MouseBatchFixed { len, frames } => {
                assert_eq!(len as usize, MOUSE_BATCH_FRAMES);
                assert_eq!(
                    frames[MOUSE_BATCH_FRAMES - 1].dx,
                    MOUSE_BATCH_FRAMES as i32 - 1
                );
            }
            other => panic!("expected MouseBatchFixed command, got {other:?}"),
        }
        match rx.try_recv().unwrap() {
            HidCommand::MouseBatchFixed { len, frames } => {
                assert_eq!(len, 2);
                assert_eq!(frames[0].dx, MOUSE_BATCH_FRAMES as i32);
                assert_eq!(frames[1].dx, MOUSE_BATCH_FRAMES as i32 + 1);
            }
            other => panic!("expected MouseBatchFixed command, got {other:?}"),
        }
    }

    #[test]
    fn mouse_frame_batcher_try_push_many_slice_keeps_batch_on_backpressure() {
        let (tx, rx) = sync_channel(1);
        let client = HidClient { tx };
        let mut batcher = MouseFrameBatcher::new(&client);
        let frames: Vec<_> = (0..MOUSE_BATCH_FRAMES)
            .map(|i| MouseFrame::motion(i as i32, 0, MouseButton::Left as u8))
            .collect();

        batcher.try_push_many_slice(&frames).unwrap();
        assert_eq!(batcher.len(), 0);
        let queued = rx.try_recv().unwrap();
        let HidCommand::MouseBatchFixed { len, .. } = queued else {
            panic!("expected first fixed mouse batch");
        };
        assert_eq!(len as usize, MOUSE_BATCH_FRAMES);

        batcher.try_push_many_slice(&frames).unwrap();
        assert_eq!(batcher.len(), 0);
        let err = batcher.try_push_many_slice(&frames).unwrap_err();
        assert!(matches!(
            err,
            Error::SessionLifecycle("channel full (back-pressure)")
        ));
        assert_eq!(batcher.len(), MOUSE_BATCH_FRAMES);
    }

    #[test]
    fn mouse_frame_batcher_try_helpers_use_expected_command() {
        let (tx, rx) = sync_channel(1);
        let client = HidClient { tx };
        let mut batcher = MouseFrameBatcher::new(&client);

        batcher
            .try_motion_buttons(1, -1, &[MouseButton::Left])
            .unwrap();
        batcher.try_buttons(MouseButton::Middle as u8).unwrap();
        batcher.try_button_state(&[MouseButton::X2]).unwrap();
        batcher.try_flush().unwrap();

        match rx.try_recv().unwrap() {
            HidCommand::MouseBatchFixed { len, frames } => {
                assert_eq!(len, 3);
                assert_eq!(
                    frames[0],
                    MouseFrame::motion(1, -1, MouseButton::Left as u8)
                );
                assert_eq!(frames[1], MouseFrame::buttons(MouseButton::Middle as u8));
                assert_eq!(frames[2], MouseFrame::buttons(MouseButton::X2 as u8));
            }
            other => panic!("expected MouseBatchFixed command, got {other:?}"),
        }
    }

    #[test]
    fn mouse_frame_batcher_dispatches_all_mouse_frames() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::mouse_only()).unwrap();
        let (client, dispatcher) = session.into_client().unwrap();
        let expected = MOUSE_BATCH_FRAMES + 3;

        {
            let mut batcher = MouseFrameBatcher::new(&client);
            for i in 0..expected {
                batcher
                    .motion(i as i32, -(i as i32), MouseButton::Left as u8)
                    .unwrap();
            }
            batcher.flush().unwrap();
        }

        client.close();
        let bytes = dispatcher.join().unwrap().into_bytes();
        let payloads = mouse_input_payloads(&bytes);
        assert_eq!(payloads.len(), expected);
        assert_eq!(payloads[0], [MouseButton::Left as u8, 0, 0, 0, 0]);
        assert_eq!(
            payloads[MOUSE_BATCH_FRAMES],
            [
                MouseButton::Left as u8,
                MOUSE_BATCH_FRAMES as u8,
                (-(MOUSE_BATCH_FRAMES as i8)) as u8,
                0,
                0,
            ]
        );
    }

    #[test]
    fn android_intent_shortcuts_use_expected_commands() {
        let (tx, rx) = sync_channel(32);
        let client = HidClient { tx };

        client.press_home().unwrap();
        client.press_back().unwrap();
        client.open_recents().unwrap();
        client.volume_up().unwrap();
        client.press_android_key(AndroidKeycode::POWER).unwrap();
        client.try_inject_keycode(1, 66, 2, 3).unwrap();
        client.try_press_android_key(AndroidKeycode::ENTER).unwrap();
        client
            .inject_android_key_event(AndroidKeyAction::UP, AndroidKeycode::MENU, 4, 5)
            .unwrap();
        client
            .try_release_android_key(AndroidKeycode::POWER)
            .unwrap();
        client.back_or_screen_on(AndroidKeyAction::UP).unwrap();
        client
            .try_back_or_screen_on(AndroidKeyAction::DOWN)
            .unwrap();
        client
            .try_scroll_with_buttons(100, 200, 8.0, -16.0, 0x11)
            .unwrap();
        client.set_screen_power(false).unwrap();
        client.show_notifications().unwrap();
        client.show_quick_settings().unwrap();
        client.collapse_panels().unwrap();
        client.rotate_device().unwrap();
        client.resize_display(720, 1280).unwrap();
        client.set_torch(true).unwrap();
        client.camera_zoom_in().unwrap();
        client.camera_zoom_out().unwrap();
        client.open_hard_keyboard_settings().unwrap();
        client.reset_video().unwrap();
        client.launch_app("com.android.settings").unwrap();
        client.set_clipboard("text", false).unwrap();
        client.set_clipboard_sequenced(9, "paste", true).unwrap();

        match rx.try_recv().unwrap() {
            HidCommand::InjectKeycode {
                action: 0,
                keycode: 3,
                repeat: 0,
                metastate: 0,
            } => {}
            other => panic!("expected Home InjectKeycode command, got {other:?}"),
        }
        match rx.try_recv().unwrap() {
            HidCommand::InjectKeycode {
                action: 0,
                keycode: 4,
                repeat: 0,
                metastate: 0,
            } => {}
            other => panic!("expected Back InjectKeycode command, got {other:?}"),
        }
        match rx.try_recv().unwrap() {
            HidCommand::InjectKeycode {
                action: 0,
                keycode: 187,
                repeat: 0,
                metastate: 0,
            } => {}
            other => panic!("expected Recents InjectKeycode command, got {other:?}"),
        }
        match rx.try_recv().unwrap() {
            HidCommand::InjectKeycode {
                action: 0,
                keycode: 24,
                repeat: 0,
                metastate: 0,
            } => {}
            other => panic!("expected VolumeUp InjectKeycode command, got {other:?}"),
        }
        match rx.try_recv().unwrap() {
            HidCommand::InjectKeycode {
                action: 0,
                keycode: 26,
                repeat: 0,
                metastate: 0,
            } => {}
            other => panic!("expected Power InjectKeycode command, got {other:?}"),
        }
        match rx.try_recv().unwrap() {
            HidCommand::InjectKeycode {
                action: 1,
                keycode: 66,
                repeat: 2,
                metastate: 3,
            } => {}
            other => panic!("expected custom InjectKeycode command, got {other:?}"),
        }
        match rx.try_recv().unwrap() {
            HidCommand::InjectKeycode {
                action: 0,
                keycode: 66,
                repeat: 0,
                metastate: 0,
            } => {}
            other => panic!("expected Enter InjectKeycode command, got {other:?}"),
        }
        match rx.try_recv().unwrap() {
            HidCommand::InjectKeycode {
                action: 1,
                keycode: 82,
                repeat: 4,
                metastate: 5,
            } => {}
            other => panic!("expected Menu release InjectKeycode command, got {other:?}"),
        }
        match rx.try_recv().unwrap() {
            HidCommand::InjectKeycode {
                action: 1,
                keycode: 26,
                repeat: 0,
                metastate: 0,
            } => {}
            other => panic!("expected Power release InjectKeycode command, got {other:?}"),
        }
        match rx.try_recv().unwrap() {
            HidCommand::BackOrScreenOn { action: 1 } => {}
            other => panic!("expected BackOrScreenOn UP command, got {other:?}"),
        }
        match rx.try_recv().unwrap() {
            HidCommand::BackOrScreenOn { action: 0 } => {}
            other => panic!("expected BackOrScreenOn DOWN command, got {other:?}"),
        }
        match rx.try_recv().unwrap() {
            HidCommand::InjectScroll {
                x: 100,
                y: 200,
                hscroll,
                vscroll,
                buttons: 0x11,
            } => {
                assert_eq!(hscroll, 8.0);
                assert_eq!(vscroll, -16.0);
            }
            other => panic!("expected InjectScroll command, got {other:?}"),
        }
        assert!(matches!(
            rx.try_recv().unwrap(),
            HidCommand::SetScreenPower { on: false }
        ));
        assert!(matches!(
            rx.try_recv().unwrap(),
            HidCommand::ShowNotifications
        ));
        assert!(matches!(
            rx.try_recv().unwrap(),
            HidCommand::ShowQuickSettings
        ));
        assert!(matches!(rx.try_recv().unwrap(), HidCommand::CollapsePanels));
        assert!(matches!(rx.try_recv().unwrap(), HidCommand::RotateDevice));
        assert!(matches!(
            rx.try_recv().unwrap(),
            HidCommand::ResizeDisplay {
                width: 720,
                height: 1280
            }
        ));
        assert!(matches!(
            rx.try_recv().unwrap(),
            HidCommand::SetTorch { on: true }
        ));
        assert!(matches!(rx.try_recv().unwrap(), HidCommand::CameraZoomIn));
        assert!(matches!(rx.try_recv().unwrap(), HidCommand::CameraZoomOut));
        assert!(matches!(
            rx.try_recv().unwrap(),
            HidCommand::OpenHardKeyboardSettings
        ));
        assert!(matches!(rx.try_recv().unwrap(), HidCommand::ResetVideo));
        assert!(matches!(
            rx.try_recv().unwrap(),
            HidCommand::LaunchApp { name } if name == "com.android.settings"
        ));
        assert!(matches!(
            rx.try_recv().unwrap(),
            HidCommand::SetClipboard { text, paste: false } if text == "text"
        ));
        assert!(matches!(
            rx.try_recv().unwrap(),
            HidCommand::SetClipboardSequenced {
                sequence: 9,
                text,
                paste: true
            } if text == "paste"
        ));
    }

    #[test]
    fn android_key_tap_shortcuts_use_single_command() {
        let (tx, rx) = sync_channel(3);
        let client = HidClient { tx };

        client.tap_android_key(AndroidKeycode::ENTER).unwrap();
        client
            .tap_android_key_with_metastate(AndroidKeycode::MENU, 0x41)
            .unwrap();
        client.try_tap_android_keycode(26, 0x80).unwrap();

        match rx.try_recv().unwrap() {
            HidCommand::AndroidKeyTap {
                keycode: 66,
                metastate: 0,
            } => {}
            other => panic!("expected Enter AndroidKeyTap command, got {other:?}"),
        }
        match rx.try_recv().unwrap() {
            HidCommand::AndroidKeyTap {
                keycode: 82,
                metastate: 0x41,
            } => {}
            other => panic!("expected Menu AndroidKeyTap command, got {other:?}"),
        }
        match rx.try_recv().unwrap() {
            HidCommand::AndroidKeyTap {
                keycode: 26,
                metastate: 0x80,
            } => {}
            other => panic!("expected Power AndroidKeyTap command, got {other:?}"),
        }
    }

    #[test]
    fn android_key_batch_fixed_uses_expected_commands() {
        let (tx, rx) = sync_channel(2);
        let client = HidClient { tx };
        let mut frames = [AndroidKeyFrame::EMPTY; ANDROID_KEY_BATCH_FRAMES];
        frames[0] = AndroidKeyFrame::typed(AndroidKeyAction::DOWN, AndroidKeycode::ENTER, 0, 3);
        frames[1] = AndroidKeyFrame::typed(AndroidKeyAction::UP, AndroidKeycode::ENTER, 0, 3);

        client.send_android_key_batch_fixed(2, frames).unwrap();
        client.try_send_android_key_batch_fixed(1, frames).unwrap();

        match rx.try_recv().unwrap() {
            HidCommand::AndroidKeyBatchFixed { len, frames } => {
                assert_eq!(len, 2);
                assert_eq!(
                    frames[0],
                    AndroidKeyFrame::typed(AndroidKeyAction::DOWN, AndroidKeycode::ENTER, 0, 3)
                );
                assert_eq!(
                    frames[1],
                    AndroidKeyFrame::typed(AndroidKeyAction::UP, AndroidKeycode::ENTER, 0, 3)
                );
            }
            other => panic!("expected AndroidKeyBatchFixed command, got {other:?}"),
        }
        match rx.try_recv().unwrap() {
            HidCommand::InjectKeycode {
                action: 0,
                keycode: 66,
                repeat: 0,
                metastate: 3,
            } => {}
            other => panic!("expected single InjectKeycode command, got {other:?}"),
        }
    }

    #[test]
    fn android_key_frame_batcher_flushes_at_fixed_capacity() {
        let (tx, rx) = sync_channel(2);
        let client = HidClient { tx };
        let mut batcher = AndroidKeyFrameBatcher::new(&client);

        for _ in 0..ANDROID_KEY_BATCH_FRAMES {
            batcher
                .key_event(AndroidKeyAction::DOWN, AndroidKeycode::ENTER, 0, 0)
                .unwrap();
        }
        assert_eq!(batcher.len(), ANDROID_KEY_BATCH_FRAMES);
        assert!(
            rx.try_recv().is_err(),
            "batcher should not flush before capacity is exceeded"
        );

        batcher
            .key_event(AndroidKeyAction::DOWN, AndroidKeycode::MENU, 0, 0)
            .unwrap();
        assert_eq!(batcher.len(), 1);
        batcher.flush().unwrap();

        match rx.try_recv().unwrap() {
            HidCommand::AndroidKeyBatchFixed { len, frames } => {
                assert_eq!(len as usize, ANDROID_KEY_BATCH_FRAMES);
                assert_eq!(frames[ANDROID_KEY_BATCH_FRAMES - 1].keycode, 66);
            }
            other => panic!("expected AndroidKeyBatchFixed command, got {other:?}"),
        }
        match rx.try_recv().unwrap() {
            HidCommand::InjectKeycode {
                action: 0,
                keycode: 82,
                repeat: 0,
                metastate: 0,
            } => {}
            other => panic!("expected single InjectKeycode command, got {other:?}"),
        }
    }

    #[test]
    fn android_key_frame_batcher_try_push_many_slice_keeps_batch_on_backpressure() {
        let (tx, rx) = sync_channel(1);
        let client = HidClient { tx };
        let mut batcher = AndroidKeyFrameBatcher::new(&client);
        let frames: Vec<_> = (0..ANDROID_KEY_BATCH_FRAMES)
            .map(|_| AndroidKeyFrame::down(AndroidKeycode::ENTER, 0))
            .collect();

        batcher.try_push_many_slice(&frames).unwrap();
        assert_eq!(batcher.len(), 0);
        let queued = rx.try_recv().unwrap();
        let HidCommand::AndroidKeyBatchFixed { len, .. } = queued else {
            panic!("expected first fixed Android key batch");
        };
        assert_eq!(len as usize, ANDROID_KEY_BATCH_FRAMES);

        batcher.try_push_many_slice(&frames).unwrap();
        assert_eq!(batcher.len(), 0);
        let err = batcher.try_push_many_slice(&frames).unwrap_err();
        assert!(matches!(
            err,
            Error::SessionLifecycle("channel full (back-pressure)")
        ));
        assert_eq!(batcher.len(), ANDROID_KEY_BATCH_FRAMES);
    }

    #[test]
    fn android_key_frame_batcher_dispatches_all_key_events() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let (client, dispatcher) = session.into_client().unwrap();

        {
            let mut batcher = AndroidKeyFrameBatcher::new(&client);
            batcher
                .tap_key_with_metastate(AndroidKeycode::ENTER, 3)
                .unwrap();
            batcher
                .key_event(AndroidKeyAction::UP, AndroidKeycode::MENU, 2, 4)
                .unwrap();
            batcher.flush().unwrap();
        }

        client.close();
        let bytes = dispatcher.join().unwrap().into_bytes();
        assert_eq!(first_inject_keycodes(&bytes, 3), vec![66, 66, 82]);
        let events: Vec<_> = bytes
            .chunks_exact(14)
            .map(|frame| {
                (
                    frame[1],
                    u32::from_be_bytes(frame[2..6].try_into().unwrap()),
                    u32::from_be_bytes(frame[6..10].try_into().unwrap()),
                    u32::from_be_bytes(frame[10..14].try_into().unwrap()),
                )
            })
            .collect();
        assert_eq!(events, vec![(0, 66, 0, 3), (1, 66, 0, 3), (1, 82, 2, 4)]);
    }

    #[test]
    fn scroll_batch_fixed_uses_expected_commands() {
        let (tx, rx) = sync_channel(2);
        let client = HidClient { tx };
        let mut frames = [ScrollFrame::EMPTY; SCROLL_BATCH_FRAMES];
        frames[0] = ScrollFrame::new(100, 200, 8.0, -16.0, 0x11);
        frames[1] = ScrollFrame::scroll(300, 400, 0.0, 16.0);

        client.send_scroll_batch_fixed(2, frames).unwrap();
        client.try_send_scroll_batch_fixed(1, frames).unwrap();

        match rx.try_recv().unwrap() {
            HidCommand::InjectScrollBatchFixed { len, frames } => {
                assert_eq!(len, 2);
                assert_eq!(frames[0], ScrollFrame::new(100, 200, 8.0, -16.0, 0x11));
                assert_eq!(frames[1], ScrollFrame::scroll(300, 400, 0.0, 16.0));
            }
            other => panic!("expected InjectScrollBatchFixed command, got {other:?}"),
        }
        match rx.try_recv().unwrap() {
            HidCommand::InjectScroll {
                x: 100,
                y: 200,
                hscroll,
                vscroll,
                buttons: 0x11,
            } => {
                assert_eq!(hscroll, 8.0);
                assert_eq!(vscroll, -16.0);
            }
            other => panic!("expected single InjectScroll command, got {other:?}"),
        }
    }

    #[test]
    fn scroll_frame_batcher_flushes_at_fixed_capacity() {
        let (tx, rx) = sync_channel(2);
        let client = HidClient { tx };
        let mut batcher = ScrollFrameBatcher::new(&client);

        for i in 0..SCROLL_BATCH_FRAMES {
            batcher
                .scroll(i as i32, -(i as i32), 0.0, i as f32)
                .unwrap();
        }
        assert_eq!(batcher.len(), SCROLL_BATCH_FRAMES);
        assert!(
            rx.try_recv().is_err(),
            "batcher should not flush before capacity is exceeded"
        );

        batcher
            .scroll_with_buttons(99, 88, 1.0, -1.0, 0x11)
            .unwrap();
        assert_eq!(batcher.len(), 1);
        batcher.flush().unwrap();

        match rx.try_recv().unwrap() {
            HidCommand::InjectScrollBatchFixed { len, frames } => {
                assert_eq!(len as usize, SCROLL_BATCH_FRAMES);
                assert_eq!(
                    frames[SCROLL_BATCH_FRAMES - 1].x,
                    SCROLL_BATCH_FRAMES as i32 - 1
                );
                assert_eq!(
                    frames[SCROLL_BATCH_FRAMES - 1].y,
                    -(SCROLL_BATCH_FRAMES as i32 - 1)
                );
            }
            other => panic!("expected InjectScrollBatchFixed command, got {other:?}"),
        }
        match rx.try_recv().unwrap() {
            HidCommand::InjectScroll {
                x: 99,
                y: 88,
                hscroll,
                vscroll,
                buttons: 0x11,
            } => {
                assert_eq!(hscroll, 1.0);
                assert_eq!(vscroll, -1.0);
            }
            other => panic!("expected single InjectScroll command, got {other:?}"),
        }
    }

    #[test]
    fn scroll_frame_batcher_try_push_many_slice_keeps_batch_on_backpressure() {
        let (tx, rx) = sync_channel(1);
        let client = HidClient { tx };
        let mut batcher = ScrollFrameBatcher::new(&client);
        let frames: Vec<_> = (0..SCROLL_BATCH_FRAMES)
            .map(|i| ScrollFrame::scroll(i as i32, 0, 0.0, 1.0))
            .collect();

        batcher.try_push_many_slice(&frames).unwrap();
        assert_eq!(batcher.len(), 0);
        let queued = rx.try_recv().unwrap();
        let HidCommand::InjectScrollBatchFixed { len, .. } = queued else {
            panic!("expected first fixed scroll batch");
        };
        assert_eq!(len as usize, SCROLL_BATCH_FRAMES);

        batcher.try_push_many_slice(&frames).unwrap();
        assert_eq!(batcher.len(), 0);
        let err = batcher.try_push_many_slice(&frames).unwrap_err();
        assert!(matches!(
            err,
            Error::SessionLifecycle("channel full (back-pressure)")
        ));
        assert_eq!(batcher.len(), SCROLL_BATCH_FRAMES);
    }

    #[test]
    fn scroll_frame_batcher_dispatches_all_scroll_events() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let (client, dispatcher) = session.into_client().unwrap();
        let expected = SCROLL_BATCH_FRAMES + 3;

        {
            let mut batcher = ScrollFrameBatcher::new(&client);
            for i in 0..expected {
                batcher
                    .scroll_with_buttons(i as i32, i as i32 + 10, 0.0, 1.0, 0x11)
                    .unwrap();
            }
            batcher.flush().unwrap();
        }

        client.close();
        let bytes = dispatcher.join().unwrap().into_bytes();
        let frames: Vec<_> = bytes.chunks_exact(21).collect();
        assert_eq!(frames.len(), expected);
        assert!(frames.iter().all(|frame| frame[0] == 3));
        assert_eq!(i32::from_be_bytes(frames[0][1..5].try_into().unwrap()), 0);
        assert_eq!(
            i32::from_be_bytes(frames[SCROLL_BATCH_FRAMES][1..5].try_into().unwrap()),
            SCROLL_BATCH_FRAMES as i32
        );
        assert_eq!(
            i32::from_be_bytes(frames[expected - 1][5..9].try_into().unwrap()),
            expected as i32 + 9
        );
    }

    #[test]
    fn screen_size_shortcuts_use_expected_commands() {
        let (tx, rx) = sync_channel(2);
        let client = HidClient { tx };

        client.set_screen_size(1440, 3120).unwrap();
        client.try_set_screen_size(1080, 2400).unwrap();

        match rx.try_recv().unwrap() {
            HidCommand::SetScreenSize {
                width: 1440,
                height: 3120,
            } => {}
            other => panic!("expected SetScreenSize command, got {other:?}"),
        }
        match rx.try_recv().unwrap() {
            HidCommand::SetScreenSize {
                width: 1080,
                height: 2400,
            } => {}
            other => panic!("expected SetScreenSize command, got {other:?}"),
        }
    }

    #[test]
    fn screen_size_command_affects_subsequent_touch_frames() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let (client, dispatcher) = session.into_client().unwrap();

        client.set_screen_size(1440, 3120).unwrap();
        client
            .send(HidCommand::MultitouchDown {
                id: 0,
                x: 10,
                y: 20,
                pressure: 1.0,
            })
            .unwrap();
        client.close();
        let transport = dispatcher.join().unwrap();

        assert_eq!(
            first_touch_screen_size(&transport.into_bytes()),
            Some((1440, 3120))
        );
    }

    #[test]
    fn android_intent_commands_dispatch_to_control_messages() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let (client, dispatcher) = session.into_client().unwrap();

        client.press_home().unwrap();
        client.press_back().unwrap();
        client.open_recents().unwrap();
        client.volume_down().unwrap();
        client.volume_mute().unwrap();
        client.back_or_screen_on(AndroidKeyAction::UP).unwrap();
        client
            .try_back_or_screen_on(AndroidKeyAction::DOWN)
            .unwrap();
        client.set_screen_size(720, 1280).unwrap();
        client
            .scroll_with_buttons(100, 200, 8.0, -16.0, 0x11)
            .unwrap();
        client.try_scroll(300, 400, 0.0, 16.0).unwrap();
        client.show_notifications().unwrap();
        client.show_quick_settings().unwrap();
        client.collapse_panels().unwrap();
        client.rotate_device().unwrap();
        client.resize_display(720, 1280).unwrap();
        client.set_torch(true).unwrap();
        client.camera_zoom_in().unwrap();
        client.camera_zoom_out().unwrap();
        client.open_hard_keyboard_settings().unwrap();
        client.reset_video().unwrap();
        client.close();
        let transport = dispatcher.join().unwrap();
        let bytes = transport.into_bytes();

        assert_eq!(first_inject_keycodes(&bytes, 5), vec![3, 4, 187, 25, 164]);
        assert_eq!(&bytes[70..72], &[4, 1]);
        assert_eq!(&bytes[72..74], &[4, 0]);
        let scroll = &bytes[74..95];
        assert_eq!(scroll[0], 3);
        assert_eq!(i32::from_be_bytes(scroll[1..5].try_into().unwrap()), 100);
        assert_eq!(i32::from_be_bytes(scroll[5..9].try_into().unwrap()), 200);
        assert_eq!(u16::from_be_bytes(scroll[9..11].try_into().unwrap()), 720);
        assert_eq!(u16::from_be_bytes(scroll[11..13].try_into().unwrap()), 1280);
        assert_eq!(
            u16::from_be_bytes(scroll[13..15].try_into().unwrap()),
            0x4000
        );
        assert_eq!(
            u16::from_be_bytes(scroll[15..17].try_into().unwrap()),
            0x8000
        );
        assert_eq!(u32::from_be_bytes(scroll[17..21].try_into().unwrap()), 0x11);
        let scroll = &bytes[95..116];
        assert_eq!(scroll[0], 3);
        assert_eq!(i32::from_be_bytes(scroll[1..5].try_into().unwrap()), 300);
        assert_eq!(i32::from_be_bytes(scroll[5..9].try_into().unwrap()), 400);
        assert_eq!(u16::from_be_bytes(scroll[13..15].try_into().unwrap()), 0);
        assert_eq!(
            u16::from_be_bytes(scroll[15..17].try_into().unwrap()),
            0x7FFF
        );
        for tag in [5, 6, 7, 11, 18, 19, 20, 15, 17] {
            assert!(bytes.contains(&tag), "missing control tag {tag}");
        }
        let resize = bytes
            .windows(5)
            .find(|frame| frame[0] == 21)
            .expect("RESIZE_DISPLAY frame");
        assert_eq!(u16::from_be_bytes([resize[1], resize[2]]), 720);
        assert_eq!(u16::from_be_bytes([resize[3], resize[4]]), 1280);
    }

    #[test]
    fn flush_wait_acknowledges_prior_dispatch_work() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let (client, dispatcher) = session.into_client().unwrap();

        client
            .send(HidCommand::MultitouchDown {
                id: 0,
                x: 10,
                y: 20,
                pressure: 1.0,
            })
            .unwrap();
        client.flush_wait().unwrap();
        client.close();
        let transport = dispatcher.join().unwrap();

        assert_eq!(count_touch_events(&transport.into_bytes()), 1);
    }

    #[test]
    fn try_flush_wait_acknowledges_prior_dispatch_work() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let (client, dispatcher) = session.into_client().unwrap();

        client
            .send(HidCommand::MultitouchDown {
                id: 0,
                x: 10,
                y: 20,
                pressure: 1.0,
            })
            .unwrap();
        client.try_flush_wait().unwrap();
        client.close();
        let transport = dispatcher.join().unwrap();

        assert_eq!(count_touch_events(&transport.into_bytes()), 1);
    }

    #[test]
    fn try_flush_wait_reports_backpressure_without_blocking() {
        let (tx, _rx) = sync_channel(1);
        let client = HidClient { tx };

        client.try_flush().unwrap();
        let err = client.try_flush_wait().unwrap_err();

        assert!(matches!(
            err,
            Error::SessionLifecycle("channel full (back-pressure)")
        ));
    }

    #[test]
    fn flush_wait_surfaces_prior_command_error_once() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let (client, dispatcher) = session.into_client().unwrap();

        client.send_button(GamepadButton::South, true).unwrap();
        client
            .send(HidCommand::MultitouchDown {
                id: 0,
                x: 10,
                y: 20,
                pressure: 1.0,
            })
            .unwrap();
        let err = client.flush_wait().unwrap_err();
        assert!(matches!(err, Error::SessionLifecycle("gamepad not open")));

        assert_eq!(
            client.flush_wait().unwrap(),
            0,
            "first error barrier should still flush valid prior work"
        );
        client.close();
        let transport = dispatcher.join().unwrap();
        assert_eq!(count_touch_events(&transport.into_bytes()), 1);
    }

    #[test]
    fn try_flush_wait_surfaces_prior_command_error_once() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let (client, dispatcher) = session.into_client().unwrap();

        client.send_button(GamepadButton::South, true).unwrap();
        client
            .send(HidCommand::MultitouchDown {
                id: 0,
                x: 10,
                y: 20,
                pressure: 1.0,
            })
            .unwrap();
        let err = client.try_flush_wait().unwrap_err();
        assert!(matches!(err, Error::SessionLifecycle("gamepad not open")));

        assert_eq!(
            client.try_flush_wait().unwrap(),
            0,
            "first error barrier should still flush valid prior work"
        );
        client.close();
        let transport = dispatcher.join().unwrap();
        assert_eq!(count_touch_events(&transport.into_bytes()), 1);
    }

    #[test]
    fn close_wait_reports_prior_error_and_still_closes() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let (client, dispatcher) = session.into_client().unwrap();

        client.send_button(GamepadButton::South, true).unwrap();
        client
            .send(HidCommand::MultitouchDown {
                id: 0,
                x: 10,
                y: 20,
                pressure: 1.0,
            })
            .unwrap();

        let err = client.close_wait().unwrap_err();
        assert!(matches!(err, Error::SessionLifecycle("gamepad not open")));

        let transport = dispatcher.join().unwrap();
        assert_eq!(count_touch_events(&transport.into_bytes()), 1);
    }

    #[test]
    fn gesture_helpers_dispatch_expected_touch_frames() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let (client, dispatcher) = session.into_client().unwrap();

        client.tap(10, 20).unwrap();
        client.double_tap(30, 40).unwrap();
        client.swipe((0, 0), (30, 60), 3).unwrap();
        client.long_press(50, 60, Duration::from_millis(0)).unwrap();
        client.three_finger_screenshot(1080, 2400).unwrap();
        client.close();
        let transport = dispatcher.join().unwrap();

        assert_eq!(
            count_touch_events(&transport.into_bytes()),
            2 + 4 + 5 + 2 + 36
        );
    }

    #[test]
    fn flush_command_round_trip() {
        let (tx, rx) = sync_channel(2);
        let client = HidClient { tx };

        client.flush().unwrap();
        client.try_flush().unwrap();

        assert!(matches!(rx.try_recv().unwrap(), HidCommand::Flush));
        assert!(matches!(rx.try_recv().unwrap(), HidCommand::Flush));
    }

    #[test]
    fn clipboard_request_helpers_are_request_only_commands() {
        let (tx, rx) = sync_channel(4);
        let client = HidClient { tx };

        client.request_clipboard(1).unwrap();
        client.try_request_clipboard(2).unwrap();
        client
            .request_clipboard_key(ClipboardCopyKey::NONE)
            .unwrap();
        client
            .try_request_clipboard_key(ClipboardCopyKey::COPY)
            .unwrap();

        match rx.try_recv().unwrap() {
            HidCommand::GetClipboard { copy_key: 1 } => {}
            other => panic!("expected GetClipboard copy_key=1, got {other:?}"),
        }
        match rx.try_recv().unwrap() {
            HidCommand::GetClipboard { copy_key: 2 } => {}
            other => panic!("expected GetClipboard copy_key=2, got {other:?}"),
        }
        match rx.try_recv().unwrap() {
            HidCommand::GetClipboard { copy_key: 0 } => {}
            other => panic!("expected GetClipboard copy_key=0, got {other:?}"),
        }
        match rx.try_recv().unwrap() {
            HidCommand::GetClipboard { copy_key: 1 } => {}
            other => panic!("expected GetClipboard copy_key=1, got {other:?}"),
        }
    }

    #[test]
    fn ai_extension_shortcuts_use_expected_commands() {
        let (tx, rx) = sync_channel(6);
        let client = HidClient { tx };

        client.configure_ai(0x1f, 16, 64).unwrap();
        client.try_configure_ai(0x07, 8, 0).unwrap();
        client.query_ai(0x0102_0304_0506_0708).unwrap();
        client.try_query_ai(9).unwrap();
        client.pause_ai().unwrap();
        client.try_pause_ai().unwrap();

        assert!(matches!(
            rx.try_recv().unwrap(),
            HidCommand::AiConfig {
                flags: 0x1f,
                sample_interval_ms: 16,
                feature_dim: 64
            }
        ));
        assert!(matches!(
            rx.try_recv().unwrap(),
            HidCommand::AiConfig {
                flags: 0x07,
                sample_interval_ms: 8,
                feature_dim: 0
            }
        ));
        assert!(matches!(
            rx.try_recv().unwrap(),
            HidCommand::AiQuery {
                since_timestamp_ms: 0x0102_0304_0506_0708
            }
        ));
        assert!(matches!(
            rx.try_recv().unwrap(),
            HidCommand::AiQuery {
                since_timestamp_ms: 9
            }
        ));
        assert!(matches!(rx.try_recv().unwrap(), HidCommand::AiPause));
        assert!(matches!(rx.try_recv().unwrap(), HidCommand::AiPause));
    }

    #[test]
    fn ai_extension_commands_dispatch_to_control_messages() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let (client, dispatcher) = session.into_client().unwrap();
        let flags = crate::control::message::AI_FLAG_KEYFRAMES
            | crate::control::message::AI_FLAG_FEATURES
            | crate::control::message::AI_FLAG_MOTION
            | crate::control::message::AI_FLAG_OBJECTS
            | crate::control::message::AI_FLAG_TEXT;

        client.configure_ai(flags, 16, 64).unwrap();
        client.try_query_ai(0x0102_0304_0506_0708).unwrap();
        client.pause_ai().unwrap();
        client.close();
        let transport = dispatcher.join().unwrap();

        let mut expected = vec![22, flags, 0, 16, 0, 64, 23];
        expected.extend_from_slice(&0x0102_0304_0506_0708u64.to_be_bytes());
        expected.push(24);
        assert_eq!(transport.into_bytes(), expected);
    }

    #[test]
    fn touch_batch_fixed_dispatches_all_touch_frames() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let (client, dispatcher) = session.into_client().unwrap();
        let mut frames = [TouchFrame::EMPTY; TOUCH_BATCH_FRAMES];
        frames[0] = TouchFrame::new(crate::multitouch::ACTION_DOWN, 0, 10, 20, 1.0);
        frames[1] = TouchFrame::new(crate::multitouch::ACTION_MOVE, 0, 15, 25, 1.0);
        frames[2] = TouchFrame::new(crate::multitouch::ACTION_UP, 0, 15, 25, 0.0);

        client.send_touch_batch_fixed(3, frames).unwrap();
        client.close();
        let transport = dispatcher.join().unwrap();

        assert_eq!(count_touch_events(&transport.into_bytes()), 3);
    }

    #[test]
    fn touch_batch_fixed_rejects_oversized_len() {
        let (tx, _rx) = sync_channel(1);
        let client = HidClient { tx };
        let frames = [TouchFrame::EMPTY; TOUCH_BATCH_FRAMES];
        let err = client
            .send_touch_batch_fixed(TOUCH_BATCH_FRAMES + 1, frames)
            .unwrap_err();

        assert!(matches!(err, Error::SessionLifecycle(_)));
    }

    #[test]
    fn touch_frame_batcher_flushes_at_fixed_capacity() {
        let (tx, rx) = sync_channel(2);
        let client = HidClient { tx };
        let mut batcher = TouchFrameBatcher::new(&client);

        for i in 0..TOUCH_BATCH_FRAMES {
            batcher.move_to(0, i as i32, 0, 1.0).unwrap();
        }
        assert_eq!(batcher.len(), TOUCH_BATCH_FRAMES);
        assert!(
            rx.try_recv().is_err(),
            "batcher should not flush before capacity is exceeded"
        );

        batcher
            .move_to(0, TOUCH_BATCH_FRAMES as i32, 0, 1.0)
            .unwrap();
        assert_eq!(batcher.len(), 1);
        batcher.flush().unwrap();

        match rx.try_recv().unwrap() {
            HidCommand::TouchBatchFixed { len, frames } => {
                assert_eq!(len as usize, TOUCH_BATCH_FRAMES);
                assert_eq!(
                    frames[TOUCH_BATCH_FRAMES - 1].x,
                    TOUCH_BATCH_FRAMES as i32 - 1
                );
            }
            other => panic!("expected TouchBatchFixed command, got {other:?}"),
        }
        match rx.try_recv().unwrap() {
            HidCommand::TouchBatchFixed { len, frames } => {
                assert_eq!(len, 1);
                assert_eq!(frames[0].x, TOUCH_BATCH_FRAMES as i32);
            }
            other => panic!("expected TouchBatchFixed command, got {other:?}"),
        }
    }

    #[test]
    fn touch_pointer_id_helpers_preserve_scrcpy_reserved_ids_in_batches() {
        let (tx, rx) = sync_channel(1);
        let client = HidClient { tx };
        let pointer = TouchPointerId::GENERIC_FINGER;
        let mut batcher = TouchFrameBatcher::new(&client);

        batcher.down_pointer(pointer, 10, 20, 1.0).unwrap();
        batcher.move_pointer_to(pointer, 15, 25, 0.5).unwrap();
        batcher.up_pointer(pointer, 15, 25).unwrap();
        batcher.cancel_pointer(pointer).unwrap();
        batcher.flush().unwrap();

        match rx.try_recv().unwrap() {
            HidCommand::TouchBatchFixed { len, frames } => {
                assert_eq!(len, 4);
                assert_eq!(
                    frames[0],
                    TouchFrame::with_pointer(TouchAction::DOWN, pointer, 10, 20, 1.0)
                );
                assert_eq!(frames[1].pointer_id, pointer.value());
                assert_eq!(frames[2].pointer_id, pointer.value());
                assert_eq!(frames[3].pointer_id, pointer.value());
                assert_eq!(frames[3].action, TouchAction::CANCEL.value());
            }
            other => panic!("expected TouchBatchFixed command, got {other:?}"),
        }
    }

    #[test]
    fn touch_frame_batcher_push_many_slice_splits_at_capacity() {
        let (tx, rx) = sync_channel(2);
        let client = HidClient { tx };
        let mut batcher = TouchFrameBatcher::new(&client);
        let frames: Vec<_> = (0..TOUCH_BATCH_FRAMES + 2)
            .map(|i| TouchFrame::with_action(TouchAction::MOVE, 0, i as i32, 0, 1.0))
            .collect();

        batcher.push_many_slice(&frames).unwrap();
        assert_eq!(batcher.len(), 2);
        batcher.flush().unwrap();

        match rx.try_recv().unwrap() {
            HidCommand::TouchBatchFixed { len, frames } => {
                assert_eq!(len as usize, TOUCH_BATCH_FRAMES);
                assert_eq!(
                    frames[TOUCH_BATCH_FRAMES - 1].x,
                    TOUCH_BATCH_FRAMES as i32 - 1
                );
            }
            other => panic!("expected TouchBatchFixed command, got {other:?}"),
        }
        match rx.try_recv().unwrap() {
            HidCommand::TouchBatchFixed { len, frames } => {
                assert_eq!(len, 2);
                assert_eq!(frames[0].x, TOUCH_BATCH_FRAMES as i32);
                assert_eq!(frames[1].x, TOUCH_BATCH_FRAMES as i32 + 1);
            }
            other => panic!("expected TouchBatchFixed command, got {other:?}"),
        }
    }

    #[test]
    fn touch_frame_batcher_try_push_many_slice_keeps_batch_on_backpressure() {
        let (tx, rx) = sync_channel(1);
        let client = HidClient { tx };
        let mut batcher = TouchFrameBatcher::new(&client);
        let frames: Vec<_> = (0..TOUCH_BATCH_FRAMES)
            .map(|i| TouchFrame::with_action(TouchAction::MOVE, 1, i as i32, 0, 1.0))
            .collect();

        batcher.try_push_many_slice(&frames).unwrap();
        assert_eq!(batcher.len(), 0);
        let queued = rx.try_recv().unwrap();
        let HidCommand::TouchBatchFixed { len, .. } = queued else {
            panic!("expected first fixed touch batch");
        };
        assert_eq!(len as usize, TOUCH_BATCH_FRAMES);

        batcher.try_push_many_slice(&frames).unwrap();
        assert_eq!(batcher.len(), 0);
        let err = batcher.try_push_many_slice(&frames).unwrap_err();
        assert!(matches!(
            err,
            Error::SessionLifecycle("channel full (back-pressure)")
        ));
        assert_eq!(batcher.len(), TOUCH_BATCH_FRAMES);
    }

    #[test]
    fn touch_frame_batcher_try_helpers_use_expected_command() {
        let (tx, rx) = sync_channel(1);
        let client = HidClient { tx };
        let mut batcher = TouchFrameBatcher::new(&client);

        batcher.try_down(7, 10, 20, 1.0).unwrap();
        batcher.try_move_to(7, 30, 40, 0.5).unwrap();
        batcher.try_up(7, 50, 60).unwrap();
        batcher.try_cancel(7).unwrap();
        batcher.try_flush().unwrap();

        match rx.try_recv().unwrap() {
            HidCommand::TouchBatchFixed { len, frames } => {
                assert_eq!(len, 4);
                assert_eq!(frames[0].action, TouchAction::DOWN.value());
                assert_eq!(frames[1].action, TouchAction::MOVE.value());
                assert_eq!(frames[2].action, TouchAction::UP.value());
                assert_eq!(frames[3].action, TouchAction::CANCEL.value());
                assert_eq!(frames[0].pointer_id, 7);
                assert_eq!(frames[2].x, 50);
                assert_eq!(frames[2].y, 60);
                assert_eq!(frames[3].pointer_id, 7);
                assert_eq!(frames[3].pressure, 0.0);
            }
            other => panic!("expected TouchBatchFixed command, got {other:?}"),
        }
    }

    #[test]
    fn cancel_touch_dispatches_action_cancel() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let (client, dispatcher) = session.into_client().unwrap();

        client.cancel_touch(9).unwrap();
        client.close();
        let bytes = dispatcher.join().unwrap().into_bytes();

        let frame = bytes
            .windows(32)
            .find(|frame| frame[0] == 2 && frame[1] == TouchAction::CANCEL.value())
            .expect("ACTION_CANCEL touch frame");
        assert_eq!(u64::from_be_bytes(frame[2..10].try_into().unwrap()), 9);
        assert_eq!(i32::from_be_bytes(frame[10..14].try_into().unwrap()), 0);
        assert_eq!(i32::from_be_bytes(frame[14..18].try_into().unwrap()), 0);
    }

    #[test]
    fn touch_frame_batcher_dispatches_all_touch_frames() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let (client, dispatcher) = session.into_client().unwrap();
        let expected = TOUCH_BATCH_FRAMES + 3;

        {
            let mut batcher = TouchFrameBatcher::new(&client);
            for i in 0..expected {
                batcher.move_to(0, i as i32, 0, 1.0).unwrap();
            }
            batcher.flush().unwrap();
        }

        client.close();
        let transport = dispatcher.join().unwrap();
        assert_eq!(count_touch_events(&transport.into_bytes()), expected);
    }
}
