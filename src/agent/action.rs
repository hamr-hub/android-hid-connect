use std::ops::Range;
use std::time::Duration;

use crate::client::{
    AndroidKeyFrame, KeyboardChordFrame, KeyboardFrame, MouseFrame, ANDROID_KEY_BATCH_FRAMES,
    GAMEPAD_BATCH_FRAMES, KEYBOARD_BATCH_FRAMES, KEYBOARD_CHORD_KEYS, MOUSE_BATCH_FRAMES,
    SCROLL_BATCH_FRAMES, TOUCH_BATCH_FRAMES,
};
use crate::error::{Error, Result};
use crate::session::{GamepadFrameRaw, GAMEPAD_FRAME_BYTES};
use crate::types::{
    AndroidKeyAction, AndroidKeycode, ClipboardCopyKey, GamepadButton, Modifiers, MouseButton,
    Scancode, TouchPointerId,
};

use super::estimator::PlanCommandEstimator;
use super::types::{AgentPoint, AgentRect, AgentScrollFrame, AgentTouchFrame};
use super::{LAUNCH_APP_NAME_TOO_LONG, STRICT_TEXT_UNSUPPORTED, TIMED_ACTION_REQUIRES_BLOCKING};

/// One typed action in an agent control plan.
///
/// `AgentControlSession::queue_actions` enqueues these actions without a final
/// dispatcher barrier. `AgentControlSession::run_actions` enqueues them and
/// then waits for one checked flush, which is usually the right phase boundary
/// for LLM/tool workflows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentAction {
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
    KeyboardChord {
        chord: KeyboardChordFrame,
    },
    KeyBatch {
        len: usize,
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
        hscroll: i16,
        vscroll: i16,
    },
    MouseBatch {
        len: usize,
        frames: [MouseFrame; MOUSE_BATCH_FRAMES],
    },
    InjectKeycode {
        action: u8,
        keycode: u32,
        repeat: u32,
        metastate: u32,
    },
    AndroidKeyTap {
        keycode: u32,
        metastate: u32,
    },
    AndroidKeyBatch {
        len: usize,
        frames: [AndroidKeyFrame; ANDROID_KEY_BATCH_FRAMES],
    },
    BackOrScreenOn {
        action: u8,
    },
    PressHome,
    PressBack,
    OpenRecents,
    VolumeUp,
    VolumeDown,
    VolumeMute,
    Tap {
        x: i32,
        y: i32,
    },
    TapPointer {
        pointer_id: u64,
        x: i32,
        y: i32,
    },
    TapPoint {
        point: AgentPoint,
    },
    TapPointPointer {
        pointer_id: u64,
        point: AgentPoint,
    },
    TapRect {
        rect: AgentRect,
    },
    TapRectAt {
        rect: AgentRect,
        x_bp: u16,
        y_bp: u16,
    },
    TapRectPointer {
        pointer_id: u64,
        rect: AgentRect,
    },
    TapRectAtPointer {
        pointer_id: u64,
        rect: AgentRect,
        x_bp: u16,
        y_bp: u16,
    },
    DoubleTap {
        x: i32,
        y: i32,
    },
    DoubleTapPointer {
        pointer_id: u64,
        x: i32,
        y: i32,
    },
    DoubleTapPoint {
        point: AgentPoint,
    },
    DoubleTapPointPointer {
        pointer_id: u64,
        point: AgentPoint,
    },
    DoubleTapRect {
        rect: AgentRect,
    },
    DoubleTapRectAt {
        rect: AgentRect,
        x_bp: u16,
        y_bp: u16,
    },
    DoubleTapRectPointer {
        pointer_id: u64,
        rect: AgentRect,
    },
    DoubleTapRectAtPointer {
        pointer_id: u64,
        rect: AgentRect,
        x_bp: u16,
        y_bp: u16,
    },
    LongPress {
        x: i32,
        y: i32,
        duration: Duration,
    },
    LongPressPointer {
        pointer_id: u64,
        x: i32,
        y: i32,
        duration: Duration,
    },
    LongPressPoint {
        point: AgentPoint,
        duration: Duration,
    },
    LongPressPointPointer {
        pointer_id: u64,
        point: AgentPoint,
        duration: Duration,
    },
    LongPressRect {
        rect: AgentRect,
        duration: Duration,
    },
    LongPressRectAt {
        rect: AgentRect,
        x_bp: u16,
        y_bp: u16,
        duration: Duration,
    },
    LongPressRectPointer {
        pointer_id: u64,
        rect: AgentRect,
        duration: Duration,
    },
    LongPressRectAtPointer {
        pointer_id: u64,
        rect: AgentRect,
        x_bp: u16,
        y_bp: u16,
        duration: Duration,
    },
    Swipe {
        from: (i32, i32),
        to: (i32, i32),
        steps: usize,
    },
    SwipePointer {
        pointer_id: u64,
        from: (i32, i32),
        to: (i32, i32),
        steps: usize,
    },
    SwipePoints {
        from: AgentPoint,
        to: AgentPoint,
        steps: usize,
    },
    SwipePointsPointer {
        pointer_id: u64,
        from: AgentPoint,
        to: AgentPoint,
        steps: usize,
    },
    SwipeRect {
        rect: AgentRect,
        from_x_bp: u16,
        from_y_bp: u16,
        to_x_bp: u16,
        to_y_bp: u16,
        steps: usize,
    },
    SwipeRectPointer {
        pointer_id: u64,
        rect: AgentRect,
        from_x_bp: u16,
        from_y_bp: u16,
        to_x_bp: u16,
        to_y_bp: u16,
        steps: usize,
    },
    Pinch {
        first_from: (i32, i32),
        first_to: (i32, i32),
        second_from: (i32, i32),
        second_to: (i32, i32),
        steps: usize,
    },
    PinchPoints {
        first_from: AgentPoint,
        first_to: AgentPoint,
        second_from: AgentPoint,
        second_to: AgentPoint,
        steps: usize,
    },
    Scroll {
        x: i32,
        y: i32,
        hscroll: i16,
        vscroll: i16,
        buttons: u32,
    },
    ScrollPoint {
        point: AgentPoint,
        hscroll: i16,
        vscroll: i16,
        buttons: u32,
    },
    ScrollRect {
        rect: AgentRect,
        hscroll: i16,
        vscroll: i16,
        buttons: u32,
    },
    ScrollRectAt {
        rect: AgentRect,
        x_bp: u16,
        y_bp: u16,
        hscroll: i16,
        vscroll: i16,
        buttons: u32,
    },
    ScrollBatch {
        len: usize,
        frames: [AgentScrollFrame; SCROLL_BATCH_FRAMES],
    },
    CancelTouch {
        pointer_id: u64,
    },
    TouchFrames {
        len: usize,
        frames: [AgentTouchFrame; TOUCH_BATCH_FRAMES],
    },
    ThreeFingerScreenshot,
    SetScreenSize {
        width: u16,
        height: u16,
    },
    LaunchApp(String),
    SetScreenPower {
        on: bool,
    },
    ShowNotifications,
    ShowQuickSettings,
    CollapsePanels,
    RotateDevice,
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
    AiConfig {
        flags: u8,
        sample_interval_ms: u16,
        feature_dim: u16,
    },
    AiQuery {
        since_timestamp_ms: u64,
    },
    AiPause,
    SetClipboard {
        text: String,
        paste: bool,
    },
    SetClipboardSequenced {
        sequence: u64,
        text: String,
        paste: bool,
    },
    RequestClipboard {
        copy_key: u8,
    },
    GamepadButton {
        button: GamepadButton,
        pressed: bool,
    },
    GamepadButtons {
        buttons: u32,
    },
    GamepadFrame {
        frame: GamepadFrameRaw,
    },
    GamepadFrameUnchecked {
        frame: GamepadFrameRaw,
    },
    GamepadFrameBatch {
        len: usize,
        frames: [GamepadFrameRaw; GAMEPAD_BATCH_FRAMES],
    },
    GamepadFrameBatchUnchecked {
        len: usize,
        frames: [GamepadFrameRaw; GAMEPAD_BATCH_FRAMES],
    },
    GamepadPackedFrame {
        frame: [u8; GAMEPAD_FRAME_BYTES],
    },
    GamepadPackedFrameBatch {
        len: usize,
        frames: [[u8; GAMEPAD_FRAME_BYTES]; GAMEPAD_BATCH_FRAMES],
    },
    /// Flush prior work and then sleep. This is useful when a plan needs a
    /// deterministic pause between phases.
    Wait(Duration),
    /// Explicit checked dispatcher barrier inside a longer plan.
    Flush,
}

/// Pure, transport-free summary of an [`AgentAction`] plan.
///
/// The dispatch-command estimates model the current plan-scoped fixed-stack
/// batchers used by [`AgentControlSession::queue_actions`]. They are useful for
/// schedulers that need to choose between non-blocking prefix dispatch and a
/// blocking checked path before constructing a session or touching transport.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentPlanSummary {
    /// Number of high-level actions supplied by the caller.
    pub action_count: usize,
    /// First static structural validation error, if any.
    pub first_structural_error: Option<(usize, &'static str)>,
    /// First reason [`AgentControlSession::try_queue_actions`] would reject the
    /// full plan before runtime back-pressure, if any.
    pub first_try_queue_error: Option<(usize, &'static str)>,
    /// First reason [`AgentControlSession::try_queue_actions_prefix`] would
    /// reject the leading non-blocking prefix before runtime back-pressure, if
    /// any.
    pub first_try_queue_prefix_error: Option<(usize, &'static str)>,
    /// First action requiring a blocking timing/barrier path, if any.
    pub first_blocking_timing: Option<usize>,
    /// Prefix length accepted by [`AgentControlSession::try_queue_actions`]
    /// before the first static rejection.
    pub try_queueable_prefix_len: usize,
    /// Prefix length before the first blocking timing/barrier action.
    pub blocking_timing_prefix_len: usize,
    /// Estimated dispatcher commands sent by [`AgentControlSession::queue_actions`].
    ///
    /// This is zero when structural preflight would reject the full plan before
    /// dispatching anything.
    pub estimated_queue_dispatch_commands: usize,
    /// Estimated dispatcher commands sent by [`AgentControlSession::run_actions`].
    ///
    /// This includes the final checked flush command and is zero when structural
    /// preflight would reject the full plan before dispatching anything.
    pub estimated_run_dispatch_commands: usize,
    /// Estimated dispatcher commands sent by [`AgentControlSession::try_queue_actions`].
    ///
    /// This is zero when try-queue preflight would reject the full plan before
    /// dispatching anything.
    pub estimated_try_queue_dispatch_commands: usize,
    /// Estimated dispatcher commands sent by [`AgentControlSession::try_run_actions`].
    ///
    /// This includes the final checked flush barrier and is zero when try-run
    /// preflight would reject the full plan before dispatching anything.
    pub estimated_try_run_dispatch_commands: usize,
    /// Estimated dispatcher commands sent by
    /// [`AgentControlSession::try_queue_actions_prefix`].
    ///
    /// A blocking suffix is left uninspected for structural errors, matching the
    /// prefix dispatch contract.
    pub estimated_try_queue_prefix_dispatch_commands: usize,
    /// Estimated dispatcher commands sent by
    /// [`AgentControlSession::try_run_actions_prefix`].
    ///
    /// This includes the final checked flush barrier for the accepted prefix and
    /// leaves a blocking suffix uninspected for structural errors, matching the
    /// checked prefix dispatch contract.
    pub estimated_try_run_prefix_dispatch_commands: usize,
}

/// Reason a bounded non-blocking prefix analysis stopped before accepting more
/// actions.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentPlanBoundedPrefixStop {
    /// Every input action was accepted by the bounded prefix.
    EndOfPlan,
    /// The next action requires a blocking timing/barrier path and should be
    /// routed through [`AgentControlSession::queue_actions`] or
    /// [`AgentControlSession::run_actions`].
    BlockingTiming { index: usize },
    /// The next action is statically incompatible with
    /// [`AgentControlSession::try_queue_actions`].
    TryQueueError { index: usize, error: &'static str },
    /// The next action is otherwise try-queueable, but would exceed the caller's
    /// estimated dispatcher command budget.
    CommandBound {
        index: usize,
        required_dispatch_commands: usize,
    },
}

impl AgentPlanBoundedPrefixStop {
    pub const fn is_end_of_plan(&self) -> bool {
        matches!(self, Self::EndOfPlan)
    }

    pub const fn is_blocking_timing(&self) -> bool {
        matches!(self, Self::BlockingTiming { .. })
    }

    pub const fn is_try_queue_error(&self) -> bool {
        matches!(self, Self::TryQueueError { .. })
    }

    pub const fn is_command_bound(&self) -> bool {
        matches!(self, Self::CommandBound { .. })
    }

    /// Index of the action that stopped prefix growth, if the plan did not end.
    pub const fn index(&self) -> Option<usize> {
        match self {
            Self::EndOfPlan => None,
            Self::BlockingTiming { index }
            | Self::TryQueueError { index, .. }
            | Self::CommandBound { index, .. } => Some(*index),
        }
    }

    /// Static preflight error that stopped prefix growth, if any.
    pub const fn error(&self) -> Option<&'static str> {
        match self {
            Self::TryQueueError { error, .. } => Some(*error),
            _ => None,
        }
    }

    /// Dispatcher-command count needed to include the next action after a
    /// command-bound stop.
    pub const fn required_dispatch_commands(&self) -> Option<usize> {
        match self {
            Self::CommandBound {
                required_dispatch_commands,
                ..
            } => Some(*required_dispatch_commands),
            _ => None,
        }
    }
}

/// Pure result for choosing a bounded leading slice for
/// [`AgentControlSession::try_queue_actions`].
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentPlanBoundedPrefix {
    /// Total actions in the analyzed plan.
    pub action_count: usize,
    /// Number of leading actions accepted into the bounded non-blocking slice.
    pub accepted_actions: usize,
    /// Estimated dispatcher commands for `actions[..accepted_actions]`.
    pub estimated_dispatch_commands: usize,
    /// Command budget supplied by the caller.
    pub command_bound: usize,
    /// Why analysis stopped.
    pub stop: AgentPlanBoundedPrefixStop,
}

impl AgentPlanBoundedPrefix {
    pub const fn is_full_plan(&self) -> bool {
        matches!(self.stop, AgentPlanBoundedPrefixStop::EndOfPlan)
    }

    pub const fn is_empty(&self) -> bool {
        self.accepted_actions == 0
    }

    pub fn remaining_actions(&self) -> usize {
        self.action_count.saturating_sub(self.accepted_actions)
    }

    pub const fn accepted_dispatch_fits_bound(&self) -> bool {
        self.estimated_dispatch_commands <= self.command_bound
    }

    /// Estimated dispatcher commands for the accepted prefix plus one checked
    /// final barrier.
    pub fn estimated_checked_dispatch_commands(&self) -> usize {
        self.estimated_dispatch_commands.saturating_add(1)
    }

    /// Return whether the accepted prefix plus one checked final barrier fits
    /// the supplied command bound.
    pub fn checked_dispatch_fits_bound(&self) -> bool {
        self.estimated_checked_dispatch_commands() <= self.command_bound
    }

    /// Range covering the accepted leading actions in the analyzed plan.
    pub fn accepted_range(&self) -> Range<usize> {
        0..self.accepted_actions
    }

    /// Range covering the suffix left for a later scheduler decision.
    pub fn remaining_range(&self) -> Range<usize> {
        self.accepted_actions..self.action_count
    }

    /// Split a slice that corresponds to the analyzed plan into accepted and
    /// remaining portions.
    ///
    /// Returns `None` when the supplied slice length does not match
    /// [`Self::action_count`], avoiding accidental use of stale plan metadata.
    pub fn split_slice<'a, T>(&self, slice: &'a [T]) -> Option<(&'a [T], &'a [T])> {
        if slice.len() == self.action_count && self.accepted_actions <= slice.len() {
            Some(slice.split_at(self.accepted_actions))
        } else {
            None
        }
    }

    /// Return the accepted prefix for a slice that corresponds to the analyzed
    /// plan.
    pub fn accepted_slice<'a, T>(&self, slice: &'a [T]) -> Option<&'a [T]> {
        self.split_slice(slice).map(|(accepted, _)| accepted)
    }

    /// Return the suffix left for a later scheduler decision.
    pub fn remaining_slice<'a, T>(&self, slice: &'a [T]) -> Option<&'a [T]> {
        self.split_slice(slice).map(|(_, remaining)| remaining)
    }
}

impl AgentPlanSummary {
    /// Analyze a plan without creating an agent session or touching transport.
    pub fn analyze(actions: &[AgentAction]) -> Self {
        let first_structural_error = AgentAction::first_structural_error(actions);
        let first_try_queue_error = AgentAction::first_try_queue_error(actions);
        let first_blocking_timing = AgentAction::first_blocking_timing(actions);
        let try_queueable_prefix_len = AgentAction::try_queueable_prefix_len(actions);
        let blocking_timing_prefix_len = first_blocking_timing.unwrap_or(actions.len());
        let prefix = &actions[..blocking_timing_prefix_len];
        let first_try_queue_prefix_error = AgentAction::first_try_queue_error(prefix);

        let estimated_queue_dispatch_commands = if first_structural_error.is_none() {
            PlanCommandEstimator::estimate_queue_actions(actions)
        } else {
            0
        };
        let estimated_run_dispatch_commands = if first_structural_error.is_none() {
            estimated_queue_dispatch_commands.saturating_add(1)
        } else {
            0
        };
        let estimated_try_queue_dispatch_commands = if first_try_queue_error.is_none() {
            estimated_queue_dispatch_commands
        } else {
            0
        };
        let estimated_try_run_dispatch_commands = if first_try_queue_error.is_none() {
            estimated_try_queue_dispatch_commands.saturating_add(1)
        } else {
            0
        };
        let estimated_try_queue_prefix_dispatch_commands = if first_try_queue_prefix_error.is_none()
        {
            PlanCommandEstimator::estimate_queue_actions(prefix)
        } else {
            0
        };
        let estimated_try_run_prefix_dispatch_commands = if first_try_queue_prefix_error.is_none() {
            estimated_try_queue_prefix_dispatch_commands.saturating_add(1)
        } else {
            0
        };

        Self {
            action_count: actions.len(),
            first_structural_error,
            first_try_queue_error,
            first_try_queue_prefix_error,
            first_blocking_timing,
            try_queueable_prefix_len,
            blocking_timing_prefix_len,
            estimated_queue_dispatch_commands,
            estimated_run_dispatch_commands,
            estimated_try_queue_dispatch_commands,
            estimated_try_run_dispatch_commands,
            estimated_try_queue_prefix_dispatch_commands,
            estimated_try_run_prefix_dispatch_commands,
        }
    }

    pub const fn is_structurally_valid(&self) -> bool {
        self.first_structural_error.is_none()
    }

    pub const fn all_try_queueable(&self) -> bool {
        self.first_try_queue_error.is_none()
    }

    pub const fn can_try_queue_prefix(&self) -> bool {
        self.first_try_queue_prefix_error.is_none()
    }

    pub const fn has_blocking_timing(&self) -> bool {
        self.first_blocking_timing.is_some()
    }

    pub const fn has_blocking_suffix(&self) -> bool {
        self.blocking_timing_prefix_len < self.action_count
    }

    pub fn blocking_suffix_len(&self) -> usize {
        self.action_count
            .saturating_sub(self.blocking_timing_prefix_len)
    }

    pub const fn queue_dispatch_fits_bound(&self, command_bound: usize) -> bool {
        self.first_structural_error.is_none()
            && self.estimated_queue_dispatch_commands <= command_bound
    }

    pub const fn run_dispatch_fits_bound(&self, command_bound: usize) -> bool {
        self.first_structural_error.is_none()
            && self.estimated_run_dispatch_commands <= command_bound
    }

    pub const fn try_queue_dispatch_fits_bound(&self, command_bound: usize) -> bool {
        self.first_try_queue_error.is_none()
            && self.estimated_try_queue_dispatch_commands <= command_bound
    }

    pub const fn try_run_dispatch_fits_bound(&self, command_bound: usize) -> bool {
        self.first_try_queue_error.is_none()
            && self.estimated_try_run_dispatch_commands <= command_bound
    }

    pub const fn try_queue_prefix_dispatch_fits_bound(&self, command_bound: usize) -> bool {
        self.first_try_queue_prefix_error.is_none()
            && self.estimated_try_queue_prefix_dispatch_commands <= command_bound
    }

    pub const fn try_run_prefix_dispatch_fits_bound(&self, command_bound: usize) -> bool {
        self.first_try_queue_prefix_error.is_none()
            && self.estimated_try_run_prefix_dispatch_commands <= command_bound
    }
}

impl AgentAction {
    pub fn type_text(text: impl Into<String>) -> Self {
        Self::TypeText(text.into())
    }

    pub fn type_text_strict(text: impl Into<String>) -> Self {
        Self::TypeTextStrict(text.into())
    }

    pub const fn key(scancode: u8, pressed: bool, mods: Modifiers) -> Self {
        Self::Key {
            scancode,
            pressed,
            mods,
        }
    }

    pub const fn key_scancode(scancode: Scancode, pressed: bool, mods: Modifiers) -> Self {
        Self::key(scancode.to_u8(), pressed, mods)
    }

    pub const fn tap_key(scancode: u8, mods: Modifiers) -> Self {
        Self::KeyTap { scancode, mods }
    }

    pub const fn tap_scancode(scancode: Scancode, mods: Modifiers) -> Self {
        Self::tap_key(scancode.to_u8(), mods)
    }

    pub const fn keyboard_chord_fixed(chord: KeyboardChordFrame) -> Self {
        Self::KeyboardChord { chord }
    }

    pub const fn scancode_chord(scancode: Scancode, mods: Modifiers) -> Self {
        Self::KeyboardChord {
            chord: KeyboardChordFrame::scancode(scancode, mods),
        }
    }

    pub const fn ctrl_scancode(scancode: Scancode) -> Self {
        Self::scancode_chord(scancode, Modifiers::LCTRL)
    }

    pub const fn ctrl_shift_scancode(scancode: Scancode) -> Self {
        Self::scancode_chord(
            scancode,
            Modifiers(Modifiers::LCTRL.0 | Modifiers::LSHIFT.0),
        )
    }

    pub fn try_keyboard_chord(scancodes: &[u8], mods: Modifiers) -> Result<Self> {
        Ok(Self::KeyboardChord {
            chord: KeyboardChordFrame::try_new(scancodes, mods)?,
        })
    }

    pub fn try_scancode_chord(scancodes: &[Scancode], mods: Modifiers) -> Result<Self> {
        Ok(Self::KeyboardChord {
            chord: KeyboardChordFrame::try_scancodes(scancodes, mods)?,
        })
    }

    pub const fn key_batch_fixed(
        len: usize,
        frames: [KeyboardFrame; KEYBOARD_BATCH_FRAMES],
    ) -> Self {
        Self::KeyBatch { len, frames }
    }

    pub fn try_key_batch(frames: &[KeyboardFrame]) -> Result<Self> {
        let len = frames.len();
        if len > KEYBOARD_BATCH_FRAMES {
            return Err(Error::SessionLifecycle("keyboard batch too large"));
        }
        let mut batch = [KeyboardFrame::EMPTY; KEYBOARD_BATCH_FRAMES];
        batch[..len].copy_from_slice(frames);
        Ok(Self::KeyBatch { len, frames: batch })
    }

    pub fn launch_app(name: impl Into<String>) -> Self {
        Self::LaunchApp(name.into())
    }

    pub const fn mouse_motion(dx: i32, dy: i32, buttons: u8) -> Self {
        Self::MouseMotion { dx, dy, buttons }
    }

    pub fn mouse_motion_buttons(dx: i32, dy: i32, buttons: &[MouseButton]) -> Self {
        Self::mouse_motion(dx, dy, MouseButton::state(buttons))
    }

    pub const fn mouse_buttons(buttons: u8) -> Self {
        Self::MouseButtons { buttons }
    }

    pub fn mouse_button_state(buttons: &[MouseButton]) -> Self {
        Self::mouse_buttons(MouseButton::state(buttons))
    }

    pub const fn mouse_scroll(hscroll: i16, vscroll: i16) -> Self {
        Self::MouseScroll { hscroll, vscroll }
    }

    pub const fn mouse_batch_fixed(len: usize, frames: [MouseFrame; MOUSE_BATCH_FRAMES]) -> Self {
        Self::MouseBatch { len, frames }
    }

    pub fn try_mouse_batch(frames: &[MouseFrame]) -> Result<Self> {
        let len = frames.len();
        if len > MOUSE_BATCH_FRAMES {
            return Err(Error::SessionLifecycle("mouse batch too large"));
        }
        let mut batch = [MouseFrame::EMPTY; MOUSE_BATCH_FRAMES];
        batch[..len].copy_from_slice(frames);
        Ok(Self::MouseBatch { len, frames: batch })
    }

    pub const fn inject_android_keycode(
        action: u8,
        keycode: AndroidKeycode,
        repeat: u32,
        metastate: u32,
    ) -> Self {
        Self::InjectKeycode {
            action,
            keycode: keycode.value(),
            repeat,
            metastate,
        }
    }

    pub const fn inject_android_key_event(
        action: AndroidKeyAction,
        keycode: AndroidKeycode,
        repeat: u32,
        metastate: u32,
    ) -> Self {
        Self::inject_android_keycode(action.value(), keycode, repeat, metastate)
    }

    pub const fn press_android_key(keycode: AndroidKeycode) -> Self {
        Self::inject_android_key_event(AndroidKeyAction::DOWN, keycode, 0, 0)
    }

    pub const fn release_android_key(keycode: AndroidKeycode) -> Self {
        Self::inject_android_key_event(AndroidKeyAction::UP, keycode, 0, 0)
    }

    pub const fn tap_android_keycode(keycode: u32, metastate: u32) -> Self {
        Self::AndroidKeyTap { keycode, metastate }
    }

    pub const fn tap_android_key(keycode: AndroidKeycode) -> Self {
        Self::tap_android_keycode(keycode.value(), 0)
    }

    pub const fn tap_android_key_with_metastate(keycode: AndroidKeycode, metastate: u32) -> Self {
        Self::tap_android_keycode(keycode.value(), metastate)
    }

    pub const fn android_key_batch_fixed(
        len: usize,
        frames: [AndroidKeyFrame; ANDROID_KEY_BATCH_FRAMES],
    ) -> Self {
        Self::AndroidKeyBatch { len, frames }
    }

    pub fn try_android_key_batch(frames: &[AndroidKeyFrame]) -> Result<Self> {
        let len = frames.len();
        if len > ANDROID_KEY_BATCH_FRAMES {
            return Err(Error::SessionLifecycle("android key batch too large"));
        }
        let mut batch = [AndroidKeyFrame::EMPTY; ANDROID_KEY_BATCH_FRAMES];
        batch[..len].copy_from_slice(frames);
        Ok(Self::AndroidKeyBatch { len, frames: batch })
    }

    pub const fn back_or_screen_on(action: AndroidKeyAction) -> Self {
        Self::BackOrScreenOn {
            action: action.value(),
        }
    }

    pub fn set_clipboard(text: impl Into<String>, paste: bool) -> Self {
        Self::SetClipboard {
            text: text.into(),
            paste,
        }
    }

    pub fn set_clipboard_sequenced(sequence: u64, text: impl Into<String>, paste: bool) -> Self {
        Self::SetClipboardSequenced {
            sequence,
            text: text.into(),
            paste,
        }
    }

    pub const fn request_clipboard(copy_key: u8) -> Self {
        Self::RequestClipboard { copy_key }
    }

    pub const fn request_clipboard_key(copy_key: ClipboardCopyKey) -> Self {
        Self::RequestClipboard {
            copy_key: copy_key.value(),
        }
    }

    pub const fn configure_ai(flags: u8, sample_interval_ms: u16, feature_dim: u16) -> Self {
        Self::AiConfig {
            flags,
            sample_interval_ms,
            feature_dim,
        }
    }

    pub const fn query_ai(since_timestamp_ms: u64) -> Self {
        Self::AiQuery { since_timestamp_ms }
    }

    pub const fn pause_ai() -> Self {
        Self::AiPause
    }

    pub const fn tap(x: i32, y: i32) -> Self {
        Self::Tap { x, y }
    }

    pub const fn tap_pointer(pointer_id: TouchPointerId, x: i32, y: i32) -> Self {
        Self::TapPointer {
            pointer_id: pointer_id.value(),
            x,
            y,
        }
    }

    pub const fn tap_point(point: AgentPoint) -> Self {
        Self::TapPoint { point }
    }

    pub const fn tap_point_pointer(pointer_id: TouchPointerId, point: AgentPoint) -> Self {
        Self::TapPointPointer {
            pointer_id: pointer_id.value(),
            point,
        }
    }

    pub const fn tap_rect(rect: AgentRect) -> Self {
        Self::TapRect { rect }
    }

    pub const fn tap_rect_at(rect: AgentRect, x_bp: u16, y_bp: u16) -> Self {
        Self::TapRectAt { rect, x_bp, y_bp }
    }

    pub const fn tap_rect_pointer(pointer_id: TouchPointerId, rect: AgentRect) -> Self {
        Self::TapRectPointer {
            pointer_id: pointer_id.value(),
            rect,
        }
    }

    pub const fn tap_rect_at_pointer(
        pointer_id: TouchPointerId,
        rect: AgentRect,
        x_bp: u16,
        y_bp: u16,
    ) -> Self {
        Self::TapRectAtPointer {
            pointer_id: pointer_id.value(),
            rect,
            x_bp,
            y_bp,
        }
    }

    pub const fn double_tap_pointer(pointer_id: TouchPointerId, x: i32, y: i32) -> Self {
        Self::DoubleTapPointer {
            pointer_id: pointer_id.value(),
            x,
            y,
        }
    }

    pub const fn double_tap_point(point: AgentPoint) -> Self {
        Self::DoubleTapPoint { point }
    }

    pub const fn double_tap_point_pointer(pointer_id: TouchPointerId, point: AgentPoint) -> Self {
        Self::DoubleTapPointPointer {
            pointer_id: pointer_id.value(),
            point,
        }
    }

    pub const fn double_tap_rect(rect: AgentRect) -> Self {
        Self::DoubleTapRect { rect }
    }

    pub const fn double_tap_rect_at(rect: AgentRect, x_bp: u16, y_bp: u16) -> Self {
        Self::DoubleTapRectAt { rect, x_bp, y_bp }
    }

    pub const fn double_tap_rect_pointer(pointer_id: TouchPointerId, rect: AgentRect) -> Self {
        Self::DoubleTapRectPointer {
            pointer_id: pointer_id.value(),
            rect,
        }
    }

    pub const fn double_tap_rect_at_pointer(
        pointer_id: TouchPointerId,
        rect: AgentRect,
        x_bp: u16,
        y_bp: u16,
    ) -> Self {
        Self::DoubleTapRectAtPointer {
            pointer_id: pointer_id.value(),
            rect,
            x_bp,
            y_bp,
        }
    }

    pub const fn long_press_pointer(
        pointer_id: TouchPointerId,
        x: i32,
        y: i32,
        duration: Duration,
    ) -> Self {
        Self::LongPressPointer {
            pointer_id: pointer_id.value(),
            x,
            y,
            duration,
        }
    }

    pub const fn long_press_point(point: AgentPoint, duration: Duration) -> Self {
        Self::LongPressPoint { point, duration }
    }

    pub const fn long_press_point_pointer(
        pointer_id: TouchPointerId,
        point: AgentPoint,
        duration: Duration,
    ) -> Self {
        Self::LongPressPointPointer {
            pointer_id: pointer_id.value(),
            point,
            duration,
        }
    }

    pub const fn long_press_rect(rect: AgentRect, duration: Duration) -> Self {
        Self::LongPressRect { rect, duration }
    }

    pub const fn long_press_rect_at(
        rect: AgentRect,
        x_bp: u16,
        y_bp: u16,
        duration: Duration,
    ) -> Self {
        Self::LongPressRectAt {
            rect,
            x_bp,
            y_bp,
            duration,
        }
    }

    pub const fn long_press_rect_pointer(
        pointer_id: TouchPointerId,
        rect: AgentRect,
        duration: Duration,
    ) -> Self {
        Self::LongPressRectPointer {
            pointer_id: pointer_id.value(),
            rect,
            duration,
        }
    }

    pub const fn long_press_rect_at_pointer(
        pointer_id: TouchPointerId,
        rect: AgentRect,
        x_bp: u16,
        y_bp: u16,
        duration: Duration,
    ) -> Self {
        Self::LongPressRectAtPointer {
            pointer_id: pointer_id.value(),
            rect,
            x_bp,
            y_bp,
            duration,
        }
    }

    pub const fn swipe(from: (i32, i32), to: (i32, i32), steps: usize) -> Self {
        Self::Swipe { from, to, steps }
    }

    pub const fn swipe_pointer(
        pointer_id: TouchPointerId,
        from: (i32, i32),
        to: (i32, i32),
        steps: usize,
    ) -> Self {
        Self::SwipePointer {
            pointer_id: pointer_id.value(),
            from,
            to,
            steps,
        }
    }

    pub const fn swipe_points(from: AgentPoint, to: AgentPoint, steps: usize) -> Self {
        Self::SwipePoints { from, to, steps }
    }

    pub const fn swipe_points_pointer(
        pointer_id: TouchPointerId,
        from: AgentPoint,
        to: AgentPoint,
        steps: usize,
    ) -> Self {
        Self::SwipePointsPointer {
            pointer_id: pointer_id.value(),
            from,
            to,
            steps,
        }
    }

    pub const fn swipe_rect(
        rect: AgentRect,
        from: (u16, u16),
        to: (u16, u16),
        steps: usize,
    ) -> Self {
        Self::SwipeRect {
            rect,
            from_x_bp: from.0,
            from_y_bp: from.1,
            to_x_bp: to.0,
            to_y_bp: to.1,
            steps,
        }
    }

    pub const fn swipe_rect_pointer(
        pointer_id: TouchPointerId,
        rect: AgentRect,
        from: (u16, u16),
        to: (u16, u16),
        steps: usize,
    ) -> Self {
        Self::SwipeRectPointer {
            pointer_id: pointer_id.value(),
            rect,
            from_x_bp: from.0,
            from_y_bp: from.1,
            to_x_bp: to.0,
            to_y_bp: to.1,
            steps,
        }
    }

    pub const fn pinch(
        first_from: (i32, i32),
        first_to: (i32, i32),
        second_from: (i32, i32),
        second_to: (i32, i32),
        steps: usize,
    ) -> Self {
        Self::Pinch {
            first_from,
            first_to,
            second_from,
            second_to,
            steps,
        }
    }

    pub const fn pinch_points(
        first_from: AgentPoint,
        first_to: AgentPoint,
        second_from: AgentPoint,
        second_to: AgentPoint,
        steps: usize,
    ) -> Self {
        Self::PinchPoints {
            first_from,
            first_to,
            second_from,
            second_to,
            steps,
        }
    }

    pub const fn scroll(x: i32, y: i32, hscroll: i16, vscroll: i16) -> Self {
        Self::Scroll {
            x,
            y,
            hscroll,
            vscroll,
            buttons: 0,
        }
    }

    pub const fn scroll_with_buttons(
        x: i32,
        y: i32,
        hscroll: i16,
        vscroll: i16,
        buttons: u32,
    ) -> Self {
        Self::Scroll {
            x,
            y,
            hscroll,
            vscroll,
            buttons,
        }
    }

    pub const fn scroll_point(point: AgentPoint, hscroll: i16, vscroll: i16) -> Self {
        Self::ScrollPoint {
            point,
            hscroll,
            vscroll,
            buttons: 0,
        }
    }

    pub const fn scroll_point_with_buttons(
        point: AgentPoint,
        hscroll: i16,
        vscroll: i16,
        buttons: u32,
    ) -> Self {
        Self::ScrollPoint {
            point,
            hscroll,
            vscroll,
            buttons,
        }
    }

    pub const fn scroll_rect(rect: AgentRect, hscroll: i16, vscroll: i16) -> Self {
        Self::ScrollRect {
            rect,
            hscroll,
            vscroll,
            buttons: 0,
        }
    }

    pub const fn scroll_rect_at(
        rect: AgentRect,
        x_bp: u16,
        y_bp: u16,
        hscroll: i16,
        vscroll: i16,
    ) -> Self {
        Self::ScrollRectAt {
            rect,
            x_bp,
            y_bp,
            hscroll,
            vscroll,
            buttons: 0,
        }
    }

    pub const fn scroll_rect_at_with_buttons(
        rect: AgentRect,
        x_bp: u16,
        y_bp: u16,
        hscroll: i16,
        vscroll: i16,
        buttons: u32,
    ) -> Self {
        Self::ScrollRectAt {
            rect,
            x_bp,
            y_bp,
            hscroll,
            vscroll,
            buttons,
        }
    }

    pub const fn scroll_rect_with_buttons(
        rect: AgentRect,
        hscroll: i16,
        vscroll: i16,
        buttons: u32,
    ) -> Self {
        Self::ScrollRect {
            rect,
            hscroll,
            vscroll,
            buttons,
        }
    }

    pub const fn scroll_batch_fixed(
        len: usize,
        frames: [AgentScrollFrame; SCROLL_BATCH_FRAMES],
    ) -> Self {
        Self::ScrollBatch { len, frames }
    }

    pub fn try_scroll_batch(frames: &[AgentScrollFrame]) -> Result<Self> {
        let len = frames.len();
        if len > SCROLL_BATCH_FRAMES {
            return Err(Error::SessionLifecycle("scroll batch too large"));
        }
        let mut batch = [AgentScrollFrame::EMPTY; SCROLL_BATCH_FRAMES];
        batch[..len].copy_from_slice(frames);
        Ok(Self::ScrollBatch { len, frames: batch })
    }

    pub const fn cancel_touch(pointer_id: u64) -> Self {
        Self::CancelTouch { pointer_id }
    }

    pub const fn cancel_touch_pointer(pointer_id: TouchPointerId) -> Self {
        Self::CancelTouch {
            pointer_id: pointer_id.value(),
        }
    }

    pub const fn touch_frames_fixed(
        len: usize,
        frames: [AgentTouchFrame; TOUCH_BATCH_FRAMES],
    ) -> Self {
        Self::TouchFrames { len, frames }
    }

    pub fn try_touch_frames(frames: &[AgentTouchFrame]) -> Result<Self> {
        let len = frames.len();
        if len > TOUCH_BATCH_FRAMES {
            return Err(Error::SessionLifecycle("touch frame batch too large"));
        }
        let mut batch = [AgentTouchFrame::EMPTY; TOUCH_BATCH_FRAMES];
        batch[..len].copy_from_slice(frames);
        Ok(Self::TouchFrames { len, frames: batch })
    }

    pub const fn gamepad_frame(frame: GamepadFrameRaw) -> Self {
        Self::GamepadFrame { frame }
    }

    pub const fn gamepad_frame_unchecked(frame: GamepadFrameRaw) -> Self {
        Self::GamepadFrameUnchecked { frame }
    }

    pub const fn gamepad_frame_batch_fixed(
        len: usize,
        frames: [GamepadFrameRaw; GAMEPAD_BATCH_FRAMES],
    ) -> Self {
        Self::GamepadFrameBatch { len, frames }
    }

    pub const fn gamepad_frame_batch_unchecked_fixed(
        len: usize,
        frames: [GamepadFrameRaw; GAMEPAD_BATCH_FRAMES],
    ) -> Self {
        Self::GamepadFrameBatchUnchecked { len, frames }
    }

    pub fn try_gamepad_frame_batch(frames: &[GamepadFrameRaw]) -> Result<Self> {
        let len = frames.len();
        if len > GAMEPAD_BATCH_FRAMES {
            return Err(Error::SessionLifecycle("gamepad frame batch too large"));
        }
        let mut batch = [GamepadFrameRaw::new(0, 0, 0, 0, 0, 0, 0); GAMEPAD_BATCH_FRAMES];
        batch[..len].copy_from_slice(frames);
        Ok(Self::GamepadFrameBatch { len, frames: batch })
    }

    pub fn try_gamepad_frame_batch_unchecked(frames: &[GamepadFrameRaw]) -> Result<Self> {
        let len = frames.len();
        if len > GAMEPAD_BATCH_FRAMES {
            return Err(Error::SessionLifecycle("gamepad frame batch too large"));
        }
        let mut batch = [GamepadFrameRaw::new(0, 0, 0, 0, 0, 0, 0); GAMEPAD_BATCH_FRAMES];
        batch[..len].copy_from_slice(frames);
        Ok(Self::GamepadFrameBatchUnchecked { len, frames: batch })
    }

    pub const fn gamepad_packed_frame(frame: [u8; GAMEPAD_FRAME_BYTES]) -> Self {
        Self::GamepadPackedFrame { frame }
    }

    pub const fn gamepad_packed_frame_batch_fixed(
        len: usize,
        frames: [[u8; GAMEPAD_FRAME_BYTES]; GAMEPAD_BATCH_FRAMES],
    ) -> Self {
        Self::GamepadPackedFrameBatch { len, frames }
    }

    pub fn try_gamepad_packed_frame_batch(frames: &[[u8; GAMEPAD_FRAME_BYTES]]) -> Result<Self> {
        let len = frames.len();
        if len > GAMEPAD_BATCH_FRAMES {
            return Err(Error::SessionLifecycle(
                "gamepad packed frame batch too large",
            ));
        }
        let mut batch = [[0u8; GAMEPAD_FRAME_BYTES]; GAMEPAD_BATCH_FRAMES];
        batch[..len].copy_from_slice(frames);
        Ok(Self::GamepadPackedFrameBatch { len, frames: batch })
    }

    pub const fn wait(duration: Duration) -> Self {
        Self::Wait(duration)
    }

    const fn basis_points_error(x: u16, y: u16) -> Option<&'static str> {
        if x > 10_000 || y > 10_000 {
            Some("normalized point out of range")
        } else {
            None
        }
    }

    const fn basis_points_pair_error(
        from_x: u16,
        from_y: u16,
        to_x: u16,
        to_y: u16,
    ) -> Option<&'static str> {
        if from_x > 10_000 || from_y > 10_000 || to_x > 10_000 || to_y > 10_000 {
            Some("normalized point out of range")
        } else {
            None
        }
    }

    fn strict_text_error(text: &str) -> Option<&'static str> {
        for ch in text.chars() {
            let mut mods = Modifiers::empty();
            if Scancode::try_from_char(ch, &mut mods).is_none() {
                return Some(STRICT_TEXT_UNSUPPORTED);
            }
        }
        None
    }

    /// Return a static structural validation error for this action, if known.
    ///
    /// This catches malformed fixed-buffer metadata, rect-relative basis
    /// points, unsupported strict-text characters, and oversized app-launch
    /// names that would otherwise fail only after earlier plan actions had
    /// already been queued. It does not check runtime channel back-pressure or
    /// dispatcher-side command errors.
    pub fn structural_error(&self) -> Option<&'static str> {
        match self {
            Self::TypeTextStrict(text) => Self::strict_text_error(text),
            Self::LaunchApp(name) => {
                if name.len() > 255 {
                    Some(LAUNCH_APP_NAME_TOO_LONG)
                } else {
                    None
                }
            }
            Self::KeyboardChord { chord } => {
                if chord.len as usize > KEYBOARD_CHORD_KEYS {
                    Some("keyboard chord length overflow")
                } else {
                    None
                }
            }
            Self::KeyBatch { len, .. } => {
                if *len > KEYBOARD_BATCH_FRAMES {
                    Some("keyboard batch length overflow")
                } else {
                    None
                }
            }
            Self::MouseBatch { len, .. } => {
                if *len > MOUSE_BATCH_FRAMES {
                    Some("mouse batch length overflow")
                } else {
                    None
                }
            }
            Self::AndroidKeyBatch { len, .. } => {
                if *len > ANDROID_KEY_BATCH_FRAMES {
                    Some("android key batch length overflow")
                } else {
                    None
                }
            }
            Self::TapRectAt { x_bp, y_bp, .. }
            | Self::TapRectAtPointer { x_bp, y_bp, .. }
            | Self::DoubleTapRectAt { x_bp, y_bp, .. }
            | Self::DoubleTapRectAtPointer { x_bp, y_bp, .. }
            | Self::LongPressRectAt { x_bp, y_bp, .. }
            | Self::LongPressRectAtPointer { x_bp, y_bp, .. }
            | Self::ScrollRectAt { x_bp, y_bp, .. } => Self::basis_points_error(*x_bp, *y_bp),
            Self::SwipeRect {
                from_x_bp,
                from_y_bp,
                to_x_bp,
                to_y_bp,
                ..
            }
            | Self::SwipeRectPointer {
                from_x_bp,
                from_y_bp,
                to_x_bp,
                to_y_bp,
                ..
            } => Self::basis_points_pair_error(*from_x_bp, *from_y_bp, *to_x_bp, *to_y_bp),
            Self::ScrollBatch { len, .. } => {
                if *len > SCROLL_BATCH_FRAMES {
                    Some("scroll batch length overflow")
                } else {
                    None
                }
            }
            Self::TouchFrames { len, .. } => {
                if *len > TOUCH_BATCH_FRAMES {
                    Some("touch frame batch length overflow")
                } else {
                    None
                }
            }
            Self::GamepadFrameBatch { len, .. } | Self::GamepadFrameBatchUnchecked { len, .. } => {
                if *len > GAMEPAD_BATCH_FRAMES {
                    Some("frame batch fixed length overflow")
                } else {
                    None
                }
            }
            Self::GamepadPackedFrameBatch { len, .. } => {
                if *len > GAMEPAD_BATCH_FRAMES {
                    Some("frame packed batch fixed length overflow")
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// Validate this action's static structure before dispatch.
    pub fn validate_structure(&self) -> Result<()> {
        match self.structural_error() {
            Some(error) => Err(Error::SessionLifecycle(error)),
            None => Ok(()),
        }
    }

    /// Return the first action index and error that would fail structural
    /// validation before runtime dispatch.
    pub fn first_structural_error(actions: &[Self]) -> Option<(usize, &'static str)> {
        actions
            .iter()
            .enumerate()
            .find_map(|(idx, action)| action.structural_error().map(|error| (idx, error)))
    }

    /// Validate a full action plan's static structure before dispatching it.
    pub fn validate_plan_structure(actions: &[Self]) -> Result<()> {
        match Self::first_structural_error(actions) {
            Some((_, error)) => Err(Error::SessionLifecycle(error)),
            None => Ok(()),
        }
    }

    /// Return the reason this action cannot be sent through
    /// [`AgentControlSession::try_queue_actions`], if known before dispatch.
    pub fn try_queue_error(&self) -> Option<&'static str> {
        match self.structural_error() {
            Some(error) => Some(error),
            None => {
                if self.requires_blocking_timing() {
                    Some(TIMED_ACTION_REQUIRES_BLOCKING)
                } else {
                    None
                }
            }
        }
    }

    /// Return the first action index and error that would make
    /// [`AgentControlSession::try_queue_actions`] reject a plan before runtime
    /// back-pressure.
    pub fn first_try_queue_error(actions: &[Self]) -> Option<(usize, &'static str)> {
        actions
            .iter()
            .enumerate()
            .find_map(|(idx, action)| action.try_queue_error().map(|error| (idx, error)))
    }

    /// Validate that every action in a plan can be submitted through
    /// [`AgentControlSession::try_queue_actions`] before runtime back-pressure.
    pub fn validate_try_queue_plan(actions: &[Self]) -> Result<()> {
        match Self::first_try_queue_error(actions) {
            Some((_, error)) => Err(Error::SessionLifecycle(error)),
            None => Ok(()),
        }
    }

    /// Return `true` if this action requires a blocking timing/barrier path.
    ///
    /// These actions are valid for [`AgentControlSession::queue_actions`] and
    /// [`AgentControlSession::run_actions`], but not for
    /// [`AgentControlSession::try_queue_actions`], because the non-blocking
    /// path cannot sleep or wait for a checked dispatcher barrier mid-plan.
    pub const fn requires_blocking_timing(&self) -> bool {
        matches!(
            self,
            Self::Wait(_)
                | Self::LongPress { .. }
                | Self::LongPressPointer { .. }
                | Self::LongPressPoint { .. }
                | Self::LongPressPointPointer { .. }
                | Self::LongPressRect { .. }
                | Self::LongPressRectAt { .. }
                | Self::LongPressRectPointer { .. }
                | Self::LongPressRectAtPointer { .. }
        )
    }

    /// Return the first action index that requires a blocking timing/barrier
    /// path.
    ///
    /// This is the right handoff boundary for mixed schedulers that want to
    /// send a non-blocking prefix through [`AgentControlSession::try_queue_actions`]
    /// and route the remaining suffix through [`AgentControlSession::run_actions`].
    /// It does not treat malformed metadata as a handoff boundary; use
    /// [`Self::first_try_queue_error`] when the caller needs the first rejection
    /// reason.
    pub fn first_blocking_timing(actions: &[Self]) -> Option<usize> {
        actions
            .iter()
            .position(|action| action.requires_blocking_timing())
    }

    /// Return the prefix length before the first blocking timing/barrier action.
    pub fn blocking_timing_prefix_len(actions: &[Self]) -> usize {
        Self::first_blocking_timing(actions).unwrap_or(actions.len())
    }

    /// Return `true` if this action is structurally and timing compatible with
    /// [`AgentControlSession::try_queue_actions`].
    ///
    /// Runtime back-pressure can still make a non-blocking enqueue fail.
    pub fn can_try_queue(&self) -> bool {
        self.try_queue_error().is_none()
    }

    /// Return the first action index that cannot be sent through
    /// [`AgentControlSession::try_queue_actions`] before runtime back-pressure.
    pub fn first_non_try_queueable(actions: &[Self]) -> Option<usize> {
        actions.iter().position(|action| !action.can_try_queue())
    }

    /// Return the prefix length that can be sent through
    /// [`AgentControlSession::try_queue_actions`] before the first
    /// non-try-queueable action.
    pub fn try_queueable_prefix_len(actions: &[Self]) -> usize {
        Self::first_non_try_queueable(actions).unwrap_or(actions.len())
    }

    /// Return `true` if every action can be sent through
    /// [`AgentControlSession::try_queue_actions`] before runtime back-pressure.
    pub fn all_try_queueable(actions: &[Self]) -> bool {
        Self::first_non_try_queueable(actions).is_none()
    }

    /// Build a pure summary of a plan's static validity, non-blocking prefix
    /// boundaries, and estimated dispatcher command pressure.
    pub fn plan_summary(actions: &[Self]) -> AgentPlanSummary {
        AgentPlanSummary::analyze(actions)
    }

    /// Return the longest leading slice that is statically compatible with
    /// [`AgentControlSession::try_queue_actions`] and whose estimated dispatcher
    /// command pressure fits `command_bound`.
    ///
    /// Runtime back-pressure can still reject the returned prefix, but callers
    /// can use this before dispatch to avoid knowingly oversizing a bounded
    /// producer queue.
    pub fn bounded_try_queue_prefix(
        actions: &[Self],
        command_bound: usize,
    ) -> AgentPlanBoundedPrefix {
        PlanCommandEstimator::bounded_try_queue_prefix(actions, command_bound)
    }

    /// Return the longest leading slice that is compatible with
    /// [`AgentControlSession::try_run_actions`] and whose estimated dispatcher
    /// command pressure plus the final checked barrier fits `command_bound`.
    ///
    /// Runtime back-pressure can still reject the returned prefix or final
    /// barrier, but this reserves capacity for the barrier during pure planning.
    pub fn bounded_try_run_prefix(
        actions: &[Self],
        command_bound: usize,
    ) -> AgentPlanBoundedPrefix {
        PlanCommandEstimator::bounded_try_run_prefix(actions, command_bound)
    }
}
