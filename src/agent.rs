//! Agent-friendly high-level control/session facade.
//!
//! This module combines the write side (`HidClient` + dispatcher thread) with
//! the read side (`DeviceMessageReceiver`) so an AI agent can keep cheap cloned
//! command producers while retaining a single, byte-aligned device-message
//! reader.

use std::io::{self, Read};
use std::net::TcpStream;
use std::ops::Range;
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::Duration;

use crate::ai::{AiStats, FrameSummary, ObjectBox, TextRegion};
use crate::client::{
    AndroidKeyFrame, AndroidKeyFrameBatcher, GamepadFrameBatcher, HidClient, HidCommand,
    HidDispatcher, KeyboardChordFrame, KeyboardFrame, KeyboardFrameBatcher, MouseFrame,
    MouseFrameBatcher, PackedGamepadFrameBatcher, ScrollFrame, ScrollFrameBatcher, TouchFrame,
    TouchFrameBatcher, ANDROID_KEY_BATCH_FRAMES, GAMEPAD_BATCH_FRAMES, KEYBOARD_BATCH_FRAMES,
    KEYBOARD_CHORD_KEYS, MOUSE_BATCH_FRAMES, SCROLL_BATCH_FRAMES, TOUCH_BATCH_FRAMES,
};
use crate::device::{
    read_scrcpy_control_prefix, spawn_latest_frame_summary_receiver, DeviceEvent, DeviceMessage,
    DeviceMessagePump, DeviceMessageReceiver, LatestFrameSummaryBoundary,
    LatestFrameSummaryObservation, LatestFrameSummaryReceiver, LatestFrameSummarySnapshot,
    ScrcpyControlPrefix,
};
use crate::error::{Error, Result, TransportWrite};
use crate::session::{GamepadFrameRaw, HidSession, OpenRequest, GAMEPAD_FRAME_BYTES};
use crate::transport::open_tcp;
use crate::types::{
    AndroidKeyAction, AndroidKeycode, ClipboardCopyKey, GamepadAxis, GamepadButton, Modifiers,
    MouseButton, Scancode, TouchAction, TouchPointerId,
};

/// Default bound for the agent command channel.
pub const DEFAULT_AGENT_COMMAND_BOUND: usize = crate::client::DEFAULT_CHANNEL_BOUND;
/// Default touch metadata width, matching `HidSession`.
pub const DEFAULT_AGENT_SCREEN_WIDTH: u16 = 1080;
/// Default touch metadata height, matching `HidSession`.
pub const DEFAULT_AGENT_SCREEN_HEIGHT: u16 = 1920;

const TIMED_ACTION_REQUIRES_BLOCKING: &str = "timed action requires queue_actions or run_actions";
const STRICT_TEXT_UNSUPPORTED: &str = "unsupported char in type_text_strict";
const LAUNCH_APP_NAME_TOO_LONG: &str = "launch app name too long";
const TRY_RUN_EXCEEDS_COMMAND_BOUND: &str = "try_run_actions exceeds command bound";
const TRY_TAP_EXCEEDS_COMMAND_BOUND: &str = "try_tap exceeds command bound";
const TRY_DOUBLE_TAP_EXCEEDS_COMMAND_BOUND: &str = "try_double_tap exceeds command bound";
const TRY_SCROLL_EXCEEDS_COMMAND_BOUND: &str = "try_scroll exceeds command bound";
const TRY_KEY_EXCEEDS_COMMAND_BOUND: &str = "try_key exceeds command bound";
const TRY_ANDROID_KEY_EXCEEDS_COMMAND_BOUND: &str = "try_android_key exceeds command bound";
const TRY_MOUSE_EXCEEDS_COMMAND_BOUND: &str = "try_mouse exceeds command bound";
const TRY_GAMEPAD_EXCEEDS_COMMAND_BOUND: &str = "try_gamepad exceeds command bound";
const TRY_CONTROL_EXCEEDS_COMMAND_BOUND: &str = "try_control exceeds command bound";
const TRY_AI_EXCEEDS_COMMAND_BOUND: &str = "try_ai exceeds command bound";
const TRY_CLIPBOARD_EXCEEDS_COMMAND_BOUND: &str = "try_clipboard exceeds command bound";

/// One owned agent control session.
pub struct AgentControlSession<T: TransportWrite + Send + 'static, R: Read> {
    client: HidClient,
    dispatcher: Option<HidDispatcher<T>>,
    receiver: Option<DeviceMessageReceiver<R>>,
    command_bound: usize,
    next_clipboard_sequence: u64,
    screen_width: AtomicU16,
    screen_height: AtomicU16,
}

impl<T: TransportWrite + Send + 'static, R: Read> std::fmt::Debug for AgentControlSession<T, R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentControlSession")
            .field("client", &self.client)
            .finish_non_exhaustive()
    }
}

/// Resources recovered after closing an [`AgentControlSession`].
#[derive(Debug)]
pub struct AgentControlClosed<T, R> {
    pub transport: T,
    pub reader: R,
}

/// Resources recovered from a checked close plus the dispatcher command result.
#[derive(Debug)]
pub struct AgentControlCloseReport<T, R> {
    pub closed: AgentControlClosed<T, R>,
    pub command_result: Result<()>,
}

/// Result of closing an agent after its reader has been detached.
#[derive(Debug)]
pub struct AgentControlCommandCloseReport<T> {
    pub transport: T,
    pub command_result: Result<()>,
}

impl<T, R> AgentControlCloseReport<T, R> {
    /// Convert into the recovered resources, returning the dispatcher command
    /// error if one was observed before shutdown.
    pub fn into_result(self) -> Result<AgentControlClosed<T, R>> {
        self.command_result.map(|()| self.closed)
    }
}

impl<T> AgentControlCommandCloseReport<T> {
    /// Convert into the recovered write transport, returning the dispatcher
    /// command error if one was observed before shutdown.
    pub fn into_result(self) -> Result<T> {
        self.command_result.map(|()| self.transport)
    }
}

/// Screen-size independent point for AI/vision driven plans.
///
/// Coordinates are stored as unsigned normalized units where `0` is the
/// top/left edge and `u16::MAX` is the bottom/right edge. Conversion to pixels
/// happens at dispatch time using the session's tracked screen size.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AgentPoint {
    pub x: u16,
    pub y: u16,
}

impl AgentPoint {
    pub const TOP_LEFT: Self = Self::new(0, 0);
    pub const CENTER: Self = Self::new((u16::MAX / 2) + 1, (u16::MAX / 2) + 1);
    pub const BOTTOM_RIGHT: Self = Self::new(u16::MAX, u16::MAX);

    pub const fn new(x: u16, y: u16) -> Self {
        Self { x, y }
    }

    pub fn try_from_unit(x: f32, y: f32) -> Result<Self> {
        if !(0.0..=1.0).contains(&x)
            || !(0.0..=1.0).contains(&y)
            || !x.is_finite()
            || !y.is_finite()
        {
            return Err(Error::SessionLifecycle("normalized point out of range"));
        }
        Ok(Self::new(
            (x * u16::MAX as f32).round() as u16,
            (y * u16::MAX as f32).round() as u16,
        ))
    }

    pub fn try_from_basis_points(x: u16, y: u16) -> Result<Self> {
        if x > 10_000 || y > 10_000 {
            return Err(Error::SessionLifecycle("normalized point out of range"));
        }
        Ok(Self::new(basis_points_to_unit(x), basis_points_to_unit(y)))
    }

    pub fn to_pixels(self, width: u16, height: u16) -> (i32, i32) {
        (
            normalized_axis_to_pixel(self.x, width),
            normalized_axis_to_pixel(self.y, height),
        )
    }
}

/// Filter used when selecting object-detection targets from a [`FrameSummary`].
///
/// Matching objects are scored the same way as [`AgentRect::try_from_best_object`]:
/// highest confidence wins, with larger boxes breaking confidence ties. This
/// keeps selection deterministic while letting agents express class and
/// confidence requirements declaratively.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AgentObjectSelector {
    pub class_id: Option<u8>,
    pub min_confidence: u8,
}

impl AgentObjectSelector {
    pub const ANY: Self = Self::new(None, 0);

    pub const fn new(class_id: Option<u8>, min_confidence: u8) -> Self {
        Self {
            class_id,
            min_confidence,
        }
    }

    pub const fn class_id(class_id: u8) -> Self {
        Self::new(Some(class_id), 0)
    }

    pub const fn min_confidence(min_confidence: u8) -> Self {
        Self::new(None, min_confidence)
    }

    pub const fn class_min_confidence(class_id: u8, min_confidence: u8) -> Self {
        Self::new(Some(class_id), min_confidence)
    }

    pub const fn with_class_id(self, class_id: u8) -> Self {
        Self {
            class_id: Some(class_id),
            ..self
        }
    }

    pub const fn with_min_confidence(self, min_confidence: u8) -> Self {
        Self {
            min_confidence,
            ..self
        }
    }

    pub fn matches(self, object: ObjectBox) -> bool {
        if object.confidence < self.min_confidence {
            return false;
        }
        match self.class_id {
            Some(class_id) => object.class_id == class_id,
            None => true,
        }
    }

    pub fn select(self, summary: &FrameSummary) -> Option<ObjectBox> {
        best_object(
            summary
                .objects
                .iter()
                .copied()
                .filter(|object| self.matches(*object)),
        )
    }

    pub fn select_rect(self, summary: &FrameSummary) -> Result<Option<AgentRect>> {
        self.select(summary)
            .map(|object| AgentRect::try_from_object_box(object, summary.width, summary.height))
            .transpose()
    }
}

/// Screen-size independent rectangle for vision/object-detection targets.
///
/// Edges are stored in the same normalized unit space as [`AgentPoint`].
/// Methods that produce input events use the rectangle center so object boxes
/// and text regions can become stable tap/scroll targets without callers
/// hand-rolling coordinate math.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AgentRect {
    pub left: u16,
    pub top: u16,
    pub right: u16,
    pub bottom: u16,
}

impl AgentRect {
    pub const FULL_SCREEN: Self = Self::new(0, 0, u16::MAX, u16::MAX);

    pub const fn new(left: u16, top: u16, right: u16, bottom: u16) -> Self {
        let (left, right) = if left <= right {
            (left, right)
        } else {
            (right, left)
        };
        let (top, bottom) = if top <= bottom {
            (top, bottom)
        } else {
            (bottom, top)
        };
        Self {
            left,
            top,
            right,
            bottom,
        }
    }

    pub const fn from_points(a: AgentPoint, b: AgentPoint) -> Self {
        Self::new(a.x, a.y, b.x, b.y)
    }

    pub fn try_from_unit(left: f32, top: f32, right: f32, bottom: f32) -> Result<Self> {
        Ok(Self::from_points(
            AgentPoint::try_from_unit(left, top)?,
            AgentPoint::try_from_unit(right, bottom)?,
        ))
    }

    pub fn try_from_basis_points(left: u16, top: u16, right: u16, bottom: u16) -> Result<Self> {
        Ok(Self::from_points(
            AgentPoint::try_from_basis_points(left, top)?,
            AgentPoint::try_from_basis_points(right, bottom)?,
        ))
    }

    pub fn try_from_pixels(
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        frame_width: u16,
        frame_height: u16,
    ) -> Result<Self> {
        let (left, right) = pixel_rect_axis_to_unit(x, w, frame_width)?;
        let (top, bottom) = pixel_rect_axis_to_unit(y, h, frame_height)?;
        Ok(Self::new(left, top, right, bottom))
    }

    pub fn try_from_object_box(
        object: ObjectBox,
        frame_width: u16,
        frame_height: u16,
    ) -> Result<Self> {
        Self::try_from_pixels(
            object.x as i32,
            object.y as i32,
            object.w as i32,
            object.h as i32,
            frame_width,
            frame_height,
        )
    }

    pub fn try_from_text_region(
        region: TextRegion,
        frame_width: u16,
        frame_height: u16,
    ) -> Result<Self> {
        Self::try_from_pixels(
            region.x as i32,
            region.y as i32,
            region.w as i32,
            region.h as i32,
            frame_width,
            frame_height,
        )
    }

    pub fn try_from_frame_object(summary: &FrameSummary, index: usize) -> Result<Option<Self>> {
        summary
            .objects
            .get(index)
            .copied()
            .map(|object| Self::try_from_object_box(object, summary.width, summary.height))
            .transpose()
    }

    pub fn try_from_frame_text_region(
        summary: &FrameSummary,
        index: usize,
    ) -> Result<Option<Self>> {
        summary
            .text_regions
            .get(index)
            .copied()
            .map(|region| Self::try_from_text_region(region, summary.width, summary.height))
            .transpose()
    }

    pub fn try_from_best_object(summary: &FrameSummary) -> Result<Option<Self>> {
        Self::try_from_best_object_matching(summary, AgentObjectSelector::ANY)
    }

    pub fn try_from_best_object_class(
        summary: &FrameSummary,
        class_id: u8,
    ) -> Result<Option<Self>> {
        Self::try_from_best_object_matching(summary, AgentObjectSelector::class_id(class_id))
    }

    pub fn try_from_best_object_matching(
        summary: &FrameSummary,
        selector: AgentObjectSelector,
    ) -> Result<Option<Self>> {
        selector.select_rect(summary)
    }

    pub fn try_from_largest_text_region(summary: &FrameSummary) -> Result<Option<Self>> {
        summary
            .text_regions
            .iter()
            .copied()
            .max_by_key(text_region_area)
            .map(|region| Self::try_from_text_region(region, summary.width, summary.height))
            .transpose()
    }

    pub const fn top_left(self) -> AgentPoint {
        AgentPoint::new(self.left, self.top)
    }

    pub const fn top_right(self) -> AgentPoint {
        AgentPoint::new(self.right, self.top)
    }

    pub const fn bottom_left(self) -> AgentPoint {
        AgentPoint::new(self.left, self.bottom)
    }

    pub const fn bottom_right(self) -> AgentPoint {
        AgentPoint::new(self.right, self.bottom)
    }

    pub const fn center(self) -> AgentPoint {
        AgentPoint::new(
            ((self.left as u32) + (self.right as u32)).div_ceil(2) as u16,
            ((self.top as u32) + (self.bottom as u32)).div_ceil(2) as u16,
        )
    }

    /// Select a point inside the rectangle using unit coordinates where
    /// `(0.0, 0.0)` is the top-left edge and `(1.0, 1.0)` is the bottom-right
    /// edge.
    pub fn try_point_at_unit(self, x: f32, y: f32) -> Result<AgentPoint> {
        if !(0.0..=1.0).contains(&x)
            || !(0.0..=1.0).contains(&y)
            || !x.is_finite()
            || !y.is_finite()
        {
            return Err(Error::SessionLifecycle("normalized point out of range"));
        }

        let left = self.left.min(self.right) as f32;
        let right = self.left.max(self.right) as f32;
        let top = self.top.min(self.bottom) as f32;
        let bottom = self.top.max(self.bottom) as f32;
        Ok(AgentPoint::new(
            (left + ((right - left) * x)).round() as u16,
            (top + ((bottom - top) * y)).round() as u16,
        ))
    }

    /// Select a point inside the rectangle using basis points where
    /// `(0, 0)` is the top-left edge and `(10_000, 10_000)` is the
    /// bottom-right edge.
    pub fn try_point_at_basis_points(self, x: u16, y: u16) -> Result<AgentPoint> {
        if x > 10_000 || y > 10_000 {
            return Err(Error::SessionLifecycle("normalized point out of range"));
        }
        Ok(AgentPoint::new(
            normalized_rect_axis_at_basis_points(self.left, self.right, x),
            normalized_rect_axis_at_basis_points(self.top, self.bottom, y),
        ))
    }

    pub fn to_pixels(self, width: u16, height: u16) -> (i32, i32, i32, i32) {
        let (left, top) = self.top_left().to_pixels(width, height);
        let (right, bottom) = self.bottom_right().to_pixels(width, height);
        (left, top, right, bottom)
    }
}

/// Target selector for AI frame-summary object/text regions.
///
/// This lets planners pass around one typed target value instead of branching
/// across object-index, best-object, class-filtered object, text-index, and
/// largest-text helper families.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AgentTargetSelector {
    Object(usize),
    BestObject,
    ObjectMatching(AgentObjectSelector),
    TextRegion(usize),
    LargestTextRegion,
}

impl AgentTargetSelector {
    pub const fn object(index: usize) -> Self {
        Self::Object(index)
    }

    pub const fn best_object() -> Self {
        Self::BestObject
    }

    pub const fn object_matching(selector: AgentObjectSelector) -> Self {
        Self::ObjectMatching(selector)
    }

    pub const fn object_class(class_id: u8) -> Self {
        Self::ObjectMatching(AgentObjectSelector::class_id(class_id))
    }

    pub const fn object_class_min_confidence(class_id: u8, min_confidence: u8) -> Self {
        Self::ObjectMatching(AgentObjectSelector::class_min_confidence(
            class_id,
            min_confidence,
        ))
    }

    pub const fn text_region(index: usize) -> Self {
        Self::TextRegion(index)
    }

    pub const fn largest_text_region() -> Self {
        Self::LargestTextRegion
    }

    pub fn is_present(self, summary: &FrameSummary) -> bool {
        match self {
            Self::Object(index) => summary.objects.get(index).is_some(),
            Self::BestObject => !summary.objects.is_empty(),
            Self::ObjectMatching(selector) => selector.select(summary).is_some(),
            Self::TextRegion(index) => summary.text_regions.get(index).is_some(),
            Self::LargestTextRegion => !summary.text_regions.is_empty(),
        }
    }

    pub fn select_rect(self, summary: &FrameSummary) -> Result<Option<AgentRect>> {
        match self {
            Self::Object(index) => AgentRect::try_from_frame_object(summary, index),
            Self::BestObject => AgentRect::try_from_best_object(summary),
            Self::ObjectMatching(selector) => {
                AgentRect::try_from_best_object_matching(summary, selector)
            }
            Self::TextRegion(index) => AgentRect::try_from_frame_text_region(summary, index),
            Self::LargestTextRegion => AgentRect::try_from_largest_text_region(summary),
        }
    }
}

/// One exact touch sample for [`AgentAction`] plans.
///
/// Pressure is stored as the u16 value used by scrcpy's touch wire format so
/// action plans remain `Eq` and avoid float comparison semantics. Conversion to
/// the client-side `f32` pressure happens only at dispatch time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AgentTouchFrame {
    pub action: TouchAction,
    pub pointer_id: u64,
    pub x: i32,
    pub y: i32,
    pub pressure: u16,
}

impl AgentTouchFrame {
    pub const EMPTY: Self = Self {
        action: TouchAction::MOVE,
        pointer_id: 0,
        x: 0,
        y: 0,
        pressure: 0,
    };

    pub const fn new(action: TouchAction, pointer_id: u64, x: i32, y: i32, pressure: u16) -> Self {
        Self {
            action,
            pointer_id,
            x,
            y,
            pressure,
        }
    }

    pub const fn with_pointer(
        action: TouchAction,
        pointer_id: TouchPointerId,
        x: i32,
        y: i32,
        pressure: u16,
    ) -> Self {
        Self::new(action, pointer_id.value(), x, y, pressure)
    }

    pub const fn down(pointer_id: u64, x: i32, y: i32, pressure: u16) -> Self {
        Self::new(TouchAction::DOWN, pointer_id, x, y, pressure)
    }

    pub const fn down_pointer(pointer_id: TouchPointerId, x: i32, y: i32, pressure: u16) -> Self {
        Self::with_pointer(TouchAction::DOWN, pointer_id, x, y, pressure)
    }

    pub const fn move_to(pointer_id: u64, x: i32, y: i32, pressure: u16) -> Self {
        Self::new(TouchAction::MOVE, pointer_id, x, y, pressure)
    }

    pub const fn move_pointer_to(
        pointer_id: TouchPointerId,
        x: i32,
        y: i32,
        pressure: u16,
    ) -> Self {
        Self::with_pointer(TouchAction::MOVE, pointer_id, x, y, pressure)
    }

    pub const fn up(pointer_id: u64, x: i32, y: i32) -> Self {
        Self::new(TouchAction::UP, pointer_id, x, y, 0)
    }

    pub const fn up_pointer(pointer_id: TouchPointerId, x: i32, y: i32) -> Self {
        Self::with_pointer(TouchAction::UP, pointer_id, x, y, 0)
    }

    pub const fn cancel(pointer_id: u64) -> Self {
        Self::new(TouchAction::CANCEL, pointer_id, 0, 0, 0)
    }

    pub const fn cancel_pointer(pointer_id: TouchPointerId) -> Self {
        Self::with_pointer(TouchAction::CANCEL, pointer_id, 0, 0, 0)
    }

    fn into_touch_frame(self) -> TouchFrame {
        TouchFrame::with_action(
            self.action,
            self.pointer_id,
            self.x,
            self.y,
            self.pressure as f32 / u16::MAX as f32,
        )
    }
}

/// One integer Android absolute scroll sample for [`AgentAction`] plans.
///
/// The lower-level client stores scroll deltas as `f32` because the scrcpy
/// wire format accepts floats. Agent plans use integer deltas so actions remain
/// cheap to compare, hash, and serialize deterministically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AgentScrollFrame {
    pub x: i32,
    pub y: i32,
    pub hscroll: i16,
    pub vscroll: i16,
    pub buttons: u32,
}

impl AgentScrollFrame {
    pub const EMPTY: Self = Self {
        x: 0,
        y: 0,
        hscroll: 0,
        vscroll: 0,
        buttons: 0,
    };

    pub const fn new(x: i32, y: i32, hscroll: i16, vscroll: i16, buttons: u32) -> Self {
        Self {
            x,
            y,
            hscroll,
            vscroll,
            buttons,
        }
    }

    pub const fn scroll(x: i32, y: i32, hscroll: i16, vscroll: i16) -> Self {
        Self::new(x, y, hscroll, vscroll, 0)
    }

    fn into_scroll_frame(self) -> ScrollFrame {
        ScrollFrame::new(
            self.x,
            self.y,
            self.hscroll as f32,
            self.vscroll as f32,
            self.buttons,
        )
    }
}

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlanGamepadBatchMode {
    Empty,
    Dedupe,
    Unchecked,
    Packed,
}

#[derive(Debug)]
struct PlanGamepadBatcher<'a> {
    mode: PlanGamepadBatchMode,
    dedupe: GamepadFrameBatcher<'a>,
    unchecked: GamepadFrameBatcher<'a>,
    packed: PackedGamepadFrameBatcher<'a>,
}

type PlanBatchers<'a, 'b> = (
    &'b mut TouchFrameBatcher<'a>,
    &'b mut KeyboardFrameBatcher<'a>,
    &'b mut AndroidKeyFrameBatcher<'a>,
    &'b mut MouseFrameBatcher<'a>,
    &'b mut ScrollFrameBatcher<'a>,
    &'b mut PlanGamepadBatcher<'a>,
);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EstimatedFrameBatch {
    len: usize,
    capacity: usize,
}

impl EstimatedFrameBatch {
    const fn new(capacity: usize) -> Self {
        Self { len: 0, capacity }
    }

    fn push_frames(&mut self, frames: usize) -> usize {
        if frames == 0 {
            return 0;
        }
        if self.len == 0 {
            let sends = frames / self.capacity;
            self.len = frames % self.capacity;
            return sends;
        }

        let room = self.capacity - self.len;
        if frames < room {
            self.len += frames;
            return 0;
        }

        let remaining = frames - room;
        self.len = remaining % self.capacity;
        1usize.saturating_add(remaining / self.capacity)
    }

    fn flush(&mut self) -> usize {
        if self.len == 0 {
            0
        } else {
            self.len = 0;
            1
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EstimatedGamepadBatch {
    mode: PlanGamepadBatchMode,
    batch: EstimatedFrameBatch,
}

impl EstimatedGamepadBatch {
    const fn new() -> Self {
        Self {
            mode: PlanGamepadBatchMode::Empty,
            batch: EstimatedFrameBatch::new(GAMEPAD_BATCH_FRAMES),
        }
    }

    fn push_frames(&mut self, mode: PlanGamepadBatchMode, frames: usize) -> usize {
        if frames == 0 {
            return 0;
        }
        let mut sends = 0usize;
        if self.mode != PlanGamepadBatchMode::Empty && self.mode != mode {
            sends = sends.saturating_add(self.flush());
        }
        if self.mode == PlanGamepadBatchMode::Empty {
            self.mode = mode;
        }
        sends.saturating_add(self.batch.push_frames(frames))
    }

    fn flush(&mut self) -> usize {
        let sends = self.batch.flush();
        self.mode = PlanGamepadBatchMode::Empty;
        sends
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PlanCommandEstimator {
    commands: usize,
    touch: EstimatedFrameBatch,
    key: EstimatedFrameBatch,
    android_key: EstimatedFrameBatch,
    mouse: EstimatedFrameBatch,
    scroll: EstimatedFrameBatch,
    gamepad: EstimatedGamepadBatch,
}

impl PlanCommandEstimator {
    const fn new() -> Self {
        Self {
            commands: 0,
            touch: EstimatedFrameBatch::new(TOUCH_BATCH_FRAMES),
            key: EstimatedFrameBatch::new(KEYBOARD_BATCH_FRAMES),
            android_key: EstimatedFrameBatch::new(ANDROID_KEY_BATCH_FRAMES),
            mouse: EstimatedFrameBatch::new(MOUSE_BATCH_FRAMES),
            scroll: EstimatedFrameBatch::new(SCROLL_BATCH_FRAMES),
            gamepad: EstimatedGamepadBatch::new(),
        }
    }

    fn estimate_queue_actions(actions: &[AgentAction]) -> usize {
        let mut estimator = Self::new();
        for action in actions {
            estimator.push_action(action);
        }
        estimator.commands_after_final_flush()
    }

    fn bounded_try_queue_prefix(
        actions: &[AgentAction],
        command_bound: usize,
    ) -> AgentPlanBoundedPrefix {
        let mut estimator = Self::new();
        let mut accepted_actions = 0usize;
        let mut estimated_dispatch_commands = 0usize;

        for (idx, action) in actions.iter().enumerate() {
            if let Some(error) = action.structural_error() {
                return AgentPlanBoundedPrefix {
                    action_count: actions.len(),
                    accepted_actions,
                    estimated_dispatch_commands,
                    command_bound,
                    stop: AgentPlanBoundedPrefixStop::TryQueueError { index: idx, error },
                };
            }
            if action.requires_blocking_timing() {
                return AgentPlanBoundedPrefix {
                    action_count: actions.len(),
                    accepted_actions,
                    estimated_dispatch_commands,
                    command_bound,
                    stop: AgentPlanBoundedPrefixStop::BlockingTiming { index: idx },
                };
            }

            let mut next = estimator;
            next.push_action(action);
            let required_dispatch_commands = next.commands_after_final_flush();
            if required_dispatch_commands > command_bound {
                return AgentPlanBoundedPrefix {
                    action_count: actions.len(),
                    accepted_actions,
                    estimated_dispatch_commands,
                    command_bound,
                    stop: AgentPlanBoundedPrefixStop::CommandBound {
                        index: idx,
                        required_dispatch_commands,
                    },
                };
            }

            estimator = next;
            accepted_actions = idx + 1;
            estimated_dispatch_commands = required_dispatch_commands;
        }

        AgentPlanBoundedPrefix {
            action_count: actions.len(),
            accepted_actions,
            estimated_dispatch_commands,
            command_bound,
            stop: AgentPlanBoundedPrefixStop::EndOfPlan,
        }
    }

    fn bounded_try_run_prefix(
        actions: &[AgentAction],
        command_bound: usize,
    ) -> AgentPlanBoundedPrefix {
        if command_bound == 0 {
            return AgentPlanBoundedPrefix {
                action_count: actions.len(),
                accepted_actions: 0,
                estimated_dispatch_commands: 0,
                command_bound,
                stop: AgentPlanBoundedPrefixStop::CommandBound {
                    index: 0,
                    required_dispatch_commands: 1,
                },
            };
        }

        let mut prefix = Self::bounded_try_queue_prefix(actions, command_bound - 1);
        prefix.command_bound = command_bound;
        if let AgentPlanBoundedPrefixStop::CommandBound {
            index,
            required_dispatch_commands,
        } = prefix.stop
        {
            prefix.stop = AgentPlanBoundedPrefixStop::CommandBound {
                index,
                required_dispatch_commands: required_dispatch_commands.saturating_add(1),
            };
        }
        prefix
    }

    fn commands_after_final_flush(mut self) -> usize {
        self.flush_all();
        self.commands
    }

    fn add_commands(&mut self, commands: usize) {
        self.commands = self.commands.saturating_add(commands);
    }

    fn flush_touch(&mut self) {
        let commands = self.touch.flush();
        self.add_commands(commands);
    }

    fn flush_key(&mut self) {
        let commands = self.key.flush();
        self.add_commands(commands);
    }

    fn flush_android_key(&mut self) {
        let commands = self.android_key.flush();
        self.add_commands(commands);
    }

    fn flush_mouse(&mut self) {
        let commands = self.mouse.flush();
        self.add_commands(commands);
    }

    fn flush_scroll(&mut self) {
        let commands = self.scroll.flush();
        self.add_commands(commands);
    }

    fn flush_gamepad(&mut self) {
        let commands = self.gamepad.flush();
        self.add_commands(commands);
    }

    fn flush_all(&mut self) {
        self.flush_touch();
        self.flush_key();
        self.flush_android_key();
        self.flush_mouse();
        self.flush_scroll();
        self.flush_gamepad();
    }

    fn flush_non_touch(&mut self) {
        self.flush_key();
        self.flush_android_key();
        self.flush_mouse();
        self.flush_gamepad();
    }

    fn flush_non_key(&mut self) {
        self.flush_touch();
        self.flush_android_key();
        self.flush_mouse();
        self.flush_gamepad();
    }

    fn flush_non_android_key(&mut self) {
        self.flush_touch();
        self.flush_key();
        self.flush_mouse();
        self.flush_gamepad();
    }

    fn flush_non_mouse(&mut self) {
        self.flush_touch();
        self.flush_key();
        self.flush_android_key();
        self.flush_gamepad();
    }

    fn flush_non_scroll(&mut self) {
        self.flush_touch();
        self.flush_key();
        self.flush_android_key();
        self.flush_mouse();
        self.flush_gamepad();
    }

    fn flush_non_gamepad(&mut self) {
        self.flush_touch();
        self.flush_key();
        self.flush_android_key();
        self.flush_mouse();
    }

    fn push_touch(&mut self, frames: usize) {
        let commands = self.touch.push_frames(frames);
        self.add_commands(commands);
    }

    fn push_key(&mut self, frames: usize) {
        let commands = self.key.push_frames(frames);
        self.add_commands(commands);
    }

    fn push_android_key(&mut self, frames: usize) {
        let commands = self.android_key.push_frames(frames);
        self.add_commands(commands);
    }

    fn push_mouse(&mut self, frames: usize) {
        let commands = self.mouse.push_frames(frames);
        self.add_commands(commands);
    }

    fn push_scroll(&mut self, frames: usize) {
        let commands = self.scroll.push_frames(frames);
        self.add_commands(commands);
    }

    fn push_gamepad(&mut self, mode: PlanGamepadBatchMode, frames: usize) {
        let commands = self.gamepad.push_frames(mode, frames);
        self.add_commands(commands);
    }

    fn push_action(&mut self, action: &AgentAction) {
        if !Self::is_scroll_action(action) {
            self.flush_scroll();
        }

        match action {
            AgentAction::Tap { .. }
            | AgentAction::TapPointer { .. }
            | AgentAction::TapPoint { .. }
            | AgentAction::TapPointPointer { .. }
            | AgentAction::TapRect { .. }
            | AgentAction::TapRectAt { .. }
            | AgentAction::TapRectPointer { .. }
            | AgentAction::TapRectAtPointer { .. } => {
                self.flush_non_touch();
                self.push_touch(2);
            }
            AgentAction::DoubleTap { .. }
            | AgentAction::DoubleTapPointer { .. }
            | AgentAction::DoubleTapPoint { .. }
            | AgentAction::DoubleTapPointPointer { .. }
            | AgentAction::DoubleTapRect { .. }
            | AgentAction::DoubleTapRectAt { .. }
            | AgentAction::DoubleTapRectPointer { .. }
            | AgentAction::DoubleTapRectAtPointer { .. } => {
                self.flush_non_touch();
                self.push_touch(4);
            }
            AgentAction::Swipe { steps, .. }
            | AgentAction::SwipePointer { steps, .. }
            | AgentAction::SwipePoints { steps, .. }
            | AgentAction::SwipePointsPointer { steps, .. }
            | AgentAction::SwipeRect { steps, .. }
            | AgentAction::SwipeRectPointer { steps, .. } => {
                self.flush_non_touch();
                self.push_touch((*steps).max(1).saturating_add(2));
            }
            AgentAction::Pinch { steps, .. } | AgentAction::PinchPoints { steps, .. } => {
                self.flush_non_touch();
                self.push_touch((*steps).max(1).saturating_mul(2).saturating_add(4));
            }
            AgentAction::CancelTouch { .. } => {
                self.flush_non_touch();
                self.push_touch(1);
            }
            AgentAction::TouchFrames { len, .. } => {
                self.flush_non_touch();
                self.push_touch(*len);
            }
            AgentAction::ThreeFingerScreenshot => {
                self.flush_non_touch();
                self.push_touch(36);
            }
            AgentAction::Key { .. } => {
                self.flush_non_key();
                self.push_key(1);
            }
            AgentAction::KeyTap { .. } => {
                self.flush_non_key();
                self.push_key(2);
            }
            AgentAction::KeyboardChord { chord } => {
                self.flush_non_key();
                self.push_key((chord.len as usize).saturating_mul(2));
            }
            AgentAction::KeyBatch { len, .. } => {
                self.flush_non_key();
                self.push_key(*len);
            }
            AgentAction::InjectKeycode { .. }
            | AgentAction::PressHome
            | AgentAction::PressBack
            | AgentAction::OpenRecents
            | AgentAction::VolumeUp
            | AgentAction::VolumeDown
            | AgentAction::VolumeMute => {
                self.flush_non_android_key();
                self.push_android_key(1);
            }
            AgentAction::AndroidKeyTap { .. } => {
                self.flush_non_android_key();
                self.push_android_key(2);
            }
            AgentAction::AndroidKeyBatch { len, .. } => {
                self.flush_non_android_key();
                self.push_android_key(*len);
            }
            AgentAction::MouseMotion { .. } | AgentAction::MouseButtons { .. } => {
                self.flush_non_mouse();
                self.push_mouse(1);
            }
            AgentAction::MouseBatch { len, .. } => {
                self.flush_non_mouse();
                self.push_mouse(*len);
            }
            AgentAction::Scroll { .. }
            | AgentAction::ScrollPoint { .. }
            | AgentAction::ScrollRect { .. }
            | AgentAction::ScrollRectAt { .. } => {
                self.flush_non_scroll();
                self.push_scroll(1);
            }
            AgentAction::ScrollBatch { len, .. } => {
                self.flush_non_scroll();
                self.push_scroll(*len);
            }
            AgentAction::GamepadFrame { .. } => {
                self.flush_non_gamepad();
                self.push_gamepad(PlanGamepadBatchMode::Dedupe, 1);
            }
            AgentAction::GamepadFrameUnchecked { .. } => {
                self.flush_non_gamepad();
                self.push_gamepad(PlanGamepadBatchMode::Unchecked, 1);
            }
            AgentAction::GamepadFrameBatch { len, .. } => {
                self.flush_non_gamepad();
                self.push_gamepad(PlanGamepadBatchMode::Dedupe, *len);
            }
            AgentAction::GamepadFrameBatchUnchecked { len, .. } => {
                self.flush_non_gamepad();
                self.push_gamepad(PlanGamepadBatchMode::Unchecked, *len);
            }
            AgentAction::GamepadPackedFrame { .. } => {
                self.flush_non_gamepad();
                self.push_gamepad(PlanGamepadBatchMode::Packed, 1);
            }
            AgentAction::GamepadPackedFrameBatch { len, .. } => {
                self.flush_non_gamepad();
                self.push_gamepad(PlanGamepadBatchMode::Packed, *len);
            }
            AgentAction::LongPress { .. }
            | AgentAction::LongPressPointer { .. }
            | AgentAction::LongPressPoint { .. }
            | AgentAction::LongPressPointPointer { .. }
            | AgentAction::LongPressRect { .. }
            | AgentAction::LongPressRectAt { .. }
            | AgentAction::LongPressRectPointer { .. }
            | AgentAction::LongPressRectAtPointer { .. } => {
                self.flush_all();
                self.add_commands(3);
            }
            AgentAction::Wait(_) | AgentAction::Flush => {
                self.flush_all();
                self.add_commands(1);
            }
            _ => {
                self.flush_all();
                self.add_commands(1);
            }
        }
    }

    fn is_scroll_action(action: &AgentAction) -> bool {
        matches!(
            action,
            AgentAction::Scroll { .. }
                | AgentAction::ScrollPoint { .. }
                | AgentAction::ScrollRect { .. }
                | AgentAction::ScrollRectAt { .. }
                | AgentAction::ScrollBatch { .. }
        )
    }
}

impl<'a> PlanGamepadBatcher<'a> {
    fn new(client: &'a HidClient) -> Self {
        Self {
            mode: PlanGamepadBatchMode::Empty,
            dedupe: GamepadFrameBatcher::dedupe(client, GAMEPAD_BATCH_FRAMES),
            unchecked: GamepadFrameBatcher::unchecked(client, GAMEPAD_BATCH_FRAMES),
            packed: PackedGamepadFrameBatcher::new(client, GAMEPAD_BATCH_FRAMES),
        }
    }

    fn push_dedupe(&mut self, frame: GamepadFrameRaw) -> Result<()> {
        self.ensure_mode(PlanGamepadBatchMode::Dedupe)?;
        self.dedupe.push(frame)
    }

    fn try_push_dedupe(&mut self, frame: GamepadFrameRaw) -> Result<()> {
        self.try_ensure_mode(PlanGamepadBatchMode::Dedupe)?;
        self.dedupe.try_push(frame)
    }

    fn push_unchecked(&mut self, frame: GamepadFrameRaw) -> Result<()> {
        self.ensure_mode(PlanGamepadBatchMode::Unchecked)?;
        self.unchecked.push(frame)
    }

    fn try_push_unchecked(&mut self, frame: GamepadFrameRaw) -> Result<()> {
        self.try_ensure_mode(PlanGamepadBatchMode::Unchecked)?;
        self.unchecked.try_push(frame)
    }

    fn push_packed(&mut self, frame: [u8; GAMEPAD_FRAME_BYTES]) -> Result<()> {
        self.ensure_mode(PlanGamepadBatchMode::Packed)?;
        self.packed.push(frame)
    }

    fn try_push_packed(&mut self, frame: [u8; GAMEPAD_FRAME_BYTES]) -> Result<()> {
        self.try_ensure_mode(PlanGamepadBatchMode::Packed)?;
        self.packed.try_push(frame)
    }

    fn push_dedupe_slice(
        &mut self,
        len: usize,
        frames: &[GamepadFrameRaw; GAMEPAD_BATCH_FRAMES],
    ) -> Result<()> {
        if len == 0 {
            return Ok(());
        }
        if len > GAMEPAD_BATCH_FRAMES {
            return Err(Error::SessionLifecycle("frame batch fixed length overflow"));
        }
        self.ensure_mode(PlanGamepadBatchMode::Dedupe)?;
        self.dedupe.push_many_slice(&frames[..len])
    }

    fn try_push_dedupe_slice(
        &mut self,
        len: usize,
        frames: &[GamepadFrameRaw; GAMEPAD_BATCH_FRAMES],
    ) -> Result<()> {
        if len == 0 {
            return Ok(());
        }
        if len > GAMEPAD_BATCH_FRAMES {
            return Err(Error::SessionLifecycle("frame batch fixed length overflow"));
        }
        self.try_ensure_mode(PlanGamepadBatchMode::Dedupe)?;
        self.dedupe.try_push_many_slice(&frames[..len])
    }

    fn push_unchecked_slice(
        &mut self,
        len: usize,
        frames: &[GamepadFrameRaw; GAMEPAD_BATCH_FRAMES],
    ) -> Result<()> {
        if len == 0 {
            return Ok(());
        }
        if len > GAMEPAD_BATCH_FRAMES {
            return Err(Error::SessionLifecycle("frame batch fixed length overflow"));
        }
        self.ensure_mode(PlanGamepadBatchMode::Unchecked)?;
        self.unchecked.push_many_slice(&frames[..len])
    }

    fn try_push_unchecked_slice(
        &mut self,
        len: usize,
        frames: &[GamepadFrameRaw; GAMEPAD_BATCH_FRAMES],
    ) -> Result<()> {
        if len == 0 {
            return Ok(());
        }
        if len > GAMEPAD_BATCH_FRAMES {
            return Err(Error::SessionLifecycle("frame batch fixed length overflow"));
        }
        self.try_ensure_mode(PlanGamepadBatchMode::Unchecked)?;
        self.unchecked.try_push_many_slice(&frames[..len])
    }

    fn push_packed_slice(
        &mut self,
        len: usize,
        frames: &[[u8; GAMEPAD_FRAME_BYTES]; GAMEPAD_BATCH_FRAMES],
    ) -> Result<()> {
        if len == 0 {
            return Ok(());
        }
        if len > GAMEPAD_BATCH_FRAMES {
            return Err(Error::SessionLifecycle(
                "frame packed batch fixed length overflow",
            ));
        }
        self.ensure_mode(PlanGamepadBatchMode::Packed)?;
        self.packed.push_many_slice(&frames[..len])
    }

    fn try_push_packed_slice(
        &mut self,
        len: usize,
        frames: &[[u8; GAMEPAD_FRAME_BYTES]; GAMEPAD_BATCH_FRAMES],
    ) -> Result<()> {
        if len == 0 {
            return Ok(());
        }
        if len > GAMEPAD_BATCH_FRAMES {
            return Err(Error::SessionLifecycle(
                "frame packed batch fixed length overflow",
            ));
        }
        self.try_ensure_mode(PlanGamepadBatchMode::Packed)?;
        self.packed.try_push_many_slice(&frames[..len])
    }

    fn flush(&mut self) -> Result<()> {
        let result = match self.mode {
            PlanGamepadBatchMode::Empty => Ok(()),
            PlanGamepadBatchMode::Dedupe => self.dedupe.flush(),
            PlanGamepadBatchMode::Unchecked => self.unchecked.flush(),
            PlanGamepadBatchMode::Packed => self.packed.flush(),
        };
        if result.is_ok() {
            self.mode = PlanGamepadBatchMode::Empty;
        }
        result
    }

    fn try_flush(&mut self) -> Result<()> {
        let result = match self.mode {
            PlanGamepadBatchMode::Empty => Ok(()),
            PlanGamepadBatchMode::Dedupe => self.dedupe.try_flush(),
            PlanGamepadBatchMode::Unchecked => self.unchecked.try_flush(),
            PlanGamepadBatchMode::Packed => self.packed.try_flush(),
        };
        if result.is_ok() {
            self.mode = PlanGamepadBatchMode::Empty;
        }
        result
    }

    fn ensure_mode(&mut self, mode: PlanGamepadBatchMode) -> Result<()> {
        if self.mode != PlanGamepadBatchMode::Empty && self.mode != mode {
            self.flush()?;
        }
        if self.mode == PlanGamepadBatchMode::Empty {
            self.mode = mode;
        }
        Ok(())
    }

    fn try_ensure_mode(&mut self, mode: PlanGamepadBatchMode) -> Result<()> {
        if self.mode != PlanGamepadBatchMode::Empty && self.mode != mode {
            self.try_flush()?;
        }
        if self.mode == PlanGamepadBatchMode::Empty {
            self.mode = mode;
        }
        Ok(())
    }
}

impl<T, R> AgentControlSession<T, R>
where
    T: TransportWrite + Send + 'static,
    R: Read,
{
    /// Build an agent session from an already-opened `HidSession` and a
    /// byte-aligned device-message reader.
    pub fn from_parts(session: HidSession<T>, reader: R) -> Result<Self> {
        Self::from_parts_with_bound(session, reader, DEFAULT_AGENT_COMMAND_BOUND)
    }

    /// Same as [`Self::from_parts`], but with an explicit command-channel
    /// bound for high-rate producer loops.
    pub fn from_parts_with_bound(
        session: HidSession<T>,
        reader: R,
        command_bound: usize,
    ) -> Result<Self> {
        let (client, dispatcher) = session.into_client_with_bound(command_bound)?;
        Ok(Self {
            client,
            dispatcher: Some(dispatcher),
            receiver: Some(DeviceMessageReceiver::new(reader)),
            command_bound,
            next_clipboard_sequence: 1,
            screen_width: AtomicU16::new(DEFAULT_AGENT_SCREEN_WIDTH),
            screen_height: AtomicU16::new(DEFAULT_AGENT_SCREEN_HEIGHT),
        })
    }

    /// Producer handle for sending control commands.
    pub fn client(&self) -> &HidClient {
        &self.client
    }

    /// Cloneable producer handle for worker threads or agent tools.
    pub fn clone_client(&self) -> HidClient {
        self.client.clone()
    }

    /// Configured dispatcher command-channel bound for this session.
    pub const fn command_bound(&self) -> usize {
        self.command_bound
    }

    /// Analyze the longest safe non-blocking prefix using this session's
    /// configured dispatcher command-channel bound, without dispatching it.
    ///
    /// Use this when a scheduler needs to split or route a plan before touching
    /// the bounded producer queue. Use
    /// [`Self::try_queue_actions_bounded_prefix_with_session_bound`] when the
    /// accepted prefix should be dispatched immediately.
    pub fn bounded_try_queue_prefix_with_session_bound(
        &self,
        actions: &[AgentAction],
    ) -> AgentPlanBoundedPrefix {
        AgentAction::bounded_try_queue_prefix(actions, self.command_bound)
    }

    /// Analyze the longest checked non-blocking prefix using this session's
    /// configured dispatcher command-channel bound, without dispatching it.
    ///
    /// Unlike [`Self::bounded_try_queue_prefix_with_session_bound`], this
    /// reserves one command slot for the final checked barrier used by
    /// [`Self::try_run_actions_bounded_prefix_with_session_bound`].
    pub fn bounded_try_run_prefix_with_session_bound(
        &self,
        actions: &[AgentAction],
    ) -> AgentPlanBoundedPrefix {
        AgentAction::bounded_try_run_prefix(actions, self.command_bound)
    }

    /// Update screen dimensions used by subsequent touch injection.
    pub fn set_screen_size(&self, width: u16, height: u16) -> Result<()> {
        self.client.set_screen_size(width, height)?;
        self.screen_width.store(width, Ordering::Relaxed);
        self.screen_height.store(height, Ordering::Relaxed);
        Ok(())
    }

    /// Update screen dimensions using non-blocking dispatcher send, then
    /// enqueue one checked dispatcher barrier before updating agent-local
    /// coordinate metadata.
    pub fn try_set_screen_size(&self, width: u16, height: u16) -> Result<()> {
        self.ensure_direct_try_capacity(TRY_CONTROL_EXCEEDS_COMMAND_BOUND)?;
        self.client.try_set_screen_size(width, height)?;
        self.client.try_flush_wait()?;
        self.screen_width.store(width, Ordering::Relaxed);
        self.screen_height.store(height, Ordering::Relaxed);
        Ok(())
    }

    /// Current agent-local screen size used for generated gesture paths.
    pub fn screen_size(&self) -> (u16, u16) {
        (
            self.screen_width.load(Ordering::Relaxed),
            self.screen_height.load(Ordering::Relaxed),
        )
    }

    fn ensure_direct_try_capacity(&self, msg: &'static str) -> Result<()> {
        if self.command_bound < 2 {
            return Err(Error::SessionLifecycle(msg));
        }
        Ok(())
    }

    fn try_finish_direct_gamepad_command(&self) -> Result<()> {
        self.client.try_flush_wait().map(|_| ())
    }

    fn try_run_direct_command(&self, cmd: HidCommand, msg: &'static str) -> Result<()> {
        self.ensure_direct_try_capacity(msg)?;
        self.client.try_send(cmd)?;
        self.client.try_flush_wait().map(|_| ())
    }

    /// Read the next server→host message from the byte-aligned receiver.
    pub fn recv_device_message(&mut self) -> io::Result<DeviceMessage> {
        self.receiver_mut()?.read_next()
    }

    /// Read the next native scrcpy message or AI extension event.
    pub fn recv_device_event(&mut self) -> io::Result<DeviceEvent> {
        self.receiver_mut()?.read_next_event()
    }

    /// Move the session's byte-aligned reader into a latest-frame background
    /// pump.
    ///
    /// This is an explicit mode switch for low-latency perception loops:
    /// the returned [`LatestFrameSummaryReceiver`] continuously drains the
    /// mixed server→host stream and keeps only the newest AI frame summary.
    /// After detaching, ordered read helpers such as clipboard ACK waits are no
    /// longer available on this agent; use [`Self::close_transport`] or
    /// [`Self::close_transport_checked`] to close the write side, and join the
    /// returned pump to recover the reader.
    pub fn detach_latest_frame_summary_receiver(
        &mut self,
    ) -> Result<(LatestFrameSummaryReceiver, DeviceMessagePump<R>)>
    where
        R: Send + 'static,
    {
        let reader = self
            .receiver
            .take()
            .ok_or(Error::DispatcherDown("agent receiver already taken"))?
            .into_inner();
        spawn_latest_frame_summary_receiver(reader).map_err(io_to_error)
    }

    /// Type text into the focused field using the dispatcher thread.
    pub fn type_text(&self, text: impl Into<String>) -> Result<()> {
        self.client.type_text(text)
    }

    /// Type text into the focused field and fail at the next checked
    /// dispatcher boundary if any character cannot be represented as a USB HID
    /// keyboard scancode.
    pub fn type_text_strict(&self, text: impl Into<String>) -> Result<()> {
        self.client.type_text_strict(text)
    }

    /// Send one raw USB HID keyboard scancode edge.
    pub fn key(&self, scancode: u8, pressed: bool, mods: Modifiers) -> Result<()> {
        self.client.key(scancode, pressed, mods)
    }

    /// Send one raw USB HID keyboard scancode edge using non-blocking
    /// dispatcher send, then enqueue one checked dispatcher barrier.
    pub fn try_key(&self, scancode: u8, pressed: bool, mods: Modifiers) -> Result<()> {
        self.ensure_direct_try_capacity(TRY_KEY_EXCEEDS_COMMAND_BOUND)?;
        self.client.try_key(scancode, pressed, mods)?;
        self.client.try_flush_wait().map(|_| ())
    }

    /// Send one typed USB HID keyboard scancode edge.
    pub fn key_scancode(&self, scancode: Scancode, pressed: bool, mods: Modifiers) -> Result<()> {
        self.client.key_scancode(scancode, pressed, mods)
    }

    /// Send one typed USB HID keyboard scancode edge using non-blocking
    /// dispatcher send, then enqueue one checked dispatcher barrier.
    pub fn try_key_scancode(
        &self,
        scancode: Scancode,
        pressed: bool,
        mods: Modifiers,
    ) -> Result<()> {
        self.try_key(scancode.to_u8(), pressed, mods)
    }

    /// Press and release one raw USB HID keyboard scancode through one
    /// dispatcher command.
    pub fn tap_key(&self, scancode: u8, mods: Modifiers) -> Result<()> {
        self.client.tap_key(scancode, mods)
    }

    /// Press and release one raw USB HID keyboard scancode using non-blocking
    /// dispatcher send, then enqueue one checked dispatcher barrier.
    pub fn try_tap_key(&self, scancode: u8, mods: Modifiers) -> Result<()> {
        self.ensure_direct_try_capacity(TRY_KEY_EXCEEDS_COMMAND_BOUND)?;
        self.client.try_tap_key(scancode, mods)?;
        self.client.try_flush_wait().map(|_| ())
    }

    /// Press and release one typed USB HID keyboard scancode through one
    /// dispatcher command.
    pub fn tap_scancode(&self, scancode: Scancode, mods: Modifiers) -> Result<()> {
        self.client.tap_scancode(scancode, mods)
    }

    /// Press and release one typed USB HID keyboard scancode using
    /// non-blocking dispatcher send, then enqueue one checked dispatcher
    /// barrier.
    pub fn try_tap_scancode(&self, scancode: Scancode, mods: Modifiers) -> Result<()> {
        self.try_tap_key(scancode.to_u8(), mods)
    }

    /// Send one keyboard chord as a fixed-buffer edge batch.
    pub fn key_chord(&self, chord: KeyboardChordFrame) -> Result<()> {
        self.client.key_chord(chord)
    }

    /// Send one keyboard chord as a fixed-buffer edge batch using
    /// non-blocking dispatcher send, then enqueue one checked dispatcher
    /// barrier.
    pub fn try_key_chord(&self, chord: KeyboardChordFrame) -> Result<()> {
        self.ensure_direct_try_capacity(TRY_KEY_EXCEEDS_COMMAND_BOUND)?;
        self.client.try_key_chord(chord)?;
        self.client.try_flush_wait().map(|_| ())
    }

    /// Send one keyboard chord from typed scancodes.
    pub fn scancode_chord(&self, scancodes: &[Scancode], mods: Modifiers) -> Result<()> {
        self.client.scancode_chord(scancodes, mods)
    }

    /// Send one keyboard chord from typed scancodes using non-blocking
    /// dispatcher send, then enqueue one checked dispatcher barrier.
    pub fn try_scancode_chord(&self, scancodes: &[Scancode], mods: Modifiers) -> Result<()> {
        self.ensure_direct_try_capacity(TRY_KEY_EXCEEDS_COMMAND_BOUND)?;
        self.client.try_scancode_chord(scancodes, mods)?;
        self.client.try_flush_wait().map(|_| ())
    }

    /// Inject a raw Android `KeyEvent.KEYCODE_*` control message.
    pub fn inject_keycode(
        &self,
        action: u8,
        keycode: u32,
        repeat: u32,
        metastate: u32,
    ) -> Result<()> {
        self.client
            .inject_keycode(action, keycode, repeat, metastate)
    }

    /// Inject a raw Android `KeyEvent.KEYCODE_*` control message using
    /// non-blocking dispatcher send, then enqueue one checked dispatcher
    /// barrier.
    pub fn try_inject_keycode(
        &self,
        action: u8,
        keycode: u32,
        repeat: u32,
        metastate: u32,
    ) -> Result<()> {
        self.ensure_direct_try_capacity(TRY_ANDROID_KEY_EXCEEDS_COMMAND_BOUND)?;
        self.client
            .try_inject_keycode(action, keycode, repeat, metastate)?;
        self.client.try_flush_wait().map(|_| ())
    }

    /// Inject a typed Android `KeyEvent.KEYCODE_*` control message.
    pub fn inject_android_keycode(
        &self,
        action: u8,
        keycode: AndroidKeycode,
        repeat: u32,
        metastate: u32,
    ) -> Result<()> {
        self.client
            .inject_android_keycode(action, keycode, repeat, metastate)
    }

    /// Inject a typed Android `KeyEvent.KEYCODE_*` control message using
    /// non-blocking dispatcher send, then enqueue one checked dispatcher
    /// barrier.
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
        self.client
            .inject_android_key_event(action, keycode, repeat, metastate)
    }

    /// Inject a fully typed Android key event using non-blocking dispatcher
    /// send, then enqueue one checked dispatcher barrier.
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
        self.client.press_android_key(keycode)
    }

    /// Press one typed Android keycode with action DOWN using non-blocking
    /// dispatcher send, then enqueue one checked dispatcher barrier.
    pub fn try_press_android_key(&self, keycode: AndroidKeycode) -> Result<()> {
        self.ensure_direct_try_capacity(TRY_ANDROID_KEY_EXCEEDS_COMMAND_BOUND)?;
        self.client.try_press_android_key(keycode)?;
        self.client.try_flush_wait().map(|_| ())
    }

    /// Release one typed Android keycode with action UP.
    pub fn release_android_key(&self, keycode: AndroidKeycode) -> Result<()> {
        self.client.release_android_key(keycode)
    }

    /// Release one typed Android keycode with action UP using non-blocking
    /// dispatcher send, then enqueue one checked dispatcher barrier.
    pub fn try_release_android_key(&self, keycode: AndroidKeycode) -> Result<()> {
        self.ensure_direct_try_capacity(TRY_ANDROID_KEY_EXCEEDS_COMMAND_BOUND)?;
        self.client.try_release_android_key(keycode)?;
        self.client.try_flush_wait().map(|_| ())
    }

    /// Press and release one raw Android `KeyEvent.KEYCODE_*` through one
    /// dispatcher command.
    pub fn tap_android_keycode(&self, keycode: u32, metastate: u32) -> Result<()> {
        self.client.tap_android_keycode(keycode, metastate)
    }

    /// Press and release one raw Android `KeyEvent.KEYCODE_*` using
    /// non-blocking dispatcher send, then enqueue one checked dispatcher
    /// barrier.
    pub fn try_tap_android_keycode(&self, keycode: u32, metastate: u32) -> Result<()> {
        self.ensure_direct_try_capacity(TRY_ANDROID_KEY_EXCEEDS_COMMAND_BOUND)?;
        self.client.try_tap_android_keycode(keycode, metastate)?;
        self.client.try_flush_wait().map(|_| ())
    }

    /// Press and release one typed Android keycode through one dispatcher
    /// command.
    pub fn tap_android_key(&self, keycode: AndroidKeycode) -> Result<()> {
        self.client.tap_android_key(keycode)
    }

    /// Press and release one typed Android keycode using non-blocking
    /// dispatcher send, then enqueue one checked dispatcher barrier.
    pub fn try_tap_android_key(&self, keycode: AndroidKeycode) -> Result<()> {
        self.try_tap_android_keycode(keycode.value(), 0)
    }

    /// Press and release one typed Android keycode with a metastate.
    pub fn tap_android_key_with_metastate(
        &self,
        keycode: AndroidKeycode,
        metastate: u32,
    ) -> Result<()> {
        self.client
            .tap_android_key_with_metastate(keycode, metastate)
    }

    /// Press and release one typed Android keycode with a metastate using
    /// non-blocking dispatcher send, then enqueue one checked dispatcher
    /// barrier.
    pub fn try_tap_android_key_with_metastate(
        &self,
        keycode: AndroidKeycode,
        metastate: u32,
    ) -> Result<()> {
        self.try_tap_android_keycode(keycode.value(), metastate)
    }

    /// Send scrcpy BACK_OR_SCREEN_ON. If the screen is off, scrcpy wakes it;
    /// otherwise it behaves like Back for the supplied key action.
    pub fn back_or_screen_on(&self, action: AndroidKeyAction) -> Result<()> {
        self.client.back_or_screen_on(action)
    }

    /// Send scrcpy BACK_OR_SCREEN_ON using non-blocking dispatcher send, then
    /// enqueue one checked dispatcher barrier.
    pub fn try_back_or_screen_on(&self, action: AndroidKeyAction) -> Result<()> {
        self.ensure_direct_try_capacity(TRY_ANDROID_KEY_EXCEEDS_COMMAND_BOUND)?;
        self.client.try_back_or_screen_on(action)?;
        self.client.try_flush_wait().map(|_| ())
    }

    /// Press the Home key.
    pub fn press_home(&self) -> Result<()> {
        self.client.press_home()
    }

    /// Press the Home key using non-blocking dispatcher send, then enqueue one
    /// checked dispatcher barrier.
    pub fn try_press_home(&self) -> Result<()> {
        self.try_press_android_key(AndroidKeycode::HOME)
    }

    /// Press the Back key.
    pub fn press_back(&self) -> Result<()> {
        self.client.press_back()
    }

    /// Press the Back key using non-blocking dispatcher send, then enqueue one
    /// checked dispatcher barrier.
    pub fn try_press_back(&self) -> Result<()> {
        self.try_press_android_key(AndroidKeycode::BACK)
    }

    /// Open the Android recents / app switcher.
    pub fn open_recents(&self) -> Result<()> {
        self.client.open_recents()
    }

    /// Open the Android recents / app switcher using non-blocking dispatcher
    /// send, then enqueue one checked dispatcher barrier.
    pub fn try_open_recents(&self) -> Result<()> {
        self.try_press_android_key(AndroidKeycode::APP_SWITCH)
    }

    /// Press Volume Up.
    pub fn volume_up(&self) -> Result<()> {
        self.client.volume_up()
    }

    /// Press Volume Up using non-blocking dispatcher send, then enqueue one
    /// checked dispatcher barrier.
    pub fn try_volume_up(&self) -> Result<()> {
        self.try_press_android_key(AndroidKeycode::VOLUME_UP)
    }

    /// Press Volume Down.
    pub fn volume_down(&self) -> Result<()> {
        self.client.volume_down()
    }

    /// Press Volume Down using non-blocking dispatcher send, then enqueue one
    /// checked dispatcher barrier.
    pub fn try_volume_down(&self) -> Result<()> {
        self.try_press_android_key(AndroidKeycode::VOLUME_DOWN)
    }

    /// Press Volume Mute.
    pub fn volume_mute(&self) -> Result<()> {
        self.client.volume_mute()
    }

    /// Press Volume Mute using non-blocking dispatcher send, then enqueue one
    /// checked dispatcher barrier.
    pub fn try_volume_mute(&self) -> Result<()> {
        self.try_press_android_key(AndroidKeycode::VOLUME_MUTE)
    }

    /// Send one relative UHID mouse motion report.
    pub fn mouse_motion(&self, dx: i32, dy: i32, buttons: u8) -> Result<()> {
        self.client.mouse_motion(dx, dy, buttons)
    }

    /// Send one relative UHID mouse motion report using non-blocking dispatcher
    /// send, then enqueue one checked dispatcher barrier.
    pub fn try_mouse_motion(&self, dx: i32, dy: i32, buttons: u8) -> Result<()> {
        if self.command_bound < 2 {
            return Err(Error::SessionLifecycle(TRY_MOUSE_EXCEEDS_COMMAND_BOUND));
        }
        self.client.try_mouse_motion(dx, dy, buttons)?;
        self.client.try_flush_wait().map(|_| ())
    }

    /// Send one relative UHID mouse motion report with typed buttons.
    pub fn mouse_motion_buttons(&self, dx: i32, dy: i32, buttons: &[MouseButton]) -> Result<()> {
        self.client.mouse_motion_buttons(dx, dy, buttons)
    }

    /// Send one relative UHID mouse motion report with typed buttons using
    /// non-blocking dispatcher send, then enqueue one checked dispatcher
    /// barrier.
    pub fn try_mouse_motion_buttons(
        &self,
        dx: i32,
        dy: i32,
        buttons: &[MouseButton],
    ) -> Result<()> {
        self.try_mouse_motion(dx, dy, MouseButton::state(buttons))
    }

    /// Send one UHID mouse button-state report.
    pub fn mouse_buttons(&self, buttons: u8) -> Result<()> {
        self.client.mouse_buttons(buttons)
    }

    /// Send one UHID mouse button-state report using non-blocking dispatcher
    /// send, then enqueue one checked dispatcher barrier.
    pub fn try_mouse_buttons(&self, buttons: u8) -> Result<()> {
        if self.command_bound < 2 {
            return Err(Error::SessionLifecycle(TRY_MOUSE_EXCEEDS_COMMAND_BOUND));
        }
        self.client.try_mouse_buttons(buttons)?;
        self.client.try_flush_wait().map(|_| ())
    }

    /// Send one UHID mouse button-state report with typed buttons.
    pub fn mouse_button_state(&self, buttons: &[MouseButton]) -> Result<()> {
        self.client.mouse_button_state(buttons)
    }

    /// Send one UHID mouse button-state report with typed buttons using
    /// non-blocking dispatcher send, then enqueue one checked dispatcher
    /// barrier.
    pub fn try_mouse_button_state(&self, buttons: &[MouseButton]) -> Result<()> {
        self.try_mouse_buttons(MouseButton::state(buttons))
    }

    /// Send one UHID mouse scroll sample.
    pub fn mouse_scroll(&self, hscroll: f32, vscroll: f32) -> Result<()> {
        self.client.mouse_scroll(hscroll, vscroll)
    }

    /// Send one UHID mouse scroll sample using non-blocking dispatcher send,
    /// then enqueue one checked dispatcher barrier.
    pub fn try_mouse_scroll(&self, hscroll: f32, vscroll: f32) -> Result<()> {
        if self.command_bound < 2 {
            return Err(Error::SessionLifecycle(TRY_MOUSE_EXCEEDS_COMMAND_BOUND));
        }
        self.client.try_mouse_scroll(hscroll, vscroll)?;
        self.client.try_flush_wait().map(|_| ())
    }

    /// Send one gamepad button edge.
    pub fn send_button(&self, button: GamepadButton, pressed: bool) -> Result<()> {
        self.client.send_button(button, pressed)
    }

    /// Send one gamepad button edge using non-blocking dispatcher send, then
    /// enqueue one checked dispatcher barrier.
    pub fn try_send_button(&self, button: GamepadButton, pressed: bool) -> Result<()> {
        self.ensure_direct_try_capacity(TRY_GAMEPAD_EXCEEDS_COMMAND_BOUND)?;
        self.client.try_send_button(button, pressed)?;
        self.try_finish_direct_gamepad_command()
    }

    /// Replace all gamepad buttons from a single bitframe.
    pub fn send_buttons(&self, buttons: u32) -> Result<()> {
        self.client.send_buttons(buttons)
    }

    /// Replace all gamepad buttons from a single bitframe using non-blocking
    /// dispatcher send, then enqueue one checked dispatcher barrier.
    pub fn try_send_buttons(&self, buttons: u32) -> Result<()> {
        self.ensure_direct_try_capacity(TRY_GAMEPAD_EXCEEDS_COMMAND_BOUND)?;
        self.client.try_send_buttons(buttons)?;
        self.try_finish_direct_gamepad_command()
    }

    /// Send one normalized gamepad stick/trigger axis update.
    pub fn send_stick(&self, axis: GamepadAxis, value: f32) -> Result<()> {
        self.client.send_stick(axis, value)
    }

    /// Send one normalized gamepad stick/trigger axis update using
    /// non-blocking dispatcher send, then enqueue one checked dispatcher
    /// barrier.
    pub fn try_send_stick(&self, axis: GamepadAxis, value: f32) -> Result<()> {
        self.ensure_direct_try_capacity(TRY_GAMEPAD_EXCEEDS_COMMAND_BOUND)?;
        self.client.try_send_stick(axis, value)?;
        self.try_finish_direct_gamepad_command()
    }

    /// Send one raw gamepad stick/trigger axis update.
    pub fn send_stick_raw(&self, axis: GamepadAxis, value: i16) -> Result<()> {
        self.client.send_stick_raw(axis, value)
    }

    /// Send one raw gamepad stick/trigger axis update using non-blocking
    /// dispatcher send, then enqueue one checked dispatcher barrier.
    pub fn try_send_stick_raw(&self, axis: GamepadAxis, value: i16) -> Result<()> {
        self.ensure_direct_try_capacity(TRY_GAMEPAD_EXCEEDS_COMMAND_BOUND)?;
        self.client.try_send_stick_raw(axis, value)?;
        self.try_finish_direct_gamepad_command()
    }

    /// Send a raw left-stick pair update.
    pub fn send_left_stick_raw(&self, x: i16, y: i16) -> Result<()> {
        self.client.send_left_stick_raw(x, y)
    }

    /// Send a raw left-stick pair update using non-blocking dispatcher send,
    /// then enqueue one checked dispatcher barrier.
    pub fn try_send_left_stick_raw(&self, x: i16, y: i16) -> Result<()> {
        self.ensure_direct_try_capacity(TRY_GAMEPAD_EXCEEDS_COMMAND_BOUND)?;
        self.client.try_send_left_stick_raw(x, y)?;
        self.try_finish_direct_gamepad_command()
    }

    /// Send a raw right-stick pair update.
    pub fn send_right_stick_raw(&self, x: i16, y: i16) -> Result<()> {
        self.client.send_right_stick_raw(x, y)
    }

    /// Send a raw right-stick pair update using non-blocking dispatcher send,
    /// then enqueue one checked dispatcher barrier.
    pub fn try_send_right_stick_raw(&self, x: i16, y: i16) -> Result<()> {
        self.ensure_direct_try_capacity(TRY_GAMEPAD_EXCEEDS_COMMAND_BOUND)?;
        self.client.try_send_right_stick_raw(x, y)?;
        self.try_finish_direct_gamepad_command()
    }

    /// Send raw left/right trigger updates.
    pub fn send_triggers_raw(&self, left: i16, right: i16) -> Result<()> {
        self.client.send_triggers_raw(left, right)
    }

    /// Send raw left/right trigger updates using non-blocking dispatcher send,
    /// then enqueue one checked dispatcher barrier.
    pub fn try_send_triggers_raw(&self, left: i16, right: i16) -> Result<()> {
        self.ensure_direct_try_capacity(TRY_GAMEPAD_EXCEEDS_COMMAND_BOUND)?;
        self.client.try_send_triggers_raw(left, right)?;
        self.try_finish_direct_gamepad_command()
    }

    /// Send raw left/right stick and trigger updates in one command.
    pub fn send_sticks_raw(
        &self,
        left_x: i16,
        left_y: i16,
        right_x: i16,
        right_y: i16,
        left_trigger: i16,
        right_trigger: i16,
    ) -> Result<()> {
        self.client.send_sticks_raw(
            left_x,
            left_y,
            right_x,
            right_y,
            left_trigger,
            right_trigger,
        )
    }

    /// Send raw left/right stick and trigger updates in one command using
    /// non-blocking dispatcher send, then enqueue one checked dispatcher
    /// barrier.
    pub fn try_send_sticks_raw(
        &self,
        left_x: i16,
        left_y: i16,
        right_x: i16,
        right_y: i16,
        left_trigger: i16,
        right_trigger: i16,
    ) -> Result<()> {
        self.ensure_direct_try_capacity(TRY_GAMEPAD_EXCEEDS_COMMAND_BOUND)?;
        self.client.try_send_sticks_raw(
            left_x,
            left_y,
            right_x,
            right_y,
            left_trigger,
            right_trigger,
        )?;
        self.try_finish_direct_gamepad_command()
    }

    /// Send one full gamepad frame with server-side state dedupe.
    pub fn send_frame(&self, frame: GamepadFrameRaw) -> Result<()> {
        self.client.send_frame(frame)
    }

    /// Send one full gamepad frame with server-side state dedupe using
    /// non-blocking dispatcher send, then enqueue one checked dispatcher
    /// barrier.
    pub fn try_send_frame(&self, frame: GamepadFrameRaw) -> Result<()> {
        self.ensure_direct_try_capacity(TRY_GAMEPAD_EXCEEDS_COMMAND_BOUND)?;
        self.client.try_send_frame(frame)?;
        self.try_finish_direct_gamepad_command()
    }

    /// Send one full gamepad frame without state dedupe.
    pub fn send_frame_unchecked(&self, frame: GamepadFrameRaw) -> Result<()> {
        self.client.send_frame_unchecked(frame)
    }

    /// Send one full gamepad frame without state dedupe using non-blocking
    /// dispatcher send, then enqueue one checked dispatcher barrier.
    pub fn try_send_frame_unchecked(&self, frame: GamepadFrameRaw) -> Result<()> {
        self.ensure_direct_try_capacity(TRY_GAMEPAD_EXCEEDS_COMMAND_BOUND)?;
        self.client.try_send_frame_unchecked(frame)?;
        self.try_finish_direct_gamepad_command()
    }

    /// Send full gamepad frames with a fixed stack buffer and state dedupe.
    pub fn send_frame_batch_fixed(
        &self,
        len: usize,
        frames: [GamepadFrameRaw; GAMEPAD_BATCH_FRAMES],
    ) -> Result<()> {
        self.client.send_frame_batch_fixed(len, frames)
    }

    /// Send full gamepad frames with a fixed stack buffer and state dedupe using
    /// non-blocking dispatcher send, then enqueue one checked dispatcher
    /// barrier.
    pub fn try_send_frame_batch_fixed(
        &self,
        len: usize,
        frames: [GamepadFrameRaw; GAMEPAD_BATCH_FRAMES],
    ) -> Result<()> {
        if len == 0 {
            return Ok(());
        }
        self.ensure_direct_try_capacity(TRY_GAMEPAD_EXCEEDS_COMMAND_BOUND)?;
        self.client.try_send_frame_batch_fixed(len, frames)?;
        self.try_finish_direct_gamepad_command()
    }

    /// Send full gamepad frames with a fixed stack buffer and no state dedupe.
    pub fn send_frame_batch_fixed_unchecked(
        &self,
        len: usize,
        frames: [GamepadFrameRaw; GAMEPAD_BATCH_FRAMES],
    ) -> Result<()> {
        self.client.send_frame_batch_fixed_unchecked(len, frames)
    }

    /// Send full gamepad frames with a fixed stack buffer and no state dedupe
    /// using non-blocking dispatcher send, then enqueue one checked dispatcher
    /// barrier.
    pub fn try_send_frame_batch_fixed_unchecked(
        &self,
        len: usize,
        frames: [GamepadFrameRaw; GAMEPAD_BATCH_FRAMES],
    ) -> Result<()> {
        if len == 0 {
            return Ok(());
        }
        self.ensure_direct_try_capacity(TRY_GAMEPAD_EXCEEDS_COMMAND_BOUND)?;
        self.client
            .try_send_frame_batch_fixed_unchecked(len, frames)?;
        self.try_finish_direct_gamepad_command()
    }

    /// Send one packed 15-byte gamepad report.
    pub fn send_frame_packed(&self, frame: [u8; GAMEPAD_FRAME_BYTES]) -> Result<()> {
        self.client.send_frame_packed(frame)
    }

    /// Send one packed 15-byte gamepad report using non-blocking dispatcher
    /// send, then enqueue one checked dispatcher barrier.
    pub fn try_send_frame_packed(&self, frame: [u8; GAMEPAD_FRAME_BYTES]) -> Result<()> {
        self.ensure_direct_try_capacity(TRY_GAMEPAD_EXCEEDS_COMMAND_BOUND)?;
        self.client.try_send_frame_packed(frame)?;
        self.try_finish_direct_gamepad_command()
    }

    /// Send packed 15-byte gamepad frames with a fixed stack buffer.
    pub fn send_frame_packed_batch_fixed(
        &self,
        len: usize,
        frames: [[u8; GAMEPAD_FRAME_BYTES]; GAMEPAD_BATCH_FRAMES],
    ) -> Result<()> {
        self.client.send_frame_packed_batch_fixed(len, frames)
    }

    /// Send packed 15-byte gamepad frames with a fixed stack buffer using
    /// non-blocking dispatcher send, then enqueue one checked dispatcher
    /// barrier.
    pub fn try_send_frame_packed_batch_fixed(
        &self,
        len: usize,
        frames: [[u8; GAMEPAD_FRAME_BYTES]; GAMEPAD_BATCH_FRAMES],
    ) -> Result<()> {
        if len == 0 {
            return Ok(());
        }
        self.ensure_direct_try_capacity(TRY_GAMEPAD_EXCEEDS_COMMAND_BOUND)?;
        self.client.try_send_frame_packed_batch_fixed(len, frames)?;
        self.try_finish_direct_gamepad_command()
    }

    /// Tap one screen coordinate using touch down/up control messages.
    pub fn tap(&self, x: i32, y: i32) -> Result<()> {
        self.client.tap(x, y)
    }

    /// Tap one screen coordinate using non-blocking dispatcher sends, then
    /// enqueue one checked dispatcher barrier.
    pub fn try_tap(&self, x: i32, y: i32) -> Result<()> {
        self.try_tap_pointer(TouchPointerId::finger(0), x, y)
    }

    /// Tap one screen coordinate with a typed scrcpy pointer id.
    pub fn tap_pointer(&self, pointer_id: TouchPointerId, x: i32, y: i32) -> Result<()> {
        self.client.tap_pointer(pointer_id, x, y)
    }

    /// Tap one screen coordinate with a typed scrcpy pointer id using
    /// non-blocking dispatcher sends, then enqueue one checked dispatcher
    /// barrier.
    pub fn try_tap_pointer(&self, pointer_id: TouchPointerId, x: i32, y: i32) -> Result<()> {
        if self.command_bound < 2 {
            return Err(Error::SessionLifecycle(TRY_TAP_EXCEEDS_COMMAND_BOUND));
        }
        self.try_queue_tap_pointer(pointer_id, x, y)?;
        self.client.try_flush_wait().map(|_| ())
    }

    /// Tap one normalized screen point using the tracked screen size.
    pub fn tap_point(&self, point: AgentPoint) -> Result<()> {
        let (x, y) = self.point_to_pixels(point);
        self.tap(x, y)
    }

    /// Tap one normalized screen point using non-blocking dispatcher sends,
    /// then enqueue one checked dispatcher barrier.
    pub fn try_tap_point(&self, point: AgentPoint) -> Result<()> {
        let (x, y) = self.point_to_pixels(point);
        self.try_tap(x, y)
    }

    /// Tap one normalized screen point with a typed scrcpy pointer id.
    pub fn tap_point_pointer(&self, pointer_id: TouchPointerId, point: AgentPoint) -> Result<()> {
        let (x, y) = self.point_to_pixels(point);
        self.tap_pointer(pointer_id, x, y)
    }

    /// Tap one normalized screen point with a typed scrcpy pointer id using
    /// non-blocking dispatcher sends, then enqueue one checked dispatcher
    /// barrier.
    pub fn try_tap_point_pointer(
        &self,
        pointer_id: TouchPointerId,
        point: AgentPoint,
    ) -> Result<()> {
        let (x, y) = self.point_to_pixels(point);
        self.try_tap_pointer(pointer_id, x, y)
    }

    /// Tap the center of one normalized screen rectangle.
    pub fn tap_rect(&self, rect: AgentRect) -> Result<()> {
        self.tap_point(rect.center())
    }

    /// Tap the center of one normalized screen rectangle using non-blocking
    /// dispatcher sends, then enqueue one checked dispatcher barrier.
    pub fn try_tap_rect(&self, rect: AgentRect) -> Result<()> {
        self.try_tap_point(rect.center())
    }

    /// Tap a relative point inside one normalized screen rectangle.
    ///
    /// `x_bp` and `y_bp` are basis points from the rectangle's top-left edge
    /// to bottom-right edge.
    pub fn tap_rect_at(&self, rect: AgentRect, x_bp: u16, y_bp: u16) -> Result<()> {
        self.tap_point(rect.try_point_at_basis_points(x_bp, y_bp)?)
    }

    /// Tap a relative point inside one normalized screen rectangle using
    /// non-blocking dispatcher sends, then enqueue one checked dispatcher
    /// barrier.
    pub fn try_tap_rect_at(&self, rect: AgentRect, x_bp: u16, y_bp: u16) -> Result<()> {
        self.try_tap_point(rect.try_point_at_basis_points(x_bp, y_bp)?)
    }

    /// Tap the center of one normalized screen rectangle with a typed scrcpy
    /// pointer id.
    pub fn tap_rect_pointer(&self, pointer_id: TouchPointerId, rect: AgentRect) -> Result<()> {
        self.tap_point_pointer(pointer_id, rect.center())
    }

    /// Tap the center of one normalized screen rectangle with a typed scrcpy
    /// pointer id using non-blocking dispatcher sends, then enqueue one checked
    /// dispatcher barrier.
    pub fn try_tap_rect_pointer(&self, pointer_id: TouchPointerId, rect: AgentRect) -> Result<()> {
        self.try_tap_point_pointer(pointer_id, rect.center())
    }

    /// Tap a relative point inside one normalized screen rectangle with a typed
    /// scrcpy pointer id.
    pub fn tap_rect_at_pointer(
        &self,
        pointer_id: TouchPointerId,
        rect: AgentRect,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<()> {
        self.tap_point_pointer(pointer_id, rect.try_point_at_basis_points(x_bp, y_bp)?)
    }

    /// Tap a relative point inside one normalized screen rectangle with a typed
    /// scrcpy pointer id using non-blocking dispatcher sends, then enqueue one
    /// checked dispatcher barrier.
    pub fn try_tap_rect_at_pointer(
        &self,
        pointer_id: TouchPointerId,
        rect: AgentRect,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<()> {
        self.try_tap_point_pointer(pointer_id, rect.try_point_at_basis_points(x_bp, y_bp)?)
    }

    /// Two quick taps at one coordinate.
    pub fn double_tap(&self, x: i32, y: i32) -> Result<()> {
        self.client.double_tap(x, y)
    }

    /// Two quick taps at one coordinate using non-blocking dispatcher sends,
    /// then enqueue one checked dispatcher barrier.
    pub fn try_double_tap(&self, x: i32, y: i32) -> Result<()> {
        self.try_double_tap_pointer(TouchPointerId::finger(0), x, y)
    }

    /// Two quick taps at one coordinate with a typed scrcpy pointer id.
    pub fn double_tap_pointer(&self, pointer_id: TouchPointerId, x: i32, y: i32) -> Result<()> {
        self.client.double_tap_pointer(pointer_id, x, y)
    }

    /// Two quick taps at one coordinate with a typed scrcpy pointer id using
    /// non-blocking dispatcher sends, then enqueue one checked dispatcher
    /// barrier.
    pub fn try_double_tap_pointer(&self, pointer_id: TouchPointerId, x: i32, y: i32) -> Result<()> {
        if self.command_bound < 2 {
            return Err(Error::SessionLifecycle(
                TRY_DOUBLE_TAP_EXCEEDS_COMMAND_BOUND,
            ));
        }
        self.try_queue_double_tap_pointer(pointer_id, x, y)?;
        self.client.try_flush_wait().map(|_| ())
    }

    /// Two quick taps at one normalized screen point.
    pub fn double_tap_point(&self, point: AgentPoint) -> Result<()> {
        let (x, y) = self.point_to_pixels(point);
        self.double_tap(x, y)
    }

    /// Two quick taps at one normalized screen point using non-blocking
    /// dispatcher sends, then enqueue one checked dispatcher barrier.
    pub fn try_double_tap_point(&self, point: AgentPoint) -> Result<()> {
        let (x, y) = self.point_to_pixels(point);
        self.try_double_tap(x, y)
    }

    /// Two quick taps at one normalized screen point with a typed scrcpy pointer
    /// id.
    pub fn double_tap_point_pointer(
        &self,
        pointer_id: TouchPointerId,
        point: AgentPoint,
    ) -> Result<()> {
        let (x, y) = self.point_to_pixels(point);
        self.double_tap_pointer(pointer_id, x, y)
    }

    /// Two quick taps at one normalized screen point with a typed scrcpy pointer
    /// id using non-blocking dispatcher sends, then enqueue one checked
    /// dispatcher barrier.
    pub fn try_double_tap_point_pointer(
        &self,
        pointer_id: TouchPointerId,
        point: AgentPoint,
    ) -> Result<()> {
        let (x, y) = self.point_to_pixels(point);
        self.try_double_tap_pointer(pointer_id, x, y)
    }

    /// Two quick taps at the center of one normalized screen rectangle.
    pub fn double_tap_rect(&self, rect: AgentRect) -> Result<()> {
        self.double_tap_point(rect.center())
    }

    /// Two quick taps at the center of one normalized screen rectangle using
    /// non-blocking dispatcher sends, then enqueue one checked dispatcher
    /// barrier.
    pub fn try_double_tap_rect(&self, rect: AgentRect) -> Result<()> {
        self.try_double_tap_point(rect.center())
    }

    /// Two quick taps at a relative point inside one normalized screen
    /// rectangle.
    pub fn double_tap_rect_at(&self, rect: AgentRect, x_bp: u16, y_bp: u16) -> Result<()> {
        self.double_tap_point(rect.try_point_at_basis_points(x_bp, y_bp)?)
    }

    /// Two quick taps at a relative point inside one normalized screen
    /// rectangle using non-blocking dispatcher sends, then enqueue one checked
    /// dispatcher barrier.
    pub fn try_double_tap_rect_at(&self, rect: AgentRect, x_bp: u16, y_bp: u16) -> Result<()> {
        self.try_double_tap_point(rect.try_point_at_basis_points(x_bp, y_bp)?)
    }

    /// Two quick taps at the center of one normalized screen rectangle with a
    /// typed scrcpy pointer id.
    pub fn double_tap_rect_pointer(
        &self,
        pointer_id: TouchPointerId,
        rect: AgentRect,
    ) -> Result<()> {
        self.double_tap_point_pointer(pointer_id, rect.center())
    }

    /// Two quick taps at the center of one normalized screen rectangle with a
    /// typed scrcpy pointer id using non-blocking dispatcher sends, then enqueue
    /// one checked dispatcher barrier.
    pub fn try_double_tap_rect_pointer(
        &self,
        pointer_id: TouchPointerId,
        rect: AgentRect,
    ) -> Result<()> {
        self.try_double_tap_point_pointer(pointer_id, rect.center())
    }

    /// Two quick taps at a relative point inside one normalized screen
    /// rectangle with a typed scrcpy pointer id.
    pub fn double_tap_rect_at_pointer(
        &self,
        pointer_id: TouchPointerId,
        rect: AgentRect,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<()> {
        self.double_tap_point_pointer(pointer_id, rect.try_point_at_basis_points(x_bp, y_bp)?)
    }

    /// Two quick taps at a relative point inside one normalized screen
    /// rectangle with a typed scrcpy pointer id using non-blocking dispatcher
    /// sends, then enqueue one checked dispatcher barrier.
    pub fn try_double_tap_rect_at_pointer(
        &self,
        pointer_id: TouchPointerId,
        rect: AgentRect,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<()> {
        self.try_double_tap_point_pointer(pointer_id, rect.try_point_at_basis_points(x_bp, y_bp)?)
    }

    /// Press, hold for `dur`, then release.
    pub fn long_press(&self, x: i32, y: i32, dur: Duration) -> Result<()> {
        self.client.long_press(x, y, dur)
    }

    /// Press, hold, then release with a typed scrcpy pointer id.
    pub fn long_press_pointer(
        &self,
        pointer_id: TouchPointerId,
        x: i32,
        y: i32,
        dur: Duration,
    ) -> Result<()> {
        self.client.long_press_pointer(pointer_id, x, y, dur)
    }

    /// Press, hold, then release at one normalized screen point.
    pub fn long_press_point(&self, point: AgentPoint, dur: Duration) -> Result<()> {
        let (x, y) = self.point_to_pixels(point);
        self.long_press(x, y, dur)
    }

    /// Press, hold, then release at one normalized screen point with a typed
    /// scrcpy pointer id.
    pub fn long_press_point_pointer(
        &self,
        pointer_id: TouchPointerId,
        point: AgentPoint,
        dur: Duration,
    ) -> Result<()> {
        let (x, y) = self.point_to_pixels(point);
        self.long_press_pointer(pointer_id, x, y, dur)
    }

    /// Press and hold the center of one normalized screen rectangle.
    pub fn long_press_rect(&self, rect: AgentRect, dur: Duration) -> Result<()> {
        self.long_press_point(rect.center(), dur)
    }

    /// Press and hold a relative point inside one normalized screen rectangle.
    pub fn long_press_rect_at(
        &self,
        rect: AgentRect,
        x_bp: u16,
        y_bp: u16,
        dur: Duration,
    ) -> Result<()> {
        self.long_press_point(rect.try_point_at_basis_points(x_bp, y_bp)?, dur)
    }

    /// Press and hold the center of one normalized screen rectangle with a typed
    /// scrcpy pointer id.
    pub fn long_press_rect_pointer(
        &self,
        pointer_id: TouchPointerId,
        rect: AgentRect,
        dur: Duration,
    ) -> Result<()> {
        self.long_press_point_pointer(pointer_id, rect.center(), dur)
    }

    /// Press and hold a relative point inside one normalized screen rectangle
    /// with a typed scrcpy pointer id.
    pub fn long_press_rect_at_pointer(
        &self,
        pointer_id: TouchPointerId,
        rect: AgentRect,
        x_bp: u16,
        y_bp: u16,
        dur: Duration,
    ) -> Result<()> {
        self.long_press_point_pointer(pointer_id, rect.try_point_at_basis_points(x_bp, y_bp)?, dur)
    }

    /// Swipe from one coordinate to another in `steps` intermediate samples.
    pub fn swipe(&self, from: (i32, i32), to: (i32, i32), steps: usize) -> Result<()> {
        self.client.swipe(from, to, steps)
    }

    /// Swipe between two coordinates with a typed scrcpy pointer id.
    pub fn swipe_pointer(
        &self,
        pointer_id: TouchPointerId,
        from: (i32, i32),
        to: (i32, i32),
        steps: usize,
    ) -> Result<()> {
        self.client.swipe_pointer(pointer_id, from, to, steps)
    }

    /// Swipe between two normalized screen points.
    pub fn swipe_points(&self, from: AgentPoint, to: AgentPoint, steps: usize) -> Result<()> {
        self.swipe(self.point_to_pixels(from), self.point_to_pixels(to), steps)
    }

    /// Swipe between two normalized screen points with a typed scrcpy pointer
    /// id.
    pub fn swipe_points_pointer(
        &self,
        pointer_id: TouchPointerId,
        from: AgentPoint,
        to: AgentPoint,
        steps: usize,
    ) -> Result<()> {
        self.swipe_pointer(
            pointer_id,
            self.point_to_pixels(from),
            self.point_to_pixels(to),
            steps,
        )
    }

    /// Swipe between two relative points inside one normalized screen rectangle.
    pub fn swipe_rect(
        &self,
        rect: AgentRect,
        from: (u16, u16),
        to: (u16, u16),
        steps: usize,
    ) -> Result<()> {
        self.swipe_points(
            rect.try_point_at_basis_points(from.0, from.1)?,
            rect.try_point_at_basis_points(to.0, to.1)?,
            steps,
        )
    }

    /// Swipe between two relative points inside one normalized screen rectangle
    /// with a typed scrcpy pointer id.
    pub fn swipe_rect_pointer(
        &self,
        pointer_id: TouchPointerId,
        rect: AgentRect,
        from: (u16, u16),
        to: (u16, u16),
        steps: usize,
    ) -> Result<()> {
        self.swipe_points_pointer(
            pointer_id,
            rect.try_point_at_basis_points(from.0, from.1)?,
            rect.try_point_at_basis_points(to.0, to.1)?,
            steps,
        )
    }

    /// Two-pointer pinch/spread using raw pixel endpoints.
    ///
    /// Pointer ids `0` and `1` are pressed, moved in alternating samples, and
    /// released. Moving endpoints closer performs pinch-in; farther performs
    /// spread/zoom-out.
    pub fn pinch(
        &self,
        first_from: (i32, i32),
        first_to: (i32, i32),
        second_from: (i32, i32),
        second_to: (i32, i32),
        steps: usize,
    ) -> Result<()> {
        self.queue_pinch(first_from, first_to, second_from, second_to, steps)
    }

    /// Two-pointer pinch/spread using normalized screen points.
    pub fn pinch_points(
        &self,
        first_from: AgentPoint,
        first_to: AgentPoint,
        second_from: AgentPoint,
        second_to: AgentPoint,
        steps: usize,
    ) -> Result<()> {
        self.pinch(
            self.point_to_pixels(first_from),
            self.point_to_pixels(first_to),
            self.point_to_pixels(second_from),
            self.point_to_pixels(second_to),
            steps,
        )
    }

    /// Absolute scroll with no pressed mouse buttons.
    pub fn scroll(&self, x: i32, y: i32, hscroll: f32, vscroll: f32) -> Result<()> {
        self.client.scroll(x, y, hscroll, vscroll)
    }

    /// Absolute scroll using non-blocking dispatcher send, then enqueue one
    /// checked dispatcher barrier.
    pub fn try_scroll(&self, x: i32, y: i32, hscroll: f32, vscroll: f32) -> Result<()> {
        self.try_scroll_with_buttons(x, y, hscroll, vscroll, 0)
    }

    /// Absolute scroll at one normalized screen point.
    pub fn scroll_point(&self, point: AgentPoint, hscroll: f32, vscroll: f32) -> Result<()> {
        let (x, y) = self.point_to_pixels(point);
        self.scroll(x, y, hscroll, vscroll)
    }

    /// Absolute scroll at one normalized screen point using non-blocking
    /// dispatcher send, then enqueue one checked dispatcher barrier.
    pub fn try_scroll_point(&self, point: AgentPoint, hscroll: f32, vscroll: f32) -> Result<()> {
        let (x, y) = self.point_to_pixels(point);
        self.try_scroll(x, y, hscroll, vscroll)
    }

    /// Absolute scroll at the center of one normalized screen rectangle.
    pub fn scroll_rect(&self, rect: AgentRect, hscroll: f32, vscroll: f32) -> Result<()> {
        self.scroll_point(rect.center(), hscroll, vscroll)
    }

    /// Absolute scroll at the center of one normalized screen rectangle using
    /// non-blocking dispatcher send, then enqueue one checked dispatcher
    /// barrier.
    pub fn try_scroll_rect(&self, rect: AgentRect, hscroll: f32, vscroll: f32) -> Result<()> {
        self.try_scroll_point(rect.center(), hscroll, vscroll)
    }

    /// Absolute scroll at a relative point inside one normalized screen
    /// rectangle.
    pub fn scroll_rect_at(
        &self,
        rect: AgentRect,
        x_bp: u16,
        y_bp: u16,
        hscroll: f32,
        vscroll: f32,
    ) -> Result<()> {
        self.scroll_point(
            rect.try_point_at_basis_points(x_bp, y_bp)?,
            hscroll,
            vscroll,
        )
    }

    /// Absolute scroll at a relative point inside one normalized screen
    /// rectangle using non-blocking dispatcher send, then enqueue one checked
    /// dispatcher barrier.
    pub fn try_scroll_rect_at(
        &self,
        rect: AgentRect,
        x_bp: u16,
        y_bp: u16,
        hscroll: f32,
        vscroll: f32,
    ) -> Result<()> {
        self.try_scroll_point(
            rect.try_point_at_basis_points(x_bp, y_bp)?,
            hscroll,
            vscroll,
        )
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
        self.client
            .scroll_with_buttons(x, y, hscroll, vscroll, buttons)
    }

    /// Absolute scroll with an explicit Android mouse-button bitmask using
    /// non-blocking dispatcher send, then enqueue one checked dispatcher
    /// barrier.
    pub fn try_scroll_with_buttons(
        &self,
        x: i32,
        y: i32,
        hscroll: f32,
        vscroll: f32,
        buttons: u32,
    ) -> Result<()> {
        if self.command_bound < 2 {
            return Err(Error::SessionLifecycle(TRY_SCROLL_EXCEEDS_COMMAND_BOUND));
        }
        self.client
            .try_scroll_with_buttons(x, y, hscroll, vscroll, buttons)?;
        self.client.try_flush_wait().map(|_| ())
    }

    /// Absolute scroll at one normalized screen point with a button bitmask.
    pub fn scroll_point_with_buttons(
        &self,
        point: AgentPoint,
        hscroll: f32,
        vscroll: f32,
        buttons: u32,
    ) -> Result<()> {
        let (x, y) = self.point_to_pixels(point);
        self.scroll_with_buttons(x, y, hscroll, vscroll, buttons)
    }

    /// Absolute scroll at one normalized screen point with a button bitmask
    /// using non-blocking dispatcher send, then enqueue one checked dispatcher
    /// barrier.
    pub fn try_scroll_point_with_buttons(
        &self,
        point: AgentPoint,
        hscroll: f32,
        vscroll: f32,
        buttons: u32,
    ) -> Result<()> {
        let (x, y) = self.point_to_pixels(point);
        self.try_scroll_with_buttons(x, y, hscroll, vscroll, buttons)
    }

    /// Absolute scroll at a normalized rectangle center with a button bitmask.
    pub fn scroll_rect_with_buttons(
        &self,
        rect: AgentRect,
        hscroll: f32,
        vscroll: f32,
        buttons: u32,
    ) -> Result<()> {
        self.scroll_point_with_buttons(rect.center(), hscroll, vscroll, buttons)
    }

    /// Absolute scroll at a normalized rectangle center with a button bitmask
    /// using non-blocking dispatcher send, then enqueue one checked dispatcher
    /// barrier.
    pub fn try_scroll_rect_with_buttons(
        &self,
        rect: AgentRect,
        hscroll: f32,
        vscroll: f32,
        buttons: u32,
    ) -> Result<()> {
        self.try_scroll_point_with_buttons(rect.center(), hscroll, vscroll, buttons)
    }

    /// Absolute scroll at a relative point inside a normalized rectangle with a
    /// button bitmask.
    pub fn scroll_rect_at_with_buttons(
        &self,
        rect: AgentRect,
        x_bp: u16,
        y_bp: u16,
        hscroll: f32,
        vscroll: f32,
        buttons: u32,
    ) -> Result<()> {
        self.scroll_point_with_buttons(
            rect.try_point_at_basis_points(x_bp, y_bp)?,
            hscroll,
            vscroll,
            buttons,
        )
    }

    /// Absolute scroll at a relative point inside a normalized rectangle with a
    /// button bitmask using non-blocking dispatcher send, then enqueue one
    /// checked dispatcher barrier.
    pub fn try_scroll_rect_at_with_buttons(
        &self,
        rect: AgentRect,
        x_bp: u16,
        y_bp: u16,
        hscroll: f32,
        vscroll: f32,
        buttons: u32,
    ) -> Result<()> {
        self.try_scroll_point_with_buttons(
            rect.try_point_at_basis_points(x_bp, y_bp)?,
            hscroll,
            vscroll,
            buttons,
        )
    }

    /// Cancel one active touch pointer.
    pub fn cancel_touch(&self, pointer_id: u64) -> Result<()> {
        self.client.cancel_touch(pointer_id)
    }

    /// Cancel one active typed scrcpy touch pointer.
    pub fn cancel_touch_pointer(&self, pointer_id: TouchPointerId) -> Result<()> {
        self.client.cancel_touch_pointer(pointer_id)
    }

    /// Three-finger swipe down using the current agent-local screen size.
    pub fn three_finger_screenshot(&self) -> Result<()> {
        let (width, height) = self.screen_size();
        self.client.three_finger_screenshot(width, height)
    }

    /// Launch an app by Android package name.
    pub fn launch_app(&self, name: impl Into<String>) -> Result<()> {
        self.client.launch_app(name)
    }

    /// Launch an app by Android package name using non-blocking dispatcher
    /// send, then enqueue one checked dispatcher barrier.
    pub fn try_launch_app(&self, name: impl Into<String>) -> Result<()> {
        let name = name.into();
        if name.len() > 255 {
            return Err(Error::SessionLifecycle(LAUNCH_APP_NAME_TOO_LONG));
        }
        self.try_run_direct_command(
            HidCommand::LaunchApp { name },
            TRY_CONTROL_EXCEEDS_COMMAND_BOUND,
        )
    }

    /// Turn the display on/off through scrcpy control.
    pub fn set_screen_power(&self, on: bool) -> Result<()> {
        self.client.set_screen_power(on)
    }

    /// Turn the display on/off through scrcpy control using non-blocking
    /// dispatcher send, then enqueue one checked dispatcher barrier.
    pub fn try_set_screen_power(&self, on: bool) -> Result<()> {
        self.try_run_direct_command(
            HidCommand::SetScreenPower { on },
            TRY_CONTROL_EXCEEDS_COMMAND_BOUND,
        )
    }

    /// Expand the notification panel.
    pub fn show_notifications(&self) -> Result<()> {
        self.client.show_notifications()
    }

    /// Expand the notification panel using non-blocking dispatcher send, then
    /// enqueue one checked dispatcher barrier.
    pub fn try_show_notifications(&self) -> Result<()> {
        self.try_run_direct_command(
            HidCommand::ShowNotifications,
            TRY_CONTROL_EXCEEDS_COMMAND_BOUND,
        )
    }

    /// Expand the quick-settings panel.
    pub fn show_quick_settings(&self) -> Result<()> {
        self.client.show_quick_settings()
    }

    /// Expand the quick-settings panel using non-blocking dispatcher send, then
    /// enqueue one checked dispatcher barrier.
    pub fn try_show_quick_settings(&self) -> Result<()> {
        self.try_run_direct_command(
            HidCommand::ShowQuickSettings,
            TRY_CONTROL_EXCEEDS_COMMAND_BOUND,
        )
    }

    /// Collapse notification and quick-settings panels.
    pub fn collapse_panels(&self) -> Result<()> {
        self.client.collapse_panels()
    }

    /// Collapse notification and quick-settings panels using non-blocking
    /// dispatcher send, then enqueue one checked dispatcher barrier.
    pub fn try_collapse_panels(&self) -> Result<()> {
        self.try_run_direct_command(
            HidCommand::CollapsePanels,
            TRY_CONTROL_EXCEEDS_COMMAND_BOUND,
        )
    }

    /// Rotate the device display.
    pub fn rotate_device(&self) -> Result<()> {
        self.client.rotate_device()
    }

    /// Rotate the device display using non-blocking dispatcher send, then
    /// enqueue one checked dispatcher barrier.
    pub fn try_rotate_device(&self) -> Result<()> {
        self.try_run_direct_command(HidCommand::RotateDevice, TRY_CONTROL_EXCEEDS_COMMAND_BOUND)
    }

    /// Ask the device/server to resize its display.
    ///
    /// This emits scrcpy `RESIZE_DISPLAY`. Use [`Self::set_screen_size`] when
    /// you only need to update local touch-coordinate metadata.
    pub fn resize_display(&self, width: u16, height: u16) -> Result<()> {
        self.client.resize_display(width, height)
    }

    /// Ask the device/server to resize its display using non-blocking
    /// dispatcher send, then enqueue one checked dispatcher barrier.
    pub fn try_resize_display(&self, width: u16, height: u16) -> Result<()> {
        self.try_run_direct_command(
            HidCommand::ResizeDisplay { width, height },
            TRY_CONTROL_EXCEEDS_COMMAND_BOUND,
        )
    }

    /// Toggle the camera torch.
    pub fn set_torch(&self, on: bool) -> Result<()> {
        self.client.set_torch(on)
    }

    /// Toggle the camera torch using non-blocking dispatcher send, then enqueue
    /// one checked dispatcher barrier.
    pub fn try_set_torch(&self, on: bool) -> Result<()> {
        self.try_run_direct_command(
            HidCommand::SetTorch { on },
            TRY_CONTROL_EXCEEDS_COMMAND_BOUND,
        )
    }

    /// Camera zoom in.
    pub fn camera_zoom_in(&self) -> Result<()> {
        self.client.camera_zoom_in()
    }

    /// Camera zoom in using non-blocking dispatcher send, then enqueue one
    /// checked dispatcher barrier.
    pub fn try_camera_zoom_in(&self) -> Result<()> {
        self.try_run_direct_command(HidCommand::CameraZoomIn, TRY_CONTROL_EXCEEDS_COMMAND_BOUND)
    }

    /// Camera zoom out.
    pub fn camera_zoom_out(&self) -> Result<()> {
        self.client.camera_zoom_out()
    }

    /// Camera zoom out using non-blocking dispatcher send, then enqueue one
    /// checked dispatcher barrier.
    pub fn try_camera_zoom_out(&self) -> Result<()> {
        self.try_run_direct_command(HidCommand::CameraZoomOut, TRY_CONTROL_EXCEEDS_COMMAND_BOUND)
    }

    /// Open the physical-keyboard settings activity.
    pub fn open_hard_keyboard_settings(&self) -> Result<()> {
        self.client.open_hard_keyboard_settings()
    }

    /// Open the physical-keyboard settings activity using non-blocking
    /// dispatcher send, then enqueue one checked dispatcher barrier.
    pub fn try_open_hard_keyboard_settings(&self) -> Result<()> {
        self.try_run_direct_command(
            HidCommand::OpenHardKeyboardSettings,
            TRY_CONTROL_EXCEEDS_COMMAND_BOUND,
        )
    }

    /// Reset the scrcpy video stream.
    pub fn reset_video(&self) -> Result<()> {
        self.client.reset_video()
    }

    /// Reset the scrcpy video stream using non-blocking dispatcher send, then
    /// enqueue one checked dispatcher barrier.
    pub fn try_reset_video(&self) -> Result<()> {
        self.try_run_direct_command(HidCommand::ResetVideo, TRY_CONTROL_EXCEEDS_COMMAND_BOUND)
    }

    /// Configure the AI summary pipeline on an AI-enabled scrcpy server.
    pub fn configure_ai(&self, flags: u8, sample_interval_ms: u16, feature_dim: u16) -> Result<()> {
        self.client
            .configure_ai(flags, sample_interval_ms, feature_dim)
    }

    /// Configure the AI summary pipeline using non-blocking dispatcher send,
    /// then enqueue one checked dispatcher barrier.
    pub fn try_configure_ai(
        &self,
        flags: u8,
        sample_interval_ms: u16,
        feature_dim: u16,
    ) -> Result<()> {
        self.try_run_direct_command(
            HidCommand::AiConfig {
                flags,
                sample_interval_ms,
                feature_dim,
            },
            TRY_AI_EXCEEDS_COMMAND_BOUND,
        )
    }

    /// Query the AI extension for summaries or stats since a timestamp.
    pub fn query_ai(&self, since_timestamp_ms: u64) -> Result<()> {
        self.client.query_ai(since_timestamp_ms)
    }

    /// Query the AI extension using non-blocking dispatcher send, then enqueue
    /// one checked dispatcher barrier.
    pub fn try_query_ai(&self, since_timestamp_ms: u64) -> Result<()> {
        self.try_run_direct_command(
            HidCommand::AiQuery { since_timestamp_ms },
            TRY_AI_EXCEEDS_COMMAND_BOUND,
        )
    }

    /// Query the AI extension and wait for the next AI stats envelope.
    pub fn query_ai_and_wait_stats(&mut self, since_timestamp_ms: u64) -> Result<AiStats> {
        self.query_ai(since_timestamp_ms)?;
        self.flush()?;
        self.wait_for_ai_stats()
    }

    /// Run an action plan, query the AI extension, and wait for the next AI
    /// stats envelope.
    ///
    /// The action plan and AI_QUERY command share one checked dispatcher
    /// boundary before the stats wait.
    pub fn run_actions_and_query_ai_and_wait_stats(
        &mut self,
        actions: &[AgentAction],
        since_timestamp_ms: u64,
    ) -> Result<AiStats> {
        self.queue_actions(actions)?;
        self.query_ai(since_timestamp_ms)?;
        self.flush()?;
        self.wait_for_ai_stats()
    }

    /// Pause the AI summary pipeline on an AI-enabled scrcpy server.
    pub fn pause_ai(&self) -> Result<()> {
        self.client.pause_ai()
    }

    /// Pause the AI summary pipeline using non-blocking dispatcher send, then
    /// enqueue one checked dispatcher barrier.
    pub fn try_pause_ai(&self) -> Result<()> {
        self.try_run_direct_command(HidCommand::AiPause, TRY_AI_EXCEEDS_COMMAND_BOUND)
    }

    /// Set the device clipboard without waiting for an ACK.
    pub fn set_clipboard(&self, text: impl Into<String>, paste: bool) -> Result<()> {
        self.client.set_clipboard(text, paste)
    }

    /// Set the device clipboard without waiting for an ACK using non-blocking
    /// dispatcher send, then enqueue one checked dispatcher barrier.
    pub fn try_set_clipboard(&self, text: impl Into<String>, paste: bool) -> Result<()> {
        self.try_run_direct_command(
            HidCommand::SetClipboard {
                text: text.into(),
                paste,
            },
            TRY_CLIPBOARD_EXCEEDS_COMMAND_BOUND,
        )
    }

    /// Set the device clipboard with a specific sequence number.
    pub fn set_clipboard_sequenced(
        &self,
        sequence: u64,
        text: impl Into<String>,
        paste: bool,
    ) -> Result<()> {
        self.client.set_clipboard_sequenced(sequence, text, paste)
    }

    /// Set the device clipboard with a specific sequence number using
    /// non-blocking dispatcher send, then enqueue one checked dispatcher
    /// barrier.
    pub fn try_set_clipboard_sequenced(
        &self,
        sequence: u64,
        text: impl Into<String>,
        paste: bool,
    ) -> Result<()> {
        self.try_run_direct_command(
            HidCommand::SetClipboardSequenced {
                sequence,
                text: text.into(),
                paste,
            },
            TRY_CLIPBOARD_EXCEEDS_COMMAND_BOUND,
        )
    }

    /// Request the current device clipboard. `copy_key` follows scrcpy:
    /// `0 = none`, `1 = copy`, `2 = cut`.
    pub fn request_clipboard(&self, copy_key: u8) -> Result<()> {
        self.client.request_clipboard(copy_key)
    }

    /// Request the current device clipboard using non-blocking dispatcher send,
    /// then enqueue one checked dispatcher barrier.
    pub fn try_request_clipboard(&self, copy_key: u8) -> Result<()> {
        self.try_run_direct_command(
            HidCommand::GetClipboard { copy_key },
            TRY_CLIPBOARD_EXCEEDS_COMMAND_BOUND,
        )
    }

    /// Request the current device clipboard with a typed scrcpy copy-key.
    pub fn request_clipboard_key(&self, copy_key: ClipboardCopyKey) -> Result<()> {
        self.client.request_clipboard_key(copy_key)
    }

    /// Request the current device clipboard with a typed scrcpy copy-key using
    /// non-blocking dispatcher send, then enqueue one checked dispatcher
    /// barrier.
    pub fn try_request_clipboard_key(&self, copy_key: ClipboardCopyKey) -> Result<()> {
        self.try_request_clipboard(copy_key.value())
    }

    /// Queue a typed agent action plan without waiting for a dispatcher
    /// acknowledgement.
    ///
    /// Use this when the caller already owns a wider flush/close boundary.
    /// For most agent workflows, prefer [`Self::run_actions`].
    pub fn queue_actions(&self, actions: &[AgentAction]) -> Result<()> {
        AgentAction::validate_plan_structure(actions)?;
        let mut touch_batch = self.client.touch_frame_batcher();
        let mut key_batch = self.client.keyboard_frame_batcher();
        let mut android_key_batch = self.client.android_key_frame_batcher();
        let mut mouse_batch = self.client.mouse_frame_batcher();
        let mut scroll_batch = self.client.scroll_frame_batcher();
        let mut gamepad_batch = PlanGamepadBatcher::new(&self.client);
        for action in actions {
            self.queue_planned_action(
                action,
                (
                    &mut touch_batch,
                    &mut key_batch,
                    &mut android_key_batch,
                    &mut mouse_batch,
                    &mut scroll_batch,
                    &mut gamepad_batch,
                ),
            )?;
        }
        touch_batch.flush()?;
        key_batch.flush()?;
        android_key_batch.flush()?;
        mouse_batch.flush()?;
        scroll_batch.flush()?;
        gamepad_batch.flush()
    }

    /// Queue a typed agent action plan using non-blocking dispatcher sends.
    ///
    /// This is the back-pressure aware variant for high-contention agent
    /// schedulers. It returns `SessionLifecycle("channel full ...")` when the
    /// bounded command queue is full. Timing-dependent actions (`Wait` and
    /// `LongPress`) require [`Self::queue_actions`] or [`Self::run_actions`].
    pub fn try_queue_actions(&self, actions: &[AgentAction]) -> Result<()> {
        AgentAction::validate_try_queue_plan(actions)?;
        let mut touch_batch = self.client.touch_frame_batcher();
        let mut key_batch = self.client.keyboard_frame_batcher();
        let mut android_key_batch = self.client.android_key_frame_batcher();
        let mut mouse_batch = self.client.mouse_frame_batcher();
        let mut scroll_batch = self.client.scroll_frame_batcher();
        let mut gamepad_batch = PlanGamepadBatcher::new(&self.client);
        for action in actions {
            self.try_queue_planned_action(
                action,
                (
                    &mut touch_batch,
                    &mut key_batch,
                    &mut android_key_batch,
                    &mut mouse_batch,
                    &mut scroll_batch,
                    &mut gamepad_batch,
                ),
            )?;
        }
        touch_batch.try_flush()?;
        key_batch.try_flush()?;
        android_key_batch.try_flush()?;
        mouse_batch.try_flush()?;
        scroll_batch.try_flush()?;
        gamepad_batch.try_flush()
    }

    /// Queue the longest non-blocking prefix of a typed agent action plan.
    ///
    /// This is useful for schedulers that accept mixed plans: the returned
    /// count is the number of leading actions sent through
    /// [`Self::try_queue_actions`]. Only a blocking timing/barrier requirement
    /// can produce a short successful prefix; malformed fixed-buffer/chord or
    /// rect-anchor metadata before that timing barrier is rejected before any
    /// prefix action is dispatched. Runtime back-pressure can still return an
    /// error while sending the prefix.
    pub fn try_queue_actions_prefix(&self, actions: &[AgentAction]) -> Result<usize> {
        let len = AgentAction::blocking_timing_prefix_len(actions);
        self.try_queue_actions(&actions[..len])?;
        Ok(len)
    }

    /// Queue the longest non-blocking prefix of a typed agent action plan, then
    /// enqueue one checked dispatcher barrier without blocking on a full command
    /// queue.
    ///
    /// This is the checked-barrier companion to
    /// [`Self::try_queue_actions_prefix`]. It validates and dispatches only the
    /// leading non-blocking prefix, leaves the blocking suffix for a scheduler
    /// handoff, and still reports dispatcher-side command errors once the final
    /// barrier is accepted. The accepted prefix plus barrier is preflighted
    /// against this session's configured command bound before dispatch.
    pub fn try_run_actions_prefix(&self, actions: &[AgentAction]) -> Result<usize> {
        let len = AgentAction::blocking_timing_prefix_len(actions);
        self.try_run_actions(&actions[..len])?;
        Ok(len)
    }

    /// Queue the longest statically safe non-blocking prefix that fits an
    /// estimated dispatcher-command budget.
    ///
    /// This combines full-plan structural preflight,
    /// [`AgentAction::bounded_try_queue_prefix`], and [`Self::try_queue_actions`].
    /// A command-bound or blocking-timing stop queues the accepted prefix and
    /// returns the stop metadata. Malformed metadata anywhere in the supplied
    /// plan returns an error without dispatching any accepted prefix.
    pub fn try_queue_actions_bounded_prefix(
        &self,
        actions: &[AgentAction],
        command_bound: usize,
    ) -> Result<AgentPlanBoundedPrefix> {
        AgentAction::validate_plan_structure(actions)?;
        let prefix = AgentAction::bounded_try_queue_prefix(actions, command_bound);
        if let AgentPlanBoundedPrefixStop::TryQueueError { error, .. } = prefix.stop {
            return Err(Error::SessionLifecycle(error));
        }
        self.try_queue_actions(&actions[..prefix.accepted_actions])?;
        Ok(prefix)
    }

    /// Queue a bounded non-blocking prefix using this session's configured
    /// dispatcher command-channel bound.
    ///
    /// This avoids planning against a caller-supplied bound that differs from the
    /// actual bound passed to [`Self::from_parts_with_bound`].
    pub fn try_queue_actions_bounded_prefix_with_session_bound(
        &self,
        actions: &[AgentAction],
    ) -> Result<AgentPlanBoundedPrefix> {
        self.try_queue_actions_bounded_prefix(actions, self.command_bound)
    }

    /// Queue the longest statically safe non-blocking prefix that fits an
    /// estimated dispatcher-command budget while reserving one command for a
    /// checked final barrier.
    ///
    /// This is the bounded-prefix companion to [`Self::try_run_actions`].
    /// Malformed metadata anywhere in the supplied plan returns an error before
    /// dispatching any accepted prefix. A command-bound or blocking-timing stop
    /// queues the accepted prefix, enqueues one checked barrier, and returns the
    /// stop metadata.
    pub fn try_run_actions_bounded_prefix(
        &self,
        actions: &[AgentAction],
        command_bound: usize,
    ) -> Result<AgentPlanBoundedPrefix> {
        AgentAction::validate_plan_structure(actions)?;
        let prefix = AgentAction::bounded_try_run_prefix(actions, command_bound);
        if !prefix.checked_dispatch_fits_bound() {
            return Err(Error::SessionLifecycle(TRY_RUN_EXCEEDS_COMMAND_BOUND));
        }
        if let AgentPlanBoundedPrefixStop::TryQueueError { error, .. } = prefix.stop {
            return Err(Error::SessionLifecycle(error));
        }
        self.try_queue_actions(&actions[..prefix.accepted_actions])?;
        self.client.try_flush_wait()?;
        Ok(prefix)
    }

    /// Queue a checked bounded non-blocking prefix using this session's
    /// configured dispatcher command-channel bound.
    pub fn try_run_actions_bounded_prefix_with_session_bound(
        &self,
        actions: &[AgentAction],
    ) -> Result<AgentPlanBoundedPrefix> {
        self.try_run_actions_bounded_prefix(actions, self.command_bound)
    }

    /// Queue a typed agent action plan and wait for one checked dispatcher
    /// barrier after the final action.
    ///
    /// Touch, low-level keyboard, relative mouse, and full-frame gamepad
    /// actions are internally batched across compatible adjacent plan steps,
    /// while still reporting dispatcher-side command errors at the end.
    pub fn run_actions(&self, actions: &[AgentAction]) -> Result<()> {
        self.queue_actions(actions)?;
        self.flush()
    }

    /// Queue a typed agent action plan using non-blocking dispatcher sends,
    /// then enqueue one checked dispatcher barrier without blocking on a full
    /// command queue.
    ///
    /// This is the checked-barrier companion to [`Self::try_queue_actions`] for
    /// high-contention schedulers. It rejects timing-dependent actions like
    /// `Wait` / `LongPress` before dispatch, rejects plans that cannot fit this
    /// session's empty command queue with the final barrier, returns
    /// back-pressure if the live queue cannot accept the plan or final barrier,
    /// and surfaces dispatcher-side command errors once the barrier is accepted.
    pub fn try_run_actions(&self, actions: &[AgentAction]) -> Result<()> {
        let summary = AgentAction::plan_summary(actions);
        if let Some((_, error)) = summary.first_try_queue_error {
            return Err(Error::SessionLifecycle(error));
        }
        if !summary.try_run_dispatch_fits_bound(self.command_bound) {
            return Err(Error::SessionLifecycle(TRY_RUN_EXCEEDS_COMMAND_BOUND));
        }
        self.try_queue_actions(actions)?;
        self.client.try_flush_wait().map(|_| ())
    }

    /// Queue an action plan through [`Self::try_run_actions`], then wait for the
    /// next newest-only frame snapshot observed after the checked barrier.
    pub fn try_run_actions_and_wait_for_next_latest_frame(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
    ) -> Result<LatestFrameSummarySnapshot> {
        self.try_run_actions_and_wait_for_next_latest_frame_matching(actions, latest, |_| true)
    }

    /// Queue an action plan through [`Self::try_run_actions`], then wait for the
    /// next newest-only frame snapshot accepted by `predicate`.
    pub fn try_run_actions_and_wait_for_next_latest_frame_matching(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        mut predicate: impl FnMut(&FrameSummary) -> bool,
    ) -> Result<LatestFrameSummarySnapshot> {
        self.try_run_actions(actions)?;
        let after_version = latest.version();
        latest
            .wait_next_matching(after_version, |snapshot| predicate(&snapshot.summary))
            .map_err(|e| io_to_wait_error(e, "latest frame summary"))
    }

    /// Queue an action plan through [`Self::try_run_actions`], then wait up to
    /// `timeout` for the next newest-only frame snapshot.
    pub fn try_run_actions_and_wait_for_next_latest_frame_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        timeout: Duration,
    ) -> Result<LatestFrameSummarySnapshot> {
        self.try_run_actions_and_wait_for_next_latest_frame_matching_timeout(
            actions,
            latest,
            timeout,
            |_| true,
        )
    }

    /// Queue an action plan through [`Self::try_run_actions`], then wait up to
    /// `timeout` for the next newest-only frame accepted by `predicate`.
    pub fn try_run_actions_and_wait_for_next_latest_frame_matching_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        timeout: Duration,
        mut predicate: impl FnMut(&FrameSummary) -> bool,
    ) -> Result<LatestFrameSummarySnapshot> {
        self.try_run_actions(actions)?;
        let after_version = latest.version();
        latest
            .wait_next_matching_timeout(after_version, timeout, |snapshot| {
                predicate(&snapshot.summary)
            })
            .map_err(|e| io_to_wait_error(e, "latest frame summary"))
    }

    /// Queue an action plan through [`Self::try_run_actions`], then wait for a
    /// newest-only frame snapshot with `version > after_version`.
    pub fn try_run_actions_and_wait_for_next_latest_frame_after_version(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        after_version: u64,
    ) -> Result<LatestFrameSummarySnapshot> {
        self.try_run_actions_and_wait_for_next_latest_frame_matching_after_version(
            actions,
            latest,
            after_version,
            |_| true,
        )
    }

    /// Queue an action plan through [`Self::try_run_actions`], then wait for a
    /// newest-only frame snapshot newer than `boundary`.
    pub fn try_run_actions_and_wait_for_next_latest_frame_after_boundary(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        boundary: LatestFrameSummaryBoundary,
    ) -> Result<LatestFrameSummarySnapshot> {
        self.try_run_actions_and_wait_for_next_latest_frame_after_version(
            actions,
            latest,
            boundary.version(),
        )
    }

    /// Queue an action plan through [`Self::try_run_actions`], then wait for a
    /// newest-only frame snapshot newer than `observation.boundary`.
    pub fn try_run_actions_and_wait_for_next_latest_frame_after_observation(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        observation: &LatestFrameSummaryObservation,
    ) -> Result<LatestFrameSummarySnapshot> {
        self.try_run_actions_and_wait_for_next_latest_frame_after_boundary(
            actions,
            latest,
            observation.boundary(),
        )
    }

    /// Queue an action plan through [`Self::try_run_actions`], then wait for a
    /// newest-only frame snapshot newer than `after_version` and accepted by
    /// `predicate`.
    pub fn try_run_actions_and_wait_for_next_latest_frame_matching_after_version(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        after_version: u64,
        mut predicate: impl FnMut(&FrameSummary) -> bool,
    ) -> Result<LatestFrameSummarySnapshot> {
        self.try_run_actions(actions)?;
        latest
            .wait_next_matching(after_version, |snapshot| predicate(&snapshot.summary))
            .map_err(|e| io_to_wait_error(e, "latest frame summary"))
    }

    /// Queue an action plan through [`Self::try_run_actions`], then wait for a
    /// newest-only frame snapshot newer than `boundary` and accepted by
    /// `predicate`.
    pub fn try_run_actions_and_wait_for_next_latest_frame_matching_after_boundary(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        boundary: LatestFrameSummaryBoundary,
        predicate: impl FnMut(&FrameSummary) -> bool,
    ) -> Result<LatestFrameSummarySnapshot> {
        self.try_run_actions_and_wait_for_next_latest_frame_matching_after_version(
            actions,
            latest,
            boundary.version(),
            predicate,
        )
    }

    /// Queue an action plan through [`Self::try_run_actions`], then wait for a
    /// newest-only frame snapshot newer than `observation.boundary` and accepted
    /// by `predicate`.
    pub fn try_run_actions_and_wait_for_next_latest_frame_matching_after_observation(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        observation: &LatestFrameSummaryObservation,
        predicate: impl FnMut(&FrameSummary) -> bool,
    ) -> Result<LatestFrameSummarySnapshot> {
        self.try_run_actions_and_wait_for_next_latest_frame_matching_after_boundary(
            actions,
            latest,
            observation.boundary(),
            predicate,
        )
    }

    /// Queue an action plan through [`Self::try_run_actions`], then wait up to
    /// `timeout` for a newest-only frame with `version > after_version`.
    pub fn try_run_actions_and_wait_for_next_latest_frame_after_version_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        after_version: u64,
        timeout: Duration,
    ) -> Result<LatestFrameSummarySnapshot> {
        self.try_run_actions_and_wait_for_next_latest_frame_matching_after_version_timeout(
            actions,
            latest,
            after_version,
            timeout,
            |_| true,
        )
    }

    /// Queue an action plan through [`Self::try_run_actions`], then wait up to
    /// `timeout` for a newest-only frame snapshot newer than `boundary`.
    pub fn try_run_actions_and_wait_for_next_latest_frame_after_boundary_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        boundary: LatestFrameSummaryBoundary,
        timeout: Duration,
    ) -> Result<LatestFrameSummarySnapshot> {
        self.try_run_actions_and_wait_for_next_latest_frame_after_version_timeout(
            actions,
            latest,
            boundary.version(),
            timeout,
        )
    }

    /// Queue an action plan through [`Self::try_run_actions`], then wait up to
    /// `timeout` for a newest-only frame snapshot newer than
    /// `observation.boundary`.
    pub fn try_run_actions_and_wait_for_next_latest_frame_after_observation_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        observation: &LatestFrameSummaryObservation,
        timeout: Duration,
    ) -> Result<LatestFrameSummarySnapshot> {
        self.try_run_actions_and_wait_for_next_latest_frame_after_boundary_timeout(
            actions,
            latest,
            observation.boundary(),
            timeout,
        )
    }

    /// Queue an action plan through [`Self::try_run_actions`], then wait up to
    /// `timeout` for a newest-only frame with `version > after_version` accepted
    /// by `predicate`.
    pub fn try_run_actions_and_wait_for_next_latest_frame_matching_after_version_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        after_version: u64,
        timeout: Duration,
        mut predicate: impl FnMut(&FrameSummary) -> bool,
    ) -> Result<LatestFrameSummarySnapshot> {
        self.try_run_actions(actions)?;
        latest
            .wait_next_matching_timeout(after_version, timeout, |snapshot| {
                predicate(&snapshot.summary)
            })
            .map_err(|e| io_to_wait_error(e, "latest frame summary"))
    }

    /// Queue an action plan through [`Self::try_run_actions`], then wait up to
    /// `timeout` for a newest-only frame snapshot newer than `boundary` and
    /// accepted by `predicate`.
    pub fn try_run_actions_and_wait_for_next_latest_frame_matching_after_boundary_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        boundary: LatestFrameSummaryBoundary,
        timeout: Duration,
        predicate: impl FnMut(&FrameSummary) -> bool,
    ) -> Result<LatestFrameSummarySnapshot> {
        self.try_run_actions_and_wait_for_next_latest_frame_matching_after_version_timeout(
            actions,
            latest,
            boundary.version(),
            timeout,
            predicate,
        )
    }

    /// Queue an action plan through [`Self::try_run_actions`], then wait up to
    /// `timeout` for a newest-only frame snapshot newer than
    /// `observation.boundary` and accepted by `predicate`.
    pub fn try_run_actions_and_wait_for_next_latest_frame_matching_after_observation_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        observation: &LatestFrameSummaryObservation,
        timeout: Duration,
        predicate: impl FnMut(&FrameSummary) -> bool,
    ) -> Result<LatestFrameSummarySnapshot> {
        self.try_run_actions_and_wait_for_next_latest_frame_matching_after_boundary_timeout(
            actions,
            latest,
            observation.boundary(),
            timeout,
            predicate,
        )
    }

    /// Queue an action plan through [`Self::try_run_actions`], then wait for the
    /// next newest-only frame with `frame_seq > min_frame_seq`.
    pub fn try_run_actions_and_wait_for_next_latest_frame_after_seq(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        min_frame_seq: u32,
    ) -> Result<LatestFrameSummarySnapshot> {
        self.try_run_actions_and_wait_for_next_latest_frame_matching(actions, latest, |summary| {
            summary.frame_seq > min_frame_seq
        })
    }

    /// Queue an action plan through [`Self::try_run_actions`], then wait up to
    /// `timeout` for the next newest-only frame with
    /// `frame_seq > min_frame_seq`.
    pub fn try_run_actions_and_wait_for_next_latest_frame_after_seq_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        min_frame_seq: u32,
        timeout: Duration,
    ) -> Result<LatestFrameSummarySnapshot> {
        self.try_run_actions_and_wait_for_next_latest_frame_matching_timeout(
            actions,
            latest,
            timeout,
            |summary| summary.frame_seq > min_frame_seq,
        )
    }

    /// Queue an action plan through [`Self::try_run_actions`], then wait for the
    /// next newest-only frame with `timestamp_ms > min_timestamp_ms`.
    pub fn try_run_actions_and_wait_for_next_latest_frame_after_timestamp(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        min_timestamp_ms: u64,
    ) -> Result<LatestFrameSummarySnapshot> {
        self.try_run_actions_and_wait_for_next_latest_frame_matching(actions, latest, |summary| {
            summary.timestamp_ms > min_timestamp_ms
        })
    }

    /// Queue an action plan through [`Self::try_run_actions`], then wait up to
    /// `timeout` for the next newest-only frame with
    /// `timestamp_ms > min_timestamp_ms`.
    pub fn try_run_actions_and_wait_for_next_latest_frame_after_timestamp_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        min_timestamp_ms: u64,
        timeout: Duration,
    ) -> Result<LatestFrameSummarySnapshot> {
        self.try_run_actions_and_wait_for_next_latest_frame_matching_timeout(
            actions,
            latest,
            timeout,
            |summary| summary.timestamp_ms > min_timestamp_ms,
        )
    }

    /// Run an action plan, wait for one checked dispatcher barrier, then wait
    /// for the next newest-only frame snapshot observed after that barrier.
    pub fn run_actions_and_wait_for_next_latest_frame(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
    ) -> Result<LatestFrameSummarySnapshot> {
        self.run_actions_and_wait_for_next_latest_frame_matching(actions, latest, |_| true)
    }

    /// Run an action plan, wait for one checked dispatcher barrier, then wait
    /// for the next newest-only frame snapshot accepted by `predicate`.
    pub fn run_actions_and_wait_for_next_latest_frame_matching(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        mut predicate: impl FnMut(&FrameSummary) -> bool,
    ) -> Result<LatestFrameSummarySnapshot> {
        self.run_actions(actions)?;
        let after_version = latest.version();
        latest
            .wait_next_matching(after_version, |snapshot| predicate(&snapshot.summary))
            .map_err(|e| io_to_wait_error(e, "latest frame summary"))
    }

    /// Run an action plan, wait for one checked dispatcher barrier, then wait
    /// for the next newest-only frame snapshot within `timeout`.
    pub fn run_actions_and_wait_for_next_latest_frame_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        timeout: Duration,
    ) -> Result<LatestFrameSummarySnapshot> {
        self.run_actions_and_wait_for_next_latest_frame_matching_timeout(
            actions,
            latest,
            timeout,
            |_| true,
        )
    }

    /// Run an action plan, wait for one checked dispatcher barrier, then wait
    /// up to `timeout` for the next newest-only frame accepted by `predicate`.
    pub fn run_actions_and_wait_for_next_latest_frame_matching_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        timeout: Duration,
        mut predicate: impl FnMut(&FrameSummary) -> bool,
    ) -> Result<LatestFrameSummarySnapshot> {
        self.run_actions(actions)?;
        let after_version = latest.version();
        latest
            .wait_next_matching_timeout(after_version, timeout, |snapshot| {
                predicate(&snapshot.summary)
            })
            .map_err(|e| io_to_wait_error(e, "latest frame summary"))
    }

    /// Run an action plan, wait for one checked dispatcher barrier, then wait
    /// for a newest-only frame snapshot with `version > after_version`.
    ///
    /// Use this when the caller already captured a latest-frame boundary before
    /// deciding which actions to send and wants to accept any cached/newer frame
    /// observed since that boundary.
    pub fn run_actions_and_wait_for_next_latest_frame_after_version(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        after_version: u64,
    ) -> Result<LatestFrameSummarySnapshot> {
        self.run_actions_and_wait_for_next_latest_frame_matching_after_version(
            actions,
            latest,
            after_version,
            |_| true,
        )
    }

    /// Run an action plan, wait for one checked dispatcher barrier, then wait
    /// for a newest-only frame snapshot newer than `boundary`.
    pub fn run_actions_and_wait_for_next_latest_frame_after_boundary(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        boundary: LatestFrameSummaryBoundary,
    ) -> Result<LatestFrameSummarySnapshot> {
        self.run_actions_and_wait_for_next_latest_frame_after_version(
            actions,
            latest,
            boundary.version(),
        )
    }

    /// Run an action plan, wait for one checked dispatcher barrier, then wait
    /// for a newest-only frame snapshot newer than `observation.boundary`.
    pub fn run_actions_and_wait_for_next_latest_frame_after_observation(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        observation: &LatestFrameSummaryObservation,
    ) -> Result<LatestFrameSummarySnapshot> {
        self.run_actions_and_wait_for_next_latest_frame_after_boundary(
            actions,
            latest,
            observation.boundary(),
        )
    }

    /// Run an action plan, wait for one checked dispatcher barrier, then wait
    /// for a newest-only frame snapshot with `version > after_version` accepted
    /// by `predicate`.
    pub fn run_actions_and_wait_for_next_latest_frame_matching_after_version(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        after_version: u64,
        mut predicate: impl FnMut(&FrameSummary) -> bool,
    ) -> Result<LatestFrameSummarySnapshot> {
        self.run_actions(actions)?;
        latest
            .wait_next_matching(after_version, |snapshot| predicate(&snapshot.summary))
            .map_err(|e| io_to_wait_error(e, "latest frame summary"))
    }

    /// Run an action plan, wait for one checked dispatcher barrier, then wait
    /// for a newest-only frame snapshot newer than `boundary` and accepted by
    /// `predicate`.
    pub fn run_actions_and_wait_for_next_latest_frame_matching_after_boundary(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        boundary: LatestFrameSummaryBoundary,
        predicate: impl FnMut(&FrameSummary) -> bool,
    ) -> Result<LatestFrameSummarySnapshot> {
        self.run_actions_and_wait_for_next_latest_frame_matching_after_version(
            actions,
            latest,
            boundary.version(),
            predicate,
        )
    }

    /// Run an action plan, wait for one checked dispatcher barrier, then wait
    /// for a newest-only frame snapshot newer than `observation.boundary` and
    /// accepted by `predicate`.
    pub fn run_actions_and_wait_for_next_latest_frame_matching_after_observation(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        observation: &LatestFrameSummaryObservation,
        predicate: impl FnMut(&FrameSummary) -> bool,
    ) -> Result<LatestFrameSummarySnapshot> {
        self.run_actions_and_wait_for_next_latest_frame_matching_after_boundary(
            actions,
            latest,
            observation.boundary(),
            predicate,
        )
    }

    /// Run an action plan, wait for one checked dispatcher barrier, then wait
    /// up to `timeout` for a newest-only frame with `version > after_version`.
    pub fn run_actions_and_wait_for_next_latest_frame_after_version_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        after_version: u64,
        timeout: Duration,
    ) -> Result<LatestFrameSummarySnapshot> {
        self.run_actions_and_wait_for_next_latest_frame_matching_after_version_timeout(
            actions,
            latest,
            after_version,
            timeout,
            |_| true,
        )
    }

    /// Run an action plan, wait for one checked dispatcher barrier, then wait
    /// up to `timeout` for a newest-only frame snapshot newer than `boundary`.
    pub fn run_actions_and_wait_for_next_latest_frame_after_boundary_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        boundary: LatestFrameSummaryBoundary,
        timeout: Duration,
    ) -> Result<LatestFrameSummarySnapshot> {
        self.run_actions_and_wait_for_next_latest_frame_after_version_timeout(
            actions,
            latest,
            boundary.version(),
            timeout,
        )
    }

    /// Run an action plan, wait for one checked dispatcher barrier, then wait up
    /// to `timeout` for a newest-only frame snapshot newer than
    /// `observation.boundary`.
    pub fn run_actions_and_wait_for_next_latest_frame_after_observation_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        observation: &LatestFrameSummaryObservation,
        timeout: Duration,
    ) -> Result<LatestFrameSummarySnapshot> {
        self.run_actions_and_wait_for_next_latest_frame_after_boundary_timeout(
            actions,
            latest,
            observation.boundary(),
            timeout,
        )
    }

    /// Run an action plan, wait for one checked dispatcher barrier, then wait
    /// up to `timeout` for a newest-only frame with `version > after_version`
    /// accepted by `predicate`.
    pub fn run_actions_and_wait_for_next_latest_frame_matching_after_version_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        after_version: u64,
        timeout: Duration,
        mut predicate: impl FnMut(&FrameSummary) -> bool,
    ) -> Result<LatestFrameSummarySnapshot> {
        self.run_actions(actions)?;
        latest
            .wait_next_matching_timeout(after_version, timeout, |snapshot| {
                predicate(&snapshot.summary)
            })
            .map_err(|e| io_to_wait_error(e, "latest frame summary"))
    }

    /// Run an action plan, wait for one checked dispatcher barrier, then wait
    /// up to `timeout` for a newest-only frame snapshot newer than `boundary`
    /// and accepted by `predicate`.
    pub fn run_actions_and_wait_for_next_latest_frame_matching_after_boundary_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        boundary: LatestFrameSummaryBoundary,
        timeout: Duration,
        predicate: impl FnMut(&FrameSummary) -> bool,
    ) -> Result<LatestFrameSummarySnapshot> {
        self.run_actions_and_wait_for_next_latest_frame_matching_after_version_timeout(
            actions,
            latest,
            boundary.version(),
            timeout,
            predicate,
        )
    }

    /// Run an action plan, wait for one checked dispatcher barrier, then wait up
    /// to `timeout` for a newest-only frame snapshot newer than
    /// `observation.boundary` and accepted by `predicate`.
    pub fn run_actions_and_wait_for_next_latest_frame_matching_after_observation_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        observation: &LatestFrameSummaryObservation,
        timeout: Duration,
        predicate: impl FnMut(&FrameSummary) -> bool,
    ) -> Result<LatestFrameSummarySnapshot> {
        self.run_actions_and_wait_for_next_latest_frame_matching_after_boundary_timeout(
            actions,
            latest,
            observation.boundary(),
            timeout,
            predicate,
        )
    }

    /// Run an action plan, wait for one checked dispatcher barrier, then wait
    /// for the next newest-only frame with `frame_seq > min_frame_seq`.
    pub fn run_actions_and_wait_for_next_latest_frame_after_seq(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        min_frame_seq: u32,
    ) -> Result<LatestFrameSummarySnapshot> {
        self.run_actions_and_wait_for_next_latest_frame_matching(actions, latest, |summary| {
            summary.frame_seq > min_frame_seq
        })
    }

    /// Run an action plan, wait for one checked dispatcher barrier, then wait
    /// up to `timeout` for the next newest-only frame with
    /// `frame_seq > min_frame_seq`.
    pub fn run_actions_and_wait_for_next_latest_frame_after_seq_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        min_frame_seq: u32,
        timeout: Duration,
    ) -> Result<LatestFrameSummarySnapshot> {
        self.run_actions_and_wait_for_next_latest_frame_matching_timeout(
            actions,
            latest,
            timeout,
            |summary| summary.frame_seq > min_frame_seq,
        )
    }

    /// Run an action plan, wait for one checked dispatcher barrier, then wait
    /// for the next newest-only frame with
    /// `timestamp_ms > min_timestamp_ms`.
    pub fn run_actions_and_wait_for_next_latest_frame_after_timestamp(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        min_timestamp_ms: u64,
    ) -> Result<LatestFrameSummarySnapshot> {
        self.run_actions_and_wait_for_next_latest_frame_matching(actions, latest, |summary| {
            summary.timestamp_ms > min_timestamp_ms
        })
    }

    /// Run an action plan, wait for one checked dispatcher barrier, then wait
    /// up to `timeout` for the next newest-only frame with
    /// `timestamp_ms > min_timestamp_ms`.
    pub fn run_actions_and_wait_for_next_latest_frame_after_timestamp_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        min_timestamp_ms: u64,
        timeout: Duration,
    ) -> Result<LatestFrameSummarySnapshot> {
        self.run_actions_and_wait_for_next_latest_frame_matching_timeout(
            actions,
            latest,
            timeout,
            |summary| summary.timestamp_ms > min_timestamp_ms,
        )
    }

    /// Queue an action plan through [`Self::try_run_actions`], wait for the
    /// next newest-only frame containing `target`, and return its rectangle.
    pub fn try_run_actions_and_wait_for_next_latest_target_rect(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        target: AgentTargetSelector,
    ) -> Result<AgentRect> {
        self.try_run_actions_and_select_next_latest_target(actions, latest, target)
    }

    /// Queue an action plan through [`Self::try_run_actions`], wait up to
    /// `timeout` for the next newest-only frame containing `target`, and return
    /// its rectangle.
    pub fn try_run_actions_and_wait_for_next_latest_target_rect_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        target: AgentTargetSelector,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.try_run_actions_and_select_next_latest_target_timeout(actions, latest, target, timeout)
    }

    /// Queue an action plan through [`Self::try_run_actions`], wait for the
    /// next newest-only frame newer than `after_version` and containing
    /// `target`, and return its rectangle.
    pub fn try_run_actions_and_wait_for_next_latest_target_rect_after_version(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        target: AgentTargetSelector,
        after_version: u64,
    ) -> Result<AgentRect> {
        self.try_run_actions_and_select_next_latest_target_after_version(
            actions,
            latest,
            target,
            after_version,
        )
    }

    /// Queue an action plan through [`Self::try_run_actions`], wait for the
    /// next newest-only frame newer than `boundary` and containing `target`,
    /// and return its rectangle.
    pub fn try_run_actions_and_wait_for_next_latest_target_rect_after_boundary(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        target: AgentTargetSelector,
        boundary: LatestFrameSummaryBoundary,
    ) -> Result<AgentRect> {
        self.try_run_actions_and_wait_for_next_latest_target_rect_after_version(
            actions,
            latest,
            target,
            boundary.version(),
        )
    }

    /// Queue an action plan through [`Self::try_run_actions`], wait for the
    /// next newest-only frame newer than `observation.boundary` and containing
    /// `target`, and return its rectangle.
    pub fn try_run_actions_and_wait_for_next_latest_target_rect_after_observation(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        target: AgentTargetSelector,
        observation: &LatestFrameSummaryObservation,
    ) -> Result<AgentRect> {
        self.try_run_actions_and_wait_for_next_latest_target_rect_after_boundary(
            actions,
            latest,
            target,
            observation.boundary(),
        )
    }

    /// Queue an action plan through [`Self::try_run_actions`], wait up to
    /// `timeout` for the next newest-only frame newer than `after_version` and
    /// containing `target`, and return its rectangle.
    pub fn try_run_actions_and_wait_for_next_latest_target_rect_after_version_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        target: AgentTargetSelector,
        after_version: u64,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.try_run_actions_and_select_next_latest_target_after_version_timeout(
            actions,
            latest,
            target,
            after_version,
            timeout,
        )
    }

    /// Queue an action plan through [`Self::try_run_actions`], wait up to
    /// `timeout` for the next newest-only frame newer than `boundary` and
    /// containing `target`, and return its rectangle.
    pub fn try_run_actions_and_wait_for_next_latest_target_rect_after_boundary_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        target: AgentTargetSelector,
        boundary: LatestFrameSummaryBoundary,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.try_run_actions_and_wait_for_next_latest_target_rect_after_version_timeout(
            actions,
            latest,
            target,
            boundary.version(),
            timeout,
        )
    }

    /// Queue an action plan through [`Self::try_run_actions`], wait up to
    /// `timeout` for the next newest-only frame newer than
    /// `observation.boundary` and containing `target`, and return its rectangle.
    pub fn try_run_actions_and_wait_for_next_latest_target_rect_after_observation_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        target: AgentTargetSelector,
        observation: &LatestFrameSummaryObservation,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.try_run_actions_and_wait_for_next_latest_target_rect_after_boundary_timeout(
            actions,
            latest,
            target,
            observation.boundary(),
            timeout,
        )
    }

    /// Queue an action plan through [`Self::try_run_actions`], wait up to
    /// `timeout` for the next newest-only frame containing `target`, tap a
    /// relative point inside it with a typed scrcpy pointer id, and return it.
    pub fn try_run_actions_and_tap_next_latest_target_at_pointer_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        target: AgentTargetSelector,
        pointer_id: TouchPointerId,
        anchor_bp: (u16, u16),
        timeout: Duration,
    ) -> Result<AgentRect> {
        let rect = self.try_run_actions_and_select_next_latest_target_timeout(
            actions, latest, target, timeout,
        )?;
        self.try_tap_rect_at_pointer(pointer_id, rect, anchor_bp.0, anchor_bp.1)?;
        Ok(rect)
    }

    /// Run an action plan, wait for the next newest-only frame containing
    /// `target`, and return its rectangle.
    pub fn run_actions_and_wait_for_next_latest_target_rect(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        target: AgentTargetSelector,
    ) -> Result<AgentRect> {
        self.run_actions_and_select_next_latest_target(actions, latest, target)
    }

    /// Run an action plan, wait up to `timeout` for the next newest-only frame
    /// containing `target`, and return its rectangle.
    pub fn run_actions_and_wait_for_next_latest_target_rect_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        target: AgentTargetSelector,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.run_actions_and_select_next_latest_target_timeout(actions, latest, target, timeout)
    }

    /// Run an action plan, wait for the next newest-only frame newer than
    /// `after_version` and containing `target`, and return its rectangle.
    pub fn run_actions_and_wait_for_next_latest_target_rect_after_version(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        target: AgentTargetSelector,
        after_version: u64,
    ) -> Result<AgentRect> {
        self.run_actions_and_select_next_latest_target_after_version(
            actions,
            latest,
            target,
            after_version,
        )
    }

    /// Run an action plan, wait for the next newest-only frame newer than
    /// `boundary` and containing `target`, and return its rectangle.
    pub fn run_actions_and_wait_for_next_latest_target_rect_after_boundary(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        target: AgentTargetSelector,
        boundary: LatestFrameSummaryBoundary,
    ) -> Result<AgentRect> {
        self.run_actions_and_wait_for_next_latest_target_rect_after_version(
            actions,
            latest,
            target,
            boundary.version(),
        )
    }

    /// Run an action plan, wait for the next newest-only frame newer than
    /// `observation.boundary` and containing `target`, and return its rectangle.
    pub fn run_actions_and_wait_for_next_latest_target_rect_after_observation(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        target: AgentTargetSelector,
        observation: &LatestFrameSummaryObservation,
    ) -> Result<AgentRect> {
        self.run_actions_and_wait_for_next_latest_target_rect_after_boundary(
            actions,
            latest,
            target,
            observation.boundary(),
        )
    }

    /// Run an action plan, wait up to `timeout` for the next newest-only frame
    /// newer than `after_version` and containing `target`, and return its
    /// rectangle.
    pub fn run_actions_and_wait_for_next_latest_target_rect_after_version_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        target: AgentTargetSelector,
        after_version: u64,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.run_actions_and_select_next_latest_target_after_version_timeout(
            actions,
            latest,
            target,
            after_version,
            timeout,
        )
    }

    /// Run an action plan, wait up to `timeout` for the next newest-only frame
    /// newer than `boundary` and containing `target`, and return its rectangle.
    pub fn run_actions_and_wait_for_next_latest_target_rect_after_boundary_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        target: AgentTargetSelector,
        boundary: LatestFrameSummaryBoundary,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.run_actions_and_wait_for_next_latest_target_rect_after_version_timeout(
            actions,
            latest,
            target,
            boundary.version(),
            timeout,
        )
    }

    /// Run an action plan, wait up to `timeout` for the next newest-only frame
    /// newer than `observation.boundary` and containing `target`, and return its
    /// rectangle.
    pub fn run_actions_and_wait_for_next_latest_target_rect_after_observation_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        target: AgentTargetSelector,
        observation: &LatestFrameSummaryObservation,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.run_actions_and_wait_for_next_latest_target_rect_after_boundary_timeout(
            actions,
            latest,
            target,
            observation.boundary(),
            timeout,
        )
    }

    /// Run an action plan, wait for the next newest-only frame containing
    /// `target`, tap its center, and return it.
    pub fn run_actions_and_tap_next_latest_target(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        target: AgentTargetSelector,
    ) -> Result<AgentRect> {
        let rect = self.run_actions_and_select_next_latest_target(actions, latest, target)?;
        self.tap_rect(rect)?;
        Ok(rect)
    }

    /// Run an action plan, wait for the next newest-only frame newer than
    /// `after_version` and containing `target`, tap its center, and return it.
    pub fn run_actions_and_tap_next_latest_target_after_version(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        target: AgentTargetSelector,
        after_version: u64,
    ) -> Result<AgentRect> {
        let rect = self.run_actions_and_select_next_latest_target_after_version(
            actions,
            latest,
            target,
            after_version,
        )?;
        self.tap_rect(rect)?;
        Ok(rect)
    }

    /// Run an action plan, wait for the next newest-only frame newer than
    /// `boundary` and containing `target`, tap its center, and return it.
    pub fn run_actions_and_tap_next_latest_target_after_boundary(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        target: AgentTargetSelector,
        boundary: LatestFrameSummaryBoundary,
    ) -> Result<AgentRect> {
        self.run_actions_and_tap_next_latest_target_after_version(
            actions,
            latest,
            target,
            boundary.version(),
        )
    }

    /// Run an action plan, wait for the next newest-only frame newer than
    /// `observation.boundary` and containing `target`, tap its center, and
    /// return it.
    pub fn run_actions_and_tap_next_latest_target_after_observation(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        target: AgentTargetSelector,
        observation: &LatestFrameSummaryObservation,
    ) -> Result<AgentRect> {
        self.run_actions_and_tap_next_latest_target_after_boundary(
            actions,
            latest,
            target,
            observation.boundary(),
        )
    }

    /// Run an action plan, wait up to `timeout` for the next newest-only frame
    /// newer than `after_version` and containing `target`, tap its center, and
    /// return it.
    pub fn run_actions_and_tap_next_latest_target_after_version_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        target: AgentTargetSelector,
        after_version: u64,
        timeout: Duration,
    ) -> Result<AgentRect> {
        let rect = self.run_actions_and_select_next_latest_target_after_version_timeout(
            actions,
            latest,
            target,
            after_version,
            timeout,
        )?;
        self.tap_rect(rect)?;
        Ok(rect)
    }

    /// Run an action plan, wait up to `timeout` for the next newest-only frame
    /// newer than `boundary` and containing `target`, tap its center, and return
    /// it.
    pub fn run_actions_and_tap_next_latest_target_after_boundary_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        target: AgentTargetSelector,
        boundary: LatestFrameSummaryBoundary,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.run_actions_and_tap_next_latest_target_after_version_timeout(
            actions,
            latest,
            target,
            boundary.version(),
            timeout,
        )
    }

    /// Run an action plan, wait up to `timeout` for the next newest-only frame
    /// newer than `observation.boundary` and containing `target`, tap its center,
    /// and return it.
    pub fn run_actions_and_tap_next_latest_target_after_observation_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        target: AgentTargetSelector,
        observation: &LatestFrameSummaryObservation,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.run_actions_and_tap_next_latest_target_after_boundary_timeout(
            actions,
            latest,
            target,
            observation.boundary(),
            timeout,
        )
    }

    /// Run an action plan, wait up to `timeout` for the next newest-only frame
    /// containing `target`, tap its center, and return it.
    pub fn run_actions_and_tap_next_latest_target_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        target: AgentTargetSelector,
        timeout: Duration,
    ) -> Result<AgentRect> {
        let rect = self
            .run_actions_and_select_next_latest_target_timeout(actions, latest, target, timeout)?;
        self.tap_rect(rect)?;
        Ok(rect)
    }

    /// Run an action plan, wait for the next newest-only frame containing
    /// `target`, tap its center with a typed scrcpy pointer id, and return it.
    pub fn run_actions_and_tap_next_latest_target_pointer(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        target: AgentTargetSelector,
        pointer_id: TouchPointerId,
    ) -> Result<AgentRect> {
        let rect = self.run_actions_and_select_next_latest_target(actions, latest, target)?;
        self.tap_rect_pointer(pointer_id, rect)?;
        Ok(rect)
    }

    /// Run an action plan, wait up to `timeout` for the next newest-only frame
    /// containing `target`, tap its center with a typed scrcpy pointer id, and
    /// return it.
    pub fn run_actions_and_tap_next_latest_target_pointer_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        target: AgentTargetSelector,
        pointer_id: TouchPointerId,
        timeout: Duration,
    ) -> Result<AgentRect> {
        let rect = self
            .run_actions_and_select_next_latest_target_timeout(actions, latest, target, timeout)?;
        self.tap_rect_pointer(pointer_id, rect)?;
        Ok(rect)
    }

    /// Run an action plan, wait for the next newest-only frame containing
    /// `target`, tap a relative point inside it, and return it.
    pub fn run_actions_and_tap_next_latest_target_at(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        target: AgentTargetSelector,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<AgentRect> {
        let rect = self.run_actions_and_select_next_latest_target(actions, latest, target)?;
        self.tap_rect_at(rect, x_bp, y_bp)?;
        Ok(rect)
    }

    /// Run an action plan, wait up to `timeout` for the next newest-only frame
    /// containing `target`, tap a relative point inside it, and return it.
    pub fn run_actions_and_tap_next_latest_target_at_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        target: AgentTargetSelector,
        x_bp: u16,
        y_bp: u16,
        timeout: Duration,
    ) -> Result<AgentRect> {
        let rect = self
            .run_actions_and_select_next_latest_target_timeout(actions, latest, target, timeout)?;
        self.tap_rect_at(rect, x_bp, y_bp)?;
        Ok(rect)
    }

    /// Run an action plan, wait for the next newest-only frame containing
    /// `target`, tap a relative point inside it with a typed scrcpy pointer id,
    /// and return it.
    pub fn run_actions_and_tap_next_latest_target_at_pointer(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        target: AgentTargetSelector,
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<AgentRect> {
        let rect = self.run_actions_and_select_next_latest_target(actions, latest, target)?;
        self.tap_rect_at_pointer(pointer_id, rect, x_bp, y_bp)?;
        Ok(rect)
    }

    /// Run an action plan, wait up to `timeout` for the next newest-only frame
    /// containing `target`, tap a relative point inside it with a typed scrcpy
    /// pointer id, and return it.
    pub fn run_actions_and_tap_next_latest_target_at_pointer_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        target: AgentTargetSelector,
        pointer_id: TouchPointerId,
        anchor_bp: (u16, u16),
        timeout: Duration,
    ) -> Result<AgentRect> {
        let rect = self
            .run_actions_and_select_next_latest_target_timeout(actions, latest, target, timeout)?;
        self.tap_rect_at_pointer(pointer_id, rect, anchor_bp.0, anchor_bp.1)?;
        Ok(rect)
    }

    /// Run an action plan, wait for the next newest-only frame containing an
    /// object matching `selector`, tap its center, and return it.
    pub fn run_actions_and_tap_next_latest_object_selector(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        selector: AgentObjectSelector,
    ) -> Result<AgentRect> {
        let rect =
            self.run_actions_and_select_next_latest_object_selector(actions, latest, selector)?;
        self.tap_rect(rect)?;
        Ok(rect)
    }

    /// Run an action plan, wait up to `timeout` for the next newest-only frame
    /// containing an object matching `selector`, tap its center, and return it.
    pub fn run_actions_and_tap_next_latest_object_selector_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        selector: AgentObjectSelector,
        timeout: Duration,
    ) -> Result<AgentRect> {
        let rect = self.run_actions_and_select_next_latest_object_selector_timeout(
            actions, latest, selector, timeout,
        )?;
        self.tap_rect(rect)?;
        Ok(rect)
    }

    /// Run an action plan, wait for the next newest-only frame containing an
    /// object matching `selector`, tap its center with a typed scrcpy pointer
    /// id, and return it.
    pub fn run_actions_and_tap_next_latest_object_selector_pointer(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        selector: AgentObjectSelector,
        pointer_id: TouchPointerId,
    ) -> Result<AgentRect> {
        let rect =
            self.run_actions_and_select_next_latest_object_selector(actions, latest, selector)?;
        self.tap_rect_pointer(pointer_id, rect)?;
        Ok(rect)
    }

    /// Run an action plan, wait up to `timeout` for the next newest-only frame
    /// containing an object matching `selector`, tap its center with a typed
    /// scrcpy pointer id, and return it.
    pub fn run_actions_and_tap_next_latest_object_selector_pointer_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        selector: AgentObjectSelector,
        pointer_id: TouchPointerId,
        timeout: Duration,
    ) -> Result<AgentRect> {
        let rect = self.run_actions_and_select_next_latest_object_selector_timeout(
            actions, latest, selector, timeout,
        )?;
        self.tap_rect_pointer(pointer_id, rect)?;
        Ok(rect)
    }

    /// Run an action plan, wait for the next newest-only frame containing an
    /// object matching `selector`, tap a relative point inside it, and return
    /// it.
    pub fn run_actions_and_tap_next_latest_object_selector_at(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        selector: AgentObjectSelector,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<AgentRect> {
        let rect =
            self.run_actions_and_select_next_latest_object_selector(actions, latest, selector)?;
        self.tap_rect_at(rect, x_bp, y_bp)?;
        Ok(rect)
    }

    /// Run an action plan, wait up to `timeout` for the next newest-only frame
    /// containing an object matching `selector`, tap a relative point inside
    /// it, and return it.
    pub fn run_actions_and_tap_next_latest_object_selector_at_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        selector: AgentObjectSelector,
        x_bp: u16,
        y_bp: u16,
        timeout: Duration,
    ) -> Result<AgentRect> {
        let rect = self.run_actions_and_select_next_latest_object_selector_timeout(
            actions, latest, selector, timeout,
        )?;
        self.tap_rect_at(rect, x_bp, y_bp)?;
        Ok(rect)
    }

    /// Run an action plan, wait for the next newest-only frame containing an
    /// object matching `selector`, tap a relative point inside it with a typed
    /// scrcpy pointer id, and return it.
    pub fn run_actions_and_tap_next_latest_object_selector_at_pointer(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        selector: AgentObjectSelector,
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<AgentRect> {
        let rect =
            self.run_actions_and_select_next_latest_object_selector(actions, latest, selector)?;
        self.tap_rect_at_pointer(pointer_id, rect, x_bp, y_bp)?;
        Ok(rect)
    }

    /// Run an action plan, wait up to `timeout` for the next newest-only frame
    /// containing an object matching `selector`, tap a relative point inside
    /// it with a typed scrcpy pointer id, and return it.
    pub fn run_actions_and_tap_next_latest_object_selector_at_pointer_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        selector: AgentObjectSelector,
        pointer_id: TouchPointerId,
        anchor_bp: (u16, u16),
        timeout: Duration,
    ) -> Result<AgentRect> {
        let rect = self.run_actions_and_select_next_latest_object_selector_timeout(
            actions, latest, selector, timeout,
        )?;
        self.tap_rect_at_pointer(pointer_id, rect, anchor_bp.0, anchor_bp.1)?;
        Ok(rect)
    }

    /// Run an action plan, wait for the next newest-only frame containing at
    /// least one text region, tap the largest region's center, and return it.
    pub fn run_actions_and_tap_next_latest_largest_text_region(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
    ) -> Result<AgentRect> {
        let rect = self.run_actions_and_select_next_latest_largest_text_region(actions, latest)?;
        self.tap_rect(rect)?;
        Ok(rect)
    }

    /// Run an action plan, wait up to `timeout` for the next newest-only frame
    /// containing at least one text region, tap the largest region's center,
    /// and return it.
    pub fn run_actions_and_tap_next_latest_largest_text_region_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        timeout: Duration,
    ) -> Result<AgentRect> {
        let rect = self.run_actions_and_select_next_latest_largest_text_region_timeout(
            actions, latest, timeout,
        )?;
        self.tap_rect(rect)?;
        Ok(rect)
    }

    /// Run an action plan, wait for the next newest-only frame containing at
    /// least one text region, tap the largest region's center with a typed
    /// scrcpy pointer id, and return it.
    pub fn run_actions_and_tap_next_latest_largest_text_region_pointer(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        pointer_id: TouchPointerId,
    ) -> Result<AgentRect> {
        let rect = self.run_actions_and_select_next_latest_largest_text_region(actions, latest)?;
        self.tap_rect_pointer(pointer_id, rect)?;
        Ok(rect)
    }

    /// Run an action plan, wait up to `timeout` for the next newest-only frame
    /// containing at least one text region, tap the largest region's center
    /// with a typed scrcpy pointer id, and return it.
    pub fn run_actions_and_tap_next_latest_largest_text_region_pointer_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        pointer_id: TouchPointerId,
        timeout: Duration,
    ) -> Result<AgentRect> {
        let rect = self.run_actions_and_select_next_latest_largest_text_region_timeout(
            actions, latest, timeout,
        )?;
        self.tap_rect_pointer(pointer_id, rect)?;
        Ok(rect)
    }

    /// Run an action plan, wait for the next newest-only frame containing at
    /// least one text region, tap a relative point inside the largest region,
    /// and return it.
    pub fn run_actions_and_tap_next_latest_largest_text_region_at(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<AgentRect> {
        let rect = self.run_actions_and_select_next_latest_largest_text_region(actions, latest)?;
        self.tap_rect_at(rect, x_bp, y_bp)?;
        Ok(rect)
    }

    /// Run an action plan, wait up to `timeout` for the next newest-only frame
    /// containing at least one text region, tap a relative point inside the
    /// largest region, and return it.
    pub fn run_actions_and_tap_next_latest_largest_text_region_at_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        x_bp: u16,
        y_bp: u16,
        timeout: Duration,
    ) -> Result<AgentRect> {
        let rect = self.run_actions_and_select_next_latest_largest_text_region_timeout(
            actions, latest, timeout,
        )?;
        self.tap_rect_at(rect, x_bp, y_bp)?;
        Ok(rect)
    }

    /// Run an action plan, wait for the next newest-only frame containing at
    /// least one text region, tap a relative point inside the largest region
    /// with a typed scrcpy pointer id, and return it.
    pub fn run_actions_and_tap_next_latest_largest_text_region_at_pointer(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<AgentRect> {
        let rect = self.run_actions_and_select_next_latest_largest_text_region(actions, latest)?;
        self.tap_rect_at_pointer(pointer_id, rect, x_bp, y_bp)?;
        Ok(rect)
    }

    /// Run an action plan, wait up to `timeout` for the next newest-only frame
    /// containing at least one text region, tap a relative point inside the
    /// largest region with a typed scrcpy pointer id, and return it.
    pub fn run_actions_and_tap_next_latest_largest_text_region_at_pointer_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        pointer_id: TouchPointerId,
        anchor_bp: (u16, u16),
        timeout: Duration,
    ) -> Result<AgentRect> {
        let rect = self.run_actions_and_select_next_latest_largest_text_region_timeout(
            actions, latest, timeout,
        )?;
        self.tap_rect_at_pointer(pointer_id, rect, anchor_bp.0, anchor_bp.1)?;
        Ok(rect)
    }

    fn run_actions_and_select_next_latest_object_selector(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        selector: AgentObjectSelector,
    ) -> Result<AgentRect> {
        self.run_actions_and_select_next_latest_target(
            actions,
            latest,
            AgentTargetSelector::object_matching(selector),
        )
    }

    fn run_actions_and_select_next_latest_object_selector_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        selector: AgentObjectSelector,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.run_actions_and_select_next_latest_target_timeout(
            actions,
            latest,
            AgentTargetSelector::object_matching(selector),
            timeout,
        )
    }

    fn run_actions_and_select_next_latest_largest_text_region(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
    ) -> Result<AgentRect> {
        self.run_actions_and_select_next_latest_target(
            actions,
            latest,
            AgentTargetSelector::largest_text_region(),
        )
    }

    fn run_actions_and_select_next_latest_largest_text_region_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.run_actions_and_select_next_latest_target_timeout(
            actions,
            latest,
            AgentTargetSelector::largest_text_region(),
            timeout,
        )
    }

    fn try_run_actions_and_select_next_latest_target(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        target: AgentTargetSelector,
    ) -> Result<AgentRect> {
        let snapshot = self.try_run_actions_and_wait_for_next_latest_frame_matching(
            actions,
            latest,
            |summary| target.is_present(summary),
        )?;
        self.latest_target_rect(&snapshot, target)?
            .ok_or(Error::SessionLifecycle("latest target disappeared"))
    }

    fn try_run_actions_and_select_next_latest_target_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        target: AgentTargetSelector,
        timeout: Duration,
    ) -> Result<AgentRect> {
        let snapshot = self.try_run_actions_and_wait_for_next_latest_frame_matching_timeout(
            actions,
            latest,
            timeout,
            |summary| target.is_present(summary),
        )?;
        self.latest_target_rect(&snapshot, target)?
            .ok_or(Error::SessionLifecycle("latest target disappeared"))
    }

    fn try_run_actions_and_select_next_latest_target_after_version(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        target: AgentTargetSelector,
        after_version: u64,
    ) -> Result<AgentRect> {
        let snapshot = self.try_run_actions_and_wait_for_next_latest_frame_matching_after_version(
            actions,
            latest,
            after_version,
            |summary| target.is_present(summary),
        )?;
        self.latest_target_rect(&snapshot, target)?
            .ok_or(Error::SessionLifecycle("latest target disappeared"))
    }

    fn try_run_actions_and_select_next_latest_target_after_version_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        target: AgentTargetSelector,
        after_version: u64,
        timeout: Duration,
    ) -> Result<AgentRect> {
        let snapshot = self
            .try_run_actions_and_wait_for_next_latest_frame_matching_after_version_timeout(
                actions,
                latest,
                after_version,
                timeout,
                |summary| target.is_present(summary),
            )?;
        self.latest_target_rect(&snapshot, target)?
            .ok_or(Error::SessionLifecycle("latest target disappeared"))
    }

    fn run_actions_and_select_next_latest_target(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        target: AgentTargetSelector,
    ) -> Result<AgentRect> {
        let snapshot =
            self.run_actions_and_wait_for_next_latest_frame_matching(actions, latest, |summary| {
                target.is_present(summary)
            })?;
        self.latest_target_rect(&snapshot, target)?
            .ok_or(Error::SessionLifecycle("latest target disappeared"))
    }

    fn run_actions_and_select_next_latest_target_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        target: AgentTargetSelector,
        timeout: Duration,
    ) -> Result<AgentRect> {
        let snapshot = self.run_actions_and_wait_for_next_latest_frame_matching_timeout(
            actions,
            latest,
            timeout,
            |summary| target.is_present(summary),
        )?;
        self.latest_target_rect(&snapshot, target)?
            .ok_or(Error::SessionLifecycle("latest target disappeared"))
    }

    fn run_actions_and_select_next_latest_target_after_version(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        target: AgentTargetSelector,
        after_version: u64,
    ) -> Result<AgentRect> {
        let snapshot = self.run_actions_and_wait_for_next_latest_frame_matching_after_version(
            actions,
            latest,
            after_version,
            |summary| target.is_present(summary),
        )?;
        self.latest_target_rect(&snapshot, target)?
            .ok_or(Error::SessionLifecycle("latest target disappeared"))
    }

    fn run_actions_and_select_next_latest_target_after_version_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        target: AgentTargetSelector,
        after_version: u64,
        timeout: Duration,
    ) -> Result<AgentRect> {
        let snapshot = self
            .run_actions_and_wait_for_next_latest_frame_matching_after_version_timeout(
                actions,
                latest,
                after_version,
                timeout,
                |summary| target.is_present(summary),
            )?;
        self.latest_target_rect(&snapshot, target)?
            .ok_or(Error::SessionLifecycle("latest target disappeared"))
    }

    /// Run an action plan, wait for one checked dispatcher barrier, then read
    /// the next AI frame summary.
    pub fn run_actions_and_wait_for_frame_summary(
        &mut self,
        actions: &[AgentAction],
    ) -> Result<FrameSummary> {
        self.run_actions(actions)?;
        self.wait_for_frame_summary()
    }

    /// Run an action plan, then wait for the next scene-change frame.
    pub fn run_actions_and_wait_for_scene_change(
        &mut self,
        actions: &[AgentAction],
    ) -> Result<FrameSummary> {
        self.run_actions(actions)?;
        self.wait_for_scene_change()
    }

    /// Run an action plan, then wait for the next frame with motion vectors.
    pub fn run_actions_and_wait_for_motion(
        &mut self,
        actions: &[AgentAction],
    ) -> Result<FrameSummary> {
        self.run_actions(actions)?;
        self.wait_for_motion()
    }

    /// Run an action plan, then wait for one stable frame.
    pub fn run_actions_and_wait_for_stable_frame(
        &mut self,
        actions: &[AgentAction],
    ) -> Result<FrameSummary> {
        self.run_actions_and_wait_for_stable_frames(actions, 1)
    }

    /// Run an action plan, then wait for `consecutive` stable frames.
    pub fn run_actions_and_wait_for_stable_frames(
        &mut self,
        actions: &[AgentAction],
        consecutive: usize,
    ) -> Result<FrameSummary> {
        self.run_actions(actions)?;
        self.wait_for_stable_frames(consecutive)
    }

    /// Run an action plan, wait for one checked dispatcher barrier, then read
    /// at most `max_summaries` frame summaries until `predicate` accepts one.
    pub fn run_actions_and_wait_for_frame_summary_matching_with_limit(
        &mut self,
        actions: &[AgentAction],
        max_summaries: usize,
        predicate: impl FnMut(&FrameSummary) -> bool,
    ) -> Result<Option<FrameSummary>> {
        self.run_actions(actions)?;
        self.wait_for_frame_summary_matching_with_limit(max_summaries, predicate)
    }

    /// Run an action plan, then wait for a frame with
    /// `frame_seq > min_frame_seq`.
    pub fn run_actions_and_wait_for_frame_summary_after_seq(
        &mut self,
        actions: &[AgentAction],
        min_frame_seq: u32,
    ) -> Result<FrameSummary> {
        self.run_actions(actions)?;
        self.wait_for_frame_summary_after_seq(min_frame_seq)
    }

    /// Run an action plan, then inspect at most `max_summaries` frame summaries
    /// until one has `frame_seq > min_frame_seq`.
    pub fn run_actions_and_wait_for_frame_summary_after_seq_with_limit(
        &mut self,
        actions: &[AgentAction],
        min_frame_seq: u32,
        max_summaries: usize,
    ) -> Result<Option<FrameSummary>> {
        self.run_actions(actions)?;
        self.wait_for_frame_summary_after_seq_with_limit(min_frame_seq, max_summaries)
    }

    /// Run an action plan, then wait for a frame with
    /// `timestamp_ms > min_timestamp_ms`.
    pub fn run_actions_and_wait_for_frame_summary_after_timestamp(
        &mut self,
        actions: &[AgentAction],
        min_timestamp_ms: u64,
    ) -> Result<FrameSummary> {
        self.run_actions(actions)?;
        self.wait_for_frame_summary_after_timestamp(min_timestamp_ms)
    }

    /// Run an action plan, then inspect at most `max_summaries` frame summaries
    /// until one has `timestamp_ms > min_timestamp_ms`.
    pub fn run_actions_and_wait_for_frame_summary_after_timestamp_with_limit(
        &mut self,
        actions: &[AgentAction],
        min_timestamp_ms: u64,
        max_summaries: usize,
    ) -> Result<Option<FrameSummary>> {
        self.run_actions(actions)?;
        self.wait_for_frame_summary_after_timestamp_with_limit(min_timestamp_ms, max_summaries)
    }

    /// Run an action plan, then read at most `max_summaries` frame summaries
    /// until the next scene-change frame is observed.
    pub fn run_actions_and_wait_for_scene_change_with_limit(
        &mut self,
        actions: &[AgentAction],
        max_summaries: usize,
    ) -> Result<Option<FrameSummary>> {
        self.run_actions(actions)?;
        self.wait_for_scene_change_with_limit(max_summaries)
    }

    /// Run an action plan, then read at most `max_summaries` frame summaries
    /// until a frame with motion vectors is observed.
    pub fn run_actions_and_wait_for_motion_with_limit(
        &mut self,
        actions: &[AgentAction],
        max_summaries: usize,
    ) -> Result<Option<FrameSummary>> {
        self.run_actions(actions)?;
        self.wait_for_motion_with_limit(max_summaries)
    }

    /// Run an action plan, then read at most `max_summaries` frame summaries
    /// until one stable frame is observed.
    pub fn run_actions_and_wait_for_stable_frame_with_limit(
        &mut self,
        actions: &[AgentAction],
        max_summaries: usize,
    ) -> Result<Option<FrameSummary>> {
        self.run_actions_and_wait_for_stable_frames_with_limit(actions, 1, max_summaries)
    }

    /// Run an action plan, then read at most `max_summaries` frame summaries
    /// until `consecutive` stable frames are observed.
    pub fn run_actions_and_wait_for_stable_frames_with_limit(
        &mut self,
        actions: &[AgentAction],
        consecutive: usize,
        max_summaries: usize,
    ) -> Result<Option<FrameSummary>> {
        self.run_actions(actions)?;
        self.wait_for_stable_frames_with_limit(consecutive, max_summaries)
    }

    /// Read messages until the next clipboard payload is observed.
    ///
    /// Live callers should configure a read timeout on the underlying stream if
    /// they need a bounded wait.
    pub fn wait_for_clipboard(&mut self) -> Result<String> {
        loop {
            if let DeviceEvent::Native(DeviceMessage::Clipboard(text)) = self
                .recv_device_event()
                .map_err(|e| io_to_wait_error(e, "clipboard"))?
            {
                return Ok(text);
            }
        }
    }

    /// Request the current device clipboard and wait for the clipboard payload.
    pub fn get_clipboard_and_wait(&mut self, copy_key: u8) -> Result<String> {
        self.request_clipboard(copy_key)?;
        self.flush()?;
        self.wait_for_clipboard()
    }

    /// Request the current device clipboard with a typed copy-key and wait.
    pub fn get_clipboard_and_wait_key(&mut self, copy_key: ClipboardCopyKey) -> Result<String> {
        self.get_clipboard_and_wait(copy_key.value())
    }

    /// Run an action plan, then wait for the next clipboard payload.
    pub fn run_actions_and_wait_for_clipboard(
        &mut self,
        actions: &[AgentAction],
    ) -> Result<String> {
        self.run_actions(actions)?;
        self.wait_for_clipboard()
    }

    /// Run an action plan, request the current device clipboard, then wait for
    /// the clipboard payload.
    ///
    /// The action plan and request share one checked dispatcher barrier.
    pub fn run_actions_and_get_clipboard_and_wait(
        &mut self,
        actions: &[AgentAction],
        copy_key: u8,
    ) -> Result<String> {
        self.queue_actions(actions)?;
        self.request_clipboard(copy_key)?;
        self.flush()?;
        self.wait_for_clipboard()
    }

    /// Run an action plan, request the current device clipboard with a typed
    /// copy-key, then wait for the clipboard payload.
    pub fn run_actions_and_get_clipboard_and_wait_key(
        &mut self,
        actions: &[AgentAction],
        copy_key: ClipboardCopyKey,
    ) -> Result<String> {
        self.run_actions_and_get_clipboard_and_wait(actions, copy_key.value())
    }

    /// Read messages until the matching clipboard ACK sequence is observed.
    ///
    /// Live callers should configure a read timeout on the underlying stream if
    /// they need a bounded wait.
    pub fn wait_for_clipboard_ack(&mut self, sequence: u64) -> Result<()> {
        loop {
            match self
                .recv_device_event()
                .map_err(|e| io_to_wait_error(e, "clipboard ack"))?
            {
                DeviceEvent::Native(DeviceMessage::AckClipboard { sequence: got })
                    if got == sequence =>
                {
                    return Ok(())
                }
                _ => {}
            }
        }
    }

    /// Run an action plan, then wait for the matching clipboard ACK sequence.
    pub fn run_actions_and_wait_for_clipboard_ack(
        &mut self,
        actions: &[AgentAction],
        sequence: u64,
    ) -> Result<()> {
        self.run_actions(actions)?;
        self.wait_for_clipboard_ack(sequence)
    }

    /// Read events until the next AI frame summary is observed.
    ///
    /// Native scrcpy messages and unknown extension envelopes are skipped.
    pub fn wait_for_frame_summary(&mut self) -> Result<FrameSummary> {
        loop {
            if let DeviceEvent::FrameSummary(summary) = self
                .recv_device_event()
                .map_err(|e| io_to_wait_error(e, "frame summary"))?
            {
                return Ok(summary);
            }
        }
    }

    /// Read frame summaries until `predicate` accepts one.
    ///
    /// Native scrcpy messages, AI stats, and unknown extension envelopes are
    /// skipped by the underlying mixed-event reader.
    pub fn wait_for_frame_summary_matching(
        &mut self,
        mut predicate: impl FnMut(&FrameSummary) -> bool,
    ) -> Result<FrameSummary> {
        loop {
            let summary = self.wait_for_frame_summary()?;
            if predicate(&summary) {
                return Ok(summary);
            }
        }
    }

    /// Read at most `max_summaries` frame summaries until `predicate` accepts
    /// one.
    ///
    /// This bounds the number of AI frame summaries inspected, not wall-clock
    /// time. Native scrcpy messages, AI stats, and unknown extension envelopes
    /// are still skipped by the underlying mixed-event reader.
    pub fn wait_for_frame_summary_matching_with_limit(
        &mut self,
        max_summaries: usize,
        mut predicate: impl FnMut(&FrameSummary) -> bool,
    ) -> Result<Option<FrameSummary>> {
        for _ in 0..max_summaries {
            let summary = self.wait_for_frame_summary()?;
            if predicate(&summary) {
                return Ok(Some(summary));
            }
        }
        Ok(None)
    }

    /// Read frame summaries until one has `frame_seq > min_frame_seq`.
    ///
    /// This is useful after an agent already observed a frame and needs to skip
    /// stale summaries still buffered in the event stream.
    pub fn wait_for_frame_summary_after_seq(&mut self, min_frame_seq: u32) -> Result<FrameSummary> {
        self.wait_for_frame_summary_matching(|summary| summary.frame_seq > min_frame_seq)
    }

    /// Read at most `max_summaries` frame summaries until one has
    /// `frame_seq > min_frame_seq`.
    pub fn wait_for_frame_summary_after_seq_with_limit(
        &mut self,
        min_frame_seq: u32,
        max_summaries: usize,
    ) -> Result<Option<FrameSummary>> {
        self.wait_for_frame_summary_matching_with_limit(max_summaries, |summary| {
            summary.frame_seq > min_frame_seq
        })
    }

    /// Read frame summaries until one has `timestamp_ms > min_timestamp_ms`.
    pub fn wait_for_frame_summary_after_timestamp(
        &mut self,
        min_timestamp_ms: u64,
    ) -> Result<FrameSummary> {
        self.wait_for_frame_summary_matching(|summary| summary.timestamp_ms > min_timestamp_ms)
    }

    /// Read at most `max_summaries` frame summaries until one has
    /// `timestamp_ms > min_timestamp_ms`.
    pub fn wait_for_frame_summary_after_timestamp_with_limit(
        &mut self,
        min_timestamp_ms: u64,
        max_summaries: usize,
    ) -> Result<Option<FrameSummary>> {
        self.wait_for_frame_summary_matching_with_limit(max_summaries, |summary| {
            summary.timestamp_ms > min_timestamp_ms
        })
    }

    /// Read frame summaries until the next scene-change frame is observed.
    pub fn wait_for_scene_change(&mut self) -> Result<FrameSummary> {
        self.wait_for_frame_summary_matching(FrameSummary::is_scene_change)
    }

    /// Read at most `max_summaries` frame summaries until the next
    /// scene-change frame is observed.
    pub fn wait_for_scene_change_with_limit(
        &mut self,
        max_summaries: usize,
    ) -> Result<Option<FrameSummary>> {
        self.wait_for_frame_summary_matching_with_limit(
            max_summaries,
            FrameSummary::is_scene_change,
        )
    }

    /// Read frame summaries until the next frame with motion vectors is
    /// observed.
    pub fn wait_for_motion(&mut self) -> Result<FrameSummary> {
        self.wait_for_frame_summary_matching(FrameSummary::is_moving)
    }

    /// Read at most `max_summaries` frame summaries until the next frame with
    /// motion vectors is observed.
    pub fn wait_for_motion_with_limit(
        &mut self,
        max_summaries: usize,
    ) -> Result<Option<FrameSummary>> {
        self.wait_for_frame_summary_matching_with_limit(max_summaries, FrameSummary::is_moving)
    }

    /// Read frame summaries until the next stable frame is observed.
    ///
    /// A stable frame has no scene-change flag and no motion vectors.
    pub fn wait_for_stable_frame(&mut self) -> Result<FrameSummary> {
        self.wait_for_stable_frames(1)
    }

    /// Read at most `max_summaries` frame summaries until the next stable frame
    /// is observed.
    pub fn wait_for_stable_frame_with_limit(
        &mut self,
        max_summaries: usize,
    ) -> Result<Option<FrameSummary>> {
        self.wait_for_stable_frames_with_limit(1, max_summaries)
    }

    /// Read frame summaries until `consecutive` stable frames have been
    /// observed, then return the final stable frame in that run.
    pub fn wait_for_stable_frames(&mut self, consecutive: usize) -> Result<FrameSummary> {
        if consecutive == 0 {
            return Err(Error::SessionLifecycle(
                "stable frame count must be nonzero",
            ));
        }

        let mut stable_frames = 0usize;
        loop {
            let summary = self.wait_for_frame_summary()?;
            if frame_summary_is_stable(&summary) {
                stable_frames += 1;
                if stable_frames >= consecutive {
                    return Ok(summary);
                }
            } else {
                stable_frames = 0;
            }
        }
    }

    /// Read at most `max_summaries` frame summaries until `consecutive` stable
    /// frames have been observed, then return the final stable frame in that
    /// run.
    pub fn wait_for_stable_frames_with_limit(
        &mut self,
        consecutive: usize,
        max_summaries: usize,
    ) -> Result<Option<FrameSummary>> {
        if consecutive == 0 {
            return Err(Error::SessionLifecycle(
                "stable frame count must be nonzero",
            ));
        }

        let mut stable_frames = 0usize;
        for _ in 0..max_summaries {
            let summary = self.wait_for_frame_summary()?;
            if frame_summary_is_stable(&summary) {
                stable_frames += 1;
                if stable_frames >= consecutive {
                    return Ok(Some(summary));
                }
            } else {
                stable_frames = 0;
            }
        }
        Ok(None)
    }

    /// Read frame summaries until the indexed object detection is present.
    pub fn wait_for_object_rect(&mut self, index: usize) -> Result<AgentRect> {
        loop {
            let summary = self.wait_for_frame_summary()?;
            if let Some(rect) = AgentRect::try_from_frame_object(&summary, index)? {
                return Ok(rect);
            }
        }
    }

    /// Read at most `max_summaries` frame summaries until the indexed object
    /// detection is present.
    pub fn wait_for_object_rect_with_limit(
        &mut self,
        index: usize,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.wait_for_rect_with_limit(max_summaries, |summary| {
            AgentRect::try_from_frame_object(summary, index)
        })
    }

    /// Read frame summaries until any object detection is present, then return
    /// the highest-confidence target. Ties prefer the larger box.
    pub fn wait_for_best_object_rect(&mut self) -> Result<AgentRect> {
        self.wait_for_object_selector_rect(AgentObjectSelector::ANY)
    }

    /// Read at most `max_summaries` frame summaries until any object detection
    /// is present, then return the highest-confidence target. Ties prefer the
    /// larger box.
    pub fn wait_for_best_object_rect_with_limit(
        &mut self,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.wait_for_object_selector_rect_with_limit(AgentObjectSelector::ANY, max_summaries)
    }

    /// Read frame summaries until the requested object class is present, then
    /// return the highest-confidence target for that class.
    pub fn wait_for_best_object_class_rect(&mut self, class_id: u8) -> Result<AgentRect> {
        self.wait_for_object_selector_rect(AgentObjectSelector::class_id(class_id))
    }

    /// Read at most `max_summaries` frame summaries until the requested object
    /// class is present, then return the highest-confidence target for that
    /// class.
    pub fn wait_for_best_object_class_rect_with_limit(
        &mut self,
        class_id: u8,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.wait_for_object_selector_rect_with_limit(
            AgentObjectSelector::class_id(class_id),
            max_summaries,
        )
    }

    /// Read frame summaries until an object matching `selector` is present,
    /// then return the highest-confidence matching target. Ties prefer the
    /// larger box.
    pub fn wait_for_object_selector_rect(
        &mut self,
        selector: AgentObjectSelector,
    ) -> Result<AgentRect> {
        loop {
            let summary = self.wait_for_frame_summary()?;
            if let Some(rect) = AgentRect::try_from_best_object_matching(&summary, selector)? {
                return Ok(rect);
            }
        }
    }

    /// Read at most `max_summaries` frame summaries until an object matching
    /// `selector` is present, then return the highest-confidence matching
    /// target. Ties prefer the larger box.
    pub fn wait_for_object_selector_rect_with_limit(
        &mut self,
        selector: AgentObjectSelector,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.wait_for_rect_with_limit(max_summaries, |summary| {
            AgentRect::try_from_best_object_matching(summary, selector)
        })
    }

    /// Read frame summaries until the indexed text region is present.
    pub fn wait_for_text_region_rect(&mut self, index: usize) -> Result<AgentRect> {
        loop {
            let summary = self.wait_for_frame_summary()?;
            if let Some(rect) = AgentRect::try_from_frame_text_region(&summary, index)? {
                return Ok(rect);
            }
        }
    }

    /// Read at most `max_summaries` frame summaries until the indexed text
    /// region is present.
    pub fn wait_for_text_region_rect_with_limit(
        &mut self,
        index: usize,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.wait_for_rect_with_limit(max_summaries, |summary| {
            AgentRect::try_from_frame_text_region(summary, index)
        })
    }

    /// Read frame summaries until at least one text region is present, then
    /// return the largest target.
    pub fn wait_for_largest_text_region_rect(&mut self) -> Result<AgentRect> {
        loop {
            let summary = self.wait_for_frame_summary()?;
            if let Some(rect) = AgentRect::try_from_largest_text_region(&summary)? {
                return Ok(rect);
            }
        }
    }

    /// Read at most `max_summaries` frame summaries until at least one text
    /// region is present, then return the largest target.
    pub fn wait_for_largest_text_region_rect_with_limit(
        &mut self,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.wait_for_rect_with_limit(max_summaries, AgentRect::try_from_largest_text_region)
    }

    /// Read frame summaries until any supported object/text target selected by
    /// `target` is present.
    pub fn wait_for_target_rect(&mut self, target: AgentTargetSelector) -> Result<AgentRect> {
        loop {
            let summary = self.wait_for_frame_summary()?;
            if let Some(rect) = target.select_rect(&summary)? {
                return Ok(rect);
            }
        }
    }

    /// Read at most `max_summaries` frame summaries until any supported
    /// object/text target selected by `target` is present.
    pub fn wait_for_target_rect_with_limit(
        &mut self,
        target: AgentTargetSelector,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.wait_for_rect_with_limit(max_summaries, |summary| target.select_rect(summary))
    }

    fn wait_for_rect_with_limit(
        &mut self,
        max_summaries: usize,
        mut select: impl FnMut(&FrameSummary) -> Result<Option<AgentRect>>,
    ) -> Result<Option<AgentRect>> {
        for _ in 0..max_summaries {
            let summary = self.wait_for_frame_summary()?;
            if let Some(rect) = select(&summary)? {
                return Ok(Some(rect));
            }
        }
        Ok(None)
    }

    /// Select the indexed object target from a newest-only frame snapshot.
    pub fn latest_object_rect(
        &self,
        snapshot: &LatestFrameSummarySnapshot,
        index: usize,
    ) -> Result<Option<AgentRect>> {
        AgentRect::try_from_frame_object(&snapshot.summary, index)
    }

    /// Select the highest-confidence object target from a newest-only frame
    /// snapshot.
    pub fn latest_best_object_rect(
        &self,
        snapshot: &LatestFrameSummarySnapshot,
    ) -> Result<Option<AgentRect>> {
        AgentRect::try_from_best_object(&snapshot.summary)
    }

    /// Select the highest-confidence object target matching `selector` from a
    /// newest-only frame snapshot.
    pub fn latest_object_selector_rect(
        &self,
        snapshot: &LatestFrameSummarySnapshot,
        selector: AgentObjectSelector,
    ) -> Result<Option<AgentRect>> {
        AgentRect::try_from_best_object_matching(&snapshot.summary, selector)
    }

    /// Select the indexed text target from a newest-only frame snapshot.
    pub fn latest_text_region_rect(
        &self,
        snapshot: &LatestFrameSummarySnapshot,
        index: usize,
    ) -> Result<Option<AgentRect>> {
        AgentRect::try_from_frame_text_region(&snapshot.summary, index)
    }

    /// Select the largest text target from a newest-only frame snapshot.
    pub fn latest_largest_text_region_rect(
        &self,
        snapshot: &LatestFrameSummarySnapshot,
    ) -> Result<Option<AgentRect>> {
        AgentRect::try_from_largest_text_region(&snapshot.summary)
    }

    /// Select any supported object/text target from a newest-only frame
    /// snapshot.
    pub fn latest_target_rect(
        &self,
        snapshot: &LatestFrameSummarySnapshot,
        target: AgentTargetSelector,
    ) -> Result<Option<AgentRect>> {
        target.select_rect(&snapshot.summary)
    }

    /// Tap the center of any supported object/text target from a newest-only
    /// frame snapshot, if present.
    pub fn tap_latest_target(
        &self,
        snapshot: &LatestFrameSummarySnapshot,
        target: AgentTargetSelector,
    ) -> Result<Option<AgentRect>> {
        let rect = self.latest_target_rect(snapshot, target)?;
        self.tap_latest_optional_rect(rect)
    }

    /// Tap a relative point inside any supported object/text target from a
    /// newest-only frame snapshot, if present.
    pub fn tap_latest_target_at(
        &self,
        snapshot: &LatestFrameSummarySnapshot,
        target: AgentTargetSelector,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<Option<AgentRect>> {
        let rect = self.latest_target_rect(snapshot, target)?;
        self.tap_latest_optional_rect_at(rect, x_bp, y_bp)
    }

    /// Tap the center of any supported object/text target from a newest-only
    /// frame snapshot with a typed scrcpy pointer id, if present.
    pub fn tap_latest_target_pointer(
        &self,
        snapshot: &LatestFrameSummarySnapshot,
        target: AgentTargetSelector,
        pointer_id: TouchPointerId,
    ) -> Result<Option<AgentRect>> {
        let rect = self.latest_target_rect(snapshot, target)?;
        self.tap_latest_optional_rect_pointer(rect, pointer_id)
    }

    /// Tap a relative point inside any supported object/text target from a
    /// newest-only frame snapshot with a typed scrcpy pointer id, if present.
    pub fn tap_latest_target_at_pointer(
        &self,
        snapshot: &LatestFrameSummarySnapshot,
        target: AgentTargetSelector,
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<Option<AgentRect>> {
        let rect = self.latest_target_rect(snapshot, target)?;
        self.tap_latest_optional_rect_at_pointer(rect, pointer_id, x_bp, y_bp)
    }

    /// Tap the center of the highest-confidence object matching `selector`
    /// from a newest-only frame snapshot, if present.
    pub fn tap_latest_object_selector(
        &self,
        snapshot: &LatestFrameSummarySnapshot,
        selector: AgentObjectSelector,
    ) -> Result<Option<AgentRect>> {
        let rect = self.latest_object_selector_rect(snapshot, selector)?;
        self.tap_latest_optional_rect(rect)
    }

    /// Tap a relative point inside the highest-confidence object matching
    /// `selector` from a newest-only frame snapshot, if present.
    pub fn tap_latest_object_selector_at(
        &self,
        snapshot: &LatestFrameSummarySnapshot,
        selector: AgentObjectSelector,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<Option<AgentRect>> {
        let rect = self.latest_object_selector_rect(snapshot, selector)?;
        self.tap_latest_optional_rect_at(rect, x_bp, y_bp)
    }

    /// Tap a relative point inside the highest-confidence object matching
    /// `selector` from a newest-only frame snapshot with a typed scrcpy pointer
    /// id, if present.
    pub fn tap_latest_object_selector_at_pointer(
        &self,
        snapshot: &LatestFrameSummarySnapshot,
        selector: AgentObjectSelector,
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<Option<AgentRect>> {
        let rect = self.latest_object_selector_rect(snapshot, selector)?;
        self.tap_latest_optional_rect_at_pointer(rect, pointer_id, x_bp, y_bp)
    }

    /// Tap the center of the largest text region from a newest-only frame
    /// snapshot, if present.
    pub fn tap_latest_largest_text_region(
        &self,
        snapshot: &LatestFrameSummarySnapshot,
    ) -> Result<Option<AgentRect>> {
        let rect = self.latest_largest_text_region_rect(snapshot)?;
        self.tap_latest_optional_rect(rect)
    }

    /// Tap a relative point inside the largest text region from a newest-only
    /// frame snapshot, if present.
    pub fn tap_latest_largest_text_region_at(
        &self,
        snapshot: &LatestFrameSummarySnapshot,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<Option<AgentRect>> {
        let rect = self.latest_largest_text_region_rect(snapshot)?;
        self.tap_latest_optional_rect_at(rect, x_bp, y_bp)
    }

    /// Tap a relative point inside the largest text region from a newest-only
    /// frame snapshot with a typed scrcpy pointer id, if present.
    pub fn tap_latest_largest_text_region_at_pointer(
        &self,
        snapshot: &LatestFrameSummarySnapshot,
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<Option<AgentRect>> {
        let rect = self.latest_largest_text_region_rect(snapshot)?;
        self.tap_latest_optional_rect_at_pointer(rect, pointer_id, x_bp, y_bp)
    }

    /// Select any supported object/text target from a one-read latest-frame
    /// observation. Returns `None` if the observation has no snapshot or the
    /// target is absent.
    pub fn latest_observation_target_rect(
        &self,
        observation: &LatestFrameSummaryObservation,
        target: AgentTargetSelector,
    ) -> Result<Option<AgentRect>> {
        let Some(snapshot) = observation.snapshot() else {
            return Ok(None);
        };
        self.latest_target_rect(snapshot, target)
    }

    /// Tap the center of any supported object/text target from a one-read
    /// latest-frame observation, if present.
    pub fn tap_latest_observation_target(
        &self,
        observation: &LatestFrameSummaryObservation,
        target: AgentTargetSelector,
    ) -> Result<Option<AgentRect>> {
        let rect = self.latest_observation_target_rect(observation, target)?;
        self.tap_latest_optional_rect(rect)
    }

    /// Tap a relative point inside any supported object/text target from a
    /// one-read latest-frame observation, if present.
    pub fn tap_latest_observation_target_at(
        &self,
        observation: &LatestFrameSummaryObservation,
        target: AgentTargetSelector,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<Option<AgentRect>> {
        let rect = self.latest_observation_target_rect(observation, target)?;
        self.tap_latest_optional_rect_at(rect, x_bp, y_bp)
    }

    /// Tap the center of any supported object/text target from a one-read
    /// latest-frame observation with a typed scrcpy pointer id, if present.
    pub fn tap_latest_observation_target_pointer(
        &self,
        observation: &LatestFrameSummaryObservation,
        target: AgentTargetSelector,
        pointer_id: TouchPointerId,
    ) -> Result<Option<AgentRect>> {
        let rect = self.latest_observation_target_rect(observation, target)?;
        self.tap_latest_optional_rect_pointer(rect, pointer_id)
    }

    /// Tap a relative point inside any supported object/text target from a
    /// one-read latest-frame observation with a typed scrcpy pointer id, if
    /// present.
    pub fn tap_latest_observation_target_at_pointer(
        &self,
        observation: &LatestFrameSummaryObservation,
        target: AgentTargetSelector,
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<Option<AgentRect>> {
        let rect = self.latest_observation_target_rect(observation, target)?;
        self.tap_latest_optional_rect_at_pointer(rect, pointer_id, x_bp, y_bp)
    }

    fn tap_latest_optional_rect(&self, rect: Option<AgentRect>) -> Result<Option<AgentRect>> {
        let Some(rect) = rect else {
            return Ok(None);
        };
        self.tap_rect(rect)?;
        Ok(Some(rect))
    }

    fn tap_latest_optional_rect_at(
        &self,
        rect: Option<AgentRect>,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<Option<AgentRect>> {
        let Some(rect) = rect else {
            return Ok(None);
        };
        self.tap_rect_at(rect, x_bp, y_bp)?;
        Ok(Some(rect))
    }

    fn tap_latest_optional_rect_pointer(
        &self,
        rect: Option<AgentRect>,
        pointer_id: TouchPointerId,
    ) -> Result<Option<AgentRect>> {
        let Some(rect) = rect else {
            return Ok(None);
        };
        self.tap_rect_pointer(pointer_id, rect)?;
        Ok(Some(rect))
    }

    fn tap_latest_optional_rect_at_pointer(
        &self,
        rect: Option<AgentRect>,
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<Option<AgentRect>> {
        let Some(rect) = rect else {
            return Ok(None);
        };
        self.tap_rect_at_pointer(pointer_id, rect, x_bp, y_bp)?;
        Ok(Some(rect))
    }

    fn tap_optional_rect(&mut self, rect: Option<AgentRect>) -> Result<Option<AgentRect>> {
        let Some(rect) = rect else {
            return Ok(None);
        };
        self.tap_rect(rect)?;
        Ok(Some(rect))
    }

    fn tap_optional_rect_pointer(
        &mut self,
        rect: Option<AgentRect>,
        pointer_id: TouchPointerId,
    ) -> Result<Option<AgentRect>> {
        let Some(rect) = rect else {
            return Ok(None);
        };
        self.tap_rect_pointer(pointer_id, rect)?;
        Ok(Some(rect))
    }

    fn tap_optional_rect_at(
        &mut self,
        rect: Option<AgentRect>,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<Option<AgentRect>> {
        let Some(rect) = rect else {
            return Ok(None);
        };
        self.tap_rect_at(rect, x_bp, y_bp)?;
        Ok(Some(rect))
    }

    fn tap_optional_rect_at_pointer(
        &mut self,
        rect: Option<AgentRect>,
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<Option<AgentRect>> {
        let Some(rect) = rect else {
            return Ok(None);
        };
        self.tap_rect_at_pointer(pointer_id, rect, x_bp, y_bp)?;
        Ok(Some(rect))
    }

    /// Wait for any supported object/text target selected by `target`, tap its
    /// center, and return it.
    pub fn tap_next_target(&mut self, target: AgentTargetSelector) -> Result<AgentRect> {
        let rect = self.wait_for_target_rect(target)?;
        self.tap_rect(rect)?;
        Ok(rect)
    }

    /// Wait for any supported object/text target selected by `target`, tap its
    /// center with a typed scrcpy pointer id, and return it.
    pub fn tap_next_target_pointer(
        &mut self,
        target: AgentTargetSelector,
        pointer_id: TouchPointerId,
    ) -> Result<AgentRect> {
        let rect = self.wait_for_target_rect(target)?;
        self.tap_rect_pointer(pointer_id, rect)?;
        Ok(rect)
    }

    /// Wait for any supported object/text target selected by `target`, tap a
    /// relative point inside it, and return it.
    pub fn tap_next_target_at(
        &mut self,
        target: AgentTargetSelector,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<AgentRect> {
        let rect = self.wait_for_target_rect(target)?;
        self.tap_rect_at(rect, x_bp, y_bp)?;
        Ok(rect)
    }

    /// Wait for any supported object/text target selected by `target`, tap a
    /// relative point inside it with a typed scrcpy pointer id, and return it.
    pub fn tap_next_target_at_pointer(
        &mut self,
        target: AgentTargetSelector,
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<AgentRect> {
        let rect = self.wait_for_target_rect(target)?;
        self.tap_rect_at_pointer(pointer_id, rect, x_bp, y_bp)?;
        Ok(rect)
    }

    /// Wait for any supported object/text target selected by `target` within
    /// `max_summaries` frame summaries, tap its center if found, and return it.
    pub fn tap_next_target_with_limit(
        &mut self,
        target: AgentTargetSelector,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        let rect = self.wait_for_target_rect_with_limit(target, max_summaries)?;
        self.tap_optional_rect(rect)
    }

    /// Wait for any supported object/text target selected by `target` within
    /// `max_summaries` frame summaries, tap its center with a typed scrcpy
    /// pointer id if found, and return it.
    pub fn tap_next_target_pointer_with_limit(
        &mut self,
        target: AgentTargetSelector,
        pointer_id: TouchPointerId,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        let rect = self.wait_for_target_rect_with_limit(target, max_summaries)?;
        self.tap_optional_rect_pointer(rect, pointer_id)
    }

    /// Wait for any supported object/text target selected by `target` within
    /// `max_summaries` frame summaries, tap a relative point inside it if
    /// found, and return it.
    pub fn tap_next_target_at_with_limit(
        &mut self,
        target: AgentTargetSelector,
        x_bp: u16,
        y_bp: u16,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        let rect = self.wait_for_target_rect_with_limit(target, max_summaries)?;
        self.tap_optional_rect_at(rect, x_bp, y_bp)
    }

    /// Wait for any supported object/text target selected by `target` within
    /// `max_summaries` frame summaries, tap a relative point inside it with a
    /// typed scrcpy pointer id if found, and return it.
    pub fn tap_next_target_at_pointer_with_limit(
        &mut self,
        target: AgentTargetSelector,
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        let rect = self.wait_for_target_rect_with_limit(target, max_summaries)?;
        self.tap_optional_rect_at_pointer(rect, pointer_id, x_bp, y_bp)
    }

    /// Run an action plan, then wait for any supported object/text target
    /// selected by `target`.
    pub fn run_actions_and_wait_for_target_rect(
        &mut self,
        actions: &[AgentAction],
        target: AgentTargetSelector,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.wait_for_target_rect(target)
    }

    /// Run an action plan, then inspect at most `max_summaries` frame summaries
    /// for any supported object/text target selected by `target`.
    pub fn run_actions_and_wait_for_target_rect_with_limit(
        &mut self,
        actions: &[AgentAction],
        target: AgentTargetSelector,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.run_actions(actions)?;
        self.wait_for_target_rect_with_limit(target, max_summaries)
    }

    /// Run an action plan, then wait for the indexed object detection.
    pub fn run_actions_and_wait_for_object_rect(
        &mut self,
        actions: &[AgentAction],
        index: usize,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.wait_for_object_rect(index)
    }

    /// Run an action plan, then wait for the highest-confidence object target.
    pub fn run_actions_and_wait_for_best_object_rect(
        &mut self,
        actions: &[AgentAction],
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.wait_for_best_object_rect()
    }

    /// Run an action plan, then wait for the highest-confidence target for
    /// `class_id`.
    pub fn run_actions_and_wait_for_best_object_class_rect(
        &mut self,
        actions: &[AgentAction],
        class_id: u8,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.wait_for_best_object_class_rect(class_id)
    }

    /// Run an action plan, then wait for the highest-confidence object target
    /// matching `selector`.
    pub fn run_actions_and_wait_for_object_selector_rect(
        &mut self,
        actions: &[AgentAction],
        selector: AgentObjectSelector,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.wait_for_object_selector_rect(selector)
    }

    /// Run an action plan, then wait for the indexed text region.
    pub fn run_actions_and_wait_for_text_region_rect(
        &mut self,
        actions: &[AgentAction],
        index: usize,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.wait_for_text_region_rect(index)
    }

    /// Run an action plan, then wait for the largest text region.
    pub fn run_actions_and_wait_for_largest_text_region_rect(
        &mut self,
        actions: &[AgentAction],
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.wait_for_largest_text_region_rect()
    }

    /// Run an action plan, then inspect at most `max_summaries` frame summaries
    /// for the indexed object detection.
    pub fn run_actions_and_wait_for_object_rect_with_limit(
        &mut self,
        actions: &[AgentAction],
        index: usize,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.run_actions(actions)?;
        self.wait_for_object_rect_with_limit(index, max_summaries)
    }

    /// Run an action plan, then inspect at most `max_summaries` frame summaries
    /// for the highest-confidence object target.
    pub fn run_actions_and_wait_for_best_object_rect_with_limit(
        &mut self,
        actions: &[AgentAction],
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.run_actions(actions)?;
        self.wait_for_best_object_rect_with_limit(max_summaries)
    }

    /// Run an action plan, then inspect at most `max_summaries` frame summaries
    /// for the highest-confidence target for `class_id`.
    pub fn run_actions_and_wait_for_best_object_class_rect_with_limit(
        &mut self,
        actions: &[AgentAction],
        class_id: u8,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.run_actions(actions)?;
        self.wait_for_best_object_class_rect_with_limit(class_id, max_summaries)
    }

    /// Run an action plan, then inspect at most `max_summaries` frame summaries
    /// for the highest-confidence object target matching `selector`.
    pub fn run_actions_and_wait_for_object_selector_rect_with_limit(
        &mut self,
        actions: &[AgentAction],
        selector: AgentObjectSelector,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.run_actions(actions)?;
        self.wait_for_object_selector_rect_with_limit(selector, max_summaries)
    }

    /// Run an action plan, then inspect at most `max_summaries` frame summaries
    /// for the indexed text region.
    pub fn run_actions_and_wait_for_text_region_rect_with_limit(
        &mut self,
        actions: &[AgentAction],
        index: usize,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.run_actions(actions)?;
        self.wait_for_text_region_rect_with_limit(index, max_summaries)
    }

    /// Run an action plan, then inspect at most `max_summaries` frame summaries
    /// for the largest text region.
    pub fn run_actions_and_wait_for_largest_text_region_rect_with_limit(
        &mut self,
        actions: &[AgentAction],
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.run_actions(actions)?;
        self.wait_for_largest_text_region_rect_with_limit(max_summaries)
    }

    /// Run an action plan, then wait for any supported object/text target
    /// selected by `target` and tap its center.
    pub fn run_actions_and_tap_next_target(
        &mut self,
        actions: &[AgentAction],
        target: AgentTargetSelector,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_target(target)
    }

    /// Run an action plan, then wait for any supported object/text target
    /// selected by `target` and tap its center with a typed scrcpy pointer id.
    pub fn run_actions_and_tap_next_target_pointer(
        &mut self,
        actions: &[AgentAction],
        target: AgentTargetSelector,
        pointer_id: TouchPointerId,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_target_pointer(target, pointer_id)
    }

    /// Run an action plan, then wait for any supported object/text target
    /// selected by `target` and tap a relative point inside it.
    pub fn run_actions_and_tap_next_target_at(
        &mut self,
        actions: &[AgentAction],
        target: AgentTargetSelector,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_target_at(target, x_bp, y_bp)
    }

    /// Run an action plan, then wait for any supported object/text target
    /// selected by `target` and tap a relative point inside it with a typed
    /// scrcpy pointer id.
    pub fn run_actions_and_tap_next_target_at_pointer(
        &mut self,
        actions: &[AgentAction],
        target: AgentTargetSelector,
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_target_at_pointer(target, pointer_id, x_bp, y_bp)
    }

    /// Run an action plan, then wait for any supported object/text target
    /// selected by `target` within `max_summaries` frame summaries and tap its
    /// center if found.
    pub fn run_actions_and_tap_next_target_with_limit(
        &mut self,
        actions: &[AgentAction],
        target: AgentTargetSelector,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.run_actions(actions)?;
        self.tap_next_target_with_limit(target, max_summaries)
    }

    /// Run an action plan, then wait for any supported object/text target
    /// selected by `target` within `max_summaries` frame summaries and tap its
    /// center with a typed scrcpy pointer id if found.
    pub fn run_actions_and_tap_next_target_pointer_with_limit(
        &mut self,
        actions: &[AgentAction],
        target: AgentTargetSelector,
        pointer_id: TouchPointerId,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.run_actions(actions)?;
        self.tap_next_target_pointer_with_limit(target, pointer_id, max_summaries)
    }

    /// Run an action plan, then wait for any supported object/text target
    /// selected by `target` within `max_summaries` frame summaries and tap a
    /// relative point inside it if found.
    pub fn run_actions_and_tap_next_target_at_with_limit(
        &mut self,
        actions: &[AgentAction],
        target: AgentTargetSelector,
        x_bp: u16,
        y_bp: u16,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.run_actions(actions)?;
        self.tap_next_target_at_with_limit(target, x_bp, y_bp, max_summaries)
    }

    /// Run an action plan, then wait for any supported object/text target
    /// selected by `target` within `max_summaries` frame summaries and tap a
    /// relative point inside it with a typed scrcpy pointer id if found.
    pub fn run_actions_and_tap_next_target_at_pointer_with_limit(
        &mut self,
        actions: &[AgentAction],
        target: AgentTargetSelector,
        pointer_id: TouchPointerId,
        anchor_bp: (u16, u16),
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.run_actions(actions)?;
        self.tap_next_target_at_pointer_with_limit(
            target,
            pointer_id,
            anchor_bp.0,
            anchor_bp.1,
            max_summaries,
        )
    }

    /// Wait for the indexed object within `max_summaries` frame summaries, tap
    /// its center if found, and return the selected rectangle.
    pub fn tap_next_object_with_limit(
        &mut self,
        index: usize,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        let rect = self.wait_for_object_rect_with_limit(index, max_summaries)?;
        self.tap_optional_rect(rect)
    }

    /// Wait for the indexed object within `max_summaries` frame summaries, tap
    /// its center with a typed scrcpy pointer id if found, and return the
    /// selected rectangle.
    pub fn tap_next_object_pointer_with_limit(
        &mut self,
        index: usize,
        pointer_id: TouchPointerId,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        let rect = self.wait_for_object_rect_with_limit(index, max_summaries)?;
        self.tap_optional_rect_pointer(rect, pointer_id)
    }

    /// Wait for the indexed object within `max_summaries` frame summaries, tap a
    /// relative point inside it if found, and return the selected rectangle.
    pub fn tap_next_object_at_with_limit(
        &mut self,
        index: usize,
        x_bp: u16,
        y_bp: u16,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        let rect = self.wait_for_object_rect_with_limit(index, max_summaries)?;
        self.tap_optional_rect_at(rect, x_bp, y_bp)
    }

    /// Wait for the indexed object within `max_summaries` frame summaries, tap a
    /// relative point inside it with a typed scrcpy pointer id if found, and
    /// return the selected rectangle.
    pub fn tap_next_object_at_pointer_with_limit(
        &mut self,
        index: usize,
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        let rect = self.wait_for_object_rect_with_limit(index, max_summaries)?;
        self.tap_optional_rect_at_pointer(rect, pointer_id, x_bp, y_bp)
    }

    /// Wait for the best object within `max_summaries` frame summaries, tap its
    /// center if found, and return the selected rectangle.
    pub fn tap_next_best_object_with_limit(
        &mut self,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        let rect = self.wait_for_best_object_rect_with_limit(max_summaries)?;
        self.tap_optional_rect(rect)
    }

    /// Wait for the best object within `max_summaries` frame summaries, tap its
    /// center with a typed scrcpy pointer id if found, and return the selected
    /// rectangle.
    pub fn tap_next_best_object_pointer_with_limit(
        &mut self,
        pointer_id: TouchPointerId,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        let rect = self.wait_for_best_object_rect_with_limit(max_summaries)?;
        self.tap_optional_rect_pointer(rect, pointer_id)
    }

    /// Wait for the best object within `max_summaries` frame summaries, tap a
    /// relative point inside it if found, and return the selected rectangle.
    pub fn tap_next_best_object_at_with_limit(
        &mut self,
        x_bp: u16,
        y_bp: u16,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        let rect = self.wait_for_best_object_rect_with_limit(max_summaries)?;
        self.tap_optional_rect_at(rect, x_bp, y_bp)
    }

    /// Wait for the best object within `max_summaries` frame summaries, tap a
    /// relative point inside it with a typed scrcpy pointer id if found, and
    /// return the selected rectangle.
    pub fn tap_next_best_object_at_pointer_with_limit(
        &mut self,
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        let rect = self.wait_for_best_object_rect_with_limit(max_summaries)?;
        self.tap_optional_rect_at_pointer(rect, pointer_id, x_bp, y_bp)
    }

    /// Wait for the best object of `class_id` within `max_summaries` frame
    /// summaries, tap its center if found, and return the selected rectangle.
    pub fn tap_next_object_class_with_limit(
        &mut self,
        class_id: u8,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        let rect = self.wait_for_best_object_class_rect_with_limit(class_id, max_summaries)?;
        self.tap_optional_rect(rect)
    }

    /// Wait for the best object of `class_id` within `max_summaries` frame
    /// summaries, tap its center with a typed scrcpy pointer id if found, and
    /// return the selected rectangle.
    pub fn tap_next_object_class_pointer_with_limit(
        &mut self,
        class_id: u8,
        pointer_id: TouchPointerId,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        let rect = self.wait_for_best_object_class_rect_with_limit(class_id, max_summaries)?;
        self.tap_optional_rect_pointer(rect, pointer_id)
    }

    /// Wait for the best object of `class_id` within `max_summaries` frame
    /// summaries, tap a relative point inside it if found, and return the
    /// selected rectangle.
    pub fn tap_next_object_class_at_with_limit(
        &mut self,
        class_id: u8,
        x_bp: u16,
        y_bp: u16,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        let rect = self.wait_for_best_object_class_rect_with_limit(class_id, max_summaries)?;
        self.tap_optional_rect_at(rect, x_bp, y_bp)
    }

    /// Wait for the best object of `class_id` within `max_summaries` frame
    /// summaries, tap a relative point inside it with a typed scrcpy pointer id
    /// if found, and return the selected rectangle.
    pub fn tap_next_object_class_at_pointer_with_limit(
        &mut self,
        class_id: u8,
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        let rect = self.wait_for_best_object_class_rect_with_limit(class_id, max_summaries)?;
        self.tap_optional_rect_at_pointer(rect, pointer_id, x_bp, y_bp)
    }

    /// Wait for an object matching `selector` within `max_summaries` frame
    /// summaries, tap its center if found, and return the selected rectangle.
    pub fn tap_next_object_selector_with_limit(
        &mut self,
        selector: AgentObjectSelector,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        let rect = self.wait_for_object_selector_rect_with_limit(selector, max_summaries)?;
        self.tap_optional_rect(rect)
    }

    /// Wait for an object matching `selector` within `max_summaries` frame
    /// summaries, tap its center with a typed scrcpy pointer id if found, and
    /// return the selected rectangle.
    pub fn tap_next_object_selector_pointer_with_limit(
        &mut self,
        selector: AgentObjectSelector,
        pointer_id: TouchPointerId,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        let rect = self.wait_for_object_selector_rect_with_limit(selector, max_summaries)?;
        self.tap_optional_rect_pointer(rect, pointer_id)
    }

    /// Wait for an object matching `selector` within `max_summaries` frame
    /// summaries, tap a relative point inside it if found, and return the
    /// selected rectangle.
    pub fn tap_next_object_selector_at_with_limit(
        &mut self,
        selector: AgentObjectSelector,
        x_bp: u16,
        y_bp: u16,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        let rect = self.wait_for_object_selector_rect_with_limit(selector, max_summaries)?;
        self.tap_optional_rect_at(rect, x_bp, y_bp)
    }

    /// Wait for an object matching `selector` within `max_summaries` frame
    /// summaries, tap a relative point inside it with a typed scrcpy pointer id
    /// if found, and return the selected rectangle.
    pub fn tap_next_object_selector_at_pointer_with_limit(
        &mut self,
        selector: AgentObjectSelector,
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        let rect = self.wait_for_object_selector_rect_with_limit(selector, max_summaries)?;
        self.tap_optional_rect_at_pointer(rect, pointer_id, x_bp, y_bp)
    }

    /// Wait for the indexed text region within `max_summaries` frame summaries,
    /// tap its center if found, and return the selected rectangle.
    pub fn tap_next_text_region_with_limit(
        &mut self,
        index: usize,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        let rect = self.wait_for_text_region_rect_with_limit(index, max_summaries)?;
        self.tap_optional_rect(rect)
    }

    /// Wait for the indexed text region within `max_summaries` frame summaries,
    /// tap its center with a typed scrcpy pointer id if found, and return the
    /// selected rectangle.
    pub fn tap_next_text_region_pointer_with_limit(
        &mut self,
        index: usize,
        pointer_id: TouchPointerId,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        let rect = self.wait_for_text_region_rect_with_limit(index, max_summaries)?;
        self.tap_optional_rect_pointer(rect, pointer_id)
    }

    /// Wait for the indexed text region within `max_summaries` frame summaries,
    /// tap a relative point inside it if found, and return the selected
    /// rectangle.
    pub fn tap_next_text_region_at_with_limit(
        &mut self,
        index: usize,
        x_bp: u16,
        y_bp: u16,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        let rect = self.wait_for_text_region_rect_with_limit(index, max_summaries)?;
        self.tap_optional_rect_at(rect, x_bp, y_bp)
    }

    /// Wait for the indexed text region within `max_summaries` frame summaries,
    /// tap a relative point inside it with a typed scrcpy pointer id if found,
    /// and return the selected rectangle.
    pub fn tap_next_text_region_at_pointer_with_limit(
        &mut self,
        index: usize,
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        let rect = self.wait_for_text_region_rect_with_limit(index, max_summaries)?;
        self.tap_optional_rect_at_pointer(rect, pointer_id, x_bp, y_bp)
    }

    /// Wait for the largest text region within `max_summaries` frame summaries,
    /// tap its center if found, and return the selected rectangle.
    pub fn tap_next_largest_text_region_with_limit(
        &mut self,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        let rect = self.wait_for_largest_text_region_rect_with_limit(max_summaries)?;
        self.tap_optional_rect(rect)
    }

    /// Wait for the largest text region within `max_summaries` frame summaries,
    /// tap its center with a typed scrcpy pointer id if found, and return the
    /// selected rectangle.
    pub fn tap_next_largest_text_region_pointer_with_limit(
        &mut self,
        pointer_id: TouchPointerId,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        let rect = self.wait_for_largest_text_region_rect_with_limit(max_summaries)?;
        self.tap_optional_rect_pointer(rect, pointer_id)
    }

    /// Wait for the largest text region within `max_summaries` frame summaries,
    /// tap a relative point inside it if found, and return the selected
    /// rectangle.
    pub fn tap_next_largest_text_region_at_with_limit(
        &mut self,
        x_bp: u16,
        y_bp: u16,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        let rect = self.wait_for_largest_text_region_rect_with_limit(max_summaries)?;
        self.tap_optional_rect_at(rect, x_bp, y_bp)
    }

    /// Wait for the largest text region within `max_summaries` frame summaries,
    /// tap a relative point inside it with a typed scrcpy pointer id if found,
    /// and return the selected rectangle.
    pub fn tap_next_largest_text_region_at_pointer_with_limit(
        &mut self,
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        let rect = self.wait_for_largest_text_region_rect_with_limit(max_summaries)?;
        self.tap_optional_rect_at_pointer(rect, pointer_id, x_bp, y_bp)
    }

    /// Run an action plan, then wait for the indexed object within
    /// `max_summaries` frame summaries and tap its center if found.
    pub fn run_actions_and_tap_next_object_with_limit(
        &mut self,
        actions: &[AgentAction],
        index: usize,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.run_actions(actions)?;
        self.tap_next_object_with_limit(index, max_summaries)
    }

    /// Run an action plan, then wait for the indexed object within
    /// `max_summaries` frame summaries and tap its center with a typed scrcpy
    /// pointer id if found.
    pub fn run_actions_and_tap_next_object_pointer_with_limit(
        &mut self,
        actions: &[AgentAction],
        index: usize,
        pointer_id: TouchPointerId,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.run_actions(actions)?;
        self.tap_next_object_pointer_with_limit(index, pointer_id, max_summaries)
    }

    /// Run an action plan, then wait for the indexed object within
    /// `max_summaries` frame summaries and tap a relative point inside it if
    /// found.
    pub fn run_actions_and_tap_next_object_at_with_limit(
        &mut self,
        actions: &[AgentAction],
        index: usize,
        x_bp: u16,
        y_bp: u16,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.run_actions(actions)?;
        self.tap_next_object_at_with_limit(index, x_bp, y_bp, max_summaries)
    }

    /// Run an action plan, then wait for the indexed object within
    /// `max_summaries` frame summaries and tap a relative point inside it with a
    /// typed scrcpy pointer id if found.
    pub fn run_actions_and_tap_next_object_at_pointer_with_limit(
        &mut self,
        actions: &[AgentAction],
        index: usize,
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.run_actions(actions)?;
        self.tap_next_object_at_pointer_with_limit(index, pointer_id, x_bp, y_bp, max_summaries)
    }

    /// Run an action plan, then wait for the best object within `max_summaries`
    /// frame summaries and tap its center if found.
    pub fn run_actions_and_tap_next_best_object_with_limit(
        &mut self,
        actions: &[AgentAction],
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.run_actions(actions)?;
        self.tap_next_best_object_with_limit(max_summaries)
    }

    /// Run an action plan, then wait for the best object within `max_summaries`
    /// frame summaries and tap its center with a typed scrcpy pointer id if
    /// found.
    pub fn run_actions_and_tap_next_best_object_pointer_with_limit(
        &mut self,
        actions: &[AgentAction],
        pointer_id: TouchPointerId,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.run_actions(actions)?;
        self.tap_next_best_object_pointer_with_limit(pointer_id, max_summaries)
    }

    /// Run an action plan, then wait for the best object within `max_summaries`
    /// frame summaries and tap a relative point inside it if found.
    pub fn run_actions_and_tap_next_best_object_at_with_limit(
        &mut self,
        actions: &[AgentAction],
        x_bp: u16,
        y_bp: u16,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.run_actions(actions)?;
        self.tap_next_best_object_at_with_limit(x_bp, y_bp, max_summaries)
    }

    /// Run an action plan, then wait for the best object within `max_summaries`
    /// frame summaries and tap a relative point inside it with a typed scrcpy
    /// pointer id if found.
    pub fn run_actions_and_tap_next_best_object_at_pointer_with_limit(
        &mut self,
        actions: &[AgentAction],
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.run_actions(actions)?;
        self.tap_next_best_object_at_pointer_with_limit(pointer_id, x_bp, y_bp, max_summaries)
    }

    /// Run an action plan, then wait for the best object of `class_id` within
    /// `max_summaries` frame summaries and tap its center if found.
    pub fn run_actions_and_tap_next_object_class_with_limit(
        &mut self,
        actions: &[AgentAction],
        class_id: u8,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.run_actions(actions)?;
        self.tap_next_object_class_with_limit(class_id, max_summaries)
    }

    /// Run an action plan, then wait for the best object of `class_id` within
    /// `max_summaries` frame summaries and tap its center with a typed scrcpy
    /// pointer id if found.
    pub fn run_actions_and_tap_next_object_class_pointer_with_limit(
        &mut self,
        actions: &[AgentAction],
        class_id: u8,
        pointer_id: TouchPointerId,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.run_actions(actions)?;
        self.tap_next_object_class_pointer_with_limit(class_id, pointer_id, max_summaries)
    }

    /// Run an action plan, then wait for the best object of `class_id` within
    /// `max_summaries` frame summaries and tap a relative point inside it if
    /// found.
    pub fn run_actions_and_tap_next_object_class_at_with_limit(
        &mut self,
        actions: &[AgentAction],
        class_id: u8,
        x_bp: u16,
        y_bp: u16,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.run_actions(actions)?;
        self.tap_next_object_class_at_with_limit(class_id, x_bp, y_bp, max_summaries)
    }

    /// Run an action plan, then wait for the best object of `class_id` within
    /// `max_summaries` frame summaries and tap a relative point inside it with a
    /// typed scrcpy pointer id if found.
    pub fn run_actions_and_tap_next_object_class_at_pointer_with_limit(
        &mut self,
        actions: &[AgentAction],
        class_id: u8,
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.run_actions(actions)?;
        self.tap_next_object_class_at_pointer_with_limit(
            class_id,
            pointer_id,
            x_bp,
            y_bp,
            max_summaries,
        )
    }

    /// Run an action plan, then wait for an object matching `selector` within
    /// `max_summaries` frame summaries and tap its center if found.
    pub fn run_actions_and_tap_next_object_selector_with_limit(
        &mut self,
        actions: &[AgentAction],
        selector: AgentObjectSelector,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.run_actions(actions)?;
        self.tap_next_object_selector_with_limit(selector, max_summaries)
    }

    /// Run an action plan, then wait for an object matching `selector` within
    /// `max_summaries` frame summaries and tap its center with a typed scrcpy
    /// pointer id if found.
    pub fn run_actions_and_tap_next_object_selector_pointer_with_limit(
        &mut self,
        actions: &[AgentAction],
        selector: AgentObjectSelector,
        pointer_id: TouchPointerId,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.run_actions(actions)?;
        self.tap_next_object_selector_pointer_with_limit(selector, pointer_id, max_summaries)
    }

    /// Run an action plan, then wait for an object matching `selector` within
    /// `max_summaries` frame summaries and tap a relative point inside it if
    /// found.
    pub fn run_actions_and_tap_next_object_selector_at_with_limit(
        &mut self,
        actions: &[AgentAction],
        selector: AgentObjectSelector,
        x_bp: u16,
        y_bp: u16,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.run_actions(actions)?;
        self.tap_next_object_selector_at_with_limit(selector, x_bp, y_bp, max_summaries)
    }

    /// Run an action plan, then wait for an object matching `selector` within
    /// `max_summaries` frame summaries and tap a relative point inside it with
    /// a typed scrcpy pointer id if found.
    pub fn run_actions_and_tap_next_object_selector_at_pointer_with_limit(
        &mut self,
        actions: &[AgentAction],
        selector: AgentObjectSelector,
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.run_actions(actions)?;
        self.tap_next_object_selector_at_pointer_with_limit(
            selector,
            pointer_id,
            x_bp,
            y_bp,
            max_summaries,
        )
    }

    /// Run an action plan, then wait for the indexed text region within
    /// `max_summaries` frame summaries and tap its center if found.
    pub fn run_actions_and_tap_next_text_region_with_limit(
        &mut self,
        actions: &[AgentAction],
        index: usize,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.run_actions(actions)?;
        self.tap_next_text_region_with_limit(index, max_summaries)
    }

    /// Run an action plan, then wait for the indexed text region within
    /// `max_summaries` frame summaries and tap its center with a typed scrcpy
    /// pointer id if found.
    pub fn run_actions_and_tap_next_text_region_pointer_with_limit(
        &mut self,
        actions: &[AgentAction],
        index: usize,
        pointer_id: TouchPointerId,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.run_actions(actions)?;
        self.tap_next_text_region_pointer_with_limit(index, pointer_id, max_summaries)
    }

    /// Run an action plan, then wait for the indexed text region within
    /// `max_summaries` frame summaries and tap a relative point inside it if
    /// found.
    pub fn run_actions_and_tap_next_text_region_at_with_limit(
        &mut self,
        actions: &[AgentAction],
        index: usize,
        x_bp: u16,
        y_bp: u16,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.run_actions(actions)?;
        self.tap_next_text_region_at_with_limit(index, x_bp, y_bp, max_summaries)
    }

    /// Run an action plan, then wait for the indexed text region within
    /// `max_summaries` frame summaries and tap a relative point inside it with a
    /// typed scrcpy pointer id if found.
    pub fn run_actions_and_tap_next_text_region_at_pointer_with_limit(
        &mut self,
        actions: &[AgentAction],
        index: usize,
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.run_actions(actions)?;
        self.tap_next_text_region_at_pointer_with_limit(
            index,
            pointer_id,
            x_bp,
            y_bp,
            max_summaries,
        )
    }

    /// Run an action plan, then wait for the largest text region within
    /// `max_summaries` frame summaries and tap its center if found.
    pub fn run_actions_and_tap_next_largest_text_region_with_limit(
        &mut self,
        actions: &[AgentAction],
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.run_actions(actions)?;
        self.tap_next_largest_text_region_with_limit(max_summaries)
    }

    /// Run an action plan, then wait for the largest text region within
    /// `max_summaries` frame summaries and tap its center with a typed scrcpy
    /// pointer id if found.
    pub fn run_actions_and_tap_next_largest_text_region_pointer_with_limit(
        &mut self,
        actions: &[AgentAction],
        pointer_id: TouchPointerId,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.run_actions(actions)?;
        self.tap_next_largest_text_region_pointer_with_limit(pointer_id, max_summaries)
    }

    /// Run an action plan, then wait for the largest text region within
    /// `max_summaries` frame summaries and tap a relative point inside it if
    /// found.
    pub fn run_actions_and_tap_next_largest_text_region_at_with_limit(
        &mut self,
        actions: &[AgentAction],
        x_bp: u16,
        y_bp: u16,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.run_actions(actions)?;
        self.tap_next_largest_text_region_at_with_limit(x_bp, y_bp, max_summaries)
    }

    /// Run an action plan, then wait for the largest text region within
    /// `max_summaries` frame summaries and tap a relative point inside it with
    /// a typed scrcpy pointer id if found.
    pub fn run_actions_and_tap_next_largest_text_region_at_pointer_with_limit(
        &mut self,
        actions: &[AgentAction],
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.run_actions(actions)?;
        self.tap_next_largest_text_region_at_pointer_with_limit(
            pointer_id,
            x_bp,
            y_bp,
            max_summaries,
        )
    }

    /// Run an action plan, then wait for the indexed object and tap its center.
    pub fn run_actions_and_tap_next_object(
        &mut self,
        actions: &[AgentAction],
        index: usize,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_object(index)
    }

    /// Run an action plan, then wait for the indexed object and tap its center
    /// with a typed scrcpy pointer id.
    pub fn run_actions_and_tap_next_object_pointer(
        &mut self,
        actions: &[AgentAction],
        index: usize,
        pointer_id: TouchPointerId,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_object_pointer(index, pointer_id)
    }

    /// Run an action plan, then wait for the indexed object and tap a relative
    /// point inside it.
    pub fn run_actions_and_tap_next_object_at(
        &mut self,
        actions: &[AgentAction],
        index: usize,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_object_at(index, x_bp, y_bp)
    }

    /// Run an action plan, then wait for the indexed object and tap a relative
    /// point inside it with a typed scrcpy pointer id.
    pub fn run_actions_and_tap_next_object_at_pointer(
        &mut self,
        actions: &[AgentAction],
        index: usize,
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_object_at_pointer(index, pointer_id, x_bp, y_bp)
    }

    /// Run an action plan, then wait for the next best object and tap its
    /// center.
    pub fn run_actions_and_tap_next_best_object(
        &mut self,
        actions: &[AgentAction],
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_best_object()
    }

    /// Run an action plan, then wait for the next best object and tap its
    /// center with a typed scrcpy pointer id.
    pub fn run_actions_and_tap_next_best_object_pointer(
        &mut self,
        actions: &[AgentAction],
        pointer_id: TouchPointerId,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_best_object_pointer(pointer_id)
    }

    /// Run an action plan, then wait for the next best object and tap a
    /// relative point inside it.
    pub fn run_actions_and_tap_next_best_object_at(
        &mut self,
        actions: &[AgentAction],
        x_bp: u16,
        y_bp: u16,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_best_object_at(x_bp, y_bp)
    }

    /// Run an action plan, then wait for the next best object and tap a
    /// relative point inside it with a typed scrcpy pointer id.
    pub fn run_actions_and_tap_next_best_object_at_pointer(
        &mut self,
        actions: &[AgentAction],
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_best_object_at_pointer(pointer_id, x_bp, y_bp)
    }

    /// Run an action plan, then wait for the next best object of `class_id` and
    /// tap its center.
    pub fn run_actions_and_tap_next_object_class(
        &mut self,
        actions: &[AgentAction],
        class_id: u8,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_object_class(class_id)
    }

    /// Run an action plan, then wait for the next best object of `class_id` and
    /// tap its center with a typed scrcpy pointer id.
    pub fn run_actions_and_tap_next_object_class_pointer(
        &mut self,
        actions: &[AgentAction],
        class_id: u8,
        pointer_id: TouchPointerId,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_object_class_pointer(class_id, pointer_id)
    }

    /// Run an action plan, then wait for the next best object of `class_id` and
    /// tap a relative point inside it.
    pub fn run_actions_and_tap_next_object_class_at(
        &mut self,
        actions: &[AgentAction],
        class_id: u8,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_object_class_at(class_id, x_bp, y_bp)
    }

    /// Run an action plan, then wait for the next best object of `class_id` and
    /// tap a relative point inside it with a typed scrcpy pointer id.
    pub fn run_actions_and_tap_next_object_class_at_pointer(
        &mut self,
        actions: &[AgentAction],
        class_id: u8,
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_object_class_at_pointer(class_id, pointer_id, x_bp, y_bp)
    }

    /// Run an action plan, then wait for the next object matching `selector`
    /// and tap its center.
    pub fn run_actions_and_tap_next_object_selector(
        &mut self,
        actions: &[AgentAction],
        selector: AgentObjectSelector,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_object_selector(selector)
    }

    /// Run an action plan, then wait for the next object matching `selector`
    /// and tap its center with a typed scrcpy pointer id.
    pub fn run_actions_and_tap_next_object_selector_pointer(
        &mut self,
        actions: &[AgentAction],
        selector: AgentObjectSelector,
        pointer_id: TouchPointerId,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_object_selector_pointer(selector, pointer_id)
    }

    /// Run an action plan, then wait for the next object matching `selector`
    /// and tap a relative point inside it.
    pub fn run_actions_and_tap_next_object_selector_at(
        &mut self,
        actions: &[AgentAction],
        selector: AgentObjectSelector,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_object_selector_at(selector, x_bp, y_bp)
    }

    /// Run an action plan, then wait for the next object matching `selector`
    /// and tap a relative point inside it with a typed scrcpy pointer id.
    pub fn run_actions_and_tap_next_object_selector_at_pointer(
        &mut self,
        actions: &[AgentAction],
        selector: AgentObjectSelector,
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_object_selector_at_pointer(selector, pointer_id, x_bp, y_bp)
    }

    /// Run an action plan, then wait for the indexed text region and tap its
    /// center.
    pub fn run_actions_and_tap_next_text_region(
        &mut self,
        actions: &[AgentAction],
        index: usize,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_text_region(index)
    }

    /// Run an action plan, then wait for the indexed text region and tap its
    /// center with a typed scrcpy pointer id.
    pub fn run_actions_and_tap_next_text_region_pointer(
        &mut self,
        actions: &[AgentAction],
        index: usize,
        pointer_id: TouchPointerId,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_text_region_pointer(index, pointer_id)
    }

    /// Run an action plan, then wait for the indexed text region and tap a
    /// relative point inside it.
    pub fn run_actions_and_tap_next_text_region_at(
        &mut self,
        actions: &[AgentAction],
        index: usize,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_text_region_at(index, x_bp, y_bp)
    }

    /// Run an action plan, then wait for the indexed text region and tap a
    /// relative point inside it with a typed scrcpy pointer id.
    pub fn run_actions_and_tap_next_text_region_at_pointer(
        &mut self,
        actions: &[AgentAction],
        index: usize,
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_text_region_at_pointer(index, pointer_id, x_bp, y_bp)
    }

    /// Run an action plan, then wait for the next largest text region and tap
    /// its center.
    pub fn run_actions_and_tap_next_largest_text_region(
        &mut self,
        actions: &[AgentAction],
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_largest_text_region()
    }

    /// Run an action plan, then wait for the next largest text region and tap
    /// its center with a typed scrcpy pointer id.
    pub fn run_actions_and_tap_next_largest_text_region_pointer(
        &mut self,
        actions: &[AgentAction],
        pointer_id: TouchPointerId,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_largest_text_region_pointer(pointer_id)
    }

    /// Run an action plan, then wait for the next largest text region and tap a
    /// relative point inside it.
    pub fn run_actions_and_tap_next_largest_text_region_at(
        &mut self,
        actions: &[AgentAction],
        x_bp: u16,
        y_bp: u16,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_largest_text_region_at(x_bp, y_bp)
    }

    /// Run an action plan, then wait for the next largest text region and tap a
    /// relative point inside it with a typed scrcpy pointer id.
    pub fn run_actions_and_tap_next_largest_text_region_at_pointer(
        &mut self,
        actions: &[AgentAction],
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_largest_text_region_at_pointer(pointer_id, x_bp, y_bp)
    }

    /// Wait for the next object detection at `index`, tap its center, and
    /// return the selected rectangle.
    pub fn tap_next_object(&mut self, index: usize) -> Result<AgentRect> {
        let rect = self.wait_for_object_rect(index)?;
        self.tap_rect(rect)?;
        Ok(rect)
    }

    /// Wait for the next object detection at `index`, tap its center with a
    /// typed scrcpy pointer id, and return the selected rectangle.
    pub fn tap_next_object_pointer(
        &mut self,
        index: usize,
        pointer_id: TouchPointerId,
    ) -> Result<AgentRect> {
        let rect = self.wait_for_object_rect(index)?;
        self.tap_rect_pointer(pointer_id, rect)?;
        Ok(rect)
    }

    /// Wait for the next object detection at `index`, tap a relative point
    /// inside it, and return the selected rectangle.
    pub fn tap_next_object_at(&mut self, index: usize, x_bp: u16, y_bp: u16) -> Result<AgentRect> {
        let rect = self.wait_for_object_rect(index)?;
        self.tap_rect_at(rect, x_bp, y_bp)?;
        Ok(rect)
    }

    /// Wait for the next object detection at `index`, tap a relative point
    /// inside it with a typed scrcpy pointer id, and return the selected
    /// rectangle.
    pub fn tap_next_object_at_pointer(
        &mut self,
        index: usize,
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<AgentRect> {
        let rect = self.wait_for_object_rect(index)?;
        self.tap_rect_at_pointer(pointer_id, rect, x_bp, y_bp)?;
        Ok(rect)
    }

    /// Wait for the next best object target, tap its center, and return it.
    pub fn tap_next_best_object(&mut self) -> Result<AgentRect> {
        let rect = self.wait_for_best_object_rect()?;
        self.tap_rect(rect)?;
        Ok(rect)
    }

    /// Wait for the next best object target, tap its center with a typed scrcpy
    /// pointer id, and return it.
    pub fn tap_next_best_object_pointer(
        &mut self,
        pointer_id: TouchPointerId,
    ) -> Result<AgentRect> {
        let rect = self.wait_for_best_object_rect()?;
        self.tap_rect_pointer(pointer_id, rect)?;
        Ok(rect)
    }

    /// Wait for the next best object target, tap a relative point inside it,
    /// and return it.
    pub fn tap_next_best_object_at(&mut self, x_bp: u16, y_bp: u16) -> Result<AgentRect> {
        let rect = self.wait_for_best_object_rect()?;
        self.tap_rect_at(rect, x_bp, y_bp)?;
        Ok(rect)
    }

    /// Wait for the next best object target, tap a relative point inside it
    /// with a typed scrcpy pointer id, and return it.
    pub fn tap_next_best_object_at_pointer(
        &mut self,
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<AgentRect> {
        let rect = self.wait_for_best_object_rect()?;
        self.tap_rect_at_pointer(pointer_id, rect, x_bp, y_bp)?;
        Ok(rect)
    }

    /// Wait for the next best target of `class_id`, tap its center, and return
    /// it.
    pub fn tap_next_object_class(&mut self, class_id: u8) -> Result<AgentRect> {
        let rect = self.wait_for_object_selector_rect(AgentObjectSelector::class_id(class_id))?;
        self.tap_rect(rect)?;
        Ok(rect)
    }

    /// Wait for the next best target of `class_id`, tap its center with a typed
    /// scrcpy pointer id, and return it.
    pub fn tap_next_object_class_pointer(
        &mut self,
        class_id: u8,
        pointer_id: TouchPointerId,
    ) -> Result<AgentRect> {
        let rect = self.wait_for_object_selector_rect(AgentObjectSelector::class_id(class_id))?;
        self.tap_rect_pointer(pointer_id, rect)?;
        Ok(rect)
    }

    /// Wait for the next best target of `class_id`, tap a relative point inside
    /// it, and return it.
    pub fn tap_next_object_class_at(
        &mut self,
        class_id: u8,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<AgentRect> {
        let rect = self.wait_for_object_selector_rect(AgentObjectSelector::class_id(class_id))?;
        self.tap_rect_at(rect, x_bp, y_bp)?;
        Ok(rect)
    }

    /// Wait for the next best target of `class_id`, tap a relative point inside
    /// it with a typed scrcpy pointer id, and return it.
    pub fn tap_next_object_class_at_pointer(
        &mut self,
        class_id: u8,
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<AgentRect> {
        let rect = self.wait_for_object_selector_rect(AgentObjectSelector::class_id(class_id))?;
        self.tap_rect_at_pointer(pointer_id, rect, x_bp, y_bp)?;
        Ok(rect)
    }

    /// Wait for the next object matching `selector`, tap its center, and return
    /// it.
    pub fn tap_next_object_selector(&mut self, selector: AgentObjectSelector) -> Result<AgentRect> {
        let rect = self.wait_for_object_selector_rect(selector)?;
        self.tap_rect(rect)?;
        Ok(rect)
    }

    /// Wait for the next object matching `selector`, tap its center with a typed
    /// scrcpy pointer id, and return it.
    pub fn tap_next_object_selector_pointer(
        &mut self,
        selector: AgentObjectSelector,
        pointer_id: TouchPointerId,
    ) -> Result<AgentRect> {
        let rect = self.wait_for_object_selector_rect(selector)?;
        self.tap_rect_pointer(pointer_id, rect)?;
        Ok(rect)
    }

    /// Wait for the next object matching `selector`, tap a relative point inside
    /// it, and return it.
    pub fn tap_next_object_selector_at(
        &mut self,
        selector: AgentObjectSelector,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<AgentRect> {
        let rect = self.wait_for_object_selector_rect(selector)?;
        self.tap_rect_at(rect, x_bp, y_bp)?;
        Ok(rect)
    }

    /// Wait for the next object matching `selector`, tap a relative point inside
    /// it with a typed scrcpy pointer id, and return it.
    pub fn tap_next_object_selector_at_pointer(
        &mut self,
        selector: AgentObjectSelector,
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<AgentRect> {
        let rect = self.wait_for_object_selector_rect(selector)?;
        self.tap_rect_at_pointer(pointer_id, rect, x_bp, y_bp)?;
        Ok(rect)
    }

    /// Wait for the next text region at `index`, tap its center, and return it.
    pub fn tap_next_text_region(&mut self, index: usize) -> Result<AgentRect> {
        let rect = self.wait_for_text_region_rect(index)?;
        self.tap_rect(rect)?;
        Ok(rect)
    }

    /// Wait for the next text region at `index`, tap its center with a typed
    /// scrcpy pointer id, and return it.
    pub fn tap_next_text_region_pointer(
        &mut self,
        index: usize,
        pointer_id: TouchPointerId,
    ) -> Result<AgentRect> {
        let rect = self.wait_for_text_region_rect(index)?;
        self.tap_rect_pointer(pointer_id, rect)?;
        Ok(rect)
    }

    /// Wait for the next text region at `index`, tap a relative point inside it,
    /// and return it.
    pub fn tap_next_text_region_at(
        &mut self,
        index: usize,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<AgentRect> {
        let rect = self.wait_for_text_region_rect(index)?;
        self.tap_rect_at(rect, x_bp, y_bp)?;
        Ok(rect)
    }

    /// Wait for the next text region at `index`, tap a relative point inside it
    /// with a typed scrcpy pointer id, and return it.
    pub fn tap_next_text_region_at_pointer(
        &mut self,
        index: usize,
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<AgentRect> {
        let rect = self.wait_for_text_region_rect(index)?;
        self.tap_rect_at_pointer(pointer_id, rect, x_bp, y_bp)?;
        Ok(rect)
    }

    /// Wait for the next largest text region, tap its center, and return it.
    pub fn tap_next_largest_text_region(&mut self) -> Result<AgentRect> {
        let rect = self.wait_for_largest_text_region_rect()?;
        self.tap_rect(rect)?;
        Ok(rect)
    }

    /// Wait for the next largest text region, tap its center with a typed
    /// scrcpy pointer id, and return it.
    pub fn tap_next_largest_text_region_pointer(
        &mut self,
        pointer_id: TouchPointerId,
    ) -> Result<AgentRect> {
        let rect = self.wait_for_largest_text_region_rect()?;
        self.tap_rect_pointer(pointer_id, rect)?;
        Ok(rect)
    }

    /// Wait for the next largest text region, tap a relative point inside it,
    /// and return it.
    pub fn tap_next_largest_text_region_at(&mut self, x_bp: u16, y_bp: u16) -> Result<AgentRect> {
        let rect = self.wait_for_largest_text_region_rect()?;
        self.tap_rect_at(rect, x_bp, y_bp)?;
        Ok(rect)
    }

    /// Wait for the next largest text region, tap a relative point inside it
    /// with a typed scrcpy pointer id, and return it.
    pub fn tap_next_largest_text_region_at_pointer(
        &mut self,
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<AgentRect> {
        let rect = self.wait_for_largest_text_region_rect()?;
        self.tap_rect_at_pointer(pointer_id, rect, x_bp, y_bp)?;
        Ok(rect)
    }

    /// Read events until the next AI stats envelope is observed.
    ///
    /// Native scrcpy messages and unknown extension envelopes are skipped.
    pub fn wait_for_ai_stats(&mut self) -> Result<AiStats> {
        loop {
            if let DeviceEvent::AiStats(stats) = self
                .recv_device_event()
                .map_err(|e| io_to_wait_error(e, "ai stats"))?
            {
                return Ok(stats);
            }
        }
    }

    /// Set the device clipboard and wait for the matching ACK_CLIPBOARD.
    pub fn set_clipboard_and_wait_ack(
        &mut self,
        text: impl Into<String>,
        paste: bool,
    ) -> Result<u64> {
        let sequence = self.next_clipboard_sequence();
        self.set_clipboard_sequenced(sequence, text, paste)?;
        self.flush()?;
        self.wait_for_clipboard_ack(sequence)?;
        Ok(sequence)
    }

    /// Run an action plan, set the device clipboard, and wait for the matching
    /// ACK_CLIPBOARD.
    ///
    /// The action plan and SET_CLIPBOARD command share one checked dispatcher
    /// barrier before the ACK wait.
    pub fn run_actions_and_set_clipboard_and_wait_ack(
        &mut self,
        actions: &[AgentAction],
        text: impl Into<String>,
        paste: bool,
    ) -> Result<u64> {
        let sequence = self.next_clipboard_sequence();
        self.queue_actions(actions)?;
        self.set_clipboard_sequenced(sequence, text, paste)?;
        self.flush()?;
        self.wait_for_clipboard_ack(sequence)?;
        Ok(sequence)
    }

    /// Flush pending coalesced writes at a deterministic boundary.
    pub fn flush(&self) -> Result<()> {
        self.client.flush_wait().map(|_| ())
    }

    /// Access the underlying receiver for advanced read patterns.
    pub fn receiver_mut(&mut self) -> io::Result<&mut DeviceMessageReceiver<R>> {
        self.receiver
            .as_mut()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "agent receiver closed"))
    }

    /// Close the control dispatcher and recover both underlying streams.
    pub fn close(mut self) -> Result<AgentControlClosed<T, R>> {
        self.client.close();
        let dispatcher = self
            .dispatcher
            .take()
            .ok_or(Error::DispatcherDown("agent dispatcher already joined"))?;
        let transport = dispatcher.join()?;
        let reader = self
            .receiver
            .take()
            .ok_or(Error::DispatcherDown("agent receiver already taken"))?
            .into_inner();
        Ok(AgentControlClosed { transport, reader })
    }

    /// Close only the command/write side.
    ///
    /// Use this after [`Self::detach_latest_frame_summary_receiver`] has moved
    /// the reader into a background pump, or when the caller intentionally does
    /// not need to recover the reader through this agent.
    pub fn close_transport(mut self) -> Result<T> {
        self.client.close();
        let dispatcher = self
            .dispatcher
            .take()
            .ok_or(Error::DispatcherDown("agent dispatcher already joined"))?;
        dispatcher.join()
    }

    /// Checked close: report any queued command error while still recovering
    /// the underlying transport and reader.
    pub fn close_checked(mut self) -> Result<AgentControlCloseReport<T, R>> {
        let command_result = self.client.close_wait();
        let dispatcher = self
            .dispatcher
            .take()
            .ok_or(Error::DispatcherDown("agent dispatcher already joined"))?;
        let transport = dispatcher.join()?;
        let reader = self
            .receiver
            .take()
            .ok_or(Error::DispatcherDown("agent receiver already taken"))?
            .into_inner();
        Ok(AgentControlCloseReport {
            closed: AgentControlClosed { transport, reader },
            command_result,
        })
    }

    /// Checked variant of [`Self::close_transport`].
    pub fn close_transport_checked(mut self) -> Result<AgentControlCommandCloseReport<T>> {
        let command_result = self.client.close_wait();
        let dispatcher = self
            .dispatcher
            .take()
            .ok_or(Error::DispatcherDown("agent dispatcher already joined"))?;
        let transport = dispatcher.join()?;
        Ok(AgentControlCommandCloseReport {
            transport,
            command_result,
        })
    }

    fn next_clipboard_sequence(&mut self) -> u64 {
        let sequence = self.next_clipboard_sequence;
        self.next_clipboard_sequence = self.next_clipboard_sequence.wrapping_add(1).max(1);
        sequence
    }

    fn point_to_pixels(&self, point: AgentPoint) -> (i32, i32) {
        let (width, height) = self.screen_size();
        point.to_pixels(width, height)
    }

    fn queue_action(&self, action: &AgentAction) -> Result<()> {
        match action {
            AgentAction::TypeText(text) => self.client.type_text(text.clone()),
            AgentAction::TypeTextStrict(text) => self.client.type_text_strict(text.clone()),
            AgentAction::Key {
                scancode,
                pressed,
                mods,
            } => self.client.send(HidCommand::Key {
                scancode: *scancode,
                pressed: *pressed,
                mods: *mods,
            }),
            AgentAction::KeyTap { scancode, mods } => self.client.tap_key(*scancode, *mods),
            AgentAction::KeyboardChord { chord } => self.client.key_chord(*chord),
            AgentAction::KeyBatch { len, frames } => {
                self.client.send_key_batch_fixed(*len, *frames)
            }
            AgentAction::MouseMotion { dx, dy, buttons } => {
                self.client.mouse_motion(*dx, *dy, *buttons)
            }
            AgentAction::MouseButtons { buttons } => self.client.mouse_buttons(*buttons),
            AgentAction::MouseScroll { hscroll, vscroll } => {
                self.client.mouse_scroll(*hscroll as f32, *vscroll as f32)
            }
            AgentAction::MouseBatch { len, frames } => {
                self.client.send_mouse_batch_fixed(*len, *frames)
            }
            AgentAction::InjectKeycode {
                action,
                keycode,
                repeat,
                metastate,
            } => self
                .client
                .inject_keycode(*action, *keycode, *repeat, *metastate),
            AgentAction::AndroidKeyTap { keycode, metastate } => {
                self.client.tap_android_keycode(*keycode, *metastate)
            }
            AgentAction::AndroidKeyBatch { len, frames } => {
                self.client.send_android_key_batch_fixed(*len, *frames)
            }
            AgentAction::BackOrScreenOn { action } => self
                .client
                .back_or_screen_on(AndroidKeyAction::new(*action)),
            AgentAction::PressHome => self.client.press_home(),
            AgentAction::PressBack => self.client.press_back(),
            AgentAction::OpenRecents => self.client.open_recents(),
            AgentAction::VolumeUp => self.client.volume_up(),
            AgentAction::VolumeDown => self.client.volume_down(),
            AgentAction::VolumeMute => self.client.volume_mute(),
            AgentAction::Tap { x, y } => self.queue_tap(*x, *y),
            AgentAction::TapPointer { pointer_id, x, y } => {
                self.queue_tap_pointer(TouchPointerId::new(*pointer_id), *x, *y)
            }
            AgentAction::TapPoint { point } => {
                let (x, y) = self.point_to_pixels(*point);
                self.queue_tap(x, y)
            }
            AgentAction::TapPointPointer { pointer_id, point } => {
                let (x, y) = self.point_to_pixels(*point);
                self.queue_tap_pointer(TouchPointerId::new(*pointer_id), x, y)
            }
            AgentAction::TapRect { rect } => {
                let (x, y) = self.point_to_pixels(rect.center());
                self.queue_tap(x, y)
            }
            AgentAction::TapRectAt { rect, x_bp, y_bp } => {
                let (x, y) = self.point_to_pixels(rect.try_point_at_basis_points(*x_bp, *y_bp)?);
                self.queue_tap(x, y)
            }
            AgentAction::TapRectPointer { pointer_id, rect } => {
                let (x, y) = self.point_to_pixels(rect.center());
                self.queue_tap_pointer(TouchPointerId::new(*pointer_id), x, y)
            }
            AgentAction::TapRectAtPointer {
                pointer_id,
                rect,
                x_bp,
                y_bp,
            } => {
                let (x, y) = self.point_to_pixels(rect.try_point_at_basis_points(*x_bp, *y_bp)?);
                self.queue_tap_pointer(TouchPointerId::new(*pointer_id), x, y)
            }
            AgentAction::DoubleTap { x, y } => self.queue_double_tap(*x, *y),
            AgentAction::DoubleTapPointer { pointer_id, x, y } => {
                self.queue_double_tap_pointer(TouchPointerId::new(*pointer_id), *x, *y)
            }
            AgentAction::DoubleTapPoint { point } => {
                let (x, y) = self.point_to_pixels(*point);
                self.queue_double_tap(x, y)
            }
            AgentAction::DoubleTapPointPointer { pointer_id, point } => {
                let (x, y) = self.point_to_pixels(*point);
                self.queue_double_tap_pointer(TouchPointerId::new(*pointer_id), x, y)
            }
            AgentAction::DoubleTapRect { rect } => {
                let (x, y) = self.point_to_pixels(rect.center());
                self.queue_double_tap(x, y)
            }
            AgentAction::DoubleTapRectAt { rect, x_bp, y_bp } => {
                let (x, y) = self.point_to_pixels(rect.try_point_at_basis_points(*x_bp, *y_bp)?);
                self.queue_double_tap(x, y)
            }
            AgentAction::DoubleTapRectPointer { pointer_id, rect } => {
                let (x, y) = self.point_to_pixels(rect.center());
                self.queue_double_tap_pointer(TouchPointerId::new(*pointer_id), x, y)
            }
            AgentAction::DoubleTapRectAtPointer {
                pointer_id,
                rect,
                x_bp,
                y_bp,
            } => {
                let (x, y) = self.point_to_pixels(rect.try_point_at_basis_points(*x_bp, *y_bp)?);
                self.queue_double_tap_pointer(TouchPointerId::new(*pointer_id), x, y)
            }
            AgentAction::LongPress { x, y, duration } => self.queue_long_press(*x, *y, *duration),
            AgentAction::LongPressPointer {
                pointer_id,
                x,
                y,
                duration,
            } => self.queue_long_press_pointer(TouchPointerId::new(*pointer_id), *x, *y, *duration),
            AgentAction::LongPressPoint { point, duration } => {
                let (x, y) = self.point_to_pixels(*point);
                self.queue_long_press(x, y, *duration)
            }
            AgentAction::LongPressPointPointer {
                pointer_id,
                point,
                duration,
            } => {
                let (x, y) = self.point_to_pixels(*point);
                self.queue_long_press_pointer(TouchPointerId::new(*pointer_id), x, y, *duration)
            }
            AgentAction::LongPressRect { rect, duration } => {
                let (x, y) = self.point_to_pixels(rect.center());
                self.queue_long_press(x, y, *duration)
            }
            AgentAction::LongPressRectAt {
                rect,
                x_bp,
                y_bp,
                duration,
            } => {
                let (x, y) = self.point_to_pixels(rect.try_point_at_basis_points(*x_bp, *y_bp)?);
                self.queue_long_press(x, y, *duration)
            }
            AgentAction::LongPressRectPointer {
                pointer_id,
                rect,
                duration,
            } => {
                let (x, y) = self.point_to_pixels(rect.center());
                self.queue_long_press_pointer(TouchPointerId::new(*pointer_id), x, y, *duration)
            }
            AgentAction::LongPressRectAtPointer {
                pointer_id,
                rect,
                x_bp,
                y_bp,
                duration,
            } => {
                let (x, y) = self.point_to_pixels(rect.try_point_at_basis_points(*x_bp, *y_bp)?);
                self.queue_long_press_pointer(TouchPointerId::new(*pointer_id), x, y, *duration)
            }
            AgentAction::Swipe { from, to, steps } => self.queue_swipe(*from, *to, *steps),
            AgentAction::SwipePointer {
                pointer_id,
                from,
                to,
                steps,
            } => self.queue_swipe_pointer(TouchPointerId::new(*pointer_id), *from, *to, *steps),
            AgentAction::SwipePoints { from, to, steps } => self.queue_swipe(
                self.point_to_pixels(*from),
                self.point_to_pixels(*to),
                *steps,
            ),
            AgentAction::SwipePointsPointer {
                pointer_id,
                from,
                to,
                steps,
            } => self.queue_swipe_pointer(
                TouchPointerId::new(*pointer_id),
                self.point_to_pixels(*from),
                self.point_to_pixels(*to),
                *steps,
            ),
            AgentAction::SwipeRect {
                rect,
                from_x_bp,
                from_y_bp,
                to_x_bp,
                to_y_bp,
                steps,
            } => self.queue_swipe(
                self.point_to_pixels(rect.try_point_at_basis_points(*from_x_bp, *from_y_bp)?),
                self.point_to_pixels(rect.try_point_at_basis_points(*to_x_bp, *to_y_bp)?),
                *steps,
            ),
            AgentAction::SwipeRectPointer {
                pointer_id,
                rect,
                from_x_bp,
                from_y_bp,
                to_x_bp,
                to_y_bp,
                steps,
            } => self.queue_swipe_pointer(
                TouchPointerId::new(*pointer_id),
                self.point_to_pixels(rect.try_point_at_basis_points(*from_x_bp, *from_y_bp)?),
                self.point_to_pixels(rect.try_point_at_basis_points(*to_x_bp, *to_y_bp)?),
                *steps,
            ),
            AgentAction::Pinch {
                first_from,
                first_to,
                second_from,
                second_to,
                steps,
            } => self.queue_pinch(*first_from, *first_to, *second_from, *second_to, *steps),
            AgentAction::PinchPoints {
                first_from,
                first_to,
                second_from,
                second_to,
                steps,
            } => self.queue_pinch(
                self.point_to_pixels(*first_from),
                self.point_to_pixels(*first_to),
                self.point_to_pixels(*second_from),
                self.point_to_pixels(*second_to),
                *steps,
            ),
            AgentAction::Scroll {
                x,
                y,
                hscroll,
                vscroll,
                buttons,
            } => self.client.send(HidCommand::InjectScroll {
                x: *x,
                y: *y,
                hscroll: *hscroll as f32,
                vscroll: *vscroll as f32,
                buttons: *buttons,
            }),
            AgentAction::ScrollPoint {
                point,
                hscroll,
                vscroll,
                buttons,
            } => {
                let (x, y) = self.point_to_pixels(*point);
                self.client.send(HidCommand::InjectScroll {
                    x,
                    y,
                    hscroll: *hscroll as f32,
                    vscroll: *vscroll as f32,
                    buttons: *buttons,
                })
            }
            AgentAction::ScrollRect {
                rect,
                hscroll,
                vscroll,
                buttons,
            } => {
                let (x, y) = self.point_to_pixels(rect.center());
                self.client.send(HidCommand::InjectScroll {
                    x,
                    y,
                    hscroll: *hscroll as f32,
                    vscroll: *vscroll as f32,
                    buttons: *buttons,
                })
            }
            AgentAction::ScrollRectAt {
                rect,
                x_bp,
                y_bp,
                hscroll,
                vscroll,
                buttons,
            } => {
                let (x, y) = self.point_to_pixels(rect.try_point_at_basis_points(*x_bp, *y_bp)?);
                self.client.send(HidCommand::InjectScroll {
                    x,
                    y,
                    hscroll: *hscroll as f32,
                    vscroll: *vscroll as f32,
                    buttons: *buttons,
                })
            }
            AgentAction::ScrollBatch { len, frames } => {
                let mut batch = self.client.scroll_frame_batcher();
                Self::queue_agent_scroll_frames_into(&mut batch, *len, frames)?;
                batch.flush()
            }
            AgentAction::CancelTouch { pointer_id } => self
                .client
                .send(HidCommand::MultitouchCancel { id: *pointer_id }),
            AgentAction::TouchFrames { len, frames } => {
                let mut batch = self.client.touch_frame_batcher();
                Self::queue_agent_touch_frames_into(&mut batch, *len, frames)?;
                batch.flush()
            }
            AgentAction::ThreeFingerScreenshot => self.queue_three_finger_screenshot(),
            AgentAction::SetScreenSize { width, height } => self.set_screen_size(*width, *height),
            AgentAction::LaunchApp(name) => self.client.launch_app(name.clone()),
            AgentAction::SetScreenPower { on } => self.client.set_screen_power(*on),
            AgentAction::ShowNotifications => self.client.show_notifications(),
            AgentAction::ShowQuickSettings => self.client.show_quick_settings(),
            AgentAction::CollapsePanels => self.client.collapse_panels(),
            AgentAction::RotateDevice => self.client.rotate_device(),
            AgentAction::ResizeDisplay { width, height } => {
                self.client.resize_display(*width, *height)
            }
            AgentAction::SetTorch { on } => self.client.set_torch(*on),
            AgentAction::CameraZoomIn => self.client.camera_zoom_in(),
            AgentAction::CameraZoomOut => self.client.camera_zoom_out(),
            AgentAction::OpenHardKeyboardSettings => self.client.open_hard_keyboard_settings(),
            AgentAction::ResetVideo => self.client.reset_video(),
            AgentAction::AiConfig {
                flags,
                sample_interval_ms,
                feature_dim,
            } => self
                .client
                .configure_ai(*flags, *sample_interval_ms, *feature_dim),
            AgentAction::AiQuery { since_timestamp_ms } => {
                self.client.query_ai(*since_timestamp_ms)
            }
            AgentAction::AiPause => self.client.pause_ai(),
            AgentAction::SetClipboard { text, paste } => {
                self.client.set_clipboard(text.clone(), *paste)
            }
            AgentAction::SetClipboardSequenced {
                sequence,
                text,
                paste,
            } => self
                .client
                .set_clipboard_sequenced(*sequence, text.clone(), *paste),
            AgentAction::RequestClipboard { copy_key } => self.client.request_clipboard(*copy_key),
            AgentAction::GamepadButton { button, pressed } => {
                self.client.send_button(*button, *pressed)
            }
            AgentAction::GamepadButtons { buttons } => self.client.send_buttons(*buttons),
            AgentAction::GamepadFrame { frame } => self.client.send_frame(*frame),
            AgentAction::GamepadFrameUnchecked { frame } => {
                self.client.send_frame_unchecked(*frame)
            }
            AgentAction::GamepadFrameBatch { len, frames } => {
                self.client.send_frame_batch_fixed(*len, *frames)
            }
            AgentAction::GamepadFrameBatchUnchecked { len, frames } => {
                self.client.send_frame_batch_fixed_unchecked(*len, *frames)
            }
            AgentAction::GamepadPackedFrame { frame } => self.client.send_frame_packed(*frame),
            AgentAction::GamepadPackedFrameBatch { len, frames } => {
                self.client.send_frame_packed_batch_fixed(*len, *frames)
            }
            AgentAction::Wait(duration) => {
                self.flush()?;
                std::thread::sleep(*duration);
                Ok(())
            }
            AgentAction::Flush => self.flush(),
        }
    }

    fn queue_planned_action(
        &self,
        action: &AgentAction,
        batches: PlanBatchers<'_, '_>,
    ) -> Result<()> {
        let (touch_batch, key_batch, android_key_batch, mouse_batch, scroll_batch, gamepad_batch) =
            batches;
        if !matches!(
            action,
            AgentAction::Scroll { .. }
                | AgentAction::ScrollPoint { .. }
                | AgentAction::ScrollRect { .. }
                | AgentAction::ScrollRectAt { .. }
                | AgentAction::ScrollBatch { .. }
        ) {
            scroll_batch.flush()?;
        }
        match action {
            AgentAction::Tap { x, y } => {
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                Self::queue_tap_into(touch_batch, *x, *y)
            }
            AgentAction::TapPointer { pointer_id, x, y } => {
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                Self::queue_tap_pointer_into(touch_batch, TouchPointerId::new(*pointer_id), *x, *y)
            }
            AgentAction::TapPoint { point } => {
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                let (x, y) = self.point_to_pixels(*point);
                Self::queue_tap_into(touch_batch, x, y)
            }
            AgentAction::TapPointPointer { pointer_id, point } => {
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                let (x, y) = self.point_to_pixels(*point);
                Self::queue_tap_pointer_into(touch_batch, TouchPointerId::new(*pointer_id), x, y)
            }
            AgentAction::TapRect { rect } => {
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                let (x, y) = self.point_to_pixels(rect.center());
                Self::queue_tap_into(touch_batch, x, y)
            }
            AgentAction::TapRectAt { rect, x_bp, y_bp } => {
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                let (x, y) = self.point_to_pixels(rect.try_point_at_basis_points(*x_bp, *y_bp)?);
                Self::queue_tap_into(touch_batch, x, y)
            }
            AgentAction::TapRectPointer { pointer_id, rect } => {
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                let (x, y) = self.point_to_pixels(rect.center());
                Self::queue_tap_pointer_into(touch_batch, TouchPointerId::new(*pointer_id), x, y)
            }
            AgentAction::TapRectAtPointer {
                pointer_id,
                rect,
                x_bp,
                y_bp,
            } => {
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                let (x, y) = self.point_to_pixels(rect.try_point_at_basis_points(*x_bp, *y_bp)?);
                Self::queue_tap_pointer_into(touch_batch, TouchPointerId::new(*pointer_id), x, y)
            }
            AgentAction::DoubleTap { x, y } => {
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                Self::queue_double_tap_into(touch_batch, *x, *y)
            }
            AgentAction::DoubleTapPointer { pointer_id, x, y } => {
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                Self::queue_double_tap_pointer_into(
                    touch_batch,
                    TouchPointerId::new(*pointer_id),
                    *x,
                    *y,
                )
            }
            AgentAction::DoubleTapPoint { point } => {
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                let (x, y) = self.point_to_pixels(*point);
                Self::queue_double_tap_into(touch_batch, x, y)
            }
            AgentAction::DoubleTapPointPointer { pointer_id, point } => {
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                let (x, y) = self.point_to_pixels(*point);
                Self::queue_double_tap_pointer_into(
                    touch_batch,
                    TouchPointerId::new(*pointer_id),
                    x,
                    y,
                )
            }
            AgentAction::DoubleTapRect { rect } => {
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                let (x, y) = self.point_to_pixels(rect.center());
                Self::queue_double_tap_into(touch_batch, x, y)
            }
            AgentAction::DoubleTapRectAt { rect, x_bp, y_bp } => {
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                let (x, y) = self.point_to_pixels(rect.try_point_at_basis_points(*x_bp, *y_bp)?);
                Self::queue_double_tap_into(touch_batch, x, y)
            }
            AgentAction::DoubleTapRectPointer { pointer_id, rect } => {
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                let (x, y) = self.point_to_pixels(rect.center());
                Self::queue_double_tap_pointer_into(
                    touch_batch,
                    TouchPointerId::new(*pointer_id),
                    x,
                    y,
                )
            }
            AgentAction::DoubleTapRectAtPointer {
                pointer_id,
                rect,
                x_bp,
                y_bp,
            } => {
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                let (x, y) = self.point_to_pixels(rect.try_point_at_basis_points(*x_bp, *y_bp)?);
                Self::queue_double_tap_pointer_into(
                    touch_batch,
                    TouchPointerId::new(*pointer_id),
                    x,
                    y,
                )
            }
            AgentAction::Swipe { from, to, steps } => {
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                Self::queue_swipe_into(touch_batch, *from, *to, *steps)
            }
            AgentAction::SwipePointer {
                pointer_id,
                from,
                to,
                steps,
            } => {
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                Self::queue_swipe_pointer_into(
                    touch_batch,
                    TouchPointerId::new(*pointer_id),
                    *from,
                    *to,
                    *steps,
                )
            }
            AgentAction::SwipePoints { from, to, steps } => {
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                Self::queue_swipe_into(
                    touch_batch,
                    self.point_to_pixels(*from),
                    self.point_to_pixels(*to),
                    *steps,
                )
            }
            AgentAction::SwipePointsPointer {
                pointer_id,
                from,
                to,
                steps,
            } => {
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                Self::queue_swipe_pointer_into(
                    touch_batch,
                    TouchPointerId::new(*pointer_id),
                    self.point_to_pixels(*from),
                    self.point_to_pixels(*to),
                    *steps,
                )
            }
            AgentAction::SwipeRect {
                rect,
                from_x_bp,
                from_y_bp,
                to_x_bp,
                to_y_bp,
                steps,
            } => {
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                Self::queue_swipe_into(
                    touch_batch,
                    self.point_to_pixels(rect.try_point_at_basis_points(*from_x_bp, *from_y_bp)?),
                    self.point_to_pixels(rect.try_point_at_basis_points(*to_x_bp, *to_y_bp)?),
                    *steps,
                )
            }
            AgentAction::SwipeRectPointer {
                pointer_id,
                rect,
                from_x_bp,
                from_y_bp,
                to_x_bp,
                to_y_bp,
                steps,
            } => {
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                Self::queue_swipe_pointer_into(
                    touch_batch,
                    TouchPointerId::new(*pointer_id),
                    self.point_to_pixels(rect.try_point_at_basis_points(*from_x_bp, *from_y_bp)?),
                    self.point_to_pixels(rect.try_point_at_basis_points(*to_x_bp, *to_y_bp)?),
                    *steps,
                )
            }
            AgentAction::Pinch {
                first_from,
                first_to,
                second_from,
                second_to,
                steps,
            } => {
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                Self::queue_pinch_into(
                    touch_batch,
                    *first_from,
                    *first_to,
                    *second_from,
                    *second_to,
                    *steps,
                )
            }
            AgentAction::PinchPoints {
                first_from,
                first_to,
                second_from,
                second_to,
                steps,
            } => {
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                Self::queue_pinch_into(
                    touch_batch,
                    self.point_to_pixels(*first_from),
                    self.point_to_pixels(*first_to),
                    self.point_to_pixels(*second_from),
                    self.point_to_pixels(*second_to),
                    *steps,
                )
            }
            AgentAction::CancelTouch { pointer_id } => {
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                touch_batch.cancel(*pointer_id)
            }
            AgentAction::TouchFrames { len, frames } => {
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                Self::queue_agent_touch_frames_into(touch_batch, *len, frames)
            }
            AgentAction::ThreeFingerScreenshot => {
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                self.queue_three_finger_screenshot_into(touch_batch)
            }
            AgentAction::Key {
                scancode,
                pressed,
                mods,
            } => {
                touch_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                key_batch.key(*scancode, *pressed, *mods)
            }
            AgentAction::KeyTap { scancode, mods } => {
                touch_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                key_batch.tap_key(*scancode, *mods)
            }
            AgentAction::KeyboardChord { chord } => {
                touch_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                key_batch.chord(*chord)
            }
            AgentAction::KeyBatch { len, frames } => {
                touch_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                Self::queue_agent_key_frames_into(key_batch, *len, frames)
            }
            AgentAction::InjectKeycode {
                action,
                keycode,
                repeat,
                metastate,
            } => {
                touch_batch.flush()?;
                key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                android_key_batch.keycode(*action, *keycode, *repeat, *metastate)
            }
            AgentAction::AndroidKeyTap { keycode, metastate } => {
                touch_batch.flush()?;
                key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                android_key_batch.tap_keycode(*keycode, *metastate)
            }
            AgentAction::AndroidKeyBatch { len, frames } => {
                touch_batch.flush()?;
                key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                Self::queue_agent_android_key_frames_into(android_key_batch, *len, frames)
            }
            AgentAction::PressHome => {
                touch_batch.flush()?;
                key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                android_key_batch.key_event(AndroidKeyAction::DOWN, AndroidKeycode::HOME, 0, 0)
            }
            AgentAction::PressBack => {
                touch_batch.flush()?;
                key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                android_key_batch.key_event(AndroidKeyAction::DOWN, AndroidKeycode::BACK, 0, 0)
            }
            AgentAction::OpenRecents => {
                touch_batch.flush()?;
                key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                android_key_batch.key_event(
                    AndroidKeyAction::DOWN,
                    AndroidKeycode::APP_SWITCH,
                    0,
                    0,
                )
            }
            AgentAction::VolumeUp => {
                touch_batch.flush()?;
                key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                android_key_batch.key_event(AndroidKeyAction::DOWN, AndroidKeycode::VOLUME_UP, 0, 0)
            }
            AgentAction::VolumeDown => {
                touch_batch.flush()?;
                key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                android_key_batch.key_event(
                    AndroidKeyAction::DOWN,
                    AndroidKeycode::VOLUME_DOWN,
                    0,
                    0,
                )
            }
            AgentAction::VolumeMute => {
                touch_batch.flush()?;
                key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                android_key_batch.key_event(
                    AndroidKeyAction::DOWN,
                    AndroidKeycode::VOLUME_MUTE,
                    0,
                    0,
                )
            }
            AgentAction::MouseMotion { dx, dy, buttons } => {
                touch_batch.flush()?;
                key_batch.flush()?;
                android_key_batch.flush()?;
                gamepad_batch.flush()?;
                mouse_batch.motion(*dx, *dy, *buttons)
            }
            AgentAction::MouseButtons { buttons } => {
                touch_batch.flush()?;
                key_batch.flush()?;
                android_key_batch.flush()?;
                gamepad_batch.flush()?;
                mouse_batch.buttons(*buttons)
            }
            AgentAction::MouseBatch { len, frames } => {
                touch_batch.flush()?;
                key_batch.flush()?;
                android_key_batch.flush()?;
                gamepad_batch.flush()?;
                Self::queue_agent_mouse_frames_into(mouse_batch, *len, frames)
            }
            AgentAction::Scroll {
                x,
                y,
                hscroll,
                vscroll,
                buttons,
            } => {
                touch_batch.flush()?;
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                scroll_batch.scroll_with_buttons(*x, *y, *hscroll as f32, *vscroll as f32, *buttons)
            }
            AgentAction::ScrollPoint {
                point,
                hscroll,
                vscroll,
                buttons,
            } => {
                touch_batch.flush()?;
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                let (x, y) = self.point_to_pixels(*point);
                scroll_batch.scroll_with_buttons(x, y, *hscroll as f32, *vscroll as f32, *buttons)
            }
            AgentAction::ScrollRect {
                rect,
                hscroll,
                vscroll,
                buttons,
            } => {
                touch_batch.flush()?;
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                let (x, y) = self.point_to_pixels(rect.center());
                scroll_batch.scroll_with_buttons(x, y, *hscroll as f32, *vscroll as f32, *buttons)
            }
            AgentAction::ScrollRectAt {
                rect,
                x_bp,
                y_bp,
                hscroll,
                vscroll,
                buttons,
            } => {
                touch_batch.flush()?;
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                let (x, y) = self.point_to_pixels(rect.try_point_at_basis_points(*x_bp, *y_bp)?);
                scroll_batch.scroll_with_buttons(x, y, *hscroll as f32, *vscroll as f32, *buttons)
            }
            AgentAction::ScrollBatch { len, frames } => {
                touch_batch.flush()?;
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                Self::queue_agent_scroll_frames_into(scroll_batch, *len, frames)
            }
            AgentAction::GamepadFrame { frame } => {
                touch_batch.flush()?;
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.push_dedupe(*frame)
            }
            AgentAction::GamepadFrameUnchecked { frame } => {
                touch_batch.flush()?;
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.push_unchecked(*frame)
            }
            AgentAction::GamepadFrameBatch { len, frames } => {
                touch_batch.flush()?;
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.push_dedupe_slice(*len, frames)
            }
            AgentAction::GamepadFrameBatchUnchecked { len, frames } => {
                touch_batch.flush()?;
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.push_unchecked_slice(*len, frames)
            }
            AgentAction::GamepadPackedFrame { frame } => {
                touch_batch.flush()?;
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.push_packed(*frame)
            }
            AgentAction::GamepadPackedFrameBatch { len, frames } => {
                touch_batch.flush()?;
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.push_packed_slice(*len, frames)
            }
            AgentAction::LongPress { x, y, duration } => {
                touch_batch.flush()?;
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                self.queue_long_press(*x, *y, *duration)
            }
            AgentAction::LongPressPointer {
                pointer_id,
                x,
                y,
                duration,
            } => {
                touch_batch.flush()?;
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                self.queue_long_press_pointer(TouchPointerId::new(*pointer_id), *x, *y, *duration)
            }
            AgentAction::LongPressPoint { point, duration } => {
                touch_batch.flush()?;
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                let (x, y) = self.point_to_pixels(*point);
                self.queue_long_press(x, y, *duration)
            }
            AgentAction::LongPressPointPointer {
                pointer_id,
                point,
                duration,
            } => {
                touch_batch.flush()?;
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                let (x, y) = self.point_to_pixels(*point);
                self.queue_long_press_pointer(TouchPointerId::new(*pointer_id), x, y, *duration)
            }
            AgentAction::LongPressRect { rect, duration } => {
                touch_batch.flush()?;
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                let (x, y) = self.point_to_pixels(rect.center());
                self.queue_long_press(x, y, *duration)
            }
            AgentAction::LongPressRectAt {
                rect,
                x_bp,
                y_bp,
                duration,
            } => {
                touch_batch.flush()?;
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                let (x, y) = self.point_to_pixels(rect.try_point_at_basis_points(*x_bp, *y_bp)?);
                self.queue_long_press(x, y, *duration)
            }
            AgentAction::LongPressRectPointer {
                pointer_id,
                rect,
                duration,
            } => {
                touch_batch.flush()?;
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                let (x, y) = self.point_to_pixels(rect.center());
                self.queue_long_press_pointer(TouchPointerId::new(*pointer_id), x, y, *duration)
            }
            AgentAction::LongPressRectAtPointer {
                pointer_id,
                rect,
                x_bp,
                y_bp,
                duration,
            } => {
                touch_batch.flush()?;
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                let (x, y) = self.point_to_pixels(rect.try_point_at_basis_points(*x_bp, *y_bp)?);
                self.queue_long_press_pointer(TouchPointerId::new(*pointer_id), x, y, *duration)
            }
            AgentAction::Wait(duration) => {
                touch_batch.flush()?;
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                self.flush()?;
                std::thread::sleep(*duration);
                Ok(())
            }
            AgentAction::Flush => {
                touch_batch.flush()?;
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                self.flush()
            }
            _ => {
                touch_batch.flush()?;
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                self.queue_action(action)
            }
        }
    }

    fn try_queue_action(&self, action: &AgentAction) -> Result<()> {
        match action {
            AgentAction::TypeText(text) => self.client.try_send(HidCommand::TypeText(text.clone())),
            AgentAction::TypeTextStrict(text) => self
                .client
                .try_send(HidCommand::TypeTextStrict(text.clone())),
            AgentAction::Key {
                scancode,
                pressed,
                mods,
            } => self.client.try_send(HidCommand::Key {
                scancode: *scancode,
                pressed: *pressed,
                mods: *mods,
            }),
            AgentAction::KeyTap { scancode, mods } => self.client.try_tap_key(*scancode, *mods),
            AgentAction::KeyboardChord { chord } => self.client.try_key_chord(*chord),
            AgentAction::KeyBatch { len, frames } => {
                self.client.try_send_key_batch_fixed(*len, *frames)
            }
            AgentAction::MouseMotion { dx, dy, buttons } => {
                self.client.try_mouse_motion(*dx, *dy, *buttons)
            }
            AgentAction::MouseButtons { buttons } => self.client.try_mouse_buttons(*buttons),
            AgentAction::MouseScroll { hscroll, vscroll } => self
                .client
                .try_mouse_scroll(*hscroll as f32, *vscroll as f32),
            AgentAction::MouseBatch { len, frames } => {
                self.client.try_send_mouse_batch_fixed(*len, *frames)
            }
            AgentAction::InjectKeycode {
                action,
                keycode,
                repeat,
                metastate,
            } => self
                .client
                .try_inject_keycode(*action, *keycode, *repeat, *metastate),
            AgentAction::AndroidKeyTap { keycode, metastate } => {
                self.client.try_tap_android_keycode(*keycode, *metastate)
            }
            AgentAction::AndroidKeyBatch { len, frames } => {
                self.client.try_send_android_key_batch_fixed(*len, *frames)
            }
            AgentAction::BackOrScreenOn { action } => self
                .client
                .try_back_or_screen_on(AndroidKeyAction::new(*action)),
            AgentAction::PressHome => self.client.try_press_android_key(AndroidKeycode::HOME),
            AgentAction::PressBack => self.client.try_press_android_key(AndroidKeycode::BACK),
            AgentAction::OpenRecents => self
                .client
                .try_press_android_key(AndroidKeycode::APP_SWITCH),
            AgentAction::VolumeUp => self.client.try_press_android_key(AndroidKeycode::VOLUME_UP),
            AgentAction::VolumeDown => self
                .client
                .try_press_android_key(AndroidKeycode::VOLUME_DOWN),
            AgentAction::VolumeMute => self
                .client
                .try_press_android_key(AndroidKeycode::VOLUME_MUTE),
            AgentAction::Tap { x, y } => self.try_queue_tap(*x, *y),
            AgentAction::TapPointer { pointer_id, x, y } => {
                self.try_queue_tap_pointer(TouchPointerId::new(*pointer_id), *x, *y)
            }
            AgentAction::TapPoint { point } => {
                let (x, y) = self.point_to_pixels(*point);
                self.try_queue_tap(x, y)
            }
            AgentAction::TapPointPointer { pointer_id, point } => {
                let (x, y) = self.point_to_pixels(*point);
                self.try_queue_tap_pointer(TouchPointerId::new(*pointer_id), x, y)
            }
            AgentAction::TapRect { rect } => {
                let (x, y) = self.point_to_pixels(rect.center());
                self.try_queue_tap(x, y)
            }
            AgentAction::TapRectAt { rect, x_bp, y_bp } => {
                let (x, y) = self.point_to_pixels(rect.try_point_at_basis_points(*x_bp, *y_bp)?);
                self.try_queue_tap(x, y)
            }
            AgentAction::TapRectPointer { pointer_id, rect } => {
                let (x, y) = self.point_to_pixels(rect.center());
                self.try_queue_tap_pointer(TouchPointerId::new(*pointer_id), x, y)
            }
            AgentAction::TapRectAtPointer {
                pointer_id,
                rect,
                x_bp,
                y_bp,
            } => {
                let (x, y) = self.point_to_pixels(rect.try_point_at_basis_points(*x_bp, *y_bp)?);
                self.try_queue_tap_pointer(TouchPointerId::new(*pointer_id), x, y)
            }
            AgentAction::DoubleTap { x, y } => self.try_queue_double_tap(*x, *y),
            AgentAction::DoubleTapPointer { pointer_id, x, y } => {
                self.try_queue_double_tap_pointer(TouchPointerId::new(*pointer_id), *x, *y)
            }
            AgentAction::DoubleTapPoint { point } => {
                let (x, y) = self.point_to_pixels(*point);
                self.try_queue_double_tap(x, y)
            }
            AgentAction::DoubleTapPointPointer { pointer_id, point } => {
                let (x, y) = self.point_to_pixels(*point);
                self.try_queue_double_tap_pointer(TouchPointerId::new(*pointer_id), x, y)
            }
            AgentAction::DoubleTapRect { rect } => {
                let (x, y) = self.point_to_pixels(rect.center());
                self.try_queue_double_tap(x, y)
            }
            AgentAction::DoubleTapRectAt { rect, x_bp, y_bp } => {
                let (x, y) = self.point_to_pixels(rect.try_point_at_basis_points(*x_bp, *y_bp)?);
                self.try_queue_double_tap(x, y)
            }
            AgentAction::DoubleTapRectPointer { pointer_id, rect } => {
                let (x, y) = self.point_to_pixels(rect.center());
                self.try_queue_double_tap_pointer(TouchPointerId::new(*pointer_id), x, y)
            }
            AgentAction::DoubleTapRectAtPointer {
                pointer_id,
                rect,
                x_bp,
                y_bp,
            } => {
                let (x, y) = self.point_to_pixels(rect.try_point_at_basis_points(*x_bp, *y_bp)?);
                self.try_queue_double_tap_pointer(TouchPointerId::new(*pointer_id), x, y)
            }
            AgentAction::Swipe { from, to, steps } => self.try_queue_swipe(*from, *to, *steps),
            AgentAction::SwipePointer {
                pointer_id,
                from,
                to,
                steps,
            } => self.try_queue_swipe_pointer(TouchPointerId::new(*pointer_id), *from, *to, *steps),
            AgentAction::SwipePoints { from, to, steps } => self.try_queue_swipe(
                self.point_to_pixels(*from),
                self.point_to_pixels(*to),
                *steps,
            ),
            AgentAction::SwipePointsPointer {
                pointer_id,
                from,
                to,
                steps,
            } => self.try_queue_swipe_pointer(
                TouchPointerId::new(*pointer_id),
                self.point_to_pixels(*from),
                self.point_to_pixels(*to),
                *steps,
            ),
            AgentAction::SwipeRect {
                rect,
                from_x_bp,
                from_y_bp,
                to_x_bp,
                to_y_bp,
                steps,
            } => self.try_queue_swipe(
                self.point_to_pixels(rect.try_point_at_basis_points(*from_x_bp, *from_y_bp)?),
                self.point_to_pixels(rect.try_point_at_basis_points(*to_x_bp, *to_y_bp)?),
                *steps,
            ),
            AgentAction::SwipeRectPointer {
                pointer_id,
                rect,
                from_x_bp,
                from_y_bp,
                to_x_bp,
                to_y_bp,
                steps,
            } => self.try_queue_swipe_pointer(
                TouchPointerId::new(*pointer_id),
                self.point_to_pixels(rect.try_point_at_basis_points(*from_x_bp, *from_y_bp)?),
                self.point_to_pixels(rect.try_point_at_basis_points(*to_x_bp, *to_y_bp)?),
                *steps,
            ),
            AgentAction::Pinch {
                first_from,
                first_to,
                second_from,
                second_to,
                steps,
            } => self.try_queue_pinch(*first_from, *first_to, *second_from, *second_to, *steps),
            AgentAction::PinchPoints {
                first_from,
                first_to,
                second_from,
                second_to,
                steps,
            } => self.try_queue_pinch(
                self.point_to_pixels(*first_from),
                self.point_to_pixels(*first_to),
                self.point_to_pixels(*second_from),
                self.point_to_pixels(*second_to),
                *steps,
            ),
            AgentAction::Scroll {
                x,
                y,
                hscroll,
                vscroll,
                buttons,
            } => self.client.try_scroll_with_buttons(
                *x,
                *y,
                *hscroll as f32,
                *vscroll as f32,
                *buttons,
            ),
            AgentAction::ScrollPoint {
                point,
                hscroll,
                vscroll,
                buttons,
            } => {
                let (x, y) = self.point_to_pixels(*point);
                self.client.try_scroll_with_buttons(
                    x,
                    y,
                    *hscroll as f32,
                    *vscroll as f32,
                    *buttons,
                )
            }
            AgentAction::ScrollRect {
                rect,
                hscroll,
                vscroll,
                buttons,
            } => {
                let (x, y) = self.point_to_pixels(rect.center());
                self.client.try_scroll_with_buttons(
                    x,
                    y,
                    *hscroll as f32,
                    *vscroll as f32,
                    *buttons,
                )
            }
            AgentAction::ScrollRectAt {
                rect,
                x_bp,
                y_bp,
                hscroll,
                vscroll,
                buttons,
            } => {
                let (x, y) = self.point_to_pixels(rect.try_point_at_basis_points(*x_bp, *y_bp)?);
                self.client.try_scroll_with_buttons(
                    x,
                    y,
                    *hscroll as f32,
                    *vscroll as f32,
                    *buttons,
                )
            }
            AgentAction::ScrollBatch { len, frames } => {
                let mut batch = self.client.scroll_frame_batcher();
                Self::try_queue_agent_scroll_frames_into(&mut batch, *len, frames)?;
                batch.try_flush()
            }
            AgentAction::CancelTouch { pointer_id } => self.client.try_cancel_touch(*pointer_id),
            AgentAction::TouchFrames { len, frames } => {
                let mut batch = self.client.touch_frame_batcher();
                Self::try_queue_agent_touch_frames_into(&mut batch, *len, frames)?;
                batch.try_flush()
            }
            AgentAction::ThreeFingerScreenshot => self.try_queue_three_finger_screenshot(),
            AgentAction::SetScreenSize { width, height } => {
                self.client.try_set_screen_size(*width, *height)?;
                self.screen_width.store(*width, Ordering::Relaxed);
                self.screen_height.store(*height, Ordering::Relaxed);
                Ok(())
            }
            AgentAction::LaunchApp(name) => self
                .client
                .try_send(HidCommand::LaunchApp { name: name.clone() }),
            AgentAction::SetScreenPower { on } => {
                self.client.try_send(HidCommand::SetScreenPower { on: *on })
            }
            AgentAction::ShowNotifications => self.client.try_send(HidCommand::ShowNotifications),
            AgentAction::ShowQuickSettings => self.client.try_send(HidCommand::ShowQuickSettings),
            AgentAction::CollapsePanels => self.client.try_send(HidCommand::CollapsePanels),
            AgentAction::RotateDevice => self.client.try_send(HidCommand::RotateDevice),
            AgentAction::ResizeDisplay { width, height } => {
                self.client.try_send(HidCommand::ResizeDisplay {
                    width: *width,
                    height: *height,
                })
            }
            AgentAction::SetTorch { on } => self.client.try_send(HidCommand::SetTorch { on: *on }),
            AgentAction::CameraZoomIn => self.client.try_send(HidCommand::CameraZoomIn),
            AgentAction::CameraZoomOut => self.client.try_send(HidCommand::CameraZoomOut),
            AgentAction::OpenHardKeyboardSettings => {
                self.client.try_send(HidCommand::OpenHardKeyboardSettings)
            }
            AgentAction::ResetVideo => self.client.try_send(HidCommand::ResetVideo),
            AgentAction::AiConfig {
                flags,
                sample_interval_ms,
                feature_dim,
            } => self
                .client
                .try_configure_ai(*flags, *sample_interval_ms, *feature_dim),
            AgentAction::AiQuery { since_timestamp_ms } => {
                self.client.try_query_ai(*since_timestamp_ms)
            }
            AgentAction::AiPause => self.client.try_pause_ai(),
            AgentAction::SetClipboard { text, paste } => {
                self.client.try_send(HidCommand::SetClipboard {
                    text: text.clone(),
                    paste: *paste,
                })
            }
            AgentAction::SetClipboardSequenced {
                sequence,
                text,
                paste,
            } => self.client.try_send(HidCommand::SetClipboardSequenced {
                sequence: *sequence,
                text: text.clone(),
                paste: *paste,
            }),
            AgentAction::RequestClipboard { copy_key } => {
                self.client.try_request_clipboard(*copy_key)
            }
            AgentAction::GamepadButton { button, pressed } => {
                self.client.try_send_button(*button, *pressed)
            }
            AgentAction::GamepadButtons { buttons } => self.client.try_send_buttons(*buttons),
            AgentAction::GamepadFrame { frame } => self.client.try_send_frame(*frame),
            AgentAction::GamepadFrameUnchecked { frame } => {
                self.client.try_send_frame_unchecked(*frame)
            }
            AgentAction::GamepadFrameBatch { len, frames } => {
                self.client.try_send_frame_batch_fixed(*len, *frames)
            }
            AgentAction::GamepadFrameBatchUnchecked { len, frames } => self
                .client
                .try_send_frame_batch_fixed_unchecked(*len, *frames),
            AgentAction::GamepadPackedFrame { frame } => self.client.try_send_frame_packed(*frame),
            AgentAction::GamepadPackedFrameBatch { len, frames } => {
                self.client.try_send_frame_packed_batch_fixed(*len, *frames)
            }
            AgentAction::Wait(_)
            | AgentAction::LongPress { .. }
            | AgentAction::LongPressPointer { .. }
            | AgentAction::LongPressPoint { .. }
            | AgentAction::LongPressPointPointer { .. }
            | AgentAction::LongPressRect { .. }
            | AgentAction::LongPressRectAt { .. }
            | AgentAction::LongPressRectPointer { .. }
            | AgentAction::LongPressRectAtPointer { .. } => {
                Err(Error::SessionLifecycle(TIMED_ACTION_REQUIRES_BLOCKING))
            }
            AgentAction::Flush => self.client.try_flush(),
        }
    }

    fn try_queue_planned_action(
        &self,
        action: &AgentAction,
        batches: PlanBatchers<'_, '_>,
    ) -> Result<()> {
        let (touch_batch, key_batch, android_key_batch, mouse_batch, scroll_batch, gamepad_batch) =
            batches;
        if !matches!(
            action,
            AgentAction::Scroll { .. }
                | AgentAction::ScrollPoint { .. }
                | AgentAction::ScrollRect { .. }
                | AgentAction::ScrollRectAt { .. }
                | AgentAction::ScrollBatch { .. }
        ) {
            scroll_batch.try_flush()?;
        }
        match action {
            AgentAction::Tap { x, y } => {
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                Self::try_queue_tap_into(touch_batch, *x, *y)
            }
            AgentAction::TapPointer { pointer_id, x, y } => {
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                Self::try_queue_tap_pointer_into(
                    touch_batch,
                    TouchPointerId::new(*pointer_id),
                    *x,
                    *y,
                )
            }
            AgentAction::TapPoint { point } => {
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                let (x, y) = self.point_to_pixels(*point);
                Self::try_queue_tap_into(touch_batch, x, y)
            }
            AgentAction::TapPointPointer { pointer_id, point } => {
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                let (x, y) = self.point_to_pixels(*point);
                Self::try_queue_tap_pointer_into(
                    touch_batch,
                    TouchPointerId::new(*pointer_id),
                    x,
                    y,
                )
            }
            AgentAction::TapRect { rect } => {
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                let (x, y) = self.point_to_pixels(rect.center());
                Self::try_queue_tap_into(touch_batch, x, y)
            }
            AgentAction::TapRectAt { rect, x_bp, y_bp } => {
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                let (x, y) = self.point_to_pixels(rect.try_point_at_basis_points(*x_bp, *y_bp)?);
                Self::try_queue_tap_into(touch_batch, x, y)
            }
            AgentAction::TapRectPointer { pointer_id, rect } => {
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                let (x, y) = self.point_to_pixels(rect.center());
                Self::try_queue_tap_pointer_into(
                    touch_batch,
                    TouchPointerId::new(*pointer_id),
                    x,
                    y,
                )
            }
            AgentAction::TapRectAtPointer {
                pointer_id,
                rect,
                x_bp,
                y_bp,
            } => {
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                let (x, y) = self.point_to_pixels(rect.try_point_at_basis_points(*x_bp, *y_bp)?);
                Self::try_queue_tap_pointer_into(
                    touch_batch,
                    TouchPointerId::new(*pointer_id),
                    x,
                    y,
                )
            }
            AgentAction::DoubleTap { x, y } => {
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                Self::try_queue_double_tap_into(touch_batch, *x, *y)
            }
            AgentAction::DoubleTapPointer { pointer_id, x, y } => {
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                Self::try_queue_double_tap_pointer_into(
                    touch_batch,
                    TouchPointerId::new(*pointer_id),
                    *x,
                    *y,
                )
            }
            AgentAction::DoubleTapPoint { point } => {
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                let (x, y) = self.point_to_pixels(*point);
                Self::try_queue_double_tap_into(touch_batch, x, y)
            }
            AgentAction::DoubleTapPointPointer { pointer_id, point } => {
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                let (x, y) = self.point_to_pixels(*point);
                Self::try_queue_double_tap_pointer_into(
                    touch_batch,
                    TouchPointerId::new(*pointer_id),
                    x,
                    y,
                )
            }
            AgentAction::DoubleTapRect { rect } => {
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                let (x, y) = self.point_to_pixels(rect.center());
                Self::try_queue_double_tap_into(touch_batch, x, y)
            }
            AgentAction::DoubleTapRectAt { rect, x_bp, y_bp } => {
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                let (x, y) = self.point_to_pixels(rect.try_point_at_basis_points(*x_bp, *y_bp)?);
                Self::try_queue_double_tap_into(touch_batch, x, y)
            }
            AgentAction::DoubleTapRectPointer { pointer_id, rect } => {
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                let (x, y) = self.point_to_pixels(rect.center());
                Self::try_queue_double_tap_pointer_into(
                    touch_batch,
                    TouchPointerId::new(*pointer_id),
                    x,
                    y,
                )
            }
            AgentAction::DoubleTapRectAtPointer {
                pointer_id,
                rect,
                x_bp,
                y_bp,
            } => {
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                let (x, y) = self.point_to_pixels(rect.try_point_at_basis_points(*x_bp, *y_bp)?);
                Self::try_queue_double_tap_pointer_into(
                    touch_batch,
                    TouchPointerId::new(*pointer_id),
                    x,
                    y,
                )
            }
            AgentAction::Swipe { from, to, steps } => {
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                Self::try_queue_swipe_into(touch_batch, *from, *to, *steps)
            }
            AgentAction::SwipePointer {
                pointer_id,
                from,
                to,
                steps,
            } => {
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                Self::try_queue_swipe_pointer_into(
                    touch_batch,
                    TouchPointerId::new(*pointer_id),
                    *from,
                    *to,
                    *steps,
                )
            }
            AgentAction::SwipePoints { from, to, steps } => {
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                Self::try_queue_swipe_into(
                    touch_batch,
                    self.point_to_pixels(*from),
                    self.point_to_pixels(*to),
                    *steps,
                )
            }
            AgentAction::SwipePointsPointer {
                pointer_id,
                from,
                to,
                steps,
            } => {
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                Self::try_queue_swipe_pointer_into(
                    touch_batch,
                    TouchPointerId::new(*pointer_id),
                    self.point_to_pixels(*from),
                    self.point_to_pixels(*to),
                    *steps,
                )
            }
            AgentAction::SwipeRect {
                rect,
                from_x_bp,
                from_y_bp,
                to_x_bp,
                to_y_bp,
                steps,
            } => {
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                Self::try_queue_swipe_into(
                    touch_batch,
                    self.point_to_pixels(rect.try_point_at_basis_points(*from_x_bp, *from_y_bp)?),
                    self.point_to_pixels(rect.try_point_at_basis_points(*to_x_bp, *to_y_bp)?),
                    *steps,
                )
            }
            AgentAction::SwipeRectPointer {
                pointer_id,
                rect,
                from_x_bp,
                from_y_bp,
                to_x_bp,
                to_y_bp,
                steps,
            } => {
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                Self::try_queue_swipe_pointer_into(
                    touch_batch,
                    TouchPointerId::new(*pointer_id),
                    self.point_to_pixels(rect.try_point_at_basis_points(*from_x_bp, *from_y_bp)?),
                    self.point_to_pixels(rect.try_point_at_basis_points(*to_x_bp, *to_y_bp)?),
                    *steps,
                )
            }
            AgentAction::Pinch {
                first_from,
                first_to,
                second_from,
                second_to,
                steps,
            } => {
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                Self::try_queue_pinch_into(
                    touch_batch,
                    *first_from,
                    *first_to,
                    *second_from,
                    *second_to,
                    *steps,
                )
            }
            AgentAction::PinchPoints {
                first_from,
                first_to,
                second_from,
                second_to,
                steps,
            } => {
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                Self::try_queue_pinch_into(
                    touch_batch,
                    self.point_to_pixels(*first_from),
                    self.point_to_pixels(*first_to),
                    self.point_to_pixels(*second_from),
                    self.point_to_pixels(*second_to),
                    *steps,
                )
            }
            AgentAction::CancelTouch { pointer_id } => {
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                touch_batch.try_cancel(*pointer_id)
            }
            AgentAction::TouchFrames { len, frames } => {
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                Self::try_queue_agent_touch_frames_into(touch_batch, *len, frames)
            }
            AgentAction::ThreeFingerScreenshot => {
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                self.try_queue_three_finger_screenshot_into(touch_batch)
            }
            AgentAction::Key {
                scancode,
                pressed,
                mods,
            } => {
                touch_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                key_batch.try_key(*scancode, *pressed, *mods)
            }
            AgentAction::KeyTap { scancode, mods } => {
                touch_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                key_batch.try_tap_key(*scancode, *mods)
            }
            AgentAction::KeyboardChord { chord } => {
                touch_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                key_batch.try_chord(*chord)
            }
            AgentAction::KeyBatch { len, frames } => {
                touch_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                Self::try_queue_agent_key_frames_into(key_batch, *len, frames)
            }
            AgentAction::InjectKeycode {
                action,
                keycode,
                repeat,
                metastate,
            } => {
                touch_batch.try_flush()?;
                key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                android_key_batch.try_keycode(*action, *keycode, *repeat, *metastate)
            }
            AgentAction::AndroidKeyTap { keycode, metastate } => {
                touch_batch.try_flush()?;
                key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                android_key_batch.try_tap_keycode(*keycode, *metastate)
            }
            AgentAction::AndroidKeyBatch { len, frames } => {
                touch_batch.try_flush()?;
                key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                Self::try_queue_agent_android_key_frames_into(android_key_batch, *len, frames)
            }
            AgentAction::PressHome => {
                touch_batch.try_flush()?;
                key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                android_key_batch.try_key_event(AndroidKeyAction::DOWN, AndroidKeycode::HOME, 0, 0)
            }
            AgentAction::PressBack => {
                touch_batch.try_flush()?;
                key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                android_key_batch.try_key_event(AndroidKeyAction::DOWN, AndroidKeycode::BACK, 0, 0)
            }
            AgentAction::OpenRecents => {
                touch_batch.try_flush()?;
                key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                android_key_batch.try_key_event(
                    AndroidKeyAction::DOWN,
                    AndroidKeycode::APP_SWITCH,
                    0,
                    0,
                )
            }
            AgentAction::VolumeUp => {
                touch_batch.try_flush()?;
                key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                android_key_batch.try_key_event(
                    AndroidKeyAction::DOWN,
                    AndroidKeycode::VOLUME_UP,
                    0,
                    0,
                )
            }
            AgentAction::VolumeDown => {
                touch_batch.try_flush()?;
                key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                android_key_batch.try_key_event(
                    AndroidKeyAction::DOWN,
                    AndroidKeycode::VOLUME_DOWN,
                    0,
                    0,
                )
            }
            AgentAction::VolumeMute => {
                touch_batch.try_flush()?;
                key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                android_key_batch.try_key_event(
                    AndroidKeyAction::DOWN,
                    AndroidKeycode::VOLUME_MUTE,
                    0,
                    0,
                )
            }
            AgentAction::MouseMotion { dx, dy, buttons } => {
                touch_batch.try_flush()?;
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                mouse_batch.try_motion(*dx, *dy, *buttons)
            }
            AgentAction::MouseButtons { buttons } => {
                touch_batch.try_flush()?;
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                mouse_batch.try_buttons(*buttons)
            }
            AgentAction::MouseBatch { len, frames } => {
                touch_batch.try_flush()?;
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                Self::try_queue_agent_mouse_frames_into(mouse_batch, *len, frames)
            }
            AgentAction::Scroll {
                x,
                y,
                hscroll,
                vscroll,
                buttons,
            } => {
                touch_batch.try_flush()?;
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                scroll_batch.try_scroll_with_buttons(
                    *x,
                    *y,
                    *hscroll as f32,
                    *vscroll as f32,
                    *buttons,
                )
            }
            AgentAction::ScrollPoint {
                point,
                hscroll,
                vscroll,
                buttons,
            } => {
                touch_batch.try_flush()?;
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                let (x, y) = self.point_to_pixels(*point);
                scroll_batch.try_scroll_with_buttons(
                    x,
                    y,
                    *hscroll as f32,
                    *vscroll as f32,
                    *buttons,
                )
            }
            AgentAction::ScrollRect {
                rect,
                hscroll,
                vscroll,
                buttons,
            } => {
                touch_batch.try_flush()?;
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                let (x, y) = self.point_to_pixels(rect.center());
                scroll_batch.try_scroll_with_buttons(
                    x,
                    y,
                    *hscroll as f32,
                    *vscroll as f32,
                    *buttons,
                )
            }
            AgentAction::ScrollRectAt {
                rect,
                x_bp,
                y_bp,
                hscroll,
                vscroll,
                buttons,
            } => {
                touch_batch.try_flush()?;
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                let (x, y) = self.point_to_pixels(rect.try_point_at_basis_points(*x_bp, *y_bp)?);
                scroll_batch.try_scroll_with_buttons(
                    x,
                    y,
                    *hscroll as f32,
                    *vscroll as f32,
                    *buttons,
                )
            }
            AgentAction::ScrollBatch { len, frames } => {
                touch_batch.try_flush()?;
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                Self::try_queue_agent_scroll_frames_into(scroll_batch, *len, frames)
            }
            AgentAction::GamepadFrame { frame } => {
                touch_batch.try_flush()?;
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_push_dedupe(*frame)
            }
            AgentAction::GamepadFrameUnchecked { frame } => {
                touch_batch.try_flush()?;
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_push_unchecked(*frame)
            }
            AgentAction::GamepadFrameBatch { len, frames } => {
                touch_batch.try_flush()?;
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_push_dedupe_slice(*len, frames)
            }
            AgentAction::GamepadFrameBatchUnchecked { len, frames } => {
                touch_batch.try_flush()?;
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_push_unchecked_slice(*len, frames)
            }
            AgentAction::GamepadPackedFrame { frame } => {
                touch_batch.try_flush()?;
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_push_packed(*frame)
            }
            AgentAction::GamepadPackedFrameBatch { len, frames } => {
                touch_batch.try_flush()?;
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_push_packed_slice(*len, frames)
            }
            AgentAction::Wait(_)
            | AgentAction::LongPress { .. }
            | AgentAction::LongPressPointer { .. }
            | AgentAction::LongPressPoint { .. }
            | AgentAction::LongPressPointPointer { .. }
            | AgentAction::LongPressRect { .. }
            | AgentAction::LongPressRectAt { .. }
            | AgentAction::LongPressRectPointer { .. }
            | AgentAction::LongPressRectAtPointer { .. } => {
                touch_batch.try_flush()?;
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                Err(Error::SessionLifecycle(TIMED_ACTION_REQUIRES_BLOCKING))
            }
            AgentAction::Flush => {
                touch_batch.try_flush()?;
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                self.client.try_flush()
            }
            _ => {
                touch_batch.try_flush()?;
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                self.try_queue_action(action)
            }
        }
    }

    fn queue_tap(&self, x: i32, y: i32) -> Result<()> {
        self.queue_tap_pointer(TouchPointerId::finger(0), x, y)
    }

    fn queue_tap_pointer(&self, pointer_id: TouchPointerId, x: i32, y: i32) -> Result<()> {
        let mut batch = self.client.touch_frame_batcher();
        Self::queue_tap_pointer_into(&mut batch, pointer_id, x, y)?;
        batch.flush()
    }

    fn queue_double_tap(&self, x: i32, y: i32) -> Result<()> {
        self.queue_double_tap_pointer(TouchPointerId::finger(0), x, y)
    }

    fn queue_double_tap_pointer(&self, pointer_id: TouchPointerId, x: i32, y: i32) -> Result<()> {
        let mut batch = self.client.touch_frame_batcher();
        Self::queue_double_tap_pointer_into(&mut batch, pointer_id, x, y)?;
        batch.flush()
    }

    fn queue_long_press(&self, x: i32, y: i32, duration: Duration) -> Result<()> {
        self.queue_long_press_pointer(TouchPointerId::finger(0), x, y, duration)
    }

    fn queue_long_press_pointer(
        &self,
        pointer_id: TouchPointerId,
        x: i32,
        y: i32,
        duration: Duration,
    ) -> Result<()> {
        {
            let mut batch = self.client.touch_frame_batcher();
            batch.down_pointer(pointer_id, x, y, 1.0)?;
            batch.flush()?;
        }
        self.flush()?;
        std::thread::sleep(duration);
        let mut batch = self.client.touch_frame_batcher();
        batch.up_pointer(pointer_id, x, y)?;
        batch.flush()
    }

    fn queue_swipe(&self, from: (i32, i32), to: (i32, i32), steps: usize) -> Result<()> {
        self.queue_swipe_pointer(TouchPointerId::finger(0), from, to, steps)
    }

    fn queue_swipe_pointer(
        &self,
        pointer_id: TouchPointerId,
        from: (i32, i32),
        to: (i32, i32),
        steps: usize,
    ) -> Result<()> {
        let mut batch = self.client.touch_frame_batcher();
        Self::queue_swipe_pointer_into(&mut batch, pointer_id, from, to, steps)?;
        batch.flush()
    }

    fn queue_pinch(
        &self,
        first_from: (i32, i32),
        first_to: (i32, i32),
        second_from: (i32, i32),
        second_to: (i32, i32),
        steps: usize,
    ) -> Result<()> {
        let mut batch = self.client.touch_frame_batcher();
        Self::queue_pinch_into(
            &mut batch,
            first_from,
            first_to,
            second_from,
            second_to,
            steps,
        )?;
        batch.flush()
    }

    fn queue_three_finger_screenshot(&self) -> Result<()> {
        let mut batch = self.client.touch_frame_batcher();
        self.queue_three_finger_screenshot_into(&mut batch)?;
        batch.flush()
    }

    fn try_queue_tap(&self, x: i32, y: i32) -> Result<()> {
        self.try_queue_tap_pointer(TouchPointerId::finger(0), x, y)
    }

    fn try_queue_tap_pointer(&self, pointer_id: TouchPointerId, x: i32, y: i32) -> Result<()> {
        let mut batch = self.client.touch_frame_batcher();
        Self::try_queue_tap_pointer_into(&mut batch, pointer_id, x, y)?;
        batch.try_flush()
    }

    fn try_queue_double_tap(&self, x: i32, y: i32) -> Result<()> {
        self.try_queue_double_tap_pointer(TouchPointerId::finger(0), x, y)
    }

    fn try_queue_double_tap_pointer(
        &self,
        pointer_id: TouchPointerId,
        x: i32,
        y: i32,
    ) -> Result<()> {
        let mut batch = self.client.touch_frame_batcher();
        Self::try_queue_double_tap_pointer_into(&mut batch, pointer_id, x, y)?;
        batch.try_flush()
    }

    fn try_queue_swipe(&self, from: (i32, i32), to: (i32, i32), steps: usize) -> Result<()> {
        self.try_queue_swipe_pointer(TouchPointerId::finger(0), from, to, steps)
    }

    fn try_queue_swipe_pointer(
        &self,
        pointer_id: TouchPointerId,
        from: (i32, i32),
        to: (i32, i32),
        steps: usize,
    ) -> Result<()> {
        let mut batch = self.client.touch_frame_batcher();
        Self::try_queue_swipe_pointer_into(&mut batch, pointer_id, from, to, steps)?;
        batch.try_flush()
    }

    fn try_queue_pinch(
        &self,
        first_from: (i32, i32),
        first_to: (i32, i32),
        second_from: (i32, i32),
        second_to: (i32, i32),
        steps: usize,
    ) -> Result<()> {
        let mut batch = self.client.touch_frame_batcher();
        Self::try_queue_pinch_into(
            &mut batch,
            first_from,
            first_to,
            second_from,
            second_to,
            steps,
        )?;
        batch.try_flush()
    }

    fn try_queue_three_finger_screenshot(&self) -> Result<()> {
        let mut batch = self.client.touch_frame_batcher();
        self.try_queue_three_finger_screenshot_into(&mut batch)?;
        batch.try_flush()
    }

    fn queue_agent_touch_frames_into(
        batch: &mut TouchFrameBatcher<'_>,
        len: usize,
        frames: &[AgentTouchFrame; TOUCH_BATCH_FRAMES],
    ) -> Result<()> {
        if len > TOUCH_BATCH_FRAMES {
            return Err(Error::SessionLifecycle("touch frame batch length overflow"));
        }
        let mut converted = [TouchFrame::EMPTY; TOUCH_BATCH_FRAMES];
        for (dst, src) in converted.iter_mut().zip(frames.iter()).take(len) {
            *dst = src.into_touch_frame();
        }
        batch.push_many_slice(&converted[..len])
    }

    fn try_queue_agent_touch_frames_into(
        batch: &mut TouchFrameBatcher<'_>,
        len: usize,
        frames: &[AgentTouchFrame; TOUCH_BATCH_FRAMES],
    ) -> Result<()> {
        if len > TOUCH_BATCH_FRAMES {
            return Err(Error::SessionLifecycle("touch frame batch length overflow"));
        }
        let mut converted = [TouchFrame::EMPTY; TOUCH_BATCH_FRAMES];
        for (dst, src) in converted.iter_mut().zip(frames.iter()).take(len) {
            *dst = src.into_touch_frame();
        }
        batch.try_push_many_slice(&converted[..len])
    }

    fn queue_agent_key_frames_into(
        batch: &mut KeyboardFrameBatcher<'_>,
        len: usize,
        frames: &[KeyboardFrame; KEYBOARD_BATCH_FRAMES],
    ) -> Result<()> {
        if len > KEYBOARD_BATCH_FRAMES {
            return Err(Error::SessionLifecycle("keyboard batch length overflow"));
        }
        batch.push_many_slice(&frames[..len])
    }

    fn try_queue_agent_key_frames_into(
        batch: &mut KeyboardFrameBatcher<'_>,
        len: usize,
        frames: &[KeyboardFrame; KEYBOARD_BATCH_FRAMES],
    ) -> Result<()> {
        if len > KEYBOARD_BATCH_FRAMES {
            return Err(Error::SessionLifecycle("keyboard batch length overflow"));
        }
        batch.try_push_many_slice(&frames[..len])
    }

    fn queue_agent_android_key_frames_into(
        batch: &mut AndroidKeyFrameBatcher<'_>,
        len: usize,
        frames: &[AndroidKeyFrame; ANDROID_KEY_BATCH_FRAMES],
    ) -> Result<()> {
        if len > ANDROID_KEY_BATCH_FRAMES {
            return Err(Error::SessionLifecycle("android key batch length overflow"));
        }
        batch.push_many_slice(&frames[..len])
    }

    fn try_queue_agent_android_key_frames_into(
        batch: &mut AndroidKeyFrameBatcher<'_>,
        len: usize,
        frames: &[AndroidKeyFrame; ANDROID_KEY_BATCH_FRAMES],
    ) -> Result<()> {
        if len > ANDROID_KEY_BATCH_FRAMES {
            return Err(Error::SessionLifecycle("android key batch length overflow"));
        }
        batch.try_push_many_slice(&frames[..len])
    }

    fn queue_agent_mouse_frames_into(
        batch: &mut MouseFrameBatcher<'_>,
        len: usize,
        frames: &[MouseFrame; MOUSE_BATCH_FRAMES],
    ) -> Result<()> {
        if len > MOUSE_BATCH_FRAMES {
            return Err(Error::SessionLifecycle("mouse batch length overflow"));
        }
        batch.push_many_slice(&frames[..len])
    }

    fn try_queue_agent_mouse_frames_into(
        batch: &mut MouseFrameBatcher<'_>,
        len: usize,
        frames: &[MouseFrame; MOUSE_BATCH_FRAMES],
    ) -> Result<()> {
        if len > MOUSE_BATCH_FRAMES {
            return Err(Error::SessionLifecycle("mouse batch length overflow"));
        }
        batch.try_push_many_slice(&frames[..len])
    }

    fn queue_agent_scroll_frames_into(
        batch: &mut ScrollFrameBatcher<'_>,
        len: usize,
        frames: &[AgentScrollFrame; SCROLL_BATCH_FRAMES],
    ) -> Result<()> {
        if len > SCROLL_BATCH_FRAMES {
            return Err(Error::SessionLifecycle("scroll batch length overflow"));
        }
        let mut converted = [ScrollFrame::EMPTY; SCROLL_BATCH_FRAMES];
        for (dst, src) in converted.iter_mut().zip(frames.iter()).take(len) {
            *dst = src.into_scroll_frame();
        }
        batch.push_many_slice(&converted[..len])
    }

    fn try_queue_agent_scroll_frames_into(
        batch: &mut ScrollFrameBatcher<'_>,
        len: usize,
        frames: &[AgentScrollFrame; SCROLL_BATCH_FRAMES],
    ) -> Result<()> {
        if len > SCROLL_BATCH_FRAMES {
            return Err(Error::SessionLifecycle("scroll batch length overflow"));
        }
        let mut converted = [ScrollFrame::EMPTY; SCROLL_BATCH_FRAMES];
        for (dst, src) in converted.iter_mut().zip(frames.iter()).take(len) {
            *dst = src.into_scroll_frame();
        }
        batch.try_push_many_slice(&converted[..len])
    }

    fn queue_tap_into(batch: &mut TouchFrameBatcher<'_>, x: i32, y: i32) -> Result<()> {
        Self::queue_tap_pointer_into(batch, TouchPointerId::finger(0), x, y)
    }

    fn queue_tap_pointer_into(
        batch: &mut TouchFrameBatcher<'_>,
        pointer_id: TouchPointerId,
        x: i32,
        y: i32,
    ) -> Result<()> {
        batch.down_pointer(pointer_id, x, y, 1.0)?;
        batch.up_pointer(pointer_id, x, y)
    }

    fn try_queue_tap_into(batch: &mut TouchFrameBatcher<'_>, x: i32, y: i32) -> Result<()> {
        Self::try_queue_tap_pointer_into(batch, TouchPointerId::finger(0), x, y)
    }

    fn try_queue_tap_pointer_into(
        batch: &mut TouchFrameBatcher<'_>,
        pointer_id: TouchPointerId,
        x: i32,
        y: i32,
    ) -> Result<()> {
        batch.try_down_pointer(pointer_id, x, y, 1.0)?;
        batch.try_up_pointer(pointer_id, x, y)
    }

    fn queue_double_tap_into(batch: &mut TouchFrameBatcher<'_>, x: i32, y: i32) -> Result<()> {
        Self::queue_double_tap_pointer_into(batch, TouchPointerId::finger(0), x, y)
    }

    fn queue_double_tap_pointer_into(
        batch: &mut TouchFrameBatcher<'_>,
        pointer_id: TouchPointerId,
        x: i32,
        y: i32,
    ) -> Result<()> {
        batch.down_pointer(pointer_id, x, y, 1.0)?;
        batch.up_pointer(pointer_id, x, y)?;
        batch.down_pointer(pointer_id, x, y, 1.0)?;
        batch.up_pointer(pointer_id, x, y)
    }

    fn try_queue_double_tap_into(batch: &mut TouchFrameBatcher<'_>, x: i32, y: i32) -> Result<()> {
        Self::try_queue_double_tap_pointer_into(batch, TouchPointerId::finger(0), x, y)
    }

    fn try_queue_double_tap_pointer_into(
        batch: &mut TouchFrameBatcher<'_>,
        pointer_id: TouchPointerId,
        x: i32,
        y: i32,
    ) -> Result<()> {
        batch.try_down_pointer(pointer_id, x, y, 1.0)?;
        batch.try_up_pointer(pointer_id, x, y)?;
        batch.try_down_pointer(pointer_id, x, y, 1.0)?;
        batch.try_up_pointer(pointer_id, x, y)
    }

    fn queue_swipe_into(
        batch: &mut TouchFrameBatcher<'_>,
        from: (i32, i32),
        to: (i32, i32),
        steps: usize,
    ) -> Result<()> {
        Self::queue_swipe_pointer_into(batch, TouchPointerId::finger(0), from, to, steps)
    }

    fn queue_swipe_pointer_into(
        batch: &mut TouchFrameBatcher<'_>,
        pointer_id: TouchPointerId,
        from: (i32, i32),
        to: (i32, i32),
        steps: usize,
    ) -> Result<()> {
        let steps = steps.max(1);
        batch.down_pointer(pointer_id, from.0, from.1, 1.0)?;
        for i in 1..=steps {
            let t = i as f32 / steps as f32;
            let x = lerp_i32(from.0, to.0, t);
            let y = lerp_i32(from.1, to.1, t);
            batch.move_pointer_to(pointer_id, x, y, 1.0)?;
        }
        batch.up_pointer(pointer_id, to.0, to.1)
    }

    fn try_queue_swipe_into(
        batch: &mut TouchFrameBatcher<'_>,
        from: (i32, i32),
        to: (i32, i32),
        steps: usize,
    ) -> Result<()> {
        Self::try_queue_swipe_pointer_into(batch, TouchPointerId::finger(0), from, to, steps)
    }

    fn try_queue_swipe_pointer_into(
        batch: &mut TouchFrameBatcher<'_>,
        pointer_id: TouchPointerId,
        from: (i32, i32),
        to: (i32, i32),
        steps: usize,
    ) -> Result<()> {
        let steps = steps.max(1);
        batch.try_down_pointer(pointer_id, from.0, from.1, 1.0)?;
        for i in 1..=steps {
            let t = i as f32 / steps as f32;
            let x = lerp_i32(from.0, to.0, t);
            let y = lerp_i32(from.1, to.1, t);
            batch.try_move_pointer_to(pointer_id, x, y, 1.0)?;
        }
        batch.try_up_pointer(pointer_id, to.0, to.1)
    }

    fn queue_pinch_into(
        batch: &mut TouchFrameBatcher<'_>,
        first_from: (i32, i32),
        first_to: (i32, i32),
        second_from: (i32, i32),
        second_to: (i32, i32),
        steps: usize,
    ) -> Result<()> {
        let steps = steps.max(1);
        batch.down(0, first_from.0, first_from.1, 1.0)?;
        batch.down(1, second_from.0, second_from.1, 1.0)?;
        for i in 1..=steps {
            let t = i as f32 / steps as f32;
            batch.move_to(
                0,
                lerp_i32(first_from.0, first_to.0, t),
                lerp_i32(first_from.1, first_to.1, t),
                1.0,
            )?;
            batch.move_to(
                1,
                lerp_i32(second_from.0, second_to.0, t),
                lerp_i32(second_from.1, second_to.1, t),
                1.0,
            )?;
        }
        batch.up(0, first_to.0, first_to.1)?;
        batch.up(1, second_to.0, second_to.1)
    }

    fn try_queue_pinch_into(
        batch: &mut TouchFrameBatcher<'_>,
        first_from: (i32, i32),
        first_to: (i32, i32),
        second_from: (i32, i32),
        second_to: (i32, i32),
        steps: usize,
    ) -> Result<()> {
        let steps = steps.max(1);
        batch.try_down(0, first_from.0, first_from.1, 1.0)?;
        batch.try_down(1, second_from.0, second_from.1, 1.0)?;
        for i in 1..=steps {
            let t = i as f32 / steps as f32;
            batch.try_move_to(
                0,
                lerp_i32(first_from.0, first_to.0, t),
                lerp_i32(first_from.1, first_to.1, t),
                1.0,
            )?;
            batch.try_move_to(
                1,
                lerp_i32(second_from.0, second_to.0, t),
                lerp_i32(second_from.1, second_to.1, t),
                1.0,
            )?;
        }
        batch.try_up(0, first_to.0, first_to.1)?;
        batch.try_up(1, second_to.0, second_to.1)
    }

    fn queue_three_finger_screenshot_into(&self, batch: &mut TouchFrameBatcher<'_>) -> Result<()> {
        let (screen_w, screen_h) = self.screen_size();
        let w = screen_w as i32;
        let h = screen_h as i32;
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
        Ok(())
    }

    fn try_queue_three_finger_screenshot_into(
        &self,
        batch: &mut TouchFrameBatcher<'_>,
    ) -> Result<()> {
        let (screen_w, screen_h) = self.screen_size();
        let w = screen_w as i32;
        let h = screen_h as i32;
        for id in 0u64..3 {
            batch.try_down(id, w / 4 * (id as i32 + 1), h / 4, 1.0)?;
        }
        for step in 1..=10 {
            for id in 0u64..3 {
                batch.try_move_to(
                    id,
                    w / 4 * (id as i32 + 1),
                    h / 4 + (h / 2 * step / 10),
                    1.0,
                )?;
            }
        }
        for id in 0u64..3 {
            batch.try_up(id, w / 4 * (id as i32 + 1), h * 3 / 4)?;
        }
        Ok(())
    }
}

fn io_to_error(e: io::Error) -> Error {
    Error::Transport(format!("{e}"))
}

fn io_to_wait_error(e: io::Error, operation: &'static str) -> Error {
    match e.kind() {
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => Error::AgentTimeout(operation),
        _ => io_to_error(e),
    }
}

#[inline]
fn basis_points_to_unit(value: u16) -> u16 {
    (((value as u32) * (u16::MAX as u32) + 5_000) / 10_000) as u16
}

#[inline]
fn normalized_rect_axis_at_basis_points(a: u16, b: u16, value: u16) -> u16 {
    let start = a.min(b) as u32;
    let end = a.max(b) as u32;
    (start + (((end - start) * (value as u32) + 5_000) / 10_000)) as u16
}

#[inline]
fn normalized_axis_to_pixel(value: u16, extent: u16) -> i32 {
    if extent <= 1 {
        return 0;
    }
    (((value as u64) * ((extent - 1) as u64) + ((u16::MAX as u64) / 2)) / (u16::MAX as u64)) as i32
}

#[inline]
fn pixel_axis_to_unit(value: i32, extent: u16) -> Result<u16> {
    if extent == 0 || value < 0 || value >= extent as i32 {
        return Err(Error::SessionLifecycle("agent rectangle out of range"));
    }
    if extent == 1 {
        return Ok(0);
    }
    Ok(
        (((value as u64) * (u16::MAX as u64) + (((extent - 1) as u64) / 2)) / ((extent - 1) as u64))
            as u16,
    )
}

#[inline]
fn pixel_rect_axis_to_unit(start: i32, len: i32, extent: u16) -> Result<(u16, u16)> {
    if len <= 0 {
        return Err(Error::SessionLifecycle("agent rectangle out of range"));
    }
    let end = start
        .checked_add(len - 1)
        .ok_or(Error::SessionLifecycle("agent rectangle out of range"))?;
    Ok((
        pixel_axis_to_unit(start, extent)?,
        pixel_axis_to_unit(end, extent)?,
    ))
}

fn best_object(objects: impl IntoIterator<Item = ObjectBox>) -> Option<ObjectBox> {
    let mut best = None;
    for object in objects {
        match best {
            Some(candidate) if object_score(object) <= object_score(candidate) => {}
            _ => best = Some(object),
        }
    }
    best
}

#[inline]
fn frame_summary_is_stable(summary: &FrameSummary) -> bool {
    !summary.is_scene_change() && !summary.is_moving()
}

#[inline]
fn object_score(object: ObjectBox) -> (u8, u32) {
    (object.confidence, object_area(object))
}

#[inline]
fn object_area(object: ObjectBox) -> u32 {
    object.w as u32 * object.h as u32
}

#[inline]
fn text_region_area(region: &TextRegion) -> u32 {
    region.w as u32 * region.h as u32
}

#[inline]
fn lerp_i32(a: i32, b: i32, t: f32) -> i32 {
    (a as f32 + (b - a) as f32 * t).round() as i32
}

impl AgentControlSession<TcpStream, TcpStream> {
    /// Connect to a forwarded scrcpy control socket, consume the out-of-band
    /// dummy/meta prefix, open the requested UHID devices, and return an
    /// agent-ready session.
    ///
    /// This assumes the standard scrcpy-server launch with
    /// `tunnel_forward=true send_dummy_byte=true`.
    pub fn connect_tcp(
        host: &str,
        port: u16,
        open: OpenRequest,
    ) -> Result<(ScrcpyControlPrefix, Self)> {
        let stream = open_tcp(host, port).map_err(|e| Error::Transport(format!("{e}")))?;
        let mut reader = stream
            .try_clone()
            .map_err(|e| Error::Transport(format!("tcp clone: {e}")))?;
        let prefix = read_scrcpy_control_prefix(&mut reader)
            .map_err(|e| Error::Transport(format!("scrcpy prefix: {e}")))?;
        let session = HidSession::open(stream, open)?;
        let agent = Self::from_parts(session, reader)?;
        Ok((prefix, agent))
    }
}

impl<T> AgentControlSession<T, TcpStream>
where
    T: TransportWrite + Send + 'static,
{
    /// Wait for a clipboard payload with a temporary `TcpStream` read timeout.
    pub fn wait_for_clipboard_timeout(&mut self, timeout: Duration) -> Result<String> {
        self.with_reader_timeout(timeout, |agent| agent.wait_for_clipboard())
    }

    /// Request the device clipboard and wait for the payload with a temporary
    /// `TcpStream` read timeout.
    pub fn get_clipboard_and_wait_timeout(
        &mut self,
        copy_key: u8,
        timeout: Duration,
    ) -> Result<String> {
        self.request_clipboard(copy_key)?;
        self.flush()?;
        self.wait_for_clipboard_timeout(timeout)
    }

    /// Request the device clipboard with a typed copy-key and bounded wait.
    pub fn get_clipboard_and_wait_key_timeout(
        &mut self,
        copy_key: ClipboardCopyKey,
        timeout: Duration,
    ) -> Result<String> {
        self.get_clipboard_and_wait_timeout(copy_key.value(), timeout)
    }

    /// Run an action plan, then wait for a clipboard payload with a temporary
    /// `TcpStream` read timeout.
    pub fn run_actions_and_wait_for_clipboard_timeout(
        &mut self,
        actions: &[AgentAction],
        timeout: Duration,
    ) -> Result<String> {
        self.run_actions(actions)?;
        self.wait_for_clipboard_timeout(timeout)
    }

    /// Run an action plan, request the device clipboard, then wait for the
    /// payload with a temporary `TcpStream` read timeout.
    pub fn run_actions_and_get_clipboard_and_wait_timeout(
        &mut self,
        actions: &[AgentAction],
        copy_key: u8,
        timeout: Duration,
    ) -> Result<String> {
        self.queue_actions(actions)?;
        self.request_clipboard(copy_key)?;
        self.flush()?;
        self.wait_for_clipboard_timeout(timeout)
    }

    /// Run an action plan, request the device clipboard with a typed copy-key,
    /// then wait for the payload with a temporary `TcpStream` read timeout.
    pub fn run_actions_and_get_clipboard_and_wait_key_timeout(
        &mut self,
        actions: &[AgentAction],
        copy_key: ClipboardCopyKey,
        timeout: Duration,
    ) -> Result<String> {
        self.run_actions_and_get_clipboard_and_wait_timeout(actions, copy_key.value(), timeout)
    }

    /// Wait for a matching clipboard ACK with a temporary `TcpStream` read
    /// timeout.
    pub fn wait_for_clipboard_ack_timeout(
        &mut self,
        sequence: u64,
        timeout: Duration,
    ) -> Result<()> {
        self.with_reader_timeout(timeout, |agent| agent.wait_for_clipboard_ack(sequence))
    }

    /// Run an action plan, then wait for a matching clipboard ACK with a
    /// temporary `TcpStream` read timeout.
    pub fn run_actions_and_wait_for_clipboard_ack_timeout(
        &mut self,
        actions: &[AgentAction],
        sequence: u64,
        timeout: Duration,
    ) -> Result<()> {
        self.run_actions(actions)?;
        self.wait_for_clipboard_ack_timeout(sequence, timeout)
    }

    /// Wait for the next AI frame summary with a temporary `TcpStream` read
    /// timeout.
    pub fn wait_for_frame_summary_timeout(&mut self, timeout: Duration) -> Result<FrameSummary> {
        self.with_reader_timeout(timeout, |agent| agent.wait_for_frame_summary())
    }

    /// Run an action plan, then wait for one frame summary with a temporary
    /// `TcpStream` read timeout.
    pub fn run_actions_and_wait_for_frame_summary_timeout(
        &mut self,
        actions: &[AgentAction],
        timeout: Duration,
    ) -> Result<FrameSummary> {
        self.run_actions(actions)?;
        self.wait_for_frame_summary_timeout(timeout)
    }

    /// Wait for a frame with `frame_seq > min_frame_seq` with a temporary
    /// `TcpStream` read timeout.
    pub fn wait_for_frame_summary_after_seq_timeout(
        &mut self,
        min_frame_seq: u32,
        timeout: Duration,
    ) -> Result<FrameSummary> {
        self.with_reader_timeout(timeout, |agent| {
            agent.wait_for_frame_summary_after_seq(min_frame_seq)
        })
    }

    /// Run an action plan, then wait for a frame with
    /// `frame_seq > min_frame_seq` with a temporary `TcpStream` read timeout.
    pub fn run_actions_and_wait_for_frame_summary_after_seq_timeout(
        &mut self,
        actions: &[AgentAction],
        min_frame_seq: u32,
        timeout: Duration,
    ) -> Result<FrameSummary> {
        self.run_actions(actions)?;
        self.wait_for_frame_summary_after_seq_timeout(min_frame_seq, timeout)
    }

    /// Wait for a frame with `timestamp_ms > min_timestamp_ms` with a temporary
    /// `TcpStream` read timeout.
    pub fn wait_for_frame_summary_after_timestamp_timeout(
        &mut self,
        min_timestamp_ms: u64,
        timeout: Duration,
    ) -> Result<FrameSummary> {
        self.with_reader_timeout(timeout, |agent| {
            agent.wait_for_frame_summary_after_timestamp(min_timestamp_ms)
        })
    }

    /// Run an action plan, then wait for a frame with
    /// `timestamp_ms > min_timestamp_ms` with a temporary `TcpStream` read
    /// timeout.
    pub fn run_actions_and_wait_for_frame_summary_after_timestamp_timeout(
        &mut self,
        actions: &[AgentAction],
        min_timestamp_ms: u64,
        timeout: Duration,
    ) -> Result<FrameSummary> {
        self.run_actions(actions)?;
        self.wait_for_frame_summary_after_timestamp_timeout(min_timestamp_ms, timeout)
    }

    /// Wait for the next scene-change frame with a temporary `TcpStream` read
    /// timeout.
    pub fn wait_for_scene_change_timeout(&mut self, timeout: Duration) -> Result<FrameSummary> {
        self.with_reader_timeout(timeout, |agent| agent.wait_for_scene_change())
    }

    /// Run an action plan, then wait for one scene-change frame with a
    /// temporary `TcpStream` read timeout.
    pub fn run_actions_and_wait_for_scene_change_timeout(
        &mut self,
        actions: &[AgentAction],
        timeout: Duration,
    ) -> Result<FrameSummary> {
        self.run_actions(actions)?;
        self.wait_for_scene_change_timeout(timeout)
    }

    /// Wait for the next frame with motion vectors with a temporary
    /// `TcpStream` read timeout.
    pub fn wait_for_motion_timeout(&mut self, timeout: Duration) -> Result<FrameSummary> {
        self.with_reader_timeout(timeout, |agent| agent.wait_for_motion())
    }

    /// Run an action plan, then wait for one motion frame with a temporary
    /// `TcpStream` read timeout.
    pub fn run_actions_and_wait_for_motion_timeout(
        &mut self,
        actions: &[AgentAction],
        timeout: Duration,
    ) -> Result<FrameSummary> {
        self.run_actions(actions)?;
        self.wait_for_motion_timeout(timeout)
    }

    /// Wait for the next stable frame with a temporary `TcpStream` read
    /// timeout.
    pub fn wait_for_stable_frame_timeout(&mut self, timeout: Duration) -> Result<FrameSummary> {
        self.with_reader_timeout(timeout, |agent| agent.wait_for_stable_frame())
    }

    /// Run an action plan, then wait for one stable frame with a temporary
    /// `TcpStream` read timeout.
    pub fn run_actions_and_wait_for_stable_frame_timeout(
        &mut self,
        actions: &[AgentAction],
        timeout: Duration,
    ) -> Result<FrameSummary> {
        self.run_actions_and_wait_for_stable_frames_timeout(actions, 1, timeout)
    }

    /// Wait for `consecutive` stable frames with a temporary `TcpStream` read
    /// timeout.
    pub fn wait_for_stable_frames_timeout(
        &mut self,
        consecutive: usize,
        timeout: Duration,
    ) -> Result<FrameSummary> {
        self.with_reader_timeout(timeout, |agent| agent.wait_for_stable_frames(consecutive))
    }

    /// Run an action plan, then wait for `consecutive` stable frames with a
    /// temporary `TcpStream` read timeout.
    pub fn run_actions_and_wait_for_stable_frames_timeout(
        &mut self,
        actions: &[AgentAction],
        consecutive: usize,
        timeout: Duration,
    ) -> Result<FrameSummary> {
        self.run_actions(actions)?;
        self.wait_for_stable_frames_timeout(consecutive, timeout)
    }

    /// Wait for the next best object target with a temporary `TcpStream` read
    /// timeout.
    pub fn wait_for_best_object_rect_timeout(&mut self, timeout: Duration) -> Result<AgentRect> {
        self.with_reader_timeout(timeout, |agent| agent.wait_for_best_object_rect())
    }

    /// Wait for the indexed object target with a temporary `TcpStream` read
    /// timeout.
    pub fn wait_for_object_rect_timeout(
        &mut self,
        index: usize,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.with_reader_timeout(timeout, |agent| agent.wait_for_object_rect(index))
    }

    /// Wait for the next best object target of `class_id` with a temporary
    /// `TcpStream` read timeout.
    pub fn wait_for_best_object_class_rect_timeout(
        &mut self,
        class_id: u8,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.with_reader_timeout(timeout, |agent| {
            agent.wait_for_best_object_class_rect(class_id)
        })
    }

    /// Wait for the next object matching `selector` with a temporary
    /// `TcpStream` read timeout.
    pub fn wait_for_object_selector_rect_timeout(
        &mut self,
        selector: AgentObjectSelector,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.with_reader_timeout(timeout, |agent| {
            agent.wait_for_object_selector_rect(selector)
        })
    }

    /// Wait for the next largest text region with a temporary `TcpStream` read
    /// timeout.
    pub fn wait_for_largest_text_region_rect_timeout(
        &mut self,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.with_reader_timeout(timeout, |agent| agent.wait_for_largest_text_region_rect())
    }

    /// Wait for the indexed text region with a temporary `TcpStream` read
    /// timeout.
    pub fn wait_for_text_region_rect_timeout(
        &mut self,
        index: usize,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.with_reader_timeout(timeout, |agent| agent.wait_for_text_region_rect(index))
    }

    /// Wait for any supported object/text target selected by `target` with a
    /// temporary `TcpStream` read timeout.
    pub fn wait_for_target_rect_timeout(
        &mut self,
        target: AgentTargetSelector,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.with_reader_timeout(timeout, |agent| agent.wait_for_target_rect(target))
    }

    /// Run an action plan, then wait for the next best object target with a
    /// temporary `TcpStream` read timeout.
    pub fn run_actions_and_wait_for_best_object_rect_timeout(
        &mut self,
        actions: &[AgentAction],
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.wait_for_best_object_rect_timeout(timeout)
    }

    /// Run an action plan, then wait for the indexed object target with a
    /// temporary `TcpStream` read timeout.
    pub fn run_actions_and_wait_for_object_rect_timeout(
        &mut self,
        actions: &[AgentAction],
        index: usize,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.wait_for_object_rect_timeout(index, timeout)
    }

    /// Run an action plan, then wait for the next best object target of
    /// `class_id` with a temporary `TcpStream` read timeout.
    pub fn run_actions_and_wait_for_best_object_class_rect_timeout(
        &mut self,
        actions: &[AgentAction],
        class_id: u8,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.wait_for_best_object_class_rect_timeout(class_id, timeout)
    }

    /// Run an action plan, then wait for the next object matching `selector`
    /// with a temporary `TcpStream` read timeout.
    pub fn run_actions_and_wait_for_object_selector_rect_timeout(
        &mut self,
        actions: &[AgentAction],
        selector: AgentObjectSelector,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.wait_for_object_selector_rect_timeout(selector, timeout)
    }

    /// Run an action plan, then wait for the next largest text region with a
    /// temporary `TcpStream` read timeout.
    pub fn run_actions_and_wait_for_largest_text_region_rect_timeout(
        &mut self,
        actions: &[AgentAction],
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.wait_for_largest_text_region_rect_timeout(timeout)
    }

    /// Run an action plan, then wait for the indexed text region with a
    /// temporary `TcpStream` read timeout.
    pub fn run_actions_and_wait_for_text_region_rect_timeout(
        &mut self,
        actions: &[AgentAction],
        index: usize,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.wait_for_text_region_rect_timeout(index, timeout)
    }

    /// Run an action plan, then wait for any supported object/text target
    /// selected by `target` with a temporary `TcpStream` read timeout.
    pub fn run_actions_and_wait_for_target_rect_timeout(
        &mut self,
        actions: &[AgentAction],
        target: AgentTargetSelector,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.wait_for_target_rect_timeout(target, timeout)
    }

    /// Run an action plan, then wait for any supported object/text target
    /// selected by `target` with a temporary `TcpStream` read timeout and tap
    /// its center.
    pub fn run_actions_and_tap_next_target_timeout(
        &mut self,
        actions: &[AgentAction],
        target: AgentTargetSelector,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_target_timeout(target, timeout)
    }

    /// Run an action plan, then wait for any supported object/text target
    /// selected by `target` with a temporary `TcpStream` read timeout and tap
    /// its center with a typed scrcpy pointer id.
    pub fn run_actions_and_tap_next_target_pointer_timeout(
        &mut self,
        actions: &[AgentAction],
        target: AgentTargetSelector,
        pointer_id: TouchPointerId,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_target_pointer_timeout(target, pointer_id, timeout)
    }

    /// Run an action plan, then wait for any supported object/text target
    /// selected by `target` with a temporary `TcpStream` read timeout and tap a
    /// relative point inside it.
    pub fn run_actions_and_tap_next_target_at_timeout(
        &mut self,
        actions: &[AgentAction],
        target: AgentTargetSelector,
        x_bp: u16,
        y_bp: u16,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_target_at_timeout(target, x_bp, y_bp, timeout)
    }

    /// Run an action plan, then wait for any supported object/text target
    /// selected by `target` with a temporary `TcpStream` read timeout and tap a
    /// relative point inside it with a typed scrcpy pointer id.
    pub fn run_actions_and_tap_next_target_at_pointer_timeout(
        &mut self,
        actions: &[AgentAction],
        target: AgentTargetSelector,
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_target_at_pointer_timeout(target, pointer_id, x_bp, y_bp, timeout)
    }

    /// Run an action plan, then wait for the indexed object with a temporary
    /// `TcpStream` read timeout and tap its center.
    pub fn run_actions_and_tap_next_object_timeout(
        &mut self,
        actions: &[AgentAction],
        index: usize,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_object_timeout(index, timeout)
    }

    /// Run an action plan, then wait for the indexed object with a temporary
    /// `TcpStream` read timeout and tap its center with a typed scrcpy pointer
    /// id.
    pub fn run_actions_and_tap_next_object_pointer_timeout(
        &mut self,
        actions: &[AgentAction],
        index: usize,
        pointer_id: TouchPointerId,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_object_pointer_timeout(index, pointer_id, timeout)
    }

    /// Run an action plan, then wait for the indexed object with a temporary
    /// `TcpStream` read timeout and tap a relative point inside it.
    pub fn run_actions_and_tap_next_object_at_timeout(
        &mut self,
        actions: &[AgentAction],
        index: usize,
        x_bp: u16,
        y_bp: u16,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_object_at_timeout(index, x_bp, y_bp, timeout)
    }

    /// Run an action plan, then wait for the indexed object with a temporary
    /// `TcpStream` read timeout and tap a relative point inside it with a typed
    /// scrcpy pointer id.
    pub fn run_actions_and_tap_next_object_at_pointer_timeout(
        &mut self,
        actions: &[AgentAction],
        index: usize,
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_object_at_pointer_timeout(index, pointer_id, x_bp, y_bp, timeout)
    }

    /// Run an action plan, then wait for the next best object with a temporary
    /// `TcpStream` read timeout and tap its center.
    pub fn run_actions_and_tap_next_best_object_timeout(
        &mut self,
        actions: &[AgentAction],
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_best_object_timeout(timeout)
    }

    /// Run an action plan, then wait for the next best object with a temporary
    /// `TcpStream` read timeout and tap its center with a typed scrcpy pointer
    /// id.
    pub fn run_actions_and_tap_next_best_object_pointer_timeout(
        &mut self,
        actions: &[AgentAction],
        pointer_id: TouchPointerId,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_best_object_pointer_timeout(pointer_id, timeout)
    }

    /// Run an action plan, then wait for the next best object with a temporary
    /// `TcpStream` read timeout and tap a relative point inside it.
    pub fn run_actions_and_tap_next_best_object_at_timeout(
        &mut self,
        actions: &[AgentAction],
        x_bp: u16,
        y_bp: u16,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_best_object_at_timeout(x_bp, y_bp, timeout)
    }

    /// Run an action plan, then wait for the next best object with a temporary
    /// `TcpStream` read timeout and tap a relative point inside it with a typed
    /// scrcpy pointer id.
    pub fn run_actions_and_tap_next_best_object_at_pointer_timeout(
        &mut self,
        actions: &[AgentAction],
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_best_object_at_pointer_timeout(pointer_id, x_bp, y_bp, timeout)
    }

    /// Run an action plan, then wait for the next best object of `class_id`
    /// with a temporary `TcpStream` read timeout and tap its center.
    pub fn run_actions_and_tap_next_object_class_timeout(
        &mut self,
        actions: &[AgentAction],
        class_id: u8,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_object_class_timeout(class_id, timeout)
    }

    /// Run an action plan, then wait for the next best object of `class_id`
    /// with a temporary `TcpStream` read timeout and tap its center with a typed
    /// scrcpy pointer id.
    pub fn run_actions_and_tap_next_object_class_pointer_timeout(
        &mut self,
        actions: &[AgentAction],
        class_id: u8,
        pointer_id: TouchPointerId,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_object_class_pointer_timeout(class_id, pointer_id, timeout)
    }

    /// Run an action plan, then wait for the next best object of `class_id`
    /// with a temporary `TcpStream` read timeout and tap a relative point inside
    /// it.
    pub fn run_actions_and_tap_next_object_class_at_timeout(
        &mut self,
        actions: &[AgentAction],
        class_id: u8,
        x_bp: u16,
        y_bp: u16,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_object_class_at_timeout(class_id, x_bp, y_bp, timeout)
    }

    /// Run an action plan, then wait for the next best object of `class_id`
    /// with a temporary `TcpStream` read timeout and tap a relative point inside
    /// it with a typed scrcpy pointer id.
    pub fn run_actions_and_tap_next_object_class_at_pointer_timeout(
        &mut self,
        actions: &[AgentAction],
        class_id: u8,
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_object_class_at_pointer_timeout(class_id, pointer_id, x_bp, y_bp, timeout)
    }

    /// Run an action plan, then wait for the next object matching `selector`
    /// with a temporary `TcpStream` read timeout and tap its center.
    pub fn run_actions_and_tap_next_object_selector_timeout(
        &mut self,
        actions: &[AgentAction],
        selector: AgentObjectSelector,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_object_selector_timeout(selector, timeout)
    }

    /// Run an action plan, then wait for the next object matching `selector`
    /// with a temporary `TcpStream` read timeout and tap its center with a
    /// typed scrcpy pointer id.
    pub fn run_actions_and_tap_next_object_selector_pointer_timeout(
        &mut self,
        actions: &[AgentAction],
        selector: AgentObjectSelector,
        pointer_id: TouchPointerId,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_object_selector_pointer_timeout(selector, pointer_id, timeout)
    }

    /// Run an action plan, then wait for the next object matching `selector`
    /// with a temporary `TcpStream` read timeout and tap a relative point
    /// inside it.
    pub fn run_actions_and_tap_next_object_selector_at_timeout(
        &mut self,
        actions: &[AgentAction],
        selector: AgentObjectSelector,
        x_bp: u16,
        y_bp: u16,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_object_selector_at_timeout(selector, x_bp, y_bp, timeout)
    }

    /// Run an action plan, then wait for the next object matching `selector`
    /// with a temporary `TcpStream` read timeout and tap a relative point
    /// inside it with a typed scrcpy pointer id.
    pub fn run_actions_and_tap_next_object_selector_at_pointer_timeout(
        &mut self,
        actions: &[AgentAction],
        selector: AgentObjectSelector,
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_object_selector_at_pointer_timeout(selector, pointer_id, x_bp, y_bp, timeout)
    }

    /// Run an action plan, then wait for the indexed text region with a
    /// temporary `TcpStream` read timeout and tap its center.
    pub fn run_actions_and_tap_next_text_region_timeout(
        &mut self,
        actions: &[AgentAction],
        index: usize,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_text_region_timeout(index, timeout)
    }

    /// Run an action plan, then wait for the indexed text region with a
    /// temporary `TcpStream` read timeout and tap its center with a typed scrcpy
    /// pointer id.
    pub fn run_actions_and_tap_next_text_region_pointer_timeout(
        &mut self,
        actions: &[AgentAction],
        index: usize,
        pointer_id: TouchPointerId,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_text_region_pointer_timeout(index, pointer_id, timeout)
    }

    /// Run an action plan, then wait for the indexed text region with a
    /// temporary `TcpStream` read timeout and tap a relative point inside it.
    pub fn run_actions_and_tap_next_text_region_at_timeout(
        &mut self,
        actions: &[AgentAction],
        index: usize,
        x_bp: u16,
        y_bp: u16,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_text_region_at_timeout(index, x_bp, y_bp, timeout)
    }

    /// Run an action plan, then wait for the indexed text region with a
    /// temporary `TcpStream` read timeout and tap a relative point inside it
    /// with a typed scrcpy pointer id.
    pub fn run_actions_and_tap_next_text_region_at_pointer_timeout(
        &mut self,
        actions: &[AgentAction],
        index: usize,
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_text_region_at_pointer_timeout(index, pointer_id, x_bp, y_bp, timeout)
    }

    /// Run an action plan, then wait for the next largest text region with a
    /// temporary `TcpStream` read timeout and tap its center.
    pub fn run_actions_and_tap_next_largest_text_region_timeout(
        &mut self,
        actions: &[AgentAction],
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_largest_text_region_timeout(timeout)
    }

    /// Run an action plan, then wait for the next largest text region with a
    /// temporary `TcpStream` read timeout and tap its center with a typed scrcpy
    /// pointer id.
    pub fn run_actions_and_tap_next_largest_text_region_pointer_timeout(
        &mut self,
        actions: &[AgentAction],
        pointer_id: TouchPointerId,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_largest_text_region_pointer_timeout(pointer_id, timeout)
    }

    /// Run an action plan, then wait for the next largest text region with a
    /// temporary `TcpStream` read timeout and tap a relative point inside it.
    pub fn run_actions_and_tap_next_largest_text_region_at_timeout(
        &mut self,
        actions: &[AgentAction],
        x_bp: u16,
        y_bp: u16,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_largest_text_region_at_timeout(x_bp, y_bp, timeout)
    }

    /// Run an action plan, then wait for the next largest text region with a
    /// temporary `TcpStream` read timeout and tap a relative point inside it
    /// with a typed scrcpy pointer id.
    pub fn run_actions_and_tap_next_largest_text_region_at_pointer_timeout(
        &mut self,
        actions: &[AgentAction],
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_largest_text_region_at_pointer_timeout(pointer_id, x_bp, y_bp, timeout)
    }

    /// Wait for any supported object/text target selected by `target`, tap its
    /// center, and restore the previous `TcpStream` read timeout.
    pub fn tap_next_target_timeout(
        &mut self,
        target: AgentTargetSelector,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.with_reader_timeout(timeout, |agent| agent.tap_next_target(target))
    }

    /// Wait for any supported object/text target selected by `target`, tap its
    /// center with a typed scrcpy pointer id, and restore the previous
    /// `TcpStream` read timeout.
    pub fn tap_next_target_pointer_timeout(
        &mut self,
        target: AgentTargetSelector,
        pointer_id: TouchPointerId,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.with_reader_timeout(timeout, |agent| {
            agent.tap_next_target_pointer(target, pointer_id)
        })
    }

    /// Wait for any supported object/text target selected by `target`, tap a
    /// relative point inside it, and restore the previous `TcpStream` read
    /// timeout.
    pub fn tap_next_target_at_timeout(
        &mut self,
        target: AgentTargetSelector,
        x_bp: u16,
        y_bp: u16,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.with_reader_timeout(timeout, |agent| {
            agent.tap_next_target_at(target, x_bp, y_bp)
        })
    }

    /// Wait for any supported object/text target selected by `target`, tap a
    /// relative point inside it with a typed scrcpy pointer id, and restore the
    /// previous `TcpStream` read timeout.
    pub fn tap_next_target_at_pointer_timeout(
        &mut self,
        target: AgentTargetSelector,
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.with_reader_timeout(timeout, |agent| {
            agent.tap_next_target_at_pointer(target, pointer_id, x_bp, y_bp)
        })
    }

    /// Wait for the indexed object target, tap its center, and restore the
    /// previous `TcpStream` read timeout.
    pub fn tap_next_object_timeout(
        &mut self,
        index: usize,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.with_reader_timeout(timeout, |agent| agent.tap_next_object(index))
    }

    /// Wait for the indexed object target, tap its center with a typed scrcpy
    /// pointer id, and restore the previous `TcpStream` read timeout.
    pub fn tap_next_object_pointer_timeout(
        &mut self,
        index: usize,
        pointer_id: TouchPointerId,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.with_reader_timeout(timeout, |agent| {
            agent.tap_next_object_pointer(index, pointer_id)
        })
    }

    /// Wait for the indexed object target, tap a relative point inside it, and
    /// restore the previous `TcpStream` read timeout.
    pub fn tap_next_object_at_timeout(
        &mut self,
        index: usize,
        x_bp: u16,
        y_bp: u16,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.with_reader_timeout(timeout, |agent| agent.tap_next_object_at(index, x_bp, y_bp))
    }

    /// Wait for the indexed object target, tap a relative point inside it with
    /// a typed scrcpy pointer id, and restore the previous `TcpStream` read
    /// timeout.
    pub fn tap_next_object_at_pointer_timeout(
        &mut self,
        index: usize,
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.with_reader_timeout(timeout, |agent| {
            agent.tap_next_object_at_pointer(index, pointer_id, x_bp, y_bp)
        })
    }

    /// Wait for the next best object target, tap its center, and restore the
    /// previous `TcpStream` read timeout.
    pub fn tap_next_best_object_timeout(&mut self, timeout: Duration) -> Result<AgentRect> {
        self.with_reader_timeout(timeout, |agent| agent.tap_next_best_object())
    }

    /// Wait for the next best object target, tap its center with a typed scrcpy
    /// pointer id, and restore the previous `TcpStream` read timeout.
    pub fn tap_next_best_object_pointer_timeout(
        &mut self,
        pointer_id: TouchPointerId,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.with_reader_timeout(timeout, |agent| {
            agent.tap_next_best_object_pointer(pointer_id)
        })
    }

    /// Wait for the next best object target, tap a relative point inside it,
    /// and restore the previous `TcpStream` read timeout.
    pub fn tap_next_best_object_at_timeout(
        &mut self,
        x_bp: u16,
        y_bp: u16,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.with_reader_timeout(timeout, |agent| agent.tap_next_best_object_at(x_bp, y_bp))
    }

    /// Wait for the next best object target, tap a relative point inside it
    /// with a typed scrcpy pointer id, and restore the previous `TcpStream`
    /// read timeout.
    pub fn tap_next_best_object_at_pointer_timeout(
        &mut self,
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.with_reader_timeout(timeout, |agent| {
            agent.tap_next_best_object_at_pointer(pointer_id, x_bp, y_bp)
        })
    }

    /// Wait for the next best target of `class_id`, tap its center, and restore
    /// the previous `TcpStream` read timeout.
    pub fn tap_next_object_class_timeout(
        &mut self,
        class_id: u8,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.with_reader_timeout(timeout, |agent| agent.tap_next_object_class(class_id))
    }

    /// Wait for the next best target of `class_id`, tap its center with a typed
    /// scrcpy pointer id, and restore the previous `TcpStream` read timeout.
    pub fn tap_next_object_class_pointer_timeout(
        &mut self,
        class_id: u8,
        pointer_id: TouchPointerId,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.with_reader_timeout(timeout, |agent| {
            agent.tap_next_object_class_pointer(class_id, pointer_id)
        })
    }

    /// Wait for the next best target of `class_id`, tap a relative point inside
    /// it, and restore the previous `TcpStream` read timeout.
    pub fn tap_next_object_class_at_timeout(
        &mut self,
        class_id: u8,
        x_bp: u16,
        y_bp: u16,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.with_reader_timeout(timeout, |agent| {
            agent.tap_next_object_class_at(class_id, x_bp, y_bp)
        })
    }

    /// Wait for the next best target of `class_id`, tap a relative point inside
    /// it with a typed scrcpy pointer id, and restore the previous `TcpStream`
    /// read timeout.
    pub fn tap_next_object_class_at_pointer_timeout(
        &mut self,
        class_id: u8,
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.with_reader_timeout(timeout, |agent| {
            agent.tap_next_object_class_at_pointer(class_id, pointer_id, x_bp, y_bp)
        })
    }

    /// Wait for the next object matching `selector`, tap its center, and
    /// restore the previous `TcpStream` read timeout.
    pub fn tap_next_object_selector_timeout(
        &mut self,
        selector: AgentObjectSelector,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.with_reader_timeout(timeout, |agent| agent.tap_next_object_selector(selector))
    }

    /// Wait for the next object matching `selector`, tap its center with a typed
    /// scrcpy pointer id, and restore the previous `TcpStream` read timeout.
    pub fn tap_next_object_selector_pointer_timeout(
        &mut self,
        selector: AgentObjectSelector,
        pointer_id: TouchPointerId,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.with_reader_timeout(timeout, |agent| {
            agent.tap_next_object_selector_pointer(selector, pointer_id)
        })
    }

    /// Wait for the next object matching `selector`, tap a relative point
    /// inside it, and restore the previous `TcpStream` read timeout.
    pub fn tap_next_object_selector_at_timeout(
        &mut self,
        selector: AgentObjectSelector,
        x_bp: u16,
        y_bp: u16,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.with_reader_timeout(timeout, |agent| {
            agent.tap_next_object_selector_at(selector, x_bp, y_bp)
        })
    }

    /// Wait for the next object matching `selector`, tap a relative point
    /// inside it with a typed scrcpy pointer id, and restore the previous
    /// `TcpStream` read timeout.
    pub fn tap_next_object_selector_at_pointer_timeout(
        &mut self,
        selector: AgentObjectSelector,
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.with_reader_timeout(timeout, |agent| {
            agent.tap_next_object_selector_at_pointer(selector, pointer_id, x_bp, y_bp)
        })
    }

    /// Wait for the indexed text region, tap its center, and restore the
    /// previous `TcpStream` read timeout.
    pub fn tap_next_text_region_timeout(
        &mut self,
        index: usize,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.with_reader_timeout(timeout, |agent| agent.tap_next_text_region(index))
    }

    /// Wait for the indexed text region, tap its center with a typed scrcpy
    /// pointer id, and restore the previous `TcpStream` read timeout.
    pub fn tap_next_text_region_pointer_timeout(
        &mut self,
        index: usize,
        pointer_id: TouchPointerId,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.with_reader_timeout(timeout, |agent| {
            agent.tap_next_text_region_pointer(index, pointer_id)
        })
    }

    /// Wait for the indexed text region, tap a relative point inside it, and
    /// restore the previous `TcpStream` read timeout.
    pub fn tap_next_text_region_at_timeout(
        &mut self,
        index: usize,
        x_bp: u16,
        y_bp: u16,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.with_reader_timeout(timeout, |agent| {
            agent.tap_next_text_region_at(index, x_bp, y_bp)
        })
    }

    /// Wait for the indexed text region, tap a relative point inside it with a
    /// typed scrcpy pointer id, and restore the previous `TcpStream` read
    /// timeout.
    pub fn tap_next_text_region_at_pointer_timeout(
        &mut self,
        index: usize,
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.with_reader_timeout(timeout, |agent| {
            agent.tap_next_text_region_at_pointer(index, pointer_id, x_bp, y_bp)
        })
    }

    /// Wait for the next largest text region, tap its center, and restore the
    /// previous `TcpStream` read timeout.
    pub fn tap_next_largest_text_region_timeout(&mut self, timeout: Duration) -> Result<AgentRect> {
        self.with_reader_timeout(timeout, |agent| agent.tap_next_largest_text_region())
    }

    /// Wait for the next largest text region, tap its center with a typed
    /// scrcpy pointer id, and restore the previous `TcpStream` read timeout.
    pub fn tap_next_largest_text_region_pointer_timeout(
        &mut self,
        pointer_id: TouchPointerId,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.with_reader_timeout(timeout, |agent| {
            agent.tap_next_largest_text_region_pointer(pointer_id)
        })
    }

    /// Wait for the next largest text region, tap a relative point inside it,
    /// and restore the previous `TcpStream` read timeout.
    pub fn tap_next_largest_text_region_at_timeout(
        &mut self,
        x_bp: u16,
        y_bp: u16,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.with_reader_timeout(timeout, |agent| {
            agent.tap_next_largest_text_region_at(x_bp, y_bp)
        })
    }

    /// Wait for the next largest text region, tap a relative point inside it
    /// with a typed scrcpy pointer id, and restore the previous `TcpStream`
    /// read timeout.
    pub fn tap_next_largest_text_region_at_pointer_timeout(
        &mut self,
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.with_reader_timeout(timeout, |agent| {
            agent.tap_next_largest_text_region_at_pointer(pointer_id, x_bp, y_bp)
        })
    }

    /// Wait for the next AI stats envelope with a temporary `TcpStream` read
    /// timeout.
    pub fn wait_for_ai_stats_timeout(&mut self, timeout: Duration) -> Result<AiStats> {
        self.with_reader_timeout(timeout, |agent| agent.wait_for_ai_stats())
    }

    /// Query the AI extension and wait for stats with a temporary `TcpStream`
    /// read timeout.
    pub fn query_ai_and_wait_stats_timeout(
        &mut self,
        since_timestamp_ms: u64,
        timeout: Duration,
    ) -> Result<AiStats> {
        self.query_ai(since_timestamp_ms)?;
        self.flush()?;
        self.wait_for_ai_stats_timeout(timeout)
    }

    /// Run an action plan, query the AI extension, and wait for stats with a
    /// temporary `TcpStream` read timeout.
    pub fn run_actions_and_query_ai_and_wait_stats_timeout(
        &mut self,
        actions: &[AgentAction],
        since_timestamp_ms: u64,
        timeout: Duration,
    ) -> Result<AiStats> {
        self.queue_actions(actions)?;
        self.query_ai(since_timestamp_ms)?;
        self.flush()?;
        self.wait_for_ai_stats_timeout(timeout)
    }

    /// Set the device clipboard and wait for its matching ACK with a temporary
    /// `TcpStream` read timeout.
    pub fn set_clipboard_and_wait_ack_timeout(
        &mut self,
        text: impl Into<String>,
        paste: bool,
        timeout: Duration,
    ) -> Result<u64> {
        let sequence = self.next_clipboard_sequence();
        self.set_clipboard_sequenced(sequence, text, paste)?;
        self.flush()?;
        self.wait_for_clipboard_ack_timeout(sequence, timeout)?;
        Ok(sequence)
    }

    /// Run an action plan, set the device clipboard, and wait for its matching
    /// ACK with a temporary `TcpStream` read timeout.
    pub fn run_actions_and_set_clipboard_and_wait_ack_timeout(
        &mut self,
        actions: &[AgentAction],
        text: impl Into<String>,
        paste: bool,
        timeout: Duration,
    ) -> Result<u64> {
        let sequence = self.next_clipboard_sequence();
        self.queue_actions(actions)?;
        self.set_clipboard_sequenced(sequence, text, paste)?;
        self.flush()?;
        self.wait_for_clipboard_ack_timeout(sequence, timeout)?;
        Ok(sequence)
    }

    fn with_reader_timeout<O>(
        &mut self,
        timeout: Duration,
        f: impl FnOnce(&mut Self) -> Result<O>,
    ) -> Result<O> {
        let previous = self.set_reader_timeout(Some(timeout))?;
        let result = f(self);
        let restore = self.set_reader_timeout(previous);
        match (result, restore) {
            (Ok(value), Ok(_)) => Ok(value),
            (Err(err), _) => Err(err),
            (Ok(_), Err(err)) => Err(err),
        }
    }

    fn set_reader_timeout(&mut self, timeout: Option<Duration>) -> Result<Option<Duration>> {
        let stream = self.receiver_mut().map_err(io_to_error)?.get_mut();
        let previous = stream
            .read_timeout()
            .map_err(|e| Error::Transport(format!("read_timeout: {e}")))?;
        stream
            .set_read_timeout(timeout)
            .map_err(|e| Error::Transport(format!("set_read_timeout: {e}")))?;
        Ok(previous)
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;

    use super::*;
    use crate::device::{TYPE_ACK_CLIPBOARD, TYPE_UHID_OUTPUT};
    use crate::session::GamepadFrameRaw;
    use crate::transport::MockTransport;

    #[derive(Debug)]
    struct TimedOutReader;

    impl Read for TimedOutReader {
        fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::TimedOut, "synthetic timeout"))
        }
    }

    fn ack(sequence: u64) -> Vec<u8> {
        let mut bytes = vec![TYPE_ACK_CLIPBOARD];
        bytes.extend(sequence.to_be_bytes());
        bytes
    }

    fn clipboard(text: &str) -> Vec<u8> {
        let mut bytes = vec![crate::device::TYPE_CLIPBOARD];
        bytes.extend((text.len() as u32).to_be_bytes());
        bytes.extend(text.as_bytes());
        bytes
    }

    fn frame_summary_envelope(frame_seq: u32) -> Vec<u8> {
        frame_summary_envelope_with(
            frame_seq,
            &[ObjectBox {
                x: 100,
                y: 200,
                w: 301,
                h: 101,
                class_id: 7,
                confidence: 220,
            }],
            &[],
        )
    }

    fn frame_summary_envelope_with(
        frame_seq: u32,
        objects: &[ObjectBox],
        text_regions: &[TextRegion],
    ) -> Vec<u8> {
        frame_summary_envelope_full(
            frame_seq,
            {
                let mut flags = crate::ai::FLAG_KEYFRAME;
                if !objects.is_empty() {
                    flags |= crate::ai::FLAG_OBJECTS;
                }
                if !text_regions.is_empty() {
                    flags |= crate::ai::FLAG_TEXT;
                }
                flags
            },
            &[],
            objects,
            text_regions,
        )
    }

    fn frame_summary_envelope_full(
        frame_seq: u32,
        flags: u8,
        motion: &[crate::ai::MotionVector],
        objects: &[ObjectBox],
        text_regions: &[TextRegion],
    ) -> Vec<u8> {
        frame_summary_envelope_full_at(100, frame_seq, flags, motion, objects, text_regions)
    }

    fn frame_summary_envelope_full_at(
        timestamp_ms: u64,
        frame_seq: u32,
        flags: u8,
        motion: &[crate::ai::MotionVector],
        objects: &[ObjectBox],
        text_regions: &[TextRegion],
    ) -> Vec<u8> {
        let mut payload = Vec::new();
        payload.extend(timestamp_ms.to_be_bytes());
        payload.extend(frame_seq.to_be_bytes());
        payload.extend(1000u16.to_be_bytes());
        payload.extend(2000u16.to_be_bytes());
        payload.push(flags);
        payload.extend(0u16.to_be_bytes());
        payload.extend((motion.len() as u16).to_be_bytes());
        for vector in motion {
            payload.extend(vector.x.to_be_bytes());
            payload.extend(vector.y.to_be_bytes());
            payload.extend(vector.dx.to_be_bytes());
            payload.extend(vector.dy.to_be_bytes());
        }
        payload.extend((objects.len() as u16).to_be_bytes());
        for object in objects {
            payload.extend(object.x.to_be_bytes());
            payload.extend(object.y.to_be_bytes());
            payload.extend(object.w.to_be_bytes());
            payload.extend(object.h.to_be_bytes());
            payload.push(object.class_id);
            payload.push(object.confidence);
        }
        payload.push(text_regions.len() as u8);
        for region in text_regions {
            payload.extend(region.x.to_be_bytes());
            payload.extend(region.y.to_be_bytes());
            payload.extend(region.w.to_be_bytes());
            payload.extend(region.h.to_be_bytes());
        }

        let mut bytes = vec![crate::ai::TYPE_FRAME_SUMMARY];
        bytes.extend((payload.len() as u32).to_be_bytes());
        bytes.extend(payload);
        bytes
    }

    fn ai_stats_envelope() -> Vec<u8> {
        let mut payload = Vec::new();
        payload.extend(1_000u64.to_be_bytes());
        payload.extend(10u32.to_be_bytes());
        payload.extend(1u32.to_be_bytes());
        payload.extend(2u32.to_be_bytes());
        payload.extend(300u64.to_be_bytes());
        payload.extend(4.5f32.to_be_bytes());
        payload.extend(60.0f32.to_be_bytes());

        let mut bytes = vec![crate::ai::TYPE_AI_STATS];
        bytes.extend((payload.len() as u32).to_be_bytes());
        bytes.extend(payload);
        bytes
    }

    fn latest_snapshot_from_envelope(
        version: u64,
        envelope: Vec<u8>,
    ) -> LatestFrameSummarySnapshot {
        match crate::device::read_device_event(&mut Cursor::new(envelope)).unwrap() {
            DeviceEvent::FrameSummary(summary) => LatestFrameSummarySnapshot { version, summary },
            other => panic!("expected frame summary, got {other:?}"),
        }
    }

    fn clipboard_ack_stream(sequence: u64) -> Cursor<Vec<u8>> {
        Cursor::new(ack(sequence))
    }

    fn tcp_agent_with_reader_bytes(
        bytes: Vec<u8>,
    ) -> (
        AgentControlSession<MockTransport, TcpStream>,
        std::thread::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut sock, _addr) = listener.accept().unwrap();
            Write::write_all(&mut sock, &bytes).unwrap();
        });

        let reader = TcpStream::connect(addr).unwrap();
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        (
            AgentControlSession::from_parts(session, reader).unwrap(),
            server,
        )
    }

    fn count_uhid_inputs(buf: &[u8]) -> usize {
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
                TYPE_UHID_OUTPUT => break,
                13 => {
                    if i + 5 > buf.len() {
                        break;
                    }
                    let size = u16::from_be_bytes([buf[i + 3], buf[i + 4]]) as usize;
                    n += 1;
                    i += 5 + size;
                }
                14 => i += 3,
                _ => break,
            }
        }
        n
    }

    fn count_touch_events(buf: &[u8]) -> usize {
        let mut i = 0usize;
        let mut n = 0usize;
        while i + 32 <= buf.len() {
            if buf[i] == 2 && buf[i + 1] <= 2 {
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
            if buf[i] == 2 && buf[i + 1] <= 2 {
                let width = u16::from_be_bytes([buf[i + 18], buf[i + 19]]);
                let height = u16::from_be_bytes([buf[i + 20], buf[i + 21]]);
                return Some((width, height));
            }
            i += 1;
        }
        None
    }

    fn first_touch_xy(buf: &[u8]) -> Option<(i32, i32)> {
        let mut i = 0usize;
        while i + 32 <= buf.len() {
            if buf[i] == 2 && buf[i + 1] <= 2 {
                let x = i32::from_be_bytes(buf[i + 10..i + 14].try_into().unwrap());
                let y = i32::from_be_bytes(buf[i + 14..i + 18].try_into().unwrap());
                return Some((x, y));
            }
            i += 1;
        }
        None
    }

    fn touch_events(buf: &[u8]) -> Vec<(u8, u64, i32, i32)> {
        let mut i = 0usize;
        let mut events = Vec::new();
        while i < buf.len() {
            let Some(len) = control_message_len_at(buf, i) else {
                return events;
            };
            if buf[i] == 2 {
                events.push((
                    buf[i + 1],
                    u64::from_be_bytes(buf[i + 2..i + 10].try_into().unwrap()),
                    i32::from_be_bytes(buf[i + 10..i + 14].try_into().unwrap()),
                    i32::from_be_bytes(buf[i + 14..i + 18].try_into().unwrap()),
                ));
            }
            i += len;
        }
        events
    }

    fn first_scroll_xy(buf: &[u8]) -> Option<(i32, i32)> {
        let mut i = 0usize;
        while i + 21 <= buf.len() {
            let len = control_message_len_at(buf, i)?;
            if buf[i] == 3 {
                let x = i32::from_be_bytes(buf[i + 1..i + 5].try_into().unwrap());
                let y = i32::from_be_bytes(buf[i + 5..i + 9].try_into().unwrap());
                return Some((x, y));
            }
            i += len;
        }
        None
    }

    fn mouse_input_payloads(buf: &[u8]) -> Vec<[u8; 5]> {
        let mut i = 0usize;
        let mut out = Vec::new();
        while i < buf.len() {
            let Some(len) = control_message_len_at(buf, i) else {
                return out;
            };
            if buf[i] == 13 && i + 10 <= buf.len() {
                let id = u16::from_be_bytes([buf[i + 1], buf[i + 2]]);
                let size = u16::from_be_bytes([buf[i + 3], buf[i + 4]]) as usize;
                if id == crate::types::HID_ID_MOUSE && size == 5 {
                    let mut payload = [0u8; 5];
                    payload.copy_from_slice(&buf[i + 5..i + 10]);
                    out.push(payload);
                }
            }
            i += len;
        }
        out
    }

    fn contains_touch_point(buf: &[u8], pointer_id: u64, x: i32, y: i32) -> bool {
        let mut i = 0usize;
        while i + 32 <= buf.len() {
            if buf[i] == 2 && buf[i + 1] <= 2 {
                let got_pointer = u64::from_be_bytes(buf[i + 2..i + 10].try_into().unwrap());
                let got_x = i32::from_be_bytes(buf[i + 10..i + 14].try_into().unwrap());
                let got_y = i32::from_be_bytes(buf[i + 14..i + 18].try_into().unwrap());
                if got_pointer == pointer_id && got_x == x && got_y == y {
                    return true;
                }
                i += 32;
            } else {
                i += 1;
            }
        }
        false
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

    fn control_message_len_at(buf: &[u8], i: usize) -> Option<usize> {
        if i >= buf.len() {
            return None;
        }
        let len = match buf[i] {
            0 => 14,
            1 => {
                if i + 5 > buf.len() {
                    return None;
                }
                let text_len = u32::from_be_bytes(buf[i + 1..i + 5].try_into().unwrap()) as usize;
                5 + text_len
            }
            2 => 32,
            3 => 21,
            4 => 2,
            5 | 6 | 7 | 11 | 15 | 17 | 19 | 20 => 1,
            18 => 2,
            8 => 2,
            9 => {
                if i + 14 > buf.len() {
                    return None;
                }
                let text_len = u32::from_be_bytes(buf[i + 10..i + 14].try_into().unwrap()) as usize;
                14 + text_len
            }
            10 => 2,
            12 => {
                if i + 8 > buf.len() {
                    return None;
                }
                let name_len = buf[i + 7] as usize;
                if i + 8 + name_len + 2 > buf.len() {
                    return None;
                }
                let rd_len_idx = i + 8 + name_len;
                let rd_len = u16::from_be_bytes([buf[rd_len_idx], buf[rd_len_idx + 1]]) as usize;
                8 + name_len + 2 + rd_len
            }
            13 => {
                if i + 5 > buf.len() {
                    return None;
                }
                let size = u16::from_be_bytes([buf[i + 3], buf[i + 4]]) as usize;
                5 + size
            }
            14 => 3,
            16 => {
                if i + 2 > buf.len() {
                    return None;
                }
                2 + buf[i + 1] as usize
            }
            21 => 5,
            22 => 6,
            23 => 9,
            24 => 1,
            _ => return None,
        };
        (i + len <= buf.len()).then_some(len)
    }

    fn find_control_message(buf: &[u8], tag: u8) -> Option<&[u8]> {
        let mut i = 0usize;
        while i < buf.len() {
            let len = control_message_len_at(buf, i)?;
            if buf[i] == tag {
                return Some(&buf[i..i + len]);
            }
            i += len;
        }
        None
    }

    fn count_control_messages(buf: &[u8], tag: u8) -> usize {
        let mut i = 0usize;
        let mut count = 0usize;
        while i < buf.len() {
            let Some(len) = control_message_len_at(buf, i) else {
                return count;
            };
            if buf[i] == tag {
                count += 1;
            }
            i += len;
        }
        count
    }

    fn control_message_tags(buf: &[u8]) -> Vec<u8> {
        let mut i = 0usize;
        let mut tags = Vec::new();
        while i < buf.len() {
            let Some(len) = control_message_len_at(buf, i) else {
                return tags;
            };
            tags.push(buf[i]);
            i += len;
        }
        tags
    }

    fn input_and_touch_tags(buf: &[u8]) -> Vec<u8> {
        control_message_tags(buf)
            .into_iter()
            .filter(|tag| matches!(*tag, 2 | 13))
            .collect()
    }

    fn contains_inject_keycode(buf: &[u8], keycode: u32) -> bool {
        let mut i = 0usize;
        while i < buf.len() {
            let Some(len) = control_message_len_at(buf, i) else {
                return false;
            };
            if buf[i] == 0 && i + 6 <= buf.len() {
                let got = u32::from_be_bytes(buf[i + 2..i + 6].try_into().unwrap());
                if got == keycode {
                    return true;
                }
            }
            i += len;
        }
        false
    }

    #[test]
    fn agent_session_reads_device_messages_and_dispatches_control() {
        let session =
            HidSession::open(MockTransport::new(), OpenRequest::gamepad_only_realtime()).unwrap();
        let mut agent = AgentControlSession::from_parts(session, Cursor::new(ack(42))).unwrap();

        assert_eq!(
            agent.recv_device_message().unwrap(),
            DeviceMessage::AckClipboard { sequence: 42 }
        );
        agent
            .client()
            .send_frame_unchecked(GamepadFrameRaw::new(1, 2, 3, 4, 5, 6, 7))
            .unwrap();

        let closed = agent.close().unwrap();
        assert_eq!(count_uhid_inputs(&closed.transport.bytes), 1);
        assert_eq!(closed.reader.position(), 9);
    }

    #[test]
    fn agent_session_reads_native_and_ai_device_events() {
        let session =
            HidSession::open(MockTransport::new(), OpenRequest::gamepad_only_realtime()).unwrap();
        let mut stream = Vec::new();
        stream.extend(ack(42));
        stream.extend(frame_summary_envelope(7));
        stream.extend(ai_stats_envelope());
        let mut agent = AgentControlSession::from_parts(session, Cursor::new(stream)).unwrap();

        assert_eq!(
            agent.recv_device_event().unwrap(),
            DeviceEvent::Native(DeviceMessage::AckClipboard { sequence: 42 })
        );
        match agent.recv_device_event().unwrap() {
            DeviceEvent::FrameSummary(summary) => {
                assert_eq!(summary.frame_seq, 7);
                assert_eq!(summary.objects[0].class_id, 7);
            }
            other => panic!("expected frame summary, got {other:?}"),
        }
        match agent.recv_device_event().unwrap() {
            DeviceEvent::AiStats(stats) => assert_eq!(stats.frames_sampled, 10),
            other => panic!("expected ai stats, got {other:?}"),
        }

        let _closed = agent.close().unwrap();
    }

    #[test]
    fn agent_wait_helpers_skip_unrelated_ai_and_native_events() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let mut stream = Vec::new();
        stream.extend(frame_summary_envelope(1));
        stream.extend(ack(9));
        stream.extend(clipboard("ok"));
        stream.extend(frame_summary_envelope(2));
        stream.extend(ai_stats_envelope());
        let mut agent = AgentControlSession::from_parts(session, Cursor::new(stream)).unwrap();

        agent.wait_for_clipboard_ack(9).unwrap();
        assert_eq!(agent.wait_for_clipboard().unwrap(), "ok");
        assert_eq!(agent.wait_for_frame_summary().unwrap().frame_seq, 2);
        assert_eq!(agent.wait_for_ai_stats().unwrap().frames_sampled, 10);

        let _closed = agent.close().unwrap();
    }

    #[test]
    fn agent_waits_for_frame_predicates_scene_motion_and_stability() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let motion = [crate::ai::MotionVector {
            x: 500,
            y: 900,
            dx: 3,
            dy: -2,
        }];
        let mut stream = Vec::new();
        stream.extend(frame_summary_envelope_full(1, 0, &[], &[], &[]));
        stream.extend(frame_summary_envelope_full(
            2,
            crate::ai::FLAG_SCENE_CHANGE,
            &[],
            &[],
            &[],
        ));
        stream.extend(frame_summary_envelope_full(3, 0, &[], &[], &[]));
        stream.extend(frame_summary_envelope_full(
            4,
            crate::ai::FLAG_MOTION,
            &motion,
            &[],
            &[],
        ));
        stream.extend(frame_summary_envelope_full(
            5,
            crate::ai::FLAG_MOTION,
            &motion,
            &[],
            &[],
        ));
        stream.extend(frame_summary_envelope_full(6, 0, &[], &[], &[]));
        stream.extend(frame_summary_envelope_full(7, 0, &[], &[], &[]));
        stream.extend(frame_summary_envelope_full(8, 0, &[], &[], &[]));
        let mut agent = AgentControlSession::from_parts(session, Cursor::new(stream)).unwrap();

        assert_eq!(agent.wait_for_scene_change().unwrap().frame_seq, 2);
        assert_eq!(
            agent
                .wait_for_frame_summary_matching(|summary| summary.frame_seq >= 4)
                .unwrap()
                .frame_seq,
            4
        );
        assert_eq!(agent.wait_for_motion().unwrap().frame_seq, 5);
        assert_eq!(agent.wait_for_stable_frames(2).unwrap().frame_seq, 7);
        assert_eq!(agent.wait_for_stable_frame().unwrap().frame_seq, 8);
        assert!(matches!(
            agent.wait_for_stable_frames(0),
            Err(Error::SessionLifecycle(
                "stable frame count must be nonzero"
            ))
        ));

        let _closed = agent.close().unwrap();
    }

    #[test]
    fn agent_frame_wait_limits_bound_observed_summaries() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let motion = [crate::ai::MotionVector {
            x: 500,
            y: 900,
            dx: 3,
            dy: -2,
        }];
        let mut stream = Vec::new();
        stream.extend(ack(3));
        stream.extend(frame_summary_envelope_full(1, 0, &[], &[], &[]));
        stream.extend(frame_summary_envelope_full(
            2,
            crate::ai::FLAG_SCENE_CHANGE,
            &[],
            &[],
            &[],
        ));
        stream.extend(frame_summary_envelope_full(
            3,
            crate::ai::FLAG_MOTION,
            &motion,
            &[],
            &[],
        ));
        stream.extend(frame_summary_envelope_full(4, 0, &[], &[], &[]));
        stream.extend(frame_summary_envelope_full(5, 0, &[], &[], &[]));
        stream.extend(frame_summary_envelope_full(6, 0, &[], &[], &[]));
        let mut agent = AgentControlSession::from_parts(session, Cursor::new(stream)).unwrap();

        assert!(agent.wait_for_scene_change_with_limit(0).unwrap().is_none());
        assert_eq!(agent.receiver_mut().unwrap().get_ref().position(), 0);
        assert!(agent.wait_for_scene_change_with_limit(1).unwrap().is_none());
        assert_eq!(
            agent
                .wait_for_scene_change_with_limit(1)
                .unwrap()
                .unwrap()
                .frame_seq,
            2
        );
        assert_eq!(
            agent
                .wait_for_frame_summary_matching_with_limit(1, FrameSummary::is_moving)
                .unwrap()
                .unwrap()
                .frame_seq,
            3
        );
        assert!(agent
            .wait_for_stable_frames_with_limit(2, 1)
            .unwrap()
            .is_none());
        assert_eq!(
            agent
                .wait_for_stable_frames_with_limit(2, 2)
                .unwrap()
                .unwrap()
                .frame_seq,
            6
        );

        let _closed = agent.close().unwrap();
    }

    #[test]
    fn agent_fresh_frame_waits_skip_stale_seq_and_timestamp() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let mut stream = Vec::new();
        stream.extend(frame_summary_envelope_full_at(100, 1, 0, &[], &[], &[]));
        stream.extend(frame_summary_envelope_full_at(120, 2, 0, &[], &[], &[]));
        stream.extend(frame_summary_envelope_full_at(200, 3, 0, &[], &[], &[]));
        stream.extend(frame_summary_envelope_full_at(250, 4, 0, &[], &[], &[]));
        let mut agent = AgentControlSession::from_parts(session, Cursor::new(stream)).unwrap();

        assert!(agent
            .wait_for_frame_summary_after_seq_with_limit(2, 2)
            .unwrap()
            .is_none());
        assert_eq!(
            agent
                .wait_for_frame_summary_after_seq_with_limit(2, 1)
                .unwrap()
                .unwrap()
                .frame_seq,
            3
        );
        let summary = agent.wait_for_frame_summary_after_timestamp(200).unwrap();
        assert_eq!(summary.frame_seq, 4);
        assert_eq!(summary.timestamp_ms, 250);

        let _closed = agent.close().unwrap();
    }

    #[test]
    fn agent_run_actions_and_wait_for_stable_frames_flushes_then_reads() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let motion = [crate::ai::MotionVector {
            x: 10,
            y: 20,
            dx: 1,
            dy: -1,
        }];
        let mut stream = Vec::new();
        stream.extend(frame_summary_envelope_full(
            1,
            crate::ai::FLAG_MOTION,
            &motion,
            &[],
            &[],
        ));
        stream.extend(frame_summary_envelope_full(2, 0, &[], &[], &[]));
        stream.extend(frame_summary_envelope_full(3, 0, &[], &[], &[]));
        let mut agent = AgentControlSession::from_parts(session, Cursor::new(stream)).unwrap();

        let summary = agent
            .run_actions_and_wait_for_stable_frames(&[AgentAction::tap(10, 20)], 2)
            .unwrap();

        assert_eq!(summary.frame_seq, 3);
        let closed = agent.close().unwrap();
        assert_eq!(count_touch_events(&closed.transport.bytes), 2);
    }

    #[test]
    fn agent_run_actions_and_wait_for_fresh_frame_flushes_then_reads() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let mut stream = Vec::new();
        stream.extend(frame_summary_envelope_full_at(100, 9, 0, &[], &[], &[]));
        stream.extend(frame_summary_envelope_full_at(120, 11, 0, &[], &[], &[]));
        let mut agent = AgentControlSession::from_parts(session, Cursor::new(stream)).unwrap();

        let summary = agent
            .run_actions_and_wait_for_frame_summary_after_seq(&[AgentAction::tap(10, 20)], 10)
            .unwrap();

        assert_eq!(summary.frame_seq, 11);
        let closed = agent.close().unwrap();
        assert_eq!(count_touch_events(&closed.transport.bytes), 2);
    }

    #[test]
    fn agent_detaches_latest_frame_receiver_and_keeps_command_path() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let mut stream = Vec::new();
        stream.extend(ack(7));
        stream.extend(frame_summary_envelope_full_at(100, 1, 0, &[], &[], &[]));
        stream.extend(ai_stats_envelope());
        stream.extend(frame_summary_envelope_full_at(160, 2, 0, &[], &[], &[]));
        let stream_len = stream.len() as u64;
        let mut agent = AgentControlSession::from_parts(session, Cursor::new(stream)).unwrap();

        let (latest, pump) = agent.detach_latest_frame_summary_receiver().unwrap();
        agent.run_actions(&[AgentAction::tap(10, 20)]).unwrap();

        let reader = pump.join().unwrap();
        assert_eq!(reader.position(), stream_len);
        let snapshot = latest.snapshot().unwrap();
        assert_eq!(snapshot.version, 2);
        assert_eq!(snapshot.summary.frame_seq, 2);
        assert_eq!(snapshot.summary.timestamp_ms, 160);
        assert!(matches!(
            agent.wait_for_frame_summary().unwrap_err(),
            Error::Transport(_)
        ));

        let report = agent.close_transport_checked().unwrap();
        report.command_result.unwrap();
        assert_eq!(count_touch_events(&report.transport.bytes), 2);
    }

    #[test]
    fn agent_run_actions_and_wait_for_next_latest_frame_uses_post_barrier_version() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (go_tx, go_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut sock, _addr) = listener.accept().unwrap();
            Write::write_all(
                &mut sock,
                &frame_summary_envelope_full_at(100, 1, 0, &[], &[], &[]),
            )
            .unwrap();
            go_rx.recv().unwrap();
            std::thread::sleep(Duration::from_millis(80));
            Write::write_all(
                &mut sock,
                &frame_summary_envelope_full_at(180, 2, 0, &[], &[], &[]),
            )
            .unwrap();
        });

        let reader = TcpStream::connect(addr).unwrap();
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let mut agent = AgentControlSession::from_parts(session, reader).unwrap();
        let (latest, pump) = agent.detach_latest_frame_summary_receiver().unwrap();
        assert_eq!(latest.wait_first().unwrap().summary.frame_seq, 1);

        go_tx.send(()).unwrap();
        let snapshot = agent
            .run_actions_and_wait_for_next_latest_frame_after_seq(
                &[AgentAction::tap(10, 20)],
                &latest,
                1,
            )
            .unwrap();

        assert_eq!(snapshot.summary.frame_seq, 2);
        assert_eq!(snapshot.summary.timestamp_ms, 180);
        let report = agent.close_transport_checked().unwrap();
        report.command_result.unwrap();
        assert_eq!(count_touch_events(&report.transport.bytes), 2);
        pump.join().unwrap();
        server.join().unwrap();
    }

    #[test]
    fn agent_run_actions_and_wait_for_next_latest_frame_timeout_bounds_wait() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (go_tx, go_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut sock, _addr) = listener.accept().unwrap();
            Write::write_all(
                &mut sock,
                &frame_summary_envelope_full_at(100, 1, 0, &[], &[], &[]),
            )
            .unwrap();
            go_rx.recv().unwrap();
            std::thread::sleep(Duration::from_millis(80));
            Write::write_all(
                &mut sock,
                &frame_summary_envelope_full_at(180, 2, 0, &[], &[], &[]),
            )
            .unwrap();
        });

        let reader = TcpStream::connect(addr).unwrap();
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let mut agent = AgentControlSession::from_parts(session, reader).unwrap();
        let (latest, pump) = agent.detach_latest_frame_summary_receiver().unwrap();
        assert_eq!(latest.wait_first().unwrap().summary.frame_seq, 1);

        go_tx.send(()).unwrap();
        assert!(matches!(
            agent
                .run_actions_and_wait_for_next_latest_frame_after_seq_timeout(
                    &[AgentAction::tap(10, 20)],
                    &latest,
                    1,
                    Duration::from_millis(5),
                )
                .unwrap_err(),
            Error::AgentTimeout("latest frame summary")
        ));

        let report = agent.close_transport_checked().unwrap();
        report.command_result.unwrap();
        assert_eq!(count_touch_events(&report.transport.bytes), 2);
        pump.join().unwrap();
        server.join().unwrap();
    }

    #[test]
    fn agent_try_run_actions_and_wait_for_next_latest_frame_after_version_uses_cached_boundary() {
        let mut stream = Vec::new();
        stream.extend(frame_summary_envelope_full_at(100, 1, 0, &[], &[], &[]));
        stream.extend(frame_summary_envelope_full_at(180, 2, 0, &[], &[], &[]));
        let (latest, pump) = spawn_latest_frame_summary_receiver(Cursor::new(stream)).unwrap();
        pump.join().unwrap();
        assert_eq!(latest.version(), 2);
        let prior_observation = LatestFrameSummaryObservation::at_version(1);

        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
        let snapshot = agent
            .try_run_actions_and_wait_for_next_latest_frame_after_observation(
                &[AgentAction::tap(10, 20)],
                &latest,
                &prior_observation,
            )
            .unwrap();

        assert_eq!(snapshot.version, 2);
        assert_eq!(snapshot.summary.frame_seq, 2);
        assert_eq!(snapshot.summary.timestamp_ms, 180);
        let closed = agent.close().unwrap();
        assert_eq!(count_touch_events(&closed.transport.bytes), 2);

        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
        let snapshot = agent
            .try_run_actions_and_wait_for_next_latest_frame_matching_after_observation_timeout(
                &[AgentAction::tap(12, 24)],
                &latest,
                &prior_observation,
                Duration::from_secs(1),
                |summary| summary.frame_seq == 2,
            )
            .unwrap();
        assert_eq!(snapshot.version, 2);
        assert_eq!(snapshot.summary.frame_seq, 2);
        let closed = agent.close().unwrap();
        assert_eq!(count_touch_events(&closed.transport.bytes), 2);
    }

    #[test]
    fn agent_try_run_actions_and_wait_for_next_latest_frame_timeout_bounds_wait() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
        let latest = LatestFrameSummaryReceiver::default();

        let err = agent
            .try_run_actions_and_wait_for_next_latest_frame_after_seq_timeout(
                &[AgentAction::tap(10, 20)],
                &latest,
                0,
                Duration::from_millis(1),
            )
            .unwrap_err();

        assert!(matches!(err, Error::AgentTimeout("latest frame summary")));
        let closed = agent.close().unwrap();
        assert_eq!(count_touch_events(&closed.transport.bytes), 2);
    }

    #[test]
    fn agent_try_run_actions_and_wait_for_next_latest_frame_preflights_without_dispatch() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts_with_bound(session, Cursor::new(Vec::new()), 1)
            .unwrap();
        let latest = LatestFrameSummaryReceiver::default();

        let err = agent
            .try_run_actions_and_wait_for_next_latest_frame_timeout(
                &[AgentAction::tap(10, 20)],
                &latest,
                Duration::from_millis(1),
            )
            .unwrap_err();

        assert!(matches!(
            err,
            Error::SessionLifecycle(TRY_RUN_EXCEEDS_COMMAND_BOUND)
        ));
        let closed = agent.close().unwrap();
        assert!(closed.transport.bytes.is_empty());
    }

    #[test]
    fn agent_try_run_actions_and_wait_for_next_latest_target_after_observation_selects_cached_target(
    ) {
        let mut stream = Vec::new();
        stream.extend(frame_summary_envelope_full_at(
            100,
            1,
            crate::ai::FLAG_OBJECTS,
            &[],
            &[ObjectBox {
                x: 100,
                y: 200,
                w: 301,
                h: 101,
                class_id: 7,
                confidence: 230,
            }],
            &[],
        ));
        let (latest, pump) = spawn_latest_frame_summary_receiver(Cursor::new(stream)).unwrap();
        pump.join().unwrap();
        let prior_observation = LatestFrameSummaryObservation::at_version(0);

        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
        let rect = agent
            .try_run_actions_and_wait_for_next_latest_target_rect_after_observation(
                &[AgentAction::tap(10, 20)],
                &latest,
                AgentTargetSelector::object_class_min_confidence(7, 220),
                &prior_observation,
            )
            .unwrap();

        assert_eq!(rect.to_pixels(1000, 2000), (100, 200, 400, 300));
        let closed = agent.close().unwrap();
        assert_eq!(count_touch_events(&closed.transport.bytes), 2);
    }

    #[test]
    fn agent_try_run_actions_and_tap_next_latest_target_at_pointer_timeout_taps_target() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (go_tx, go_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut sock, _addr) = listener.accept().unwrap();
            go_rx.recv().unwrap();
            std::thread::sleep(Duration::from_millis(80));
            Write::write_all(
                &mut sock,
                &frame_summary_envelope_full_at(
                    100,
                    1,
                    crate::ai::FLAG_OBJECTS,
                    &[],
                    &[ObjectBox {
                        x: 100,
                        y: 200,
                        w: 301,
                        h: 101,
                        class_id: 7,
                        confidence: 230,
                    }],
                    &[],
                ),
            )
            .unwrap();
        });

        let reader = TcpStream::connect(addr).unwrap();
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let mut agent = AgentControlSession::from_parts(session, reader).unwrap();
        agent.set_screen_size(1000, 2000).unwrap();
        let (latest, pump) = agent.detach_latest_frame_summary_receiver().unwrap();
        let pointer = TouchPointerId::VIRTUAL_FINGER;

        go_tx.send(()).unwrap();
        let rect = agent
            .try_run_actions_and_tap_next_latest_target_at_pointer_timeout(
                &[AgentAction::tap(10, 20)],
                &latest,
                AgentTargetSelector::object_class_min_confidence(7, 220),
                pointer,
                (2_500, 7_500),
                Duration::from_secs(1),
            )
            .unwrap();

        assert_eq!(rect.to_pixels(1000, 2000), (100, 200, 400, 300));
        let report = agent.close_transport_checked().unwrap();
        report.command_result.unwrap();
        let events = touch_events(&report.transport.bytes);
        assert_eq!(events.len(), 4);
        assert_eq!(events[0], (TouchAction::DOWN.value(), 0, 10, 20));
        assert_eq!(events[1], (TouchAction::UP.value(), 0, 10, 20));
        assert_eq!(
            events[2],
            (TouchAction::DOWN.value(), pointer.value(), 175, 275)
        );
        assert_eq!(
            events[3],
            (TouchAction::UP.value(), pointer.value(), 175, 275)
        );
        pump.join().unwrap();
        server.join().unwrap();
    }

    #[test]
    fn agent_try_run_actions_and_wait_for_next_latest_target_preflights_without_dispatch() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts_with_bound(session, Cursor::new(Vec::new()), 1)
            .unwrap();
        let latest = LatestFrameSummaryReceiver::default();

        let err = agent
            .try_run_actions_and_wait_for_next_latest_target_rect_timeout(
                &[AgentAction::tap(10, 20)],
                &latest,
                AgentTargetSelector::best_object(),
                Duration::from_millis(1),
            )
            .unwrap_err();

        assert!(matches!(
            err,
            Error::SessionLifecycle(TRY_RUN_EXCEEDS_COMMAND_BOUND)
        ));
        let closed = agent.close().unwrap();
        assert!(closed.transport.bytes.is_empty());
    }

    #[test]
    fn agent_run_actions_and_wait_for_next_latest_frame_after_version_uses_cached_boundary() {
        let mut stream = Vec::new();
        stream.extend(frame_summary_envelope_full_at(100, 1, 0, &[], &[], &[]));
        stream.extend(frame_summary_envelope_full_at(180, 2, 0, &[], &[], &[]));
        let (latest, pump) = spawn_latest_frame_summary_receiver(Cursor::new(stream)).unwrap();
        pump.join().unwrap();
        assert_eq!(latest.version(), 2);
        let observation = latest.observe();
        assert!(observation.has_snapshot());
        assert_eq!(observation.boundary_version(), 2);
        assert_eq!(observation.summary().unwrap().frame_seq, 2);
        let prior_observation = LatestFrameSummaryObservation::at_version(1);
        assert!(prior_observation.accepts(observation.snapshot().unwrap()));

        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
        let snapshot = agent
            .run_actions_and_wait_for_next_latest_frame_after_version(
                &[AgentAction::tap(10, 20)],
                &latest,
                1,
            )
            .unwrap();

        assert_eq!(snapshot.version, 2);
        assert_eq!(snapshot.summary.frame_seq, 2);
        assert_eq!(snapshot.summary.timestamp_ms, 180);
        let closed = agent.close().unwrap();
        assert_eq!(count_touch_events(&closed.transport.bytes), 2);

        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
        let snapshot = agent
            .run_actions_and_wait_for_next_latest_frame_matching_after_observation_timeout(
                &[AgentAction::tap(12, 24)],
                &latest,
                &prior_observation,
                Duration::from_secs(1),
                |summary| summary.frame_seq == 2,
            )
            .unwrap();
        assert_eq!(snapshot.version, 2);
        assert_eq!(snapshot.summary.frame_seq, 2);
        let closed = agent.close().unwrap();
        assert_eq!(count_touch_events(&closed.transport.bytes), 2);

        let mut stream = Vec::new();
        stream.extend(frame_summary_envelope_full_at(100, 1, 0, &[], &[], &[]));
        stream.extend(frame_summary_envelope_full_at(
            180,
            2,
            crate::ai::FLAG_OBJECTS,
            &[],
            &[ObjectBox {
                x: 100,
                y: 200,
                w: 301,
                h: 101,
                class_id: 7,
                confidence: 230,
            }],
            &[],
        ));
        let (latest, pump) = spawn_latest_frame_summary_receiver(Cursor::new(stream)).unwrap();
        pump.join().unwrap();

        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
        let rect = agent
            .run_actions_and_wait_for_next_latest_target_rect_after_observation(
                &[AgentAction::tap(10, 20)],
                &latest,
                AgentTargetSelector::object_class_min_confidence(7, 220),
                &prior_observation,
            )
            .unwrap();
        assert_eq!(rect.to_pixels(1000, 2000), (100, 200, 400, 300));
        let closed = agent.close().unwrap();
        assert_eq!(count_touch_events(&closed.transport.bytes), 2);

        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
        let rect = agent
            .run_actions_and_tap_next_latest_target_after_observation_timeout(
                &[AgentAction::tap(30, 40)],
                &latest,
                AgentTargetSelector::object_class_min_confidence(7, 220),
                &prior_observation,
                Duration::from_secs(1),
            )
            .unwrap();
        assert_eq!(rect.to_pixels(1000, 2000), (100, 200, 400, 300));
        let closed = agent.close().unwrap();
        let events = touch_events(&closed.transport.bytes);
        assert_eq!(events.len(), 4);
        assert_eq!(events[0], (TouchAction::DOWN.value(), 0, 30, 40));
        assert_eq!(events[1], (TouchAction::UP.value(), 0, 30, 40));
        assert_eq!(events[2], (TouchAction::DOWN.value(), 0, 270, 240));
        assert_eq!(events[3], (TouchAction::UP.value(), 0, 270, 240));

        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
        let empty_latest = LatestFrameSummaryReceiver::default();
        let err = agent
            .run_actions_and_wait_for_next_latest_frame_after_version_timeout(
                &[AgentAction::tap(30, 40)],
                &empty_latest,
                0,
                Duration::from_millis(1),
            )
            .unwrap_err();
        assert!(matches!(err, Error::AgentTimeout("latest frame summary")));
        let closed = agent.close().unwrap();
        assert_eq!(count_touch_events(&closed.transport.bytes), 2);
    }

    #[test]
    fn agent_run_actions_and_tap_next_latest_targets_waits_then_taps() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (go_object_tx, go_object_rx) = mpsc::channel();
        let (go_text_tx, go_text_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut sock, _addr) = listener.accept().unwrap();
            Write::write_all(
                &mut sock,
                &frame_summary_envelope_full_at(100, 1, 0, &[], &[], &[]),
            )
            .unwrap();
            go_object_rx.recv().unwrap();
            std::thread::sleep(Duration::from_millis(80));
            Write::write_all(
                &mut sock,
                &frame_summary_envelope_full_at(
                    180,
                    2,
                    crate::ai::FLAG_OBJECTS,
                    &[],
                    &[
                        ObjectBox {
                            x: 10,
                            y: 20,
                            w: 11,
                            h: 21,
                            class_id: 3,
                            confidence: 210,
                        },
                        ObjectBox {
                            x: 100,
                            y: 200,
                            w: 301,
                            h: 101,
                            class_id: 7,
                            confidence: 230,
                        },
                    ],
                    &[],
                ),
            )
            .unwrap();
            go_text_rx.recv().unwrap();
            std::thread::sleep(Duration::from_millis(80));
            Write::write_all(
                &mut sock,
                &frame_summary_envelope_full_at(
                    260,
                    3,
                    crate::ai::FLAG_TEXT,
                    &[],
                    &[],
                    &[
                        TextRegion {
                            x: 100,
                            y: 200,
                            w: 11,
                            h: 11,
                        },
                        TextRegion {
                            x: 700,
                            y: 800,
                            w: 101,
                            h: 101,
                        },
                    ],
                ),
            )
            .unwrap();
        });

        let reader = TcpStream::connect(addr).unwrap();
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let mut agent = AgentControlSession::from_parts(session, reader).unwrap();
        agent.set_screen_size(1000, 2000).unwrap();
        let (latest, pump) = agent.detach_latest_frame_summary_receiver().unwrap();
        assert_eq!(latest.wait_first().unwrap().summary.frame_seq, 1);

        let pointer = TouchPointerId::VIRTUAL_FINGER;
        go_object_tx.send(()).unwrap();
        let object = agent
            .run_actions_and_tap_next_latest_target_at_pointer_timeout(
                &[AgentAction::tap(10, 20)],
                &latest,
                AgentTargetSelector::object_class_min_confidence(7, 220),
                pointer,
                (2_500, 7_500),
                Duration::from_secs(1),
            )
            .unwrap();
        go_text_tx.send(()).unwrap();
        let text = agent
            .run_actions_and_tap_next_latest_target_timeout(
                &[AgentAction::tap(30, 40)],
                &latest,
                AgentTargetSelector::largest_text_region(),
                Duration::from_secs(1),
            )
            .unwrap();

        assert_eq!(object.to_pixels(1000, 2000), (100, 200, 400, 300));
        assert_eq!(text.to_pixels(1000, 2000), (700, 800, 800, 900));
        let report = agent.close_transport_checked().unwrap();
        report.command_result.unwrap();
        let events = touch_events(&report.transport.bytes);
        assert_eq!(events.len(), 8);
        assert_eq!(events[0], (TouchAction::DOWN.value(), 0, 10, 20));
        assert_eq!(events[1], (TouchAction::UP.value(), 0, 10, 20));
        assert_eq!(
            events[2],
            (TouchAction::DOWN.value(), pointer.value(), 175, 275)
        );
        assert_eq!(
            events[3],
            (TouchAction::UP.value(), pointer.value(), 175, 275)
        );
        assert_eq!(events[4], (TouchAction::DOWN.value(), 0, 30, 40));
        assert_eq!(events[5], (TouchAction::UP.value(), 0, 30, 40));
        assert_eq!(events[6], (TouchAction::DOWN.value(), 0, 750, 850));
        assert_eq!(events[7], (TouchAction::UP.value(), 0, 750, 850));
        pump.join().unwrap();
        server.join().unwrap();
    }

    #[test]
    fn agent_run_actions_and_wait_for_target_with_limit_flushes_then_reads() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let mut stream = Vec::new();
        stream.extend(frame_summary_envelope_with(
            1,
            &[ObjectBox {
                x: 10,
                y: 20,
                w: 11,
                h: 21,
                class_id: 1,
                confidence: 255,
            }],
            &[],
        ));
        stream.extend(frame_summary_envelope_with(
            2,
            &[ObjectBox {
                x: 100,
                y: 200,
                w: 301,
                h: 101,
                class_id: 6,
                confidence: 230,
            }],
            &[],
        ));
        let mut agent = AgentControlSession::from_parts(session, Cursor::new(stream)).unwrap();

        let missed = agent
            .run_actions_and_wait_for_object_selector_rect_with_limit(
                &[AgentAction::tap(10, 20)],
                AgentObjectSelector::class_min_confidence(6, 220),
                1,
            )
            .unwrap();
        assert!(missed.is_none());
        let rect = agent
            .wait_for_object_selector_rect_with_limit(
                AgentObjectSelector::class_min_confidence(6, 220),
                1,
            )
            .unwrap()
            .unwrap();

        assert_eq!(rect.to_pixels(1000, 2000), (100, 200, 400, 300));
        let closed = agent.close().unwrap();
        assert_eq!(count_touch_events(&closed.transport.bytes), 2);
    }

    #[test]
    fn agent_target_selector_ordered_wait_and_taps_cover_generic_api() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let mut stream = Vec::new();
        stream.extend(frame_summary_envelope_with(1, &[], &[]));
        stream.extend(frame_summary_envelope_with(
            2,
            &[ObjectBox {
                x: 100,
                y: 200,
                w: 301,
                h: 101,
                class_id: 9,
                confidence: 230,
            }],
            &[],
        ));
        stream.extend(frame_summary_envelope_with(
            3,
            &[],
            &[TextRegion {
                x: 700,
                y: 800,
                w: 101,
                h: 101,
            }],
        ));
        stream.extend(frame_summary_envelope_with(
            4,
            &[],
            &[
                TextRegion {
                    x: 10,
                    y: 20,
                    w: 11,
                    h: 11,
                },
                TextRegion {
                    x: 100,
                    y: 200,
                    w: 301,
                    h: 101,
                },
            ],
        ));
        stream.extend(frame_summary_envelope_with(
            5,
            &[ObjectBox {
                x: 300,
                y: 400,
                w: 201,
                h: 101,
                class_id: 3,
                confidence: 240,
            }],
            &[],
        ));
        let mut agent = AgentControlSession::from_parts(session, Cursor::new(stream)).unwrap();
        let pointer = TouchPointerId::VIRTUAL_FINGER;
        agent.set_screen_size(1000, 2000).unwrap();

        let missed = agent
            .run_actions_and_wait_for_target_rect_with_limit(
                &[AgentAction::tap(1, 2)],
                AgentTargetSelector::object_class_min_confidence(9, 220),
                1,
            )
            .unwrap();
        assert!(missed.is_none());
        let object = agent
            .wait_for_target_rect(AgentTargetSelector::object_class_min_confidence(9, 220))
            .unwrap();
        let text = agent
            .tap_next_target_at_pointer_with_limit(
                AgentTargetSelector::text_region(0),
                pointer,
                10_000,
                0,
                1,
            )
            .unwrap()
            .unwrap();
        let largest_text = agent
            .run_actions_and_tap_next_target_at_with_limit(
                &[AgentAction::tap(30, 40)],
                AgentTargetSelector::largest_text_region(),
                0,
                10_000,
                1,
            )
            .unwrap()
            .unwrap();
        let best = agent
            .run_actions_and_tap_next_target(
                &[AgentAction::tap(50, 60)],
                AgentTargetSelector::best_object(),
            )
            .unwrap();

        assert_eq!(object.to_pixels(1000, 2000), (100, 200, 400, 300));
        assert_eq!(text.to_pixels(1000, 2000), (700, 800, 800, 900));
        assert_eq!(largest_text.to_pixels(1000, 2000), (100, 200, 400, 300));
        assert_eq!(best.to_pixels(1000, 2000), (300, 400, 500, 500));

        let closed = agent.close().unwrap();
        let events = touch_events(&closed.transport.bytes);
        assert_eq!(events.len(), 12);
        assert_eq!(events[0], (TouchAction::DOWN.value(), 0, 1, 2));
        assert_eq!(events[1], (TouchAction::UP.value(), 0, 1, 2));
        assert_eq!(
            events[2],
            (TouchAction::DOWN.value(), pointer.value(), 800, 800)
        );
        assert_eq!(
            events[3],
            (TouchAction::UP.value(), pointer.value(), 800, 800)
        );
        assert_eq!(events[4], (TouchAction::DOWN.value(), 0, 30, 40));
        assert_eq!(events[5], (TouchAction::UP.value(), 0, 30, 40));
        assert_eq!(events[6], (TouchAction::DOWN.value(), 0, 100, 300));
        assert_eq!(events[7], (TouchAction::UP.value(), 0, 100, 300));
        assert_eq!(events[8], (TouchAction::DOWN.value(), 0, 50, 60));
        assert_eq!(events[9], (TouchAction::UP.value(), 0, 50, 60));
        assert_eq!(events[10], (TouchAction::DOWN.value(), 0, 400, 450));
        assert_eq!(events[11], (TouchAction::UP.value(), 0, 400, 450));
    }

    #[test]
    fn agent_bounded_target_taps_cover_object_best_class_and_text_families() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let mut stream = Vec::new();
        stream.extend(frame_summary_envelope_with(
            1,
            &[ObjectBox {
                x: 10,
                y: 20,
                w: 30,
                h: 40,
                class_id: 1,
                confidence: 255,
            }],
            &[],
        ));
        stream.extend(frame_summary_envelope_with(
            2,
            &[
                ObjectBox {
                    x: 10,
                    y: 20,
                    w: 30,
                    h: 40,
                    class_id: 1,
                    confidence: 255,
                },
                ObjectBox {
                    x: 100,
                    y: 200,
                    w: 301,
                    h: 101,
                    class_id: 5,
                    confidence: 220,
                },
            ],
            &[],
        ));
        stream.extend(frame_summary_envelope_with(
            3,
            &[
                ObjectBox {
                    x: 10,
                    y: 20,
                    w: 30,
                    h: 40,
                    class_id: 2,
                    confidence: 100,
                },
                ObjectBox {
                    x: 300,
                    y: 400,
                    w: 201,
                    h: 101,
                    class_id: 6,
                    confidence: 250,
                },
            ],
            &[],
        ));
        stream.extend(frame_summary_envelope_with(
            4,
            &[
                ObjectBox {
                    x: 1,
                    y: 1,
                    w: 10,
                    h: 10,
                    class_id: 1,
                    confidence: 255,
                },
                ObjectBox {
                    x: 50,
                    y: 60,
                    w: 101,
                    h: 201,
                    class_id: 7,
                    confidence: 220,
                },
            ],
            &[],
        ));
        stream.extend(frame_summary_envelope_with(
            5,
            &[],
            &[
                TextRegion {
                    x: 10,
                    y: 20,
                    w: 30,
                    h: 40,
                },
                TextRegion {
                    x: 500,
                    y: 600,
                    w: 101,
                    h: 201,
                },
            ],
        ));
        let mut agent = AgentControlSession::from_parts(session, Cursor::new(stream)).unwrap();
        let pointer = TouchPointerId::finger(42);

        agent.set_screen_size(1000, 2000).unwrap();
        assert!(agent
            .tap_next_object_at_pointer_with_limit(1, pointer, 2_500, 7_500, 1)
            .unwrap()
            .is_none());
        let indexed = agent
            .tap_next_object_at_pointer_with_limit(1, pointer, 2_500, 7_500, 1)
            .unwrap()
            .unwrap();
        let best = agent.tap_next_best_object_with_limit(1).unwrap().unwrap();
        let class = agent
            .tap_next_object_class_pointer_with_limit(7, pointer, 1)
            .unwrap()
            .unwrap();
        let text = agent
            .tap_next_text_region_at_pointer_with_limit(1, pointer, 10_000, 0, 1)
            .unwrap()
            .unwrap();

        assert_eq!(indexed.to_pixels(1000, 2000), (100, 200, 400, 300));
        assert_eq!(best.to_pixels(1000, 2000), (300, 400, 500, 500));
        assert_eq!(class.to_pixels(1000, 2000), (50, 60, 150, 260));
        assert_eq!(text.to_pixels(1000, 2000), (500, 600, 600, 800));

        let closed = agent.close().unwrap();
        let events = touch_events(&closed.transport.bytes);
        assert_eq!(events.len(), 8);
        assert_eq!(
            events[0],
            (TouchAction::DOWN.value(), pointer.value(), 175, 275)
        );
        assert_eq!(
            events[1],
            (TouchAction::UP.value(), pointer.value(), 175, 275)
        );
        assert_eq!(events[2], (TouchAction::DOWN.value(), 0, 400, 450));
        assert_eq!(events[3], (TouchAction::UP.value(), 0, 400, 450));
        assert_eq!(
            events[4],
            (TouchAction::DOWN.value(), pointer.value(), 100, 160)
        );
        assert_eq!(
            events[5],
            (TouchAction::UP.value(), pointer.value(), 100, 160)
        );
        assert_eq!(
            events[6],
            (TouchAction::DOWN.value(), pointer.value(), 600, 600)
        );
        assert_eq!(
            events[7],
            (TouchAction::UP.value(), pointer.value(), 600, 600)
        );
    }

    #[test]
    fn agent_run_actions_and_tap_next_text_region_with_limit_flushes_then_taps_target() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let mut stream = Vec::new();
        stream.extend(frame_summary_envelope_with(1, &[], &[]));
        stream.extend(frame_summary_envelope_with(
            2,
            &[],
            &[TextRegion {
                x: 100,
                y: 200,
                w: 301,
                h: 101,
            }],
        ));
        let mut agent = AgentControlSession::from_parts(session, Cursor::new(stream)).unwrap();
        let pointer = TouchPointerId::finger(9);

        agent.set_screen_size(1000, 2000).unwrap();
        let rect = agent
            .run_actions_and_tap_next_text_region_at_pointer_with_limit(
                &[AgentAction::tap(10, 20)],
                0,
                pointer,
                10_000,
                0,
                2,
            )
            .unwrap()
            .unwrap();

        assert_eq!(rect.to_pixels(1000, 2000), (100, 200, 400, 300));
        let closed = agent.close().unwrap();
        let events = touch_events(&closed.transport.bytes);
        assert_eq!(events.len(), 4);
        assert_eq!(events[0], (TouchAction::DOWN.value(), 0, 10, 20));
        assert_eq!(events[1], (TouchAction::UP.value(), 0, 10, 20));
        assert_eq!(
            events[2],
            (TouchAction::DOWN.value(), pointer.value(), 400, 200)
        );
        assert_eq!(
            events[3],
            (TouchAction::UP.value(), pointer.value(), 400, 200)
        );
    }

    #[test]
    fn agent_tap_next_object_selector_with_limit_skips_tap_on_miss() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let mut stream = Vec::new();
        stream.extend(frame_summary_envelope_with(
            1,
            &[ObjectBox {
                x: 10,
                y: 20,
                w: 11,
                h: 21,
                class_id: 1,
                confidence: 255,
            }],
            &[],
        ));
        stream.extend(frame_summary_envelope_with(
            2,
            &[ObjectBox {
                x: 100,
                y: 200,
                w: 301,
                h: 101,
                class_id: 6,
                confidence: 230,
            }],
            &[],
        ));
        let mut agent = AgentControlSession::from_parts(session, Cursor::new(stream)).unwrap();

        agent.set_screen_size(1000, 2000).unwrap();
        let missed = agent
            .tap_next_object_selector_at_pointer_with_limit(
                AgentObjectSelector::class_min_confidence(6, 220),
                TouchPointerId::VIRTUAL_FINGER,
                2_500,
                7_500,
                1,
            )
            .unwrap();
        assert!(missed.is_none());
        let rect = agent
            .tap_next_object_selector_at_pointer_with_limit(
                AgentObjectSelector::class_min_confidence(6, 220),
                TouchPointerId::VIRTUAL_FINGER,
                2_500,
                7_500,
                1,
            )
            .unwrap()
            .unwrap();

        assert_eq!(rect.to_pixels(1000, 2000), (100, 200, 400, 300));
        let closed = agent.close().unwrap();
        let events = touch_events(&closed.transport.bytes);
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[0],
            (
                TouchAction::DOWN.value(),
                TouchPointerId::VIRTUAL_FINGER.value(),
                175,
                275
            )
        );
        assert_eq!(
            events[1],
            (
                TouchAction::UP.value(),
                TouchPointerId::VIRTUAL_FINGER.value(),
                175,
                275
            )
        );
    }

    #[test]
    fn agent_run_actions_and_tap_next_largest_text_region_with_limit_taps_on_hit() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let mut stream = Vec::new();
        stream.extend(frame_summary_envelope_with(1, &[], &[]));
        stream.extend(frame_summary_envelope_with(
            2,
            &[],
            &[TextRegion {
                x: 100,
                y: 200,
                w: 301,
                h: 101,
            }],
        ));
        let mut agent = AgentControlSession::from_parts(session, Cursor::new(stream)).unwrap();

        agent.set_screen_size(1000, 2000).unwrap();
        let rect = agent
            .run_actions_and_tap_next_largest_text_region_at_with_limit(
                &[AgentAction::tap(10, 20)],
                0,
                10_000,
                2,
            )
            .unwrap()
            .unwrap();

        assert_eq!(rect.to_pixels(1000, 2000), (100, 200, 400, 300));
        let closed = agent.close().unwrap();
        let events = touch_events(&closed.transport.bytes);
        assert_eq!(events.len(), 4);
        assert_eq!(events[0], (TouchAction::DOWN.value(), 0, 10, 20));
        assert_eq!(events[1], (TouchAction::UP.value(), 0, 10, 20));
        assert_eq!(events[2], (TouchAction::DOWN.value(), 0, 100, 300));
        assert_eq!(events[3], (TouchAction::UP.value(), 0, 100, 300));
    }

    #[test]
    fn agent_run_actions_and_wait_for_object_selector_rect_flushes_then_reads() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let mut stream = Vec::new();
        stream.extend(frame_summary_envelope_with(1, &[], &[]));
        stream.extend(frame_summary_envelope_with(
            2,
            &[ObjectBox {
                x: 120,
                y: 240,
                w: 101,
                h: 201,
                class_id: 8,
                confidence: 230,
            }],
            &[],
        ));
        let mut agent = AgentControlSession::from_parts(session, Cursor::new(stream)).unwrap();

        let rect = agent
            .run_actions_and_wait_for_object_selector_rect(
                &[AgentAction::tap(10, 20)],
                AgentObjectSelector::class_min_confidence(8, 220),
            )
            .unwrap();

        assert_eq!(rect.to_pixels(1000, 2000), (120, 240, 220, 440));
        assert_eq!(rect.center().to_pixels(1000, 2000), (170, 340));
        let closed = agent.close().unwrap();
        assert_eq!(count_touch_events(&closed.transport.bytes), 2);
    }

    #[test]
    fn agent_waits_for_next_vision_targets_across_frames() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let mut stream = Vec::new();
        stream.extend(frame_summary_envelope_with(1, &[], &[]));
        stream.extend(frame_summary_envelope_with(
            2,
            &[
                ObjectBox {
                    x: 100,
                    y: 200,
                    w: 301,
                    h: 101,
                    class_id: 7,
                    confidence: 220,
                },
                ObjectBox {
                    x: 500,
                    y: 600,
                    w: 11,
                    h: 21,
                    class_id: 3,
                    confidence: 230,
                },
            ],
            &[],
        ));
        stream.extend(frame_summary_envelope_with(
            3,
            &[ObjectBox {
                x: 300,
                y: 400,
                w: 301,
                h: 101,
                class_id: 2,
                confidence: 210,
            }],
            &[],
        ));
        stream.extend(frame_summary_envelope_with(
            4,
            &[],
            &[
                TextRegion {
                    x: 10,
                    y: 20,
                    w: 11,
                    h: 21,
                },
                TextRegion {
                    x: 700,
                    y: 800,
                    w: 101,
                    h: 101,
                },
            ],
        ));
        let mut agent = AgentControlSession::from_parts(session, Cursor::new(stream)).unwrap();

        assert_eq!(
            agent
                .wait_for_best_object_rect()
                .unwrap()
                .center()
                .to_pixels(1000, 2000),
            (505, 610)
        );
        assert_eq!(
            agent
                .wait_for_best_object_class_rect(2)
                .unwrap()
                .center()
                .to_pixels(1000, 2000),
            (450, 450)
        );
        assert_eq!(
            agent
                .wait_for_largest_text_region_rect()
                .unwrap()
                .center()
                .to_pixels(1000, 2000),
            (750, 850)
        );

        let _closed = agent.close().unwrap();
    }

    #[test]
    fn agent_waits_for_object_selector_across_frames() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let mut stream = Vec::new();
        stream.extend(frame_summary_envelope_with(
            1,
            &[ObjectBox {
                x: 100,
                y: 200,
                w: 101,
                h: 101,
                class_id: 2,
                confidence: 180,
            }],
            &[],
        ));
        stream.extend(frame_summary_envelope_with(
            2,
            &[ObjectBox {
                x: 500,
                y: 600,
                w: 11,
                h: 21,
                class_id: 3,
                confidence: 255,
            }],
            &[],
        ));
        stream.extend(ack(17));
        stream.extend(frame_summary_envelope_with(
            3,
            &[ObjectBox {
                x: 300,
                y: 400,
                w: 301,
                h: 101,
                class_id: 2,
                confidence: 230,
            }],
            &[],
        ));
        let mut agent = AgentControlSession::from_parts(session, Cursor::new(stream)).unwrap();

        let rect = agent
            .wait_for_object_selector_rect(AgentObjectSelector::class_min_confidence(2, 220))
            .unwrap();
        assert_eq!(rect.center().to_pixels(1000, 2000), (450, 450));

        let _closed = agent.close().unwrap();
    }

    #[test]
    fn agent_tap_next_vision_targets_emit_touch_events() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let mut stream = Vec::new();
        stream.extend(frame_summary_envelope_with(
            1,
            &[ObjectBox {
                x: 100,
                y: 200,
                w: 301,
                h: 101,
                class_id: 7,
                confidence: 220,
            }],
            &[],
        ));
        stream.extend(frame_summary_envelope_with(
            2,
            &[],
            &[TextRegion {
                x: 700,
                y: 800,
                w: 101,
                h: 101,
            }],
        ));
        let mut agent = AgentControlSession::from_parts(session, Cursor::new(stream)).unwrap();

        agent.set_screen_size(1000, 2000).unwrap();
        let object = agent.tap_next_best_object().unwrap();
        let text = agent.tap_next_largest_text_region().unwrap();

        assert_eq!(object.center().to_pixels(1000, 2000), (250, 250));
        assert_eq!(text.center().to_pixels(1000, 2000), (750, 850));

        let closed = agent.close().unwrap();
        let events = touch_events(&closed.transport.bytes);
        assert_eq!(events.len(), 4);
        assert_eq!(events[0], (TouchAction::DOWN.value(), 0, 250, 250));
        assert_eq!(events[1], (TouchAction::UP.value(), 0, 250, 250));
        assert_eq!(events[2], (TouchAction::DOWN.value(), 0, 750, 850));
        assert_eq!(events[3], (TouchAction::UP.value(), 0, 750, 850));
    }

    #[test]
    fn agent_tap_next_object_selector_emits_touch() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let mut stream = Vec::new();
        stream.extend(frame_summary_envelope_with(
            1,
            &[ObjectBox {
                x: 100,
                y: 200,
                w: 101,
                h: 101,
                class_id: 4,
                confidence: 219,
            }],
            &[],
        ));
        stream.extend(frame_summary_envelope_with(
            2,
            &[ObjectBox {
                x: 700,
                y: 800,
                w: 101,
                h: 101,
                class_id: 4,
                confidence: 220,
            }],
            &[],
        ));
        let mut agent = AgentControlSession::from_parts(session, Cursor::new(stream)).unwrap();

        agent.set_screen_size(1000, 2000).unwrap();
        let rect = agent
            .tap_next_object_selector(AgentObjectSelector::class_min_confidence(4, 220))
            .unwrap();

        assert_eq!(rect.center().to_pixels(1000, 2000), (750, 850));
        let closed = agent.close().unwrap();
        let events = touch_events(&closed.transport.bytes);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0], (TouchAction::DOWN.value(), 0, 750, 850));
        assert_eq!(events[1], (TouchAction::UP.value(), 0, 750, 850));
    }

    #[test]
    fn agent_run_actions_and_tap_next_object_selector_at_pointer_flushes_then_taps_target() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let mut stream = Vec::new();
        stream.extend(frame_summary_envelope_with(1, &[], &[]));
        stream.extend(frame_summary_envelope_with(
            2,
            &[ObjectBox {
                x: 100,
                y: 200,
                w: 301,
                h: 101,
                class_id: 6,
                confidence: 230,
            }],
            &[],
        ));
        let pointer = TouchPointerId::VIRTUAL_FINGER;
        let mut agent = AgentControlSession::from_parts(session, Cursor::new(stream)).unwrap();

        agent.set_screen_size(1000, 2000).unwrap();
        let rect = agent
            .run_actions_and_tap_next_object_selector_at_pointer(
                &[AgentAction::tap(10, 20)],
                AgentObjectSelector::class_min_confidence(6, 220),
                pointer,
                2_500,
                7_500,
            )
            .unwrap();

        assert_eq!(rect.to_pixels(1000, 2000), (100, 200, 400, 300));
        let closed = agent.close().unwrap();
        let events = touch_events(&closed.transport.bytes);
        assert_eq!(events.len(), 4);
        assert_eq!(events[0], (TouchAction::DOWN.value(), 0, 10, 20));
        assert_eq!(events[1], (TouchAction::UP.value(), 0, 10, 20));
        assert_eq!(
            events[2],
            (TouchAction::DOWN.value(), pointer.value(), 175, 275)
        );
        assert_eq!(
            events[3],
            (TouchAction::UP.value(), pointer.value(), 175, 275)
        );
    }

    #[test]
    fn agent_run_actions_and_tap_next_object_class_at_flushes_then_taps_target() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let mut stream = Vec::new();
        stream.extend(frame_summary_envelope_with(
            1,
            &[ObjectBox {
                x: 10,
                y: 20,
                w: 11,
                h: 21,
                class_id: 8,
                confidence: 255,
            }],
            &[],
        ));
        stream.extend(frame_summary_envelope_with(
            2,
            &[ObjectBox {
                x: 100,
                y: 200,
                w: 301,
                h: 101,
                class_id: 9,
                confidence: 220,
            }],
            &[],
        ));
        let mut agent = AgentControlSession::from_parts(session, Cursor::new(stream)).unwrap();

        agent.set_screen_size(1000, 2000).unwrap();
        let rect = agent
            .run_actions_and_tap_next_object_class_at(&[AgentAction::tap(10, 20)], 9, 10_000, 0)
            .unwrap();

        assert_eq!(rect.to_pixels(1000, 2000), (100, 200, 400, 300));
        let closed = agent.close().unwrap();
        let events = touch_events(&closed.transport.bytes);
        assert_eq!(events.len(), 4);
        assert_eq!(events[0], (TouchAction::DOWN.value(), 0, 10, 20));
        assert_eq!(events[1], (TouchAction::UP.value(), 0, 10, 20));
        assert_eq!(events[2], (TouchAction::DOWN.value(), 0, 400, 200));
        assert_eq!(events[3], (TouchAction::UP.value(), 0, 400, 200));
    }

    #[test]
    fn agent_tap_next_object_anchor_helpers_emit_relative_touch_events() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let mut stream = Vec::new();
        stream.extend(frame_summary_envelope_with(
            1,
            &[ObjectBox {
                x: 100,
                y: 200,
                w: 301,
                h: 101,
                class_id: 1,
                confidence: 210,
            }],
            &[],
        ));
        stream.extend(frame_summary_envelope_with(
            2,
            &[ObjectBox {
                x: 700,
                y: 800,
                w: 101,
                h: 101,
                class_id: 2,
                confidence: 230,
            }],
            &[],
        ));
        stream.extend(frame_summary_envelope_with(
            3,
            &[ObjectBox {
                x: 100,
                y: 200,
                w: 301,
                h: 101,
                class_id: 4,
                confidence: 220,
            }],
            &[],
        ));
        stream.extend(frame_summary_envelope_with(
            4,
            &[ObjectBox {
                x: 100,
                y: 200,
                w: 301,
                h: 101,
                class_id: 5,
                confidence: 220,
            }],
            &[],
        ));
        let mut agent = AgentControlSession::from_parts(session, Cursor::new(stream)).unwrap();

        agent.set_screen_size(1000, 2000).unwrap();
        let indexed = agent.tap_next_object_at(0, 0, 10_000).unwrap();
        let best = agent.tap_next_best_object_at(10_000, 0).unwrap();
        let class = agent.tap_next_object_class_at(4, 5_000, 5_000).unwrap();
        let selected = agent
            .tap_next_object_selector_at(
                AgentObjectSelector::class_min_confidence(5, 220),
                2_500,
                7_500,
            )
            .unwrap();

        assert_eq!(indexed.to_pixels(1000, 2000), (100, 200, 400, 300));
        assert_eq!(best.to_pixels(1000, 2000), (700, 800, 800, 900));
        assert_eq!(class.center().to_pixels(1000, 2000), (250, 250));
        assert_eq!(
            selected
                .try_point_at_basis_points(2_500, 7_500)
                .unwrap()
                .to_pixels(1000, 2000),
            (175, 275)
        );

        let closed = agent.close().unwrap();
        let events = touch_events(&closed.transport.bytes);
        assert_eq!(events.len(), 8);
        assert_eq!(events[0], (TouchAction::DOWN.value(), 0, 100, 300));
        assert_eq!(events[2], (TouchAction::DOWN.value(), 0, 800, 800));
        assert_eq!(events[4], (TouchAction::DOWN.value(), 0, 250, 250));
        assert_eq!(events[6], (TouchAction::DOWN.value(), 0, 175, 275));
    }

    #[test]
    fn agent_tap_next_text_anchor_helpers_emit_relative_touch_events() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let mut stream = Vec::new();
        stream.extend(frame_summary_envelope_with(
            1,
            &[],
            &[TextRegion {
                x: 700,
                y: 800,
                w: 101,
                h: 101,
            }],
        ));
        stream.extend(frame_summary_envelope_with(
            2,
            &[],
            &[
                TextRegion {
                    x: 10,
                    y: 20,
                    w: 11,
                    h: 21,
                },
                TextRegion {
                    x: 100,
                    y: 200,
                    w: 301,
                    h: 101,
                },
            ],
        ));
        let mut agent = AgentControlSession::from_parts(session, Cursor::new(stream)).unwrap();

        agent.set_screen_size(1000, 2000).unwrap();
        let indexed = agent.tap_next_text_region_at(0, 10_000, 0).unwrap();
        let largest = agent.tap_next_largest_text_region_at(0, 10_000).unwrap();

        assert_eq!(indexed.to_pixels(1000, 2000), (700, 800, 800, 900));
        assert_eq!(largest.to_pixels(1000, 2000), (100, 200, 400, 300));

        let closed = agent.close().unwrap();
        let events = touch_events(&closed.transport.bytes);
        assert_eq!(events.len(), 4);
        assert_eq!(events[0], (TouchAction::DOWN.value(), 0, 800, 800));
        assert_eq!(events[2], (TouchAction::DOWN.value(), 0, 100, 300));
    }

    #[test]
    fn agent_tap_next_pointer_vision_targets_emit_typed_pointer_events() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let mut stream = Vec::new();
        stream.extend(frame_summary_envelope_with(
            1,
            &[ObjectBox {
                x: 100,
                y: 200,
                w: 301,
                h: 101,
                class_id: 7,
                confidence: 220,
            }],
            &[],
        ));
        stream.extend(frame_summary_envelope_with(
            2,
            &[],
            &[TextRegion {
                x: 700,
                y: 800,
                w: 101,
                h: 101,
            }],
        ));
        let pointer = TouchPointerId::VIRTUAL_FINGER;
        let mut agent = AgentControlSession::from_parts(session, Cursor::new(stream)).unwrap();

        agent.set_screen_size(1000, 2000).unwrap();
        let object = agent
            .tap_next_best_object_at_pointer(pointer, 2_500, 7_500)
            .unwrap();
        let text = agent.tap_next_largest_text_region_pointer(pointer).unwrap();

        assert_eq!(object.to_pixels(1000, 2000), (100, 200, 400, 300));
        assert_eq!(text.center().to_pixels(1000, 2000), (750, 850));

        let closed = agent.close().unwrap();
        let events = touch_events(&closed.transport.bytes);
        assert_eq!(events.len(), 4);
        assert!(events
            .iter()
            .all(|(_, pointer_id, _, _)| *pointer_id == pointer.value()));
        assert_eq!(
            events[0],
            (TouchAction::DOWN.value(), pointer.value(), 175, 275)
        );
        assert_eq!(
            events[2],
            (TouchAction::DOWN.value(), pointer.value(), 750, 850)
        );
    }

    #[test]
    fn agent_latest_snapshot_target_helpers_select_and_tap_without_waiting() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let snapshot = latest_snapshot_from_envelope(
            9,
            frame_summary_envelope_full(
                3,
                crate::ai::FLAG_OBJECTS | crate::ai::FLAG_TEXT,
                &[],
                &[
                    ObjectBox {
                        x: 10,
                        y: 20,
                        w: 11,
                        h: 21,
                        class_id: 3,
                        confidence: 210,
                    },
                    ObjectBox {
                        x: 100,
                        y: 200,
                        w: 301,
                        h: 101,
                        class_id: 7,
                        confidence: 230,
                    },
                ],
                &[
                    TextRegion {
                        x: 700,
                        y: 800,
                        w: 101,
                        h: 101,
                    },
                    TextRegion {
                        x: 100,
                        y: 200,
                        w: 301,
                        h: 101,
                    },
                ],
            ),
        );
        let pointer = TouchPointerId::VIRTUAL_FINGER;
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
        agent.set_screen_size(1000, 2000).unwrap();

        assert!(
            AgentTargetSelector::object_class_min_confidence(7, 220).is_present(&snapshot.summary)
        );
        assert!(!AgentTargetSelector::object_class(99).is_present(&snapshot.summary));
        let best = agent
            .latest_target_rect(&snapshot, AgentTargetSelector::best_object())
            .unwrap()
            .unwrap();
        let indexed_text = agent
            .latest_target_rect(&snapshot, AgentTargetSelector::text_region(0))
            .unwrap()
            .unwrap();
        let observation = LatestFrameSummaryObservation::from_snapshot(snapshot.clone());
        let empty_observation = LatestFrameSummaryObservation::at_version(0);
        let observed_best = agent
            .latest_observation_target_rect(&observation, AgentTargetSelector::best_object())
            .unwrap()
            .unwrap();
        let object = agent
            .tap_latest_object_selector_at_pointer(
                &snapshot,
                AgentObjectSelector::class_min_confidence(7, 220),
                pointer,
                2_500,
                7_500,
            )
            .unwrap()
            .unwrap();
        let text = agent
            .tap_latest_target_at(
                &snapshot,
                AgentTargetSelector::largest_text_region(),
                0,
                10_000,
            )
            .unwrap()
            .unwrap();
        let indexed_text_tap = agent
            .tap_latest_target_pointer(&snapshot, AgentTargetSelector::text_region(0), pointer)
            .unwrap()
            .unwrap();
        let observed_center = agent
            .tap_latest_observation_target(
                &observation,
                AgentTargetSelector::object_class_min_confidence(7, 220),
            )
            .unwrap()
            .unwrap();
        let observed_anchor = agent
            .tap_latest_observation_target_at(
                &observation,
                AgentTargetSelector::text_region(0),
                10_000,
                0,
            )
            .unwrap()
            .unwrap();
        let observed_pointer = agent
            .tap_latest_observation_target_at_pointer(
                &observation,
                AgentTargetSelector::largest_text_region(),
                pointer,
                0,
                10_000,
            )
            .unwrap()
            .unwrap();
        assert!(agent
            .tap_latest_object_selector(&snapshot, AgentObjectSelector::class_id(99))
            .unwrap()
            .is_none());
        assert!(agent
            .tap_latest_target(&snapshot, AgentTargetSelector::object_class(99))
            .unwrap()
            .is_none());
        assert!(agent
            .latest_observation_target_rect(&empty_observation, AgentTargetSelector::best_object())
            .unwrap()
            .is_none());
        assert!(agent
            .tap_latest_observation_target(&observation, AgentTargetSelector::object_class(99))
            .unwrap()
            .is_none());
        assert!(agent
            .tap_latest_observation_target_pointer(
                &empty_observation,
                AgentTargetSelector::text_region(0),
                pointer,
            )
            .unwrap()
            .is_none());

        assert_eq!(best.to_pixels(1000, 2000), (100, 200, 400, 300));
        assert_eq!(observed_best.to_pixels(1000, 2000), (100, 200, 400, 300));
        assert_eq!(object.to_pixels(1000, 2000), (100, 200, 400, 300));
        assert_eq!(indexed_text.to_pixels(1000, 2000), (700, 800, 800, 900));
        assert_eq!(text.to_pixels(1000, 2000), (100, 200, 400, 300));
        assert_eq!(indexed_text_tap.to_pixels(1000, 2000), (700, 800, 800, 900));
        assert_eq!(observed_center.to_pixels(1000, 2000), (100, 200, 400, 300));
        assert_eq!(observed_anchor.to_pixels(1000, 2000), (700, 800, 800, 900));
        assert_eq!(observed_pointer.to_pixels(1000, 2000), (100, 200, 400, 300));
        let closed = agent.close().unwrap();
        let events = touch_events(&closed.transport.bytes);
        assert_eq!(events.len(), 12);
        assert_eq!(
            events[0],
            (TouchAction::DOWN.value(), pointer.value(), 175, 275)
        );
        assert_eq!(
            events[1],
            (TouchAction::UP.value(), pointer.value(), 175, 275)
        );
        assert_eq!(events[2], (TouchAction::DOWN.value(), 0, 100, 300));
        assert_eq!(events[3], (TouchAction::UP.value(), 0, 100, 300));
        assert_eq!(
            events[4],
            (TouchAction::DOWN.value(), pointer.value(), 750, 850)
        );
        assert_eq!(
            events[5],
            (TouchAction::UP.value(), pointer.value(), 750, 850)
        );
        assert_eq!(events[6], (TouchAction::DOWN.value(), 0, 250, 250));
        assert_eq!(events[7], (TouchAction::UP.value(), 0, 250, 250));
        assert_eq!(events[8], (TouchAction::DOWN.value(), 0, 800, 800));
        assert_eq!(events[9], (TouchAction::UP.value(), 0, 800, 800));
        assert_eq!(
            events[10],
            (TouchAction::DOWN.value(), pointer.value(), 100, 300)
        );
        assert_eq!(
            events[11],
            (TouchAction::UP.value(), pointer.value(), 100, 300)
        );
    }

    #[test]
    fn agent_clone_client_can_send_from_worker() {
        let session =
            HidSession::open(MockTransport::new(), OpenRequest::gamepad_only_realtime()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
        let client = agent.clone_client();

        let worker = std::thread::spawn(move || {
            client
                .send_frame_unchecked(GamepadFrameRaw::new(2, 0, 0, 0, 0, 0, 0))
                .unwrap();
        });
        worker.join().unwrap();

        let closed = agent.close().unwrap();
        assert_eq!(count_uhid_inputs(&closed.transport.bytes), 1);
    }

    #[test]
    fn agent_flush_surfaces_prior_dispatch_error() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

        agent.type_text("needs keyboard").unwrap();
        let err = agent.flush().unwrap_err();
        assert!(matches!(err, Error::SessionLifecycle("keyboard not open")));

        let closed = agent.close().unwrap();
        assert!(closed.transport.bytes.is_empty());
    }

    #[test]
    fn agent_type_text_strict_surfaces_unsupported_char() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::kbd_only()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

        agent.type_text_strict("a中b").unwrap();
        let err = agent.flush().unwrap_err();
        assert!(matches!(
            err,
            Error::SessionLifecycle("unsupported char in type_text_strict")
        ));

        let closed = agent.close().unwrap();
        assert_eq!(
            count_uhid_inputs(&closed.transport.bytes),
            2,
            "strict text should stop at the first unsupported character"
        );
    }

    #[test]
    fn agent_keyboard_tap_helpers_emit_uhid_reports() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::kbd_only()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

        agent.tap_scancode(Scancode::A, Modifiers::LSHIFT).unwrap();
        agent
            .key_scancode(Scancode::B, true, Modifiers::empty())
            .unwrap();
        agent
            .key_scancode(Scancode::B, false, Modifiers::empty())
            .unwrap();

        let closed = agent.close().unwrap();
        assert_eq!(
            count_uhid_inputs(&closed.transport.bytes),
            4,
            "tap_scancode emits down/up and key_scancode emits one report per edge"
        );
    }

    #[test]
    fn agent_try_keyboard_helpers_use_nonblocking_checked_dispatch() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::kbd_only()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

        agent
            .try_tap_scancode(Scancode::A, Modifiers::LSHIFT)
            .unwrap();
        agent
            .try_key_scancode(Scancode::B, true, Modifiers::empty())
            .unwrap();
        agent
            .try_key_scancode(Scancode::B, false, Modifiers::empty())
            .unwrap();
        agent
            .try_key(Scancode::C.to_u8(), true, Modifiers::empty())
            .unwrap();
        agent
            .try_key(Scancode::C.to_u8(), false, Modifiers::empty())
            .unwrap();
        agent
            .try_tap_key(Scancode::D.to_u8(), Modifiers::LCTRL)
            .unwrap();
        agent
            .try_scancode_chord(&[Scancode::K, Scancode::C], Modifiers::LCTRL)
            .unwrap();

        let closed = agent.close().unwrap();
        assert_eq!(count_uhid_inputs(&closed.transport.bytes), 12);
    }

    #[test]
    fn agent_try_keyboard_preflights_command_bound_without_dispatch() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts_with_bound(session, Cursor::new(Vec::new()), 1)
            .unwrap();

        let err = agent
            .try_tap_scancode(Scancode::A, Modifiers::LSHIFT)
            .unwrap_err();

        assert!(matches!(
            err,
            Error::SessionLifecycle(TRY_KEY_EXCEEDS_COMMAND_BOUND)
        ));
        let closed = agent.close().unwrap();
        assert!(closed.transport.bytes.is_empty());
    }

    #[test]
    fn agent_close_checked_reports_error_and_recovers_resources() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(ack(5))).unwrap();

        agent.type_text("needs keyboard").unwrap();
        agent
            .client()
            .send(crate::client::HidCommand::MultitouchDown {
                id: 0,
                x: 10,
                y: 20,
                pressure: 1.0,
            })
            .unwrap();
        let report = agent.close_checked().unwrap();

        assert!(matches!(
            report.command_result,
            Err(Error::SessionLifecycle("keyboard not open"))
        ));
        assert_eq!(count_touch_events(&report.closed.transport.bytes), 1);
        assert_eq!(report.closed.reader.position(), 0);
    }

    #[test]
    fn agent_intent_helpers_emit_touch_text_and_launch_commands() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

        agent.tap(10, 20).unwrap();
        agent.swipe((0, 0), (30, 60), 3).unwrap();
        agent.type_text("hi").unwrap();
        agent.launch_app("com.android.settings").unwrap();
        agent.set_screen_power(false).unwrap();

        let closed = agent.close().unwrap();
        let bytes = closed.transport.bytes;
        assert_eq!(count_touch_events(&bytes), 7);
        assert!(bytes.contains(&1), "InjectText tag should be present");
        assert!(bytes.contains(&16), "StartApp tag should be present");
        assert!(bytes.contains(&10), "SetDisplayPower tag should be present");
    }

    #[test]
    fn agent_point_converts_normalized_coordinates() {
        assert_eq!(AgentPoint::CENTER.to_pixels(1080, 2400), (540, 1200));
        assert_eq!(AgentPoint::BOTTOM_RIGHT.to_pixels(1080, 2400), (1079, 2399));
        assert_eq!(
            AgentPoint::try_from_basis_points(5_000, 2_500)
                .unwrap()
                .to_pixels(1080, 2400),
            (540, 600)
        );
        assert_eq!(
            AgentPoint::try_from_unit(0.25, 0.75)
                .unwrap()
                .to_pixels(1000, 2000),
            (250, 1499)
        );
        assert!(matches!(
            AgentPoint::try_from_unit(1.1, 0.5),
            Err(Error::SessionLifecycle("normalized point out of range"))
        ));
        assert!(matches!(
            AgentPoint::try_from_basis_points(10_001, 0),
            Err(Error::SessionLifecycle("normalized point out of range"))
        ));
    }

    #[test]
    fn agent_rect_converts_detection_boxes_to_normalized_targets() {
        assert_eq!(AgentRect::FULL_SCREEN.center(), AgentPoint::CENTER);
        assert_eq!(
            AgentRect::try_from_basis_points(2_500, 2_500, 7_500, 7_500)
                .unwrap()
                .center()
                .to_pixels(1000, 2000),
            (500, 1000)
        );

        let object = ObjectBox {
            x: 100,
            y: 200,
            w: 301,
            h: 101,
            class_id: 7,
            confidence: 220,
        };
        let rect = AgentRect::try_from_object_box(object, 1000, 2000).unwrap();
        assert_eq!(rect.to_pixels(1000, 2000), (100, 200, 400, 300));
        assert_eq!(rect.center().to_pixels(1000, 2000), (250, 250));

        let text = TextRegion {
            x: 10,
            y: 20,
            w: 11,
            h: 21,
        };
        let rect = AgentRect::try_from_text_region(text, 100, 200).unwrap();
        assert_eq!(rect.center().to_pixels(100, 200), (15, 30));

        assert!(matches!(
            AgentRect::try_from_pixels(990, 0, 20, 10, 1000, 2000),
            Err(Error::SessionLifecycle("agent rectangle out of range"))
        ));
        assert!(matches!(
            AgentRect::try_from_basis_points(0, 0, 10_001, 1),
            Err(Error::SessionLifecycle("normalized point out of range"))
        ));
    }

    #[test]
    fn agent_rect_points_at_relative_anchors() {
        let rect = AgentRect::try_from_pixels(100, 200, 301, 101, 1000, 2000).unwrap();

        assert_eq!(
            rect.try_point_at_basis_points(0, 0)
                .unwrap()
                .to_pixels(1000, 2000),
            (100, 200)
        );
        assert_eq!(
            rect.try_point_at_basis_points(10_000, 10_000)
                .unwrap()
                .to_pixels(1000, 2000),
            (400, 300)
        );
        assert_eq!(
            rect.try_point_at_basis_points(2_500, 7_500)
                .unwrap()
                .to_pixels(1000, 2000),
            (175, 275)
        );
        assert_eq!(
            rect.try_point_at_unit(0.25, 0.75)
                .unwrap()
                .to_pixels(1000, 2000),
            (175, 275)
        );

        let reversed = AgentRect {
            left: rect.right,
            top: rect.bottom,
            right: rect.left,
            bottom: rect.top,
        };
        assert_eq!(
            reversed
                .try_point_at_basis_points(0, 0)
                .unwrap()
                .to_pixels(1000, 2000),
            (100, 200)
        );
        assert!(matches!(
            rect.try_point_at_basis_points(10_001, 0),
            Err(Error::SessionLifecycle("normalized point out of range"))
        ));
        assert!(matches!(
            rect.try_point_at_unit(f32::NAN, 0.5),
            Err(Error::SessionLifecycle("normalized point out of range"))
        ));
    }

    #[test]
    fn agent_rect_selects_targets_from_frame_summary() {
        let summary = FrameSummary {
            timestamp_ms: 1,
            frame_seq: 2,
            width: 1000,
            height: 2000,
            flags: crate::ai::FLAG_OBJECTS | crate::ai::FLAG_TEXT,
            features: Vec::new(),
            motion: Vec::new(),
            objects: vec![
                ObjectBox {
                    x: 10,
                    y: 20,
                    w: 11,
                    h: 21,
                    class_id: 1,
                    confidence: 200,
                },
                ObjectBox {
                    x: 100,
                    y: 200,
                    w: 101,
                    h: 101,
                    class_id: 2,
                    confidence: 220,
                },
                ObjectBox {
                    x: 300,
                    y: 400,
                    w: 301,
                    h: 101,
                    class_id: 2,
                    confidence: 220,
                },
                ObjectBox {
                    x: 500,
                    y: 600,
                    w: 11,
                    h: 21,
                    class_id: 3,
                    confidence: 230,
                },
            ],
            text_regions: vec![
                TextRegion {
                    x: 10,
                    y: 20,
                    w: 11,
                    h: 21,
                },
                TextRegion {
                    x: 700,
                    y: 800,
                    w: 101,
                    h: 101,
                },
            ],
        };

        assert_eq!(
            AgentRect::try_from_frame_object(&summary, 1)
                .unwrap()
                .unwrap()
                .center()
                .to_pixels(1000, 2000),
            (150, 250)
        );
        assert!(AgentRect::try_from_frame_object(&summary, 99)
            .unwrap()
            .is_none());
        assert_eq!(
            AgentRect::try_from_best_object(&summary)
                .unwrap()
                .unwrap()
                .center()
                .to_pixels(1000, 2000),
            (505, 610)
        );
        assert_eq!(
            AgentRect::try_from_best_object_class(&summary, 2)
                .unwrap()
                .unwrap()
                .center()
                .to_pixels(1000, 2000),
            (450, 450)
        );
        assert!(AgentRect::try_from_best_object_class(&summary, 9)
            .unwrap()
            .is_none());
        assert_eq!(
            AgentRect::try_from_frame_text_region(&summary, 0)
                .unwrap()
                .unwrap()
                .center()
                .to_pixels(1000, 2000),
            (15, 30)
        );
        assert_eq!(
            AgentRect::try_from_largest_text_region(&summary)
                .unwrap()
                .unwrap()
                .center()
                .to_pixels(1000, 2000),
            (750, 850)
        );

        let bad_summary = FrameSummary {
            width: 0,
            objects: vec![summary.objects[0]],
            ..summary
        };
        assert!(matches!(
            AgentRect::try_from_best_object(&bad_summary),
            Err(Error::SessionLifecycle("agent rectangle out of range"))
        ));
    }

    #[test]
    fn agent_object_selector_filters_class_and_confidence() {
        let summary = FrameSummary {
            timestamp_ms: 1,
            frame_seq: 2,
            width: 1000,
            height: 2000,
            flags: crate::ai::FLAG_OBJECTS,
            features: Vec::new(),
            motion: Vec::new(),
            objects: vec![
                ObjectBox {
                    x: 100,
                    y: 200,
                    w: 101,
                    h: 101,
                    class_id: 2,
                    confidence: 220,
                },
                ObjectBox {
                    x: 300,
                    y: 400,
                    w: 301,
                    h: 101,
                    class_id: 2,
                    confidence: 220,
                },
                ObjectBox {
                    x: 500,
                    y: 600,
                    w: 11,
                    h: 21,
                    class_id: 3,
                    confidence: 230,
                },
            ],
            text_regions: Vec::new(),
        };

        assert_eq!(
            AgentObjectSelector::ANY.select(&summary).unwrap().class_id,
            3
        );
        assert_eq!(
            AgentObjectSelector::class_min_confidence(2, 220)
                .select_rect(&summary)
                .unwrap()
                .unwrap()
                .center()
                .to_pixels(1000, 2000),
            (450, 450)
        );
        assert!(AgentObjectSelector::class_id(2).matches(summary.objects[0]));
        assert!(!AgentObjectSelector::min_confidence(231).matches(summary.objects[2]));
        assert!(AgentRect::try_from_best_object_matching(
            &summary,
            AgentObjectSelector::ANY
                .with_class_id(2)
                .with_min_confidence(221),
        )
        .unwrap()
        .is_none());
    }

    #[test]
    fn agent_normalized_touch_helpers_use_tracked_screen_size() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

        agent.set_screen_size(1080, 2400).unwrap();
        agent.tap_point(AgentPoint::CENTER).unwrap();

        let closed = agent.close().unwrap();
        assert_eq!(first_touch_xy(&closed.transport.bytes), Some((540, 1200)));
        assert_eq!(
            first_touch_screen_size(&closed.transport.bytes),
            Some((1080, 2400))
        );
    }

    #[test]
    fn agent_rect_touch_helpers_use_center_point() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
        let rect = AgentRect::try_from_pixels(100, 200, 301, 101, 1000, 2000).unwrap();

        agent.set_screen_size(1000, 2000).unwrap();
        agent.tap_rect(rect).unwrap();

        let closed = agent.close().unwrap();
        assert_eq!(first_touch_xy(&closed.transport.bytes), Some((250, 250)));
        assert_eq!(
            first_touch_screen_size(&closed.transport.bytes),
            Some((1000, 2000))
        );
    }

    #[test]
    fn agent_rect_anchor_touch_helpers_use_relative_points() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
        let rect = AgentRect::try_from_pixels(100, 200, 301, 101, 1000, 2000).unwrap();

        agent.set_screen_size(1000, 2000).unwrap();
        agent.tap_rect_at(rect, 2_500, 7_500).unwrap();
        agent
            .tap_rect_at_pointer(TouchPointerId::VIRTUAL_FINGER, rect, 10_000, 0)
            .unwrap();

        let closed = agent.close().unwrap();
        let events = touch_events(&closed.transport.bytes);
        assert_eq!(events.len(), 4);
        assert_eq!(events[0], (TouchAction::DOWN.value(), 0, 175, 275));
        assert_eq!(
            events[2],
            (
                TouchAction::DOWN.value(),
                TouchPointerId::VIRTUAL_FINGER.value(),
                400,
                200,
            )
        );
    }

    #[test]
    fn agent_try_tap_rect_anchor_pointer_uses_nonblocking_checked_dispatch() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
        let rect = AgentRect::try_from_pixels(100, 200, 301, 101, 1000, 2000).unwrap();

        agent.set_screen_size(1000, 2000).unwrap();
        agent
            .try_tap_rect_at_pointer(TouchPointerId::VIRTUAL_FINGER, rect, 2_500, 7_500)
            .unwrap();

        let closed = agent.close().unwrap();
        let events = touch_events(&closed.transport.bytes);
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[0],
            (
                TouchAction::DOWN.value(),
                TouchPointerId::VIRTUAL_FINGER.value(),
                175,
                275,
            )
        );
        assert_eq!(
            events[1],
            (
                TouchAction::UP.value(),
                TouchPointerId::VIRTUAL_FINGER.value(),
                175,
                275,
            )
        );
    }

    #[test]
    fn agent_try_tap_preflights_command_bound_without_dispatch() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts_with_bound(session, Cursor::new(Vec::new()), 1)
            .unwrap();

        let err = agent.try_tap(10, 20).unwrap_err();

        assert!(matches!(
            err,
            Error::SessionLifecycle(TRY_TAP_EXCEEDS_COMMAND_BOUND)
        ));
        let closed = agent.close().unwrap();
        assert!(closed.transport.bytes.is_empty());
    }

    #[test]
    fn agent_try_double_tap_rect_anchor_pointer_uses_nonblocking_checked_dispatch() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
        let rect = AgentRect::try_from_pixels(100, 200, 301, 101, 1000, 2000).unwrap();

        agent.set_screen_size(1000, 2000).unwrap();
        agent
            .try_double_tap_rect_at_pointer(TouchPointerId::VIRTUAL_FINGER, rect, 2_500, 7_500)
            .unwrap();

        let closed = agent.close().unwrap();
        let events = touch_events(&closed.transport.bytes);
        assert_eq!(events.len(), 4);
        assert_eq!(
            events[0],
            (
                TouchAction::DOWN.value(),
                TouchPointerId::VIRTUAL_FINGER.value(),
                175,
                275,
            )
        );
        assert_eq!(
            events[1],
            (
                TouchAction::UP.value(),
                TouchPointerId::VIRTUAL_FINGER.value(),
                175,
                275,
            )
        );
        assert_eq!(events[2], events[0]);
        assert_eq!(events[3], events[1]);
    }

    #[test]
    fn agent_try_double_tap_preflights_command_bound_without_dispatch() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts_with_bound(session, Cursor::new(Vec::new()), 1)
            .unwrap();

        let err = agent.try_double_tap(10, 20).unwrap_err();

        assert!(matches!(
            err,
            Error::SessionLifecycle(TRY_DOUBLE_TAP_EXCEEDS_COMMAND_BOUND)
        ));
        let closed = agent.close().unwrap();
        assert!(closed.transport.bytes.is_empty());
    }

    #[test]
    fn agent_run_actions_batches_normalized_touch_actions() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

        agent.set_screen_size(1080, 2400).unwrap();
        agent
            .run_actions(&[
                AgentAction::tap_point(AgentPoint::CENTER),
                AgentAction::swipe_points(
                    AgentPoint::try_from_basis_points(0, 0).unwrap(),
                    AgentPoint::try_from_basis_points(10_000, 10_000).unwrap(),
                    2,
                ),
                AgentAction::double_tap_point(AgentPoint::try_from_unit(0.25, 0.25).unwrap()),
            ])
            .unwrap();

        let closed = agent.close().unwrap();
        assert_eq!(count_touch_events(&closed.transport.bytes), 2 + 4 + 4);
        assert_eq!(control_message_tags(&closed.transport.bytes), vec![2; 10]);
    }

    #[test]
    fn agent_run_actions_batches_rect_touch_actions() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
        let rect = AgentRect::try_from_basis_points(4_000, 4_000, 6_000, 6_000).unwrap();

        agent.set_screen_size(1000, 2000).unwrap();
        agent
            .run_actions(&[
                AgentAction::tap_rect(rect),
                AgentAction::double_tap_rect(rect),
            ])
            .unwrap();

        let closed = agent.close().unwrap();
        assert_eq!(control_message_tags(&closed.transport.bytes), vec![2; 6]);
        assert_eq!(first_touch_xy(&closed.transport.bytes), Some((500, 1000)));
    }

    #[test]
    fn agent_run_actions_batches_rect_anchor_actions() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
        let pointer = TouchPointerId::GENERIC_FINGER;
        let rect = AgentRect::try_from_pixels(100, 200, 301, 101, 1000, 2000).unwrap();

        agent.set_screen_size(1000, 2000).unwrap();
        agent
            .run_actions(&[
                AgentAction::tap_rect_at(rect, 0, 0),
                AgentAction::tap_rect_at_pointer(pointer, rect, 10_000, 0),
                AgentAction::double_tap_rect_at(rect, 2_500, 7_500),
                AgentAction::double_tap_rect_at_pointer(pointer, rect, 10_000, 10_000),
            ])
            .unwrap();

        let closed = agent.close().unwrap();
        let events = touch_events(&closed.transport.bytes);
        assert_eq!(events.len(), 2 + 2 + 4 + 4);
        assert_eq!(control_message_tags(&closed.transport.bytes), vec![2; 12]);
        assert!(events.contains(&(TouchAction::DOWN.value(), 0, 100, 200)));
        assert!(events.contains(&(TouchAction::DOWN.value(), pointer.value(), 400, 200)));
        assert!(events.contains(&(TouchAction::DOWN.value(), 0, 175, 275)));
        assert!(events.contains(&(TouchAction::DOWN.value(), pointer.value(), 400, 300)));
    }

    #[test]
    fn agent_rect_swipe_helpers_use_relative_points() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
        let pointer = TouchPointerId::VIRTUAL_FINGER;
        let rect = AgentRect::try_from_pixels(100, 200, 301, 101, 1000, 2000).unwrap();

        agent.set_screen_size(1000, 2000).unwrap();
        agent
            .swipe_rect(rect, (0, 5_000), (10_000, 5_000), 2)
            .unwrap();
        agent
            .swipe_rect_pointer(pointer, rect, (2_500, 0), (2_500, 10_000), 1)
            .unwrap();

        let closed = agent.close().unwrap();
        let events = touch_events(&closed.transport.bytes);
        assert_eq!(events.len(), 4 + 3);
        assert_eq!(events[0], (TouchAction::DOWN.value(), 0, 100, 250));
        assert_eq!(events[2], (TouchAction::MOVE.value(), 0, 400, 250));
        assert_eq!(events[3], (TouchAction::UP.value(), 0, 400, 250));
        assert_eq!(
            events[4],
            (TouchAction::DOWN.value(), pointer.value(), 175, 200)
        );
        assert_eq!(
            events[6],
            (TouchAction::UP.value(), pointer.value(), 175, 300)
        );
    }

    #[test]
    fn agent_run_actions_batches_rect_swipe_actions() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
        let pointer = TouchPointerId::GENERIC_FINGER;
        let rect = AgentRect::try_from_pixels(100, 200, 301, 101, 1000, 2000).unwrap();

        agent.set_screen_size(1000, 2000).unwrap();
        agent
            .run_actions(&[
                AgentAction::tap_rect_at(rect, 0, 0),
                AgentAction::swipe_rect(rect, (0, 5_000), (10_000, 5_000), 2),
                AgentAction::swipe_rect_pointer(pointer, rect, (2_500, 0), (2_500, 10_000), 1),
                AgentAction::double_tap_rect_at(rect, 10_000, 10_000),
            ])
            .unwrap();

        let closed = agent.close().unwrap();
        let events = touch_events(&closed.transport.bytes);
        assert_eq!(events.len(), 2 + 4 + 3 + 4);
        assert_eq!(control_message_tags(&closed.transport.bytes), vec![2; 13]);
        assert!(events.contains(&(TouchAction::DOWN.value(), 0, 100, 250)));
        assert!(events.contains(&(TouchAction::UP.value(), 0, 400, 250)));
        assert!(events.contains(&(TouchAction::DOWN.value(), pointer.value(), 175, 200)));
        assert!(events.contains(&(TouchAction::UP.value(), pointer.value(), 175, 300)));
    }

    #[test]
    fn agent_try_queue_actions_batches_rect_swipes_with_tiny_bound() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts_with_bound(session, Cursor::new(Vec::new()), 1)
            .unwrap();

        agent
            .try_queue_actions(&[AgentAction::swipe_rect(
                AgentRect::FULL_SCREEN,
                (0, 5_000),
                (10_000, 5_000),
                2,
            )])
            .unwrap();

        let closed = agent.close().unwrap();
        let events = touch_events(&closed.transport.bytes);
        assert_eq!(events.len(), 4);
        assert_eq!(events[0], (TouchAction::DOWN.value(), 0, 0, 960));
        assert_eq!(events[3], (TouchAction::UP.value(), 0, 1079, 960));
    }

    #[test]
    fn agent_run_actions_batches_pointer_touch_actions() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
        let pointer = TouchPointerId::GENERIC_FINGER;
        let rect = AgentRect::try_from_basis_points(4_000, 4_000, 6_000, 6_000).unwrap();

        agent.set_screen_size(1000, 2000).unwrap();
        agent
            .run_actions(&[
                AgentAction::tap_pointer(pointer, 10, 20),
                AgentAction::tap_point_pointer(pointer, AgentPoint::CENTER),
                AgentAction::tap_rect_pointer(pointer, rect),
                AgentAction::swipe_points_pointer(
                    pointer,
                    AgentPoint::try_from_basis_points(0, 0).unwrap(),
                    AgentPoint::try_from_basis_points(10_000, 10_000).unwrap(),
                    2,
                ),
                AgentAction::double_tap_rect_pointer(pointer, rect),
            ])
            .unwrap();

        let closed = agent.close().unwrap();
        let events = touch_events(&closed.transport.bytes);
        assert_eq!(events.len(), 2 + 2 + 2 + 4 + 4);
        assert_eq!(control_message_tags(&closed.transport.bytes), vec![2; 14]);
        assert!(events
            .iter()
            .all(|(_, pointer_id, _, _)| *pointer_id == pointer.value()));
        assert!(events.contains(&(TouchAction::DOWN.value(), pointer.value(), 500, 1000)));
        assert!(events.contains(&(TouchAction::UP.value(), pointer.value(), 999, 1999)));
    }

    #[test]
    fn agent_pinch_helper_emits_two_pointer_touch_path() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

        agent
            .pinch((100, 1200), (240, 1200), (980, 1200), (840, 1200), 3)
            .unwrap();

        let closed = agent.close().unwrap();
        let events = touch_events(&closed.transport.bytes);
        assert_eq!(events.len(), 2 + 3 * 2 + 2);
        assert_eq!(control_message_tags(&closed.transport.bytes), vec![2; 10]);
        assert_eq!(events[0], (TouchAction::DOWN.value(), 0, 100, 1200));
        assert_eq!(events[1], (TouchAction::DOWN.value(), 1, 980, 1200));
        assert_eq!(events[8], (TouchAction::UP.value(), 0, 240, 1200));
        assert_eq!(events[9], (TouchAction::UP.value(), 1, 840, 1200));
        assert_eq!(events.iter().filter(|(_, id, _, _)| *id == 0).count(), 5);
        assert_eq!(events.iter().filter(|(_, id, _, _)| *id == 1).count(), 5);
    }

    #[test]
    fn agent_run_actions_batches_normalized_pinch_with_adjacent_touch_actions() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
        let first_from = AgentPoint::try_from_basis_points(4_000, 5_000).unwrap();
        let first_to = AgentPoint::try_from_basis_points(3_000, 5_000).unwrap();
        let second_from = AgentPoint::try_from_basis_points(6_000, 5_000).unwrap();
        let second_to = AgentPoint::try_from_basis_points(7_000, 5_000).unwrap();

        agent.set_screen_size(1000, 2000).unwrap();
        agent
            .run_actions(&[
                AgentAction::tap_point(AgentPoint::CENTER),
                AgentAction::pinch_points(first_from, first_to, second_from, second_to, 2),
                AgentAction::double_tap_point(AgentPoint::CENTER),
            ])
            .unwrap();

        let closed = agent.close().unwrap();
        let events = touch_events(&closed.transport.bytes);
        assert_eq!(events.len(), 2 + 2 + 2 * 2 + 2 + 4);
        assert_eq!(control_message_tags(&closed.transport.bytes), vec![2; 14]);

        let (x0, y0) = first_from.to_pixels(1000, 2000);
        let (x1, y1) = second_to.to_pixels(1000, 2000);
        assert!(events.contains(&(TouchAction::DOWN.value(), 0, x0, y0)));
        assert!(events.contains(&(TouchAction::UP.value(), 1, x1, y1)));
    }

    #[test]
    fn agent_try_queue_actions_batches_pinch_with_tiny_bound() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts_with_bound(session, Cursor::new(Vec::new()), 1)
            .unwrap();

        agent
            .try_queue_actions(&[AgentAction::pinch(
                (10, 20),
                (20, 20),
                (50, 20),
                (40, 20),
                1,
            )])
            .unwrap();

        let closed = agent.close().unwrap();
        let events = touch_events(&closed.transport.bytes);
        assert_eq!(events.len(), 6);
        assert_eq!(events[0], (TouchAction::DOWN.value(), 0, 10, 20));
        assert_eq!(events[5], (TouchAction::UP.value(), 1, 40, 20));
    }

    #[test]
    fn agent_normalized_scroll_helpers_use_tracked_screen_size() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

        agent.set_screen_size(720, 1280).unwrap();
        agent
            .scroll_point_with_buttons(AgentPoint::CENTER, 0.0, -16.0, 0x11)
            .unwrap();
        agent
            .run_actions(&[AgentAction::scroll_point(
                AgentPoint::try_from_basis_points(2_500, 7_500).unwrap(),
                0,
                16,
            )])
            .unwrap();

        let closed = agent.close().unwrap();
        let bytes = closed.transport.bytes;
        assert_eq!(control_message_tags(&bytes), vec![3, 3]);
        assert_eq!(first_scroll_xy(&bytes), Some((360, 640)));
        let second = &bytes[21..42];
        assert_eq!(i32::from_be_bytes(second[1..5].try_into().unwrap()), 180);
        assert_eq!(i32::from_be_bytes(second[5..9].try_into().unwrap()), 959);
    }

    #[test]
    fn agent_rect_scroll_helpers_use_center_point() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
        let rect = AgentRect::try_from_pixels(100, 200, 301, 101, 1000, 2000).unwrap();

        agent.set_screen_size(1000, 2000).unwrap();
        agent
            .scroll_rect_with_buttons(rect, 0.0, -16.0, 0x11)
            .unwrap();
        agent
            .run_actions(&[AgentAction::scroll_rect(rect, 0, 16)])
            .unwrap();

        let closed = agent.close().unwrap();
        assert_eq!(control_message_tags(&closed.transport.bytes), vec![3, 3]);
        assert_eq!(first_scroll_xy(&closed.transport.bytes), Some((250, 250)));
        let second = &closed.transport.bytes[21..42];
        assert_eq!(i32::from_be_bytes(second[1..5].try_into().unwrap()), 250);
        assert_eq!(i32::from_be_bytes(second[5..9].try_into().unwrap()), 250);
    }

    #[test]
    fn agent_rect_anchor_scroll_helpers_use_relative_point() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
        let rect = AgentRect::try_from_pixels(100, 200, 301, 101, 1000, 2000).unwrap();

        agent.set_screen_size(1000, 2000).unwrap();
        agent
            .scroll_rect_at_with_buttons(rect, 2_500, 7_500, 0.0, -16.0, 0x11)
            .unwrap();
        agent
            .run_actions(&[
                AgentAction::scroll_rect_at(rect, 10_000, 0, 0, 16),
                AgentAction::scroll_rect_at_with_buttons(rect, 0, 10_000, 0, 8, 0x22),
            ])
            .unwrap();

        let closed = agent.close().unwrap();
        assert_eq!(control_message_tags(&closed.transport.bytes), vec![3, 3, 3]);
        assert_eq!(first_scroll_xy(&closed.transport.bytes), Some((175, 275)));
        let second = &closed.transport.bytes[21..42];
        assert_eq!(i32::from_be_bytes(second[1..5].try_into().unwrap()), 400);
        assert_eq!(i32::from_be_bytes(second[5..9].try_into().unwrap()), 200);
        let third = &closed.transport.bytes[42..63];
        assert_eq!(i32::from_be_bytes(third[1..5].try_into().unwrap()), 100);
        assert_eq!(i32::from_be_bytes(third[5..9].try_into().unwrap()), 300);
        assert_eq!(u32::from_be_bytes(third[17..21].try_into().unwrap()), 0x22);
    }

    #[test]
    fn agent_try_scroll_rect_anchor_uses_nonblocking_checked_dispatch() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
        let rect = AgentRect::try_from_pixels(100, 200, 301, 101, 1000, 2000).unwrap();

        agent.set_screen_size(1000, 2000).unwrap();
        agent
            .try_scroll_rect_at_with_buttons(rect, 2_500, 7_500, 0.0, -16.0, 0x11)
            .unwrap();

        let closed = agent.close().unwrap();
        let bytes = closed.transport.bytes;
        assert_eq!(control_message_tags(&bytes), vec![3]);
        assert_eq!(first_scroll_xy(&bytes), Some((175, 275)));
        assert_eq!(u32::from_be_bytes(bytes[17..21].try_into().unwrap()), 0x11);
    }

    #[test]
    fn agent_try_scroll_preflights_command_bound_without_dispatch() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts_with_bound(session, Cursor::new(Vec::new()), 1)
            .unwrap();

        let err = agent.try_scroll(10, 20, 0.0, -16.0).unwrap_err();

        assert!(matches!(
            err,
            Error::SessionLifecycle(TRY_SCROLL_EXCEEDS_COMMAND_BOUND)
        ));
        let closed = agent.close().unwrap();
        assert!(closed.transport.bytes.is_empty());
    }

    #[test]
    fn agent_try_queue_actions_rejects_normalized_timed_actions() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

        let err = agent
            .try_queue_actions(&[AgentAction::long_press_point(
                AgentPoint::CENTER,
                Duration::from_millis(1),
            )])
            .unwrap_err();

        assert!(matches!(
            err,
            Error::SessionLifecycle("timed action requires queue_actions or run_actions")
        ));
    }

    #[test]
    fn agent_try_queue_actions_rejects_rect_timed_actions() {
        for action in [
            AgentAction::long_press_rect(AgentRect::FULL_SCREEN, Duration::from_millis(1)),
            AgentAction::long_press_rect_at(
                AgentRect::FULL_SCREEN,
                5_000,
                5_000,
                Duration::from_millis(1),
            ),
        ] {
            let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
            let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

            let err = agent.try_queue_actions(&[action]).unwrap_err();
            assert!(matches!(
                err,
                Error::SessionLifecycle("timed action requires queue_actions or run_actions")
            ));
        }
    }

    #[test]
    fn agent_try_queue_actions_rejects_pointer_timed_actions() {
        for action in [
            AgentAction::long_press_pointer(
                TouchPointerId::VIRTUAL_FINGER,
                10,
                20,
                Duration::from_millis(1),
            ),
            AgentAction::long_press_point_pointer(
                TouchPointerId::VIRTUAL_FINGER,
                AgentPoint::CENTER,
                Duration::from_millis(1),
            ),
            AgentAction::long_press_rect_pointer(
                TouchPointerId::VIRTUAL_FINGER,
                AgentRect::FULL_SCREEN,
                Duration::from_millis(1),
            ),
            AgentAction::long_press_rect_at_pointer(
                TouchPointerId::VIRTUAL_FINGER,
                AgentRect::FULL_SCREEN,
                5_000,
                5_000,
                Duration::from_millis(1),
            ),
        ] {
            let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
            let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

            let err = agent.try_queue_actions(&[action]).unwrap_err();

            assert!(matches!(
                err,
                Error::SessionLifecycle("timed action requires queue_actions or run_actions")
            ));
            let closed = agent.close().unwrap();
            assert!(closed.transport.bytes.is_empty());
        }
    }

    #[test]
    fn agent_android_intent_helpers_emit_control_messages() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

        agent.press_home().unwrap();
        agent.press_back().unwrap();
        agent.open_recents().unwrap();
        agent.volume_up().unwrap();
        agent.volume_down().unwrap();
        agent.volume_mute().unwrap();
        agent.show_notifications().unwrap();
        agent.show_quick_settings().unwrap();
        agent.collapse_panels().unwrap();
        agent.rotate_device().unwrap();
        agent.resize_display(720, 1280).unwrap();
        agent.set_torch(true).unwrap();
        agent.camera_zoom_in().unwrap();
        agent.camera_zoom_out().unwrap();
        agent.open_hard_keyboard_settings().unwrap();
        agent.reset_video().unwrap();

        let closed = agent.close().unwrap();
        let bytes = closed.transport.bytes;
        assert_eq!(
            first_inject_keycodes(&bytes, 6),
            vec![3, 4, 187, 24, 25, 164]
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
    fn agent_try_control_helpers_use_nonblocking_checked_dispatch() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

        agent.try_set_screen_power(false).unwrap();
        agent.try_show_notifications().unwrap();
        agent.try_show_quick_settings().unwrap();
        agent.try_collapse_panels().unwrap();
        agent.try_rotate_device().unwrap();
        agent.try_resize_display(720, 1280).unwrap();
        agent.try_set_torch(true).unwrap();
        agent.try_camera_zoom_in().unwrap();
        agent.try_camera_zoom_out().unwrap();
        agent.try_open_hard_keyboard_settings().unwrap();
        agent.try_reset_video().unwrap();

        let closed = agent.close().unwrap();
        let bytes = closed.transport.bytes;
        assert_eq!(
            control_message_tags(&bytes),
            vec![10, 5, 6, 7, 11, 21, 18, 19, 20, 15, 17]
        );
        assert_eq!(find_control_message(&bytes, 10), Some(&[10, 0][..]));
        assert_eq!(find_control_message(&bytes, 18), Some(&[18, 1][..]));
        let resize = find_control_message(&bytes, 21).expect("RESIZE_DISPLAY frame");
        assert_eq!(u16::from_be_bytes([resize[1], resize[2]]), 720);
        assert_eq!(u16::from_be_bytes([resize[3], resize[4]]), 1280);
    }

    #[test]
    fn agent_try_set_screen_size_updates_local_metadata_after_checked_dispatch() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

        agent.try_set_screen_size(720, 1280).unwrap();
        assert_eq!(agent.screen_size(), (720, 1280));
        agent.try_tap_point(AgentPoint::CENTER).unwrap();

        let closed = agent.close().unwrap();
        assert_eq!(
            first_touch_screen_size(&closed.transport.bytes),
            Some((720, 1280))
        );
        assert_eq!(first_touch_xy(&closed.transport.bytes), Some((360, 640)));
    }

    #[test]
    fn agent_try_control_preflights_command_bound_without_dispatch() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts_with_bound(session, Cursor::new(Vec::new()), 1)
            .unwrap();

        let err = agent.try_set_screen_power(false).unwrap_err();

        assert!(matches!(
            err,
            Error::SessionLifecycle(TRY_CONTROL_EXCEEDS_COMMAND_BOUND)
        ));
        let closed = agent.close().unwrap();
        assert!(closed.transport.bytes.is_empty());
    }

    #[test]
    fn agent_try_launch_app_preflights_oversized_name_without_dispatch() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

        let err = agent.try_launch_app("x".repeat(256)).unwrap_err();

        assert!(matches!(
            err,
            Error::SessionLifecycle(LAUNCH_APP_NAME_TOO_LONG)
        ));
        let closed = agent.close().unwrap();
        assert!(closed.transport.bytes.is_empty());
    }

    #[test]
    fn agent_ai_extension_helpers_emit_control_messages() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
        let flags = crate::control::AI_FLAG_FEATURES | crate::control::AI_FLAG_TEXT;

        agent.configure_ai(flags, 33, 64).unwrap();
        agent.query_ai(1234).unwrap();
        agent.pause_ai().unwrap();

        let closed = agent.close().unwrap();
        let bytes = closed.transport.bytes;
        assert_eq!(
            find_control_message(&bytes, 22).expect("AI_CONFIG frame"),
            &[22, flags, 0, 33, 0, 64]
        );
        let query = find_control_message(&bytes, 23).expect("AI_QUERY frame");
        assert_eq!(u64::from_be_bytes(query[1..9].try_into().unwrap()), 1234);
        assert_eq!(find_control_message(&bytes, 24), Some(&[24][..]));
    }

    #[test]
    fn agent_try_ai_helpers_use_nonblocking_checked_dispatch() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
        let flags = crate::control::AI_FLAG_FEATURES | crate::control::AI_FLAG_TEXT;

        agent.try_configure_ai(flags, 33, 64).unwrap();
        agent.try_query_ai(1234).unwrap();
        agent.try_pause_ai().unwrap();

        let closed = agent.close().unwrap();
        let bytes = closed.transport.bytes;
        assert_eq!(control_message_tags(&bytes), vec![22, 23, 24]);
        assert_eq!(
            find_control_message(&bytes, 22).expect("AI_CONFIG frame"),
            &[22, flags, 0, 33, 0, 64]
        );
        let query = find_control_message(&bytes, 23).expect("AI_QUERY frame");
        assert_eq!(u64::from_be_bytes(query[1..9].try_into().unwrap()), 1234);
    }

    #[test]
    fn agent_try_ai_preflights_command_bound_without_dispatch() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts_with_bound(session, Cursor::new(Vec::new()), 1)
            .unwrap();

        let err = agent.try_query_ai(1234).unwrap_err();

        assert!(matches!(
            err,
            Error::SessionLifecycle(TRY_AI_EXCEEDS_COMMAND_BOUND)
        ));
        let closed = agent.close().unwrap();
        assert!(closed.transport.bytes.is_empty());
    }

    #[test]
    fn agent_try_clipboard_and_launch_helpers_use_nonblocking_checked_dispatch() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

        agent.try_launch_app("com.example.app").unwrap();
        agent.try_set_clipboard("clip", false).unwrap();
        agent.try_set_clipboard_sequenced(7, "seq", true).unwrap();
        agent
            .try_request_clipboard_key(ClipboardCopyKey::CUT)
            .unwrap();

        let closed = agent.close().unwrap();
        let bytes = closed.transport.bytes;
        assert_eq!(control_message_tags(&bytes), vec![16, 9, 9, 8]);
        let launch = find_control_message(&bytes, 16).expect("START_APP frame");
        assert_eq!(launch[1] as usize, "com.example.app".len());
        assert_eq!(&launch[2..], b"com.example.app");

        let first_clipboard = find_control_message(&bytes, 9).expect("SET_CLIPBOARD frame");
        assert_eq!(
            u64::from_be_bytes(first_clipboard[1..9].try_into().unwrap()),
            0
        );
        assert_eq!(first_clipboard[9], 0);
        assert_eq!(
            u32::from_be_bytes(first_clipboard[10..14].try_into().unwrap()),
            4
        );
        assert_eq!(&first_clipboard[14..], b"clip");

        assert_eq!(count_control_messages(&bytes, 9), 2);
        let request = find_control_message(&bytes, 8).expect("GET_CLIPBOARD frame");
        assert_eq!(request, &[8, ClipboardCopyKey::CUT.value()]);
    }

    #[test]
    fn agent_try_clipboard_preflights_command_bound_without_dispatch() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts_with_bound(session, Cursor::new(Vec::new()), 1)
            .unwrap();

        let err = agent.try_request_clipboard(0).unwrap_err();

        assert!(matches!(
            err,
            Error::SessionLifecycle(TRY_CLIPBOARD_EXCEEDS_COMMAND_BOUND)
        ));
        let closed = agent.close().unwrap();
        assert!(closed.transport.bytes.is_empty());
    }

    #[test]
    fn query_ai_and_wait_stats_sends_query_and_skips_unrelated_events() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let mut stream = frame_summary_envelope(1);
        stream.extend(ai_stats_envelope());
        let mut agent = AgentControlSession::from_parts(session, Cursor::new(stream)).unwrap();

        let stats = agent
            .query_ai_and_wait_stats(0x0102_0304_0506_0708)
            .unwrap();

        assert_eq!(stats.frames_sampled, 10);
        let closed = agent.close().unwrap();
        let query = find_control_message(&closed.transport.bytes, 23).expect("AI_QUERY frame");
        assert_eq!(
            u64::from_be_bytes(query[1..9].try_into().unwrap()),
            0x0102_0304_0506_0708
        );
    }

    #[test]
    fn run_actions_and_query_ai_and_wait_stats_flushes_then_reads() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let mut stream = frame_summary_envelope(1);
        stream.extend(ai_stats_envelope());
        let mut agent = AgentControlSession::from_parts(session, Cursor::new(stream)).unwrap();

        let stats = agent
            .run_actions_and_query_ai_and_wait_stats(
                &[AgentAction::tap(10, 20)],
                0x1112_1314_1516_1718,
            )
            .unwrap();

        assert_eq!(stats.frames_sampled, 10);
        let closed = agent.close().unwrap();
        assert_eq!(control_message_tags(&closed.transport.bytes), [2, 2, 23]);
        let query = find_control_message(&closed.transport.bytes, 23).expect("AI_QUERY frame");
        assert_eq!(
            u64::from_be_bytes(query[1..9].try_into().unwrap()),
            0x1112_1314_1516_1718
        );
    }

    #[test]
    fn agent_mouse_helpers_emit_uhid_mouse_reports() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::mouse_only()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

        agent
            .mouse_motion_buttons(10, -5, &[MouseButton::Left])
            .unwrap();
        agent.mouse_button_state(&[MouseButton::Right]).unwrap();
        agent.mouse_scroll(0.0, 2.0).unwrap();

        let closed = agent.close().unwrap();
        let payloads = mouse_input_payloads(&closed.transport.bytes);
        assert_eq!(payloads.len(), 3);
        assert_eq!(
            payloads[0],
            [MouseButton::Left as u8, 10, (-5i8) as u8, 0, 0]
        );
        assert_eq!(payloads[1], [MouseButton::Right as u8, 0, 0, 0, 0]);
        assert_eq!(payloads[2], [0, 0, 0, 2, 0]);
    }

    #[test]
    fn agent_try_mouse_helpers_use_nonblocking_checked_dispatch() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::mouse_only()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

        agent
            .try_mouse_motion_buttons(10, -5, &[MouseButton::Left])
            .unwrap();
        agent.try_mouse_button_state(&[MouseButton::Right]).unwrap();
        agent.try_mouse_scroll(0.0, 2.0).unwrap();

        let closed = agent.close().unwrap();
        let payloads = mouse_input_payloads(&closed.transport.bytes);
        assert_eq!(payloads.len(), 3);
        assert_eq!(
            payloads[0],
            [MouseButton::Left as u8, 10, (-5i8) as u8, 0, 0]
        );
        assert_eq!(payloads[1], [MouseButton::Right as u8, 0, 0, 0, 0]);
        assert_eq!(payloads[2], [0, 0, 0, 2, 0]);
    }

    #[test]
    fn agent_try_mouse_preflights_command_bound_without_dispatch() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts_with_bound(session, Cursor::new(Vec::new()), 1)
            .unwrap();

        let err = agent
            .try_mouse_motion_buttons(10, -5, &[MouseButton::Left])
            .unwrap_err();

        assert!(matches!(
            err,
            Error::SessionLifecycle(TRY_MOUSE_EXCEEDS_COMMAND_BOUND)
        ));
        let closed = agent.close().unwrap();
        assert!(closed.transport.bytes.is_empty());
    }

    #[test]
    fn agent_mouse_actions_cover_batch_and_scroll() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::mouse_only()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
        let frames = [
            MouseFrame::motion(1, 2, MouseButton::Left as u8),
            MouseFrame::motion(-3, -4, 0),
        ];

        agent
            .run_actions(&[
                AgentAction::mouse_motion(5, 6, 0),
                AgentAction::mouse_buttons(MouseButton::Middle as u8),
                AgentAction::mouse_scroll(0, 3),
                AgentAction::try_mouse_batch(&frames).unwrap(),
            ])
            .unwrap();

        let closed = agent.close().unwrap();
        let payloads = mouse_input_payloads(&closed.transport.bytes);
        assert_eq!(payloads.len(), 5);
        assert_eq!(payloads[0], [0, 5, 6, 0, 0]);
        assert_eq!(payloads[1], [MouseButton::Middle as u8, 0, 0, 0, 0]);
        assert_eq!(payloads[2], [0, 0, 0, 3, 0]);
        assert_eq!(payloads[3], [MouseButton::Left as u8, 1, 2, 0, 0]);
        assert_eq!(payloads[4], [0, (-3i8) as u8, (-4i8) as u8, 0, 0]);
    }

    #[test]
    fn agent_mouse_batch_rejects_oversized_slices() {
        let frames = vec![MouseFrame::EMPTY; MOUSE_BATCH_FRAMES + 1];
        assert!(matches!(
            AgentAction::try_mouse_batch(&frames),
            Err(Error::SessionLifecycle("mouse batch too large"))
        ));
    }

    #[test]
    fn agent_run_actions_batches_consecutive_mouse_actions() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::mouse_only()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
        let frames = [
            MouseFrame::motion(1, 2, MouseButton::Left as u8),
            MouseFrame::motion(-3, -4, 0),
        ];

        agent
            .run_actions(&[
                AgentAction::mouse_motion(5, 6, 0),
                AgentAction::mouse_button_state(&[MouseButton::Middle]),
                AgentAction::try_mouse_batch(&frames).unwrap(),
            ])
            .unwrap();

        let closed = agent.close().unwrap();
        let payloads = mouse_input_payloads(&closed.transport.bytes);
        assert_eq!(payloads.len(), 4);
        assert_eq!(payloads[0], [0, 5, 6, 0, 0]);
        assert_eq!(payloads[1], [MouseButton::Middle as u8, 0, 0, 0, 0]);
        assert_eq!(payloads[2], [MouseButton::Left as u8, 1, 2, 0, 0]);
        assert_eq!(payloads[3], [0, (-3i8) as u8, (-4i8) as u8, 0, 0]);
    }

    #[test]
    fn agent_run_actions_flushes_mouse_before_touch_actions() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::mouse_only()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

        agent
            .run_actions(&[
                AgentAction::mouse_motion(5, 6, 0),
                AgentAction::tap(10, 20),
                AgentAction::mouse_buttons(MouseButton::Right as u8),
            ])
            .unwrap();

        let closed = agent.close().unwrap();
        assert_eq!(
            input_and_touch_tags(&closed.transport.bytes),
            vec![13, 2, 2, 13]
        );
    }

    #[test]
    fn agent_try_queue_actions_batches_mouse_with_tiny_bound() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::mouse_only()).unwrap();
        let agent = AgentControlSession::from_parts_with_bound(session, Cursor::new(Vec::new()), 1)
            .unwrap();

        agent
            .try_queue_actions(&[
                AgentAction::mouse_motion(5, 6, 0),
                AgentAction::mouse_button_state(&[MouseButton::Middle]),
            ])
            .unwrap();

        let closed = agent.close().unwrap();
        assert_eq!(mouse_input_payloads(&closed.transport.bytes).len(), 2);
    }

    #[test]
    fn agent_back_or_screen_on_helpers_emit_control_message() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

        agent.back_or_screen_on(AndroidKeyAction::UP).unwrap();
        agent
            .run_actions(&[AgentAction::back_or_screen_on(AndroidKeyAction::DOWN)])
            .unwrap();

        let closed = agent.close().unwrap();
        let bytes = closed.transport.bytes;
        assert_eq!(control_message_tags(&bytes), vec![4, 4]);
        assert_eq!(&bytes, &[4, 1, 4, 0]);
    }

    #[test]
    fn agent_try_back_or_screen_on_uses_nonblocking_checked_dispatch() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

        agent.try_back_or_screen_on(AndroidKeyAction::UP).unwrap();

        let closed = agent.close().unwrap();
        assert_eq!(closed.transport.bytes, vec![4, 1]);
    }

    #[test]
    fn agent_typed_android_keycode_helpers_emit_keycodes() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

        agent.press_android_key(AndroidKeycode::POWER).unwrap();
        agent
            .inject_android_key_event(AndroidKeyAction::UP, AndroidKeycode::ENTER, 2, 3)
            .unwrap();
        agent.release_android_key(AndroidKeycode::MENU).unwrap();
        agent
            .run_actions(&[
                AgentAction::press_android_key(AndroidKeycode::BACK),
                AgentAction::inject_android_key_event(
                    AndroidKeyAction::UP,
                    AndroidKeycode::MENU,
                    2,
                    3,
                ),
                AgentAction::release_android_key(AndroidKeycode::POWER),
            ])
            .unwrap();

        let closed = agent.close().unwrap();
        let bytes = closed.transport.bytes;
        assert_eq!(control_message_tags(&bytes), vec![0, 0, 0, 0, 0, 0]);
        assert_eq!(
            first_inject_keycodes(&bytes, 6),
            vec![26, 66, 82, 4, 82, 26]
        );
        let menu = &bytes[56..70];
        assert_eq!(menu[1], 1);
        assert_eq!(u32::from_be_bytes(menu[2..6].try_into().unwrap()), 82);
        assert_eq!(u32::from_be_bytes(menu[6..10].try_into().unwrap()), 2);
        assert_eq!(u32::from_be_bytes(menu[10..14].try_into().unwrap()), 3);
    }

    #[test]
    fn agent_try_android_key_helpers_use_nonblocking_checked_dispatch() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

        agent.try_press_android_key(AndroidKeycode::POWER).unwrap();
        agent
            .try_inject_android_key_event(AndroidKeyAction::UP, AndroidKeycode::ENTER, 2, 3)
            .unwrap();
        agent.try_release_android_key(AndroidKeycode::MENU).unwrap();
        agent
            .try_tap_android_key_with_metastate(AndroidKeycode::ENTER, 3)
            .unwrap();
        agent.try_tap_android_keycode(82, 0x40).unwrap();
        agent.try_press_home().unwrap();
        agent.try_press_back().unwrap();
        agent.try_open_recents().unwrap();
        agent.try_volume_up().unwrap();
        agent.try_volume_down().unwrap();
        agent.try_volume_mute().unwrap();

        let closed = agent.close().unwrap();
        let bytes = closed.transport.bytes;
        assert_eq!(control_message_tags(&bytes), vec![0; 13]);
        assert_eq!(
            first_inject_keycodes(&bytes, 13),
            vec![26, 66, 82, 66, 66, 82, 82, 3, 4, 187, 24, 25, 164]
        );
    }

    #[test]
    fn agent_try_android_key_preflights_command_bound_without_dispatch() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts_with_bound(session, Cursor::new(Vec::new()), 1)
            .unwrap();

        let err = agent.try_press_home().unwrap_err();

        assert!(matches!(
            err,
            Error::SessionLifecycle(TRY_ANDROID_KEY_EXCEEDS_COMMAND_BOUND)
        ));
        let closed = agent.close().unwrap();
        assert!(closed.transport.bytes.is_empty());
    }

    #[test]
    fn agent_android_key_tap_helpers_emit_down_up_keycodes() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

        agent
            .tap_android_key_with_metastate(AndroidKeycode::ENTER, 3)
            .unwrap();
        agent
            .run_actions(&[
                AgentAction::tap_android_key(AndroidKeycode::BACK),
                AgentAction::tap_android_keycode(82, 0x40),
            ])
            .unwrap();

        let closed = agent.close().unwrap();
        let bytes = closed.transport.bytes;
        assert_eq!(control_message_tags(&bytes), vec![0, 0, 0, 0, 0, 0]);
        assert_eq!(first_inject_keycodes(&bytes, 6), vec![66, 66, 4, 4, 82, 82]);

        let events: Vec<_> = bytes
            .chunks_exact(14)
            .map(|frame| {
                (
                    frame[1],
                    u32::from_be_bytes(frame[2..6].try_into().unwrap()),
                    u32::from_be_bytes(frame[10..14].try_into().unwrap()),
                )
            })
            .collect();
        assert_eq!(
            events,
            vec![
                (0, 66, 3),
                (1, 66, 3),
                (0, 4, 0),
                (1, 4, 0),
                (0, 82, 0x40),
                (1, 82, 0x40)
            ]
        );
    }

    #[test]
    fn agent_android_key_batch_action_dispatches_fixed_batch() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
        let frames = [
            AndroidKeyFrame::down(AndroidKeycode::ENTER, 3),
            AndroidKeyFrame::up(AndroidKeycode::ENTER, 3),
            AndroidKeyFrame::typed(AndroidKeyAction::UP, AndroidKeycode::MENU, 2, 4),
        ];

        agent
            .run_actions(&[AgentAction::try_android_key_batch(&frames).unwrap()])
            .unwrap();

        let closed = agent.close().unwrap();
        assert_eq!(control_message_tags(&closed.transport.bytes), vec![0, 0, 0]);
        assert_eq!(
            first_inject_keycodes(&closed.transport.bytes, 3),
            vec![66, 66, 82]
        );
    }

    #[test]
    fn agent_run_actions_batches_consecutive_android_key_actions() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
        let frames = [
            AndroidKeyFrame::down(AndroidKeycode::MENU, 0),
            AndroidKeyFrame::up(AndroidKeycode::MENU, 0),
        ];

        agent
            .run_actions(&[
                AgentAction::tap_android_key(AndroidKeycode::ENTER),
                AgentAction::PressBack,
                AgentAction::try_android_key_batch(&frames).unwrap(),
            ])
            .unwrap();

        let closed = agent.close().unwrap();
        let bytes = closed.transport.bytes;
        assert_eq!(control_message_tags(&bytes), vec![0, 0, 0, 0, 0]);
        assert_eq!(first_inject_keycodes(&bytes, 5), vec![66, 66, 4, 82, 82]);
    }

    #[test]
    fn agent_run_actions_flushes_android_keys_before_touch_actions() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

        agent
            .run_actions(&[
                AgentAction::tap_android_key(AndroidKeycode::ENTER),
                AgentAction::tap(10, 20),
                AgentAction::tap_android_key(AndroidKeycode::BACK),
            ])
            .unwrap();

        let closed = agent.close().unwrap();
        assert_eq!(
            control_message_tags(&closed.transport.bytes),
            vec![0, 0, 2, 2, 0, 0]
        );
    }

    #[test]
    fn agent_try_queue_actions_batches_android_keys_with_tiny_bound() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts_with_bound(session, Cursor::new(Vec::new()), 1)
            .unwrap();

        agent
            .try_queue_actions(&[
                AgentAction::tap_android_key(AndroidKeycode::ENTER),
                AgentAction::PressBack,
                AgentAction::tap_android_keycode(82, 0x40),
            ])
            .unwrap();

        let closed = agent.close().unwrap();
        assert_eq!(control_message_tags(&closed.transport.bytes), vec![0; 5]);
        assert_eq!(
            first_inject_keycodes(&closed.transport.bytes, 5),
            vec![66, 66, 4, 82, 82]
        );
    }

    #[test]
    fn agent_android_key_batch_constructor_rejects_oversized_slices() {
        let frames = vec![AndroidKeyFrame::EMPTY; ANDROID_KEY_BATCH_FRAMES + 1];
        assert!(matches!(
            AgentAction::try_android_key_batch(&frames),
            Err(Error::SessionLifecycle("android key batch too large"))
        ));
    }

    #[test]
    fn agent_screen_size_affects_touch_frames() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

        agent.set_screen_size(1440, 3120).unwrap();
        agent.tap(10, 20).unwrap();

        let closed = agent.close().unwrap();
        assert_eq!(
            first_touch_screen_size(&closed.transport.bytes),
            Some((1440, 3120))
        );
    }

    #[test]
    fn agent_composite_gesture_helpers_use_batched_touch_frames() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

        agent.set_screen_size(1440, 3120).unwrap();
        assert_eq!(agent.screen_size(), (1440, 3120));
        agent.double_tap(10, 20).unwrap();
        agent.long_press(30, 40, Duration::from_millis(0)).unwrap();
        agent.three_finger_screenshot().unwrap();

        let closed = agent.close().unwrap();
        let bytes = closed.transport.bytes;
        assert_eq!(count_touch_events(&bytes), 4 + 2 + 36);
        assert_eq!(first_touch_screen_size(&bytes), Some((1440, 3120)));
        assert!(
            contains_touch_point(&bytes, 0, 360, 780),
            "three-finger path should use agent-local 1440x3120 dimensions"
        );
    }

    #[test]
    fn agent_cancel_touch_helpers_emit_action_cancel() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

        agent.cancel_touch(5).unwrap();
        agent.run_actions(&[AgentAction::cancel_touch(6)]).unwrap();

        let closed = agent.close().unwrap();
        let bytes = closed.transport.bytes;
        assert_eq!(control_message_tags(&bytes), vec![2, 2]);
        assert_eq!(bytes[1], 3);
        assert_eq!(u64::from_be_bytes(bytes[2..10].try_into().unwrap()), 5);
        assert_eq!(bytes[33], 3);
        assert_eq!(u64::from_be_bytes(bytes[34..42].try_into().unwrap()), 6);
    }

    #[test]
    fn agent_typed_touch_pointer_helpers_preserve_scrcpy_reserved_ids() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
        let pointer = TouchPointerId::VIRTUAL_FINGER;
        let custom = [
            AgentTouchFrame::down_pointer(pointer, 30, 40, u16::MAX),
            AgentTouchFrame::move_pointer_to(pointer, 35, 45, 32768),
            AgentTouchFrame::up_pointer(pointer, 35, 45),
        ];

        agent.set_screen_size(1000, 2000).unwrap();
        agent.tap_pointer(pointer, 10, 20).unwrap();
        agent
            .tap_point_pointer(pointer, AgentPoint::CENTER)
            .unwrap();
        agent
            .run_actions(&[
                AgentAction::try_touch_frames(&custom).unwrap(),
                AgentAction::cancel_touch_pointer(pointer),
            ])
            .unwrap();

        let closed = agent.close().unwrap();
        let events = touch_events(&closed.transport.bytes);
        assert_eq!(events.len(), 8);
        assert!(events
            .iter()
            .all(|(_, pointer_id, _, _)| *pointer_id == pointer.value()));
        assert_eq!(
            events[0],
            (TouchAction::DOWN.value(), pointer.value(), 10, 20)
        );
        assert_eq!(
            events[1],
            (TouchAction::UP.value(), pointer.value(), 10, 20)
        );
        assert_eq!(
            events[2],
            (TouchAction::DOWN.value(), pointer.value(), 500, 1000)
        );
        assert_eq!(events[7].0, TouchAction::CANCEL.value());
    }

    #[test]
    fn agent_touch_frame_batch_action_batches_with_adjacent_touch_actions() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
        let custom = [
            AgentTouchFrame::down(2, 100, 200, u16::MAX),
            AgentTouchFrame::move_to(2, 110, 210, 32768),
            AgentTouchFrame::up(2, 110, 210),
        ];

        agent
            .run_actions(&[
                AgentAction::tap(10, 20),
                AgentAction::try_touch_frames(&custom).unwrap(),
                AgentAction::cancel_touch(2),
            ])
            .unwrap();

        let closed = agent.close().unwrap();
        let bytes = closed.transport.bytes;
        assert_eq!(control_message_tags(&bytes), vec![2; 6]);

        let custom_down = &bytes[64..96];
        assert_eq!(custom_down[1], TouchAction::DOWN.value());
        assert_eq!(
            u64::from_be_bytes(custom_down[2..10].try_into().unwrap()),
            2
        );
        assert_eq!(
            i32::from_be_bytes(custom_down[10..14].try_into().unwrap()),
            100
        );
        assert_eq!(
            i32::from_be_bytes(custom_down[14..18].try_into().unwrap()),
            200
        );
        assert_eq!(
            u16::from_be_bytes(custom_down[22..24].try_into().unwrap()),
            u16::MAX
        );

        let custom_move = &bytes[96..128];
        assert_eq!(custom_move[1], TouchAction::MOVE.value());
        assert_eq!(
            u16::from_be_bytes(custom_move[22..24].try_into().unwrap()),
            32768
        );

        let custom_up = &bytes[128..160];
        assert_eq!(custom_up[1], TouchAction::UP.value());
        assert_eq!(u16::from_be_bytes(custom_up[22..24].try_into().unwrap()), 0);

        let custom_cancel = &bytes[160..192];
        assert_eq!(custom_cancel[1], TouchAction::CANCEL.value());
    }

    #[test]
    fn agent_touch_frame_batch_rejects_oversized_or_malformed_batches() {
        let frame = AgentTouchFrame::move_to(0, 1, 2, 32768);
        let frames = vec![frame; TOUCH_BATCH_FRAMES + 1];
        assert!(matches!(
            AgentAction::try_touch_frames(&frames),
            Err(Error::SessionLifecycle("touch frame batch too large"))
        ));

        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
        let malformed = AgentAction::touch_frames_fixed(
            TOUCH_BATCH_FRAMES + 1,
            [AgentTouchFrame::EMPTY; TOUCH_BATCH_FRAMES],
        );
        let err = agent.run_actions(&[malformed]).unwrap_err();
        assert!(matches!(
            err,
            Error::SessionLifecycle("touch frame batch length overflow")
        ));

        let _closed = agent.close().unwrap();
    }

    #[test]
    fn agent_run_actions_executes_plan_with_one_checked_boundary() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::kbd_only()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

        agent
            .run_actions(&[
                AgentAction::SetScreenSize {
                    width: 1440,
                    height: 3120,
                },
                AgentAction::tap(10, 20),
                AgentAction::swipe((0, 0), (30, 60), 3),
                AgentAction::type_text("hi"),
                AgentAction::launch_app("com.android.settings"),
                AgentAction::PressBack,
                AgentAction::SetScreenPower { on: false },
            ])
            .unwrap();

        assert_eq!(agent.screen_size(), (1440, 3120));
        let closed = agent.close().unwrap();
        let bytes = closed.transport.bytes;
        assert_eq!(count_touch_events(&bytes), 7);
        assert_eq!(first_touch_screen_size(&bytes), Some((1440, 3120)));
        assert!(contains_inject_keycode(&bytes, 4));
        assert!(
            count_control_messages(&bytes, 13) >= 4,
            "type_text should emit keyboard UHID_INPUT reports"
        );
        assert!(
            find_control_message(&bytes, 16).is_some(),
            "StartApp tag should be present"
        );
        assert!(
            find_control_message(&bytes, 10).is_some(),
            "SetDisplayPower tag should be present"
        );
    }

    #[test]
    fn agent_run_actions_batches_consecutive_touch_actions() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

        agent
            .run_actions(&[
                AgentAction::tap(10, 20),
                AgentAction::swipe((0, 0), (30, 60), 3),
                AgentAction::DoubleTap { x: 40, y: 50 },
            ])
            .unwrap();

        let closed = agent.close().unwrap();
        let bytes = closed.transport.bytes;
        assert_eq!(count_control_messages(&bytes, 2), 2 + 5 + 4);
        assert_eq!(control_message_tags(&bytes), vec![2; 11]);
    }

    #[test]
    fn agent_run_actions_flushes_touch_before_non_touch_actions() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

        agent
            .run_actions(&[
                AgentAction::tap(10, 20),
                AgentAction::PressBack,
                AgentAction::tap(30, 40),
            ])
            .unwrap();

        let closed = agent.close().unwrap();
        assert_eq!(
            control_message_tags(&closed.transport.bytes),
            vec![2, 2, 0, 2, 2]
        );
    }

    #[test]
    fn agent_try_queue_actions_enqueues_without_checked_wait() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

        agent
            .try_queue_actions(&[
                AgentAction::SetScreenSize {
                    width: 1440,
                    height: 3120,
                },
                AgentAction::tap(10, 20),
                AgentAction::PressBack,
                AgentAction::tap(30, 40),
                AgentAction::SetScreenPower { on: true },
            ])
            .unwrap();

        assert_eq!(agent.screen_size(), (1440, 3120));
        let closed = agent.close().unwrap();
        assert_eq!(
            control_message_tags(&closed.transport.bytes),
            vec![2, 2, 0, 2, 2, 10]
        );
        assert_eq!(
            first_touch_screen_size(&closed.transport.bytes),
            Some((1440, 3120))
        );
    }

    #[test]
    fn agent_action_preflight_classifies_try_queueable_plans() {
        let rect = AgentRect::FULL_SCREEN;
        let ready = [
            AgentAction::tap(10, 20),
            AgentAction::swipe((10, 20), (30, 40), 2),
            AgentAction::scroll_rect(rect, 0, -16),
            AgentAction::Flush,
        ];
        let mixed = [
            AgentAction::tap(10, 20),
            AgentAction::wait(Duration::from_millis(1)),
            AgentAction::PressBack,
        ];

        assert!(AgentAction::all_try_queueable(&ready));
        assert_eq!(AgentAction::first_non_try_queueable(&ready), None);
        assert_eq!(AgentAction::try_queueable_prefix_len(&ready), ready.len());
        assert_eq!(AgentAction::first_blocking_timing(&ready), None);
        assert_eq!(AgentAction::blocking_timing_prefix_len(&ready), ready.len());
        assert!(!AgentAction::all_try_queueable(&mixed));
        assert_eq!(AgentAction::first_non_try_queueable(&mixed), Some(1));
        assert_eq!(AgentAction::try_queueable_prefix_len(&mixed), 1);
        assert_eq!(AgentAction::first_blocking_timing(&mixed), Some(1));
        assert_eq!(AgentAction::blocking_timing_prefix_len(&mixed), 1);
        assert_eq!(
            AgentAction::first_try_queue_error(&mixed),
            Some((1, TIMED_ACTION_REQUIRES_BLOCKING))
        );
        assert!(matches!(
            AgentAction::validate_try_queue_plan(&mixed),
            Err(Error::SessionLifecycle(TIMED_ACTION_REQUIRES_BLOCKING))
        ));
        assert!(mixed[1].requires_blocking_timing());
        assert!(!mixed[1].can_try_queue());
        assert!(
            AgentAction::long_press_rect(rect, Duration::from_millis(1)).requires_blocking_timing()
        );
    }

    #[test]
    fn agent_action_preflight_classifies_structural_plan_errors() {
        let ready = AgentAction::tap_rect_at(AgentRect::FULL_SCREEN, 10_000, 0);
        assert_eq!(ready.structural_error(), None);
        ready.validate_structure().unwrap();

        let malformed_strict_text = AgentAction::type_text_strict("ok中");
        assert_eq!(
            malformed_strict_text.structural_error(),
            Some(STRICT_TEXT_UNSUPPORTED)
        );
        assert!(matches!(
            malformed_strict_text.validate_structure(),
            Err(Error::SessionLifecycle(STRICT_TEXT_UNSUPPORTED))
        ));

        let oversized_app_name = AgentAction::launch_app("a".repeat(256));
        assert_eq!(
            oversized_app_name.structural_error(),
            Some(LAUNCH_APP_NAME_TOO_LONG)
        );
        assert!(matches!(
            oversized_app_name.validate_structure(),
            Err(Error::SessionLifecycle(LAUNCH_APP_NAME_TOO_LONG))
        ));

        let malformed_anchor = AgentAction::tap_rect_at(AgentRect::FULL_SCREEN, 10_001, 0);
        assert_eq!(
            malformed_anchor.structural_error(),
            Some("normalized point out of range")
        );
        assert!(matches!(
            malformed_anchor.validate_structure(),
            Err(Error::SessionLifecycle("normalized point out of range"))
        ));

        let malformed_chord = AgentAction::keyboard_chord_fixed(KeyboardChordFrame::new(
            (KEYBOARD_CHORD_KEYS + 1) as u8,
            [Scancode::A.to_u8(); KEYBOARD_CHORD_KEYS],
            Modifiers::LCTRL,
        ));
        assert_eq!(
            malformed_chord.structural_error(),
            Some("keyboard chord length overflow")
        );

        let malformed_batch = AgentAction::touch_frames_fixed(
            TOUCH_BATCH_FRAMES + 1,
            [AgentTouchFrame::EMPTY; TOUCH_BATCH_FRAMES],
        );
        let actions = [AgentAction::tap(10, 20), malformed_batch];
        assert_eq!(
            AgentAction::first_structural_error(&actions),
            Some((1, "touch frame batch length overflow"))
        );
        assert_eq!(AgentAction::first_non_try_queueable(&actions), Some(1));
        assert_eq!(AgentAction::try_queueable_prefix_len(&actions), 1);
        assert_eq!(AgentAction::first_blocking_timing(&actions), None);
        assert_eq!(
            AgentAction::blocking_timing_prefix_len(&actions),
            actions.len()
        );
        assert!(matches!(
            AgentAction::validate_plan_structure(&actions),
            Err(Error::SessionLifecycle("touch frame batch length overflow"))
        ));
        assert!(matches!(
            AgentAction::validate_try_queue_plan(&actions),
            Err(Error::SessionLifecycle("touch frame batch length overflow"))
        ));

        let strict_text_actions = [
            AgentAction::tap(10, 20),
            AgentAction::type_text_strict("a中"),
        ];
        assert_eq!(
            AgentAction::first_structural_error(&strict_text_actions),
            Some((1, STRICT_TEXT_UNSUPPORTED))
        );
        assert!(matches!(
            AgentAction::validate_plan_structure(&strict_text_actions),
            Err(Error::SessionLifecycle(STRICT_TEXT_UNSUPPORTED))
        ));

        let launch_app_actions = [
            AgentAction::tap(10, 20),
            AgentAction::launch_app("a".repeat(256)),
        ];
        assert_eq!(
            AgentAction::first_structural_error(&launch_app_actions),
            Some((1, LAUNCH_APP_NAME_TOO_LONG))
        );
        assert!(matches!(
            AgentAction::validate_plan_structure(&launch_app_actions),
            Err(Error::SessionLifecycle(LAUNCH_APP_NAME_TOO_LONG))
        ));
    }

    #[test]
    fn agent_action_try_queue_preflight_reports_first_error_in_plan_order() {
        let malformed = AgentAction::key_batch_fixed(
            KEYBOARD_BATCH_FRAMES + 1,
            [KeyboardFrame::EMPTY; KEYBOARD_BATCH_FRAMES],
        );
        let wait = AgentAction::wait(Duration::from_millis(0));

        let malformed_first = [AgentAction::tap(10, 20), malformed.clone(), wait.clone()];
        assert_eq!(
            AgentAction::first_try_queue_error(&malformed_first),
            Some((1, "keyboard batch length overflow"))
        );
        assert_eq!(
            AgentAction::first_blocking_timing(&malformed_first),
            Some(2)
        );
        assert_eq!(AgentAction::blocking_timing_prefix_len(&malformed_first), 2);
        assert!(matches!(
            AgentAction::validate_try_queue_plan(&malformed_first),
            Err(Error::SessionLifecycle("keyboard batch length overflow"))
        ));

        let timed_first = [AgentAction::tap(10, 20), wait, malformed];
        assert_eq!(
            AgentAction::first_try_queue_error(&timed_first),
            Some((1, TIMED_ACTION_REQUIRES_BLOCKING))
        );
        assert_eq!(AgentAction::first_blocking_timing(&timed_first), Some(1));
        assert_eq!(AgentAction::blocking_timing_prefix_len(&timed_first), 1);
        assert!(matches!(
            AgentAction::validate_try_queue_plan(&timed_first),
            Err(Error::SessionLifecycle(TIMED_ACTION_REQUIRES_BLOCKING))
        ));
    }

    #[test]
    fn agent_action_plan_summary_reports_boundaries_and_dispatch_pressure() {
        let rect = AgentRect::FULL_SCREEN;
        let ready = [
            AgentAction::tap(10, 20),
            AgentAction::swipe((10, 20), (30, 40), 2),
            AgentAction::scroll_rect(rect, 0, -16),
            AgentAction::Flush,
        ];
        let summary = AgentAction::plan_summary(&ready);

        assert_eq!(summary.action_count, ready.len());
        assert!(summary.is_structurally_valid());
        assert!(summary.all_try_queueable());
        assert!(!summary.has_blocking_timing());
        assert_eq!(summary.first_structural_error, None);
        assert_eq!(summary.first_try_queue_error, None);
        assert_eq!(summary.first_blocking_timing, None);
        assert_eq!(summary.try_queueable_prefix_len, ready.len());
        assert_eq!(summary.blocking_timing_prefix_len, ready.len());
        assert_eq!(summary.estimated_queue_dispatch_commands, 3);
        assert_eq!(summary.estimated_run_dispatch_commands, 4);
        assert_eq!(summary.estimated_try_queue_dispatch_commands, 3);
        assert_eq!(summary.estimated_try_run_dispatch_commands, 4);
        assert_eq!(summary.estimated_try_queue_prefix_dispatch_commands, 3);
        assert_eq!(summary.estimated_try_run_prefix_dispatch_commands, 4);
        assert_eq!(summary.first_try_queue_prefix_error, None);
        assert!(summary.can_try_queue_prefix());
        assert!(!summary.has_blocking_suffix());
        assert_eq!(summary.blocking_suffix_len(), 0);
        assert!(summary.queue_dispatch_fits_bound(3));
        assert!(!summary.queue_dispatch_fits_bound(2));
        assert!(summary.run_dispatch_fits_bound(4));
        assert!(!summary.run_dispatch_fits_bound(3));
        assert!(summary.try_queue_dispatch_fits_bound(3));
        assert!(!summary.try_queue_dispatch_fits_bound(2));
        assert!(summary.try_run_dispatch_fits_bound(4));
        assert!(!summary.try_run_dispatch_fits_bound(3));
        assert!(summary.try_queue_prefix_dispatch_fits_bound(3));
        assert!(!summary.try_queue_prefix_dispatch_fits_bound(2));
        assert!(summary.try_run_prefix_dispatch_fits_bound(4));
        assert!(!summary.try_run_prefix_dispatch_fits_bound(3));

        let touch_heavy = [
            AgentAction::touch_frames_fixed(
                TOUCH_BATCH_FRAMES,
                [AgentTouchFrame::EMPTY; TOUCH_BATCH_FRAMES],
            ),
            AgentAction::tap(10, 20),
        ];
        let summary = AgentPlanSummary::analyze(&touch_heavy);
        assert_eq!(summary.estimated_queue_dispatch_commands, 2);
        assert_eq!(summary.estimated_run_dispatch_commands, 3);
        assert_eq!(summary.estimated_try_run_dispatch_commands, 3);
        assert_eq!(summary.estimated_try_run_prefix_dispatch_commands, 3);
        assert!(summary.try_queue_dispatch_fits_bound(2));
        assert!(!summary.try_queue_dispatch_fits_bound(1));
        assert!(summary.try_run_dispatch_fits_bound(3));
        assert!(!summary.try_run_dispatch_fits_bound(2));
        assert!(summary.try_run_prefix_dispatch_fits_bound(3));
        assert!(!summary.try_run_prefix_dispatch_fits_bound(2));
    }

    #[test]
    fn agent_action_plan_summary_reports_blocking_prefix_pressure() {
        let malformed = AgentAction::key_batch_fixed(
            KEYBOARD_BATCH_FRAMES + 1,
            [KeyboardFrame::EMPTY; KEYBOARD_BATCH_FRAMES],
        );
        let actions = [
            AgentAction::tap(10, 20),
            AgentAction::wait(Duration::from_millis(0)),
            malformed,
        ];
        let summary = AgentAction::plan_summary(&actions);

        assert!(!summary.is_structurally_valid());
        assert!(!summary.all_try_queueable());
        assert!(summary.has_blocking_timing());
        assert_eq!(
            summary.first_structural_error,
            Some((2, "keyboard batch length overflow"))
        );
        assert_eq!(
            summary.first_try_queue_error,
            Some((1, TIMED_ACTION_REQUIRES_BLOCKING))
        );
        assert_eq!(summary.first_try_queue_prefix_error, None);
        assert_eq!(summary.first_blocking_timing, Some(1));
        assert_eq!(summary.try_queueable_prefix_len, 1);
        assert_eq!(summary.blocking_timing_prefix_len, 1);
        assert!(summary.can_try_queue_prefix());
        assert!(summary.has_blocking_suffix());
        assert_eq!(summary.blocking_suffix_len(), 2);
        assert_eq!(summary.estimated_queue_dispatch_commands, 0);
        assert_eq!(summary.estimated_run_dispatch_commands, 0);
        assert_eq!(summary.estimated_try_queue_dispatch_commands, 0);
        assert_eq!(summary.estimated_try_run_dispatch_commands, 0);
        assert_eq!(summary.estimated_try_queue_prefix_dispatch_commands, 1);
        assert_eq!(summary.estimated_try_run_prefix_dispatch_commands, 2);
        assert!(!summary.queue_dispatch_fits_bound(usize::MAX));
        assert!(!summary.run_dispatch_fits_bound(usize::MAX));
        assert!(!summary.try_queue_dispatch_fits_bound(usize::MAX));
        assert!(!summary.try_run_dispatch_fits_bound(usize::MAX));
        assert!(summary.try_queue_prefix_dispatch_fits_bound(1));
        assert!(!summary.try_queue_prefix_dispatch_fits_bound(0));
        assert!(summary.try_run_prefix_dispatch_fits_bound(2));
        assert!(!summary.try_run_prefix_dispatch_fits_bound(1));

        let blocking_first = AgentAction::plan_summary(&[
            AgentAction::wait(Duration::from_millis(0)),
            AgentAction::tap(10, 20),
        ]);
        assert_eq!(blocking_first.blocking_timing_prefix_len, 0);
        assert_eq!(
            blocking_first.estimated_try_queue_prefix_dispatch_commands,
            0
        );
        assert_eq!(blocking_first.estimated_try_run_prefix_dispatch_commands, 1);
        assert!(blocking_first.try_queue_prefix_dispatch_fits_bound(0));
        assert!(blocking_first.try_run_prefix_dispatch_fits_bound(1));
        assert!(!blocking_first.try_run_prefix_dispatch_fits_bound(0));
    }

    #[test]
    fn agent_action_plan_summary_zeroes_invalid_prefix_dispatch() {
        let malformed = AgentAction::touch_frames_fixed(
            TOUCH_BATCH_FRAMES + 1,
            [AgentTouchFrame::EMPTY; TOUCH_BATCH_FRAMES],
        );
        let actions = [AgentAction::tap(10, 20), malformed];
        let summary = AgentAction::plan_summary(&actions);

        assert_eq!(
            summary.first_structural_error,
            Some((1, "touch frame batch length overflow"))
        );
        assert_eq!(
            summary.first_try_queue_error,
            Some((1, "touch frame batch length overflow"))
        );
        assert_eq!(
            summary.first_try_queue_prefix_error,
            Some((1, "touch frame batch length overflow"))
        );
        assert_eq!(summary.first_blocking_timing, None);
        assert_eq!(summary.try_queueable_prefix_len, 1);
        assert_eq!(summary.blocking_timing_prefix_len, actions.len());
        assert!(!summary.can_try_queue_prefix());
        assert!(!summary.has_blocking_suffix());
        assert_eq!(summary.blocking_suffix_len(), 0);
        assert_eq!(summary.estimated_queue_dispatch_commands, 0);
        assert_eq!(summary.estimated_run_dispatch_commands, 0);
        assert_eq!(summary.estimated_try_queue_dispatch_commands, 0);
        assert_eq!(summary.estimated_try_run_dispatch_commands, 0);
        assert_eq!(summary.estimated_try_queue_prefix_dispatch_commands, 0);
        assert_eq!(summary.estimated_try_run_prefix_dispatch_commands, 0);
        assert!(!summary.try_queue_prefix_dispatch_fits_bound(usize::MAX));
        assert!(!summary.try_run_prefix_dispatch_fits_bound(usize::MAX));
        assert!(!summary.try_run_dispatch_fits_bound(usize::MAX));
    }

    #[test]
    fn agent_action_plan_summary_counts_gamepad_mode_switch_flushes() {
        let frame = GamepadFrameRaw::new(1, 2, 3, 4, 5, 6, 7);
        let mut frames = [GamepadFrameRaw::new(0, 0, 0, 0, 0, 0, 0); GAMEPAD_BATCH_FRAMES];
        frames[0] = frame;
        frames[1] = frame;
        let actions = [
            AgentAction::gamepad_frame(frame),
            AgentAction::gamepad_frame_unchecked(frame),
            AgentAction::gamepad_packed_frame(frame.pack()),
            AgentAction::gamepad_packed_frame_batch_fixed(
                2,
                [[0u8; GAMEPAD_FRAME_BYTES]; GAMEPAD_BATCH_FRAMES],
            ),
            AgentAction::gamepad_frame_batch_fixed(2, frames),
        ];
        let summary = AgentAction::plan_summary(&actions);

        assert!(summary.is_structurally_valid());
        assert_eq!(summary.estimated_queue_dispatch_commands, 4);
        assert_eq!(summary.estimated_run_dispatch_commands, 5);
        assert_eq!(summary.estimated_try_queue_dispatch_commands, 4);
        assert_eq!(summary.estimated_try_run_dispatch_commands, 5);
        assert_eq!(summary.estimated_try_queue_prefix_dispatch_commands, 4);
        assert_eq!(summary.estimated_try_run_prefix_dispatch_commands, 5);
        assert!(summary.try_queue_dispatch_fits_bound(4));
        assert!(!summary.try_queue_dispatch_fits_bound(3));
        assert!(summary.try_run_dispatch_fits_bound(5));
        assert!(!summary.try_run_dispatch_fits_bound(4));
        assert!(summary.try_run_prefix_dispatch_fits_bound(5));
        assert!(!summary.try_run_prefix_dispatch_fits_bound(4));
    }

    #[test]
    fn agent_action_bounded_try_queue_prefix_splits_by_command_bound() {
        let actions = [
            AgentAction::tap(10, 20),
            AgentAction::Flush,
            AgentAction::tap(30, 40),
        ];

        let prefix = AgentAction::bounded_try_queue_prefix(&actions, 2);
        assert_eq!(prefix.action_count, actions.len());
        assert_eq!(prefix.accepted_actions, 2);
        assert_eq!(prefix.estimated_dispatch_commands, 2);
        assert_eq!(prefix.command_bound, 2);
        assert!(!prefix.is_full_plan());
        assert!(!prefix.is_empty());
        assert_eq!(prefix.remaining_actions(), 1);
        assert!(prefix.accepted_dispatch_fits_bound());
        assert_eq!(prefix.accepted_range(), 0..2);
        assert_eq!(prefix.remaining_range(), 2..3);
        assert_eq!(prefix.accepted_slice(&actions), Some(&actions[..2]));
        assert_eq!(prefix.remaining_slice(&actions), Some(&actions[2..]));
        assert_eq!(
            prefix.split_slice(&actions),
            Some((&actions[..2], &actions[2..]))
        );
        assert_eq!(prefix.accepted_slice(&actions[..2]), None);
        assert_eq!(
            prefix.stop,
            AgentPlanBoundedPrefixStop::CommandBound {
                index: 2,
                required_dispatch_commands: 3,
            }
        );
        assert!(prefix.stop.is_command_bound());
        assert!(!prefix.stop.is_end_of_plan());
        assert_eq!(prefix.stop.index(), Some(2));
        assert_eq!(prefix.stop.required_dispatch_commands(), Some(3));
        assert_eq!(prefix.stop.error(), None);

        let full = AgentAction::bounded_try_queue_prefix(&actions, 3);
        assert_eq!(full.accepted_actions, actions.len());
        assert_eq!(full.estimated_dispatch_commands, 3);
        assert_eq!(full.stop, AgentPlanBoundedPrefixStop::EndOfPlan);
        assert!(full.is_full_plan());
        assert!(full.stop.is_end_of_plan());
        assert_eq!(full.stop.index(), None);
        assert_eq!(full.stop.required_dispatch_commands(), None);
        assert_eq!(full.remaining_actions(), 0);
        assert_eq!(full.accepted_range(), 0..3);
        assert_eq!(full.remaining_range(), 3..3);
        assert_eq!(
            full.split_slice(&actions),
            Some((&actions[..], &actions[3..]))
        );
    }

    #[test]
    fn agent_action_bounded_try_queue_prefix_preserves_batching_pressure() {
        let actions = [
            AgentAction::tap(10, 20),
            AgentAction::tap(30, 40),
            AgentAction::tap(50, 60),
        ];

        let prefix = AgentAction::bounded_try_queue_prefix(&actions, 1);
        assert_eq!(prefix.accepted_actions, actions.len());
        assert_eq!(prefix.estimated_dispatch_commands, 1);
        assert_eq!(prefix.stop, AgentPlanBoundedPrefixStop::EndOfPlan);
    }

    #[test]
    fn agent_action_bounded_try_queue_prefix_stops_at_static_rejection() {
        let malformed = AgentAction::touch_frames_fixed(
            TOUCH_BATCH_FRAMES + 1,
            [AgentTouchFrame::EMPTY; TOUCH_BATCH_FRAMES],
        );
        let actions = [AgentAction::tap(10, 20), malformed, AgentAction::Flush];
        let prefix = AgentAction::bounded_try_queue_prefix(&actions, usize::MAX);

        assert_eq!(prefix.accepted_actions, 1);
        assert_eq!(prefix.estimated_dispatch_commands, 1);
        assert_eq!(
            prefix.stop,
            AgentPlanBoundedPrefixStop::TryQueueError {
                index: 1,
                error: "touch frame batch length overflow",
            }
        );
        assert!(prefix.stop.is_try_queue_error());
        assert_eq!(prefix.stop.index(), Some(1));
        assert_eq!(
            prefix.stop.error(),
            Some("touch frame batch length overflow")
        );
        assert_eq!(prefix.stop.required_dispatch_commands(), None);
        assert_eq!(prefix.remaining_actions(), 2);
    }

    #[test]
    fn agent_action_bounded_try_queue_prefix_stops_at_blocking_timing() {
        let actions = [
            AgentAction::tap(10, 20),
            AgentAction::wait(Duration::from_millis(0)),
            AgentAction::tap(30, 40),
        ];
        let prefix = AgentAction::bounded_try_queue_prefix(&actions, usize::MAX);

        assert_eq!(prefix.accepted_actions, 1);
        assert_eq!(prefix.estimated_dispatch_commands, 1);
        assert_eq!(
            prefix.stop,
            AgentPlanBoundedPrefixStop::BlockingTiming { index: 1 }
        );
        assert!(prefix.stop.is_blocking_timing());
        assert_eq!(prefix.stop.index(), Some(1));
        assert_eq!(prefix.stop.error(), None);
        assert_eq!(prefix.stop.required_dispatch_commands(), None);
        assert_eq!(prefix.remaining_actions(), 2);
    }

    #[test]
    fn agent_action_bounded_try_queue_prefix_allows_zero_command_actions() {
        let actions = [
            AgentAction::touch_frames_fixed(0, [AgentTouchFrame::EMPTY; TOUCH_BATCH_FRAMES]),
            AgentAction::Flush,
        ];
        let prefix = AgentAction::bounded_try_queue_prefix(&actions, 0);

        assert_eq!(prefix.accepted_actions, 1);
        assert_eq!(prefix.estimated_dispatch_commands, 0);
        assert_eq!(
            prefix.stop,
            AgentPlanBoundedPrefixStop::CommandBound {
                index: 1,
                required_dispatch_commands: 1,
            }
        );
        assert!(prefix.accepted_dispatch_fits_bound());
    }

    #[test]
    fn agent_action_bounded_try_run_prefix_reserves_checked_barrier() {
        let actions = [
            AgentAction::tap(10, 20),
            AgentAction::Flush,
            AgentAction::tap(30, 40),
        ];

        let prefix = AgentAction::bounded_try_run_prefix(&actions, 3);
        assert_eq!(prefix.command_bound, 3);
        assert_eq!(prefix.accepted_actions, 2);
        assert_eq!(prefix.estimated_dispatch_commands, 2);
        assert_eq!(prefix.estimated_checked_dispatch_commands(), 3);
        assert!(prefix.checked_dispatch_fits_bound());
        assert_eq!(
            prefix.stop,
            AgentPlanBoundedPrefixStop::CommandBound {
                index: 2,
                required_dispatch_commands: 4,
            }
        );

        let full = AgentAction::bounded_try_run_prefix(&actions, 4);
        assert_eq!(full.command_bound, 4);
        assert_eq!(full.accepted_actions, actions.len());
        assert_eq!(full.estimated_dispatch_commands, 3);
        assert_eq!(full.estimated_checked_dispatch_commands(), 4);
        assert_eq!(full.stop, AgentPlanBoundedPrefixStop::EndOfPlan);
        assert!(full.checked_dispatch_fits_bound());

        let no_barrier = AgentAction::bounded_try_run_prefix(&actions, 0);
        assert_eq!(no_barrier.command_bound, 0);
        assert_eq!(no_barrier.accepted_actions, 0);
        assert_eq!(no_barrier.estimated_checked_dispatch_commands(), 1);
        assert!(!no_barrier.checked_dispatch_fits_bound());
        assert_eq!(
            no_barrier.stop,
            AgentPlanBoundedPrefixStop::CommandBound {
                index: 0,
                required_dispatch_commands: 1,
            }
        );
    }

    #[test]
    fn agent_try_queue_actions_bounded_prefix_dispatches_command_bound_prefix() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
        let actions = [
            AgentAction::tap(10, 20),
            AgentAction::Flush,
            AgentAction::tap(30, 40),
        ];

        assert_eq!(agent.command_bound(), DEFAULT_AGENT_COMMAND_BOUND);
        let prefix = agent.try_queue_actions_bounded_prefix(&actions, 2).unwrap();

        assert_eq!(prefix.accepted_actions, 2);
        assert_eq!(prefix.estimated_dispatch_commands, 2);
        assert_eq!(
            prefix.stop,
            AgentPlanBoundedPrefixStop::CommandBound {
                index: 2,
                required_dispatch_commands: 3,
            }
        );
        let closed = agent.close().unwrap();
        assert_eq!(control_message_tags(&closed.transport.bytes), vec![2, 2]);

        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts_with_bound(session, Cursor::new(Vec::new()), 2)
            .unwrap();
        assert_eq!(agent.command_bound(), 2);

        let prefix = agent
            .try_queue_actions_bounded_prefix_with_session_bound(&actions)
            .unwrap();

        assert_eq!(prefix.command_bound, 2);
        assert_eq!(prefix.accepted_actions, 2);
        assert_eq!(prefix.estimated_dispatch_commands, 2);
        assert_eq!(
            prefix.stop,
            AgentPlanBoundedPrefixStop::CommandBound {
                index: 2,
                required_dispatch_commands: 3,
            }
        );
        let closed = agent.close().unwrap();
        assert_eq!(control_message_tags(&closed.transport.bytes), vec![2, 2]);
    }

    #[test]
    fn agent_try_run_actions_bounded_prefix_dispatches_checked_command_bound_prefix() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
        let actions = [
            AgentAction::tap(10, 20),
            AgentAction::Flush,
            AgentAction::tap(30, 40),
        ];

        let prefix = agent.try_run_actions_bounded_prefix(&actions, 3).unwrap();

        assert_eq!(prefix.command_bound, 3);
        assert_eq!(prefix.accepted_actions, 2);
        assert_eq!(prefix.estimated_dispatch_commands, 2);
        assert_eq!(prefix.estimated_checked_dispatch_commands(), 3);
        assert_eq!(
            prefix.stop,
            AgentPlanBoundedPrefixStop::CommandBound {
                index: 2,
                required_dispatch_commands: 4,
            }
        );
        let closed = agent.close().unwrap();
        assert_eq!(control_message_tags(&closed.transport.bytes), vec![2, 2]);

        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts_with_bound(session, Cursor::new(Vec::new()), 3)
            .unwrap();

        let prefix = agent
            .try_run_actions_bounded_prefix_with_session_bound(&actions)
            .unwrap();

        assert_eq!(prefix.command_bound, 3);
        assert_eq!(prefix.accepted_actions, 2);
        assert!(prefix.checked_dispatch_fits_bound());
        let closed = agent.close().unwrap();
        assert_eq!(control_message_tags(&closed.transport.bytes), vec![2, 2]);
    }

    #[test]
    fn agent_try_run_actions_bounded_prefix_rejects_malformed_suffix_without_dispatch() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
        let malformed = AgentAction::key_batch_fixed(
            KEYBOARD_BATCH_FRAMES + 1,
            [KeyboardFrame::EMPTY; KEYBOARD_BATCH_FRAMES],
        );

        let err = agent
            .try_run_actions_bounded_prefix(
                &[AgentAction::tap(10, 20), AgentAction::Flush, malformed],
                3,
            )
            .unwrap_err();

        assert!(matches!(
            err,
            Error::SessionLifecycle("keyboard batch length overflow")
        ));
        let closed = agent.close().unwrap();
        assert!(closed.transport.bytes.is_empty());
    }

    #[test]
    fn agent_bounded_try_queue_prefix_with_session_bound_is_pure() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts_with_bound(session, Cursor::new(Vec::new()), 2)
            .unwrap();
        let actions = [
            AgentAction::tap(10, 20),
            AgentAction::Flush,
            AgentAction::tap(30, 40),
        ];

        let prefix = agent.bounded_try_queue_prefix_with_session_bound(&actions);

        assert_eq!(prefix.command_bound, 2);
        assert_eq!(prefix.accepted_actions, 2);
        assert_eq!(prefix.estimated_dispatch_commands, 2);
        assert_eq!(
            prefix.stop,
            AgentPlanBoundedPrefixStop::CommandBound {
                index: 2,
                required_dispatch_commands: 3,
            }
        );
        let checked_prefix = agent.bounded_try_run_prefix_with_session_bound(&actions);
        assert_eq!(checked_prefix.command_bound, 2);
        assert_eq!(checked_prefix.accepted_actions, 1);
        assert_eq!(checked_prefix.estimated_dispatch_commands, 1);
        assert_eq!(checked_prefix.estimated_checked_dispatch_commands(), 2);
        assert_eq!(
            checked_prefix.stop,
            AgentPlanBoundedPrefixStop::CommandBound {
                index: 1,
                required_dispatch_commands: 3,
            }
        );
        let closed = agent.close().unwrap();
        assert!(closed.transport.bytes.is_empty());
    }

    #[test]
    fn agent_try_queue_actions_bounded_prefix_returns_blocking_boundary() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
        let actions = [
            AgentAction::tap(10, 20),
            AgentAction::wait(Duration::from_millis(0)),
            AgentAction::tap(30, 40),
        ];

        let prefix = agent
            .try_queue_actions_bounded_prefix(&actions, usize::MAX)
            .unwrap();

        assert_eq!(prefix.accepted_actions, 1);
        assert_eq!(
            prefix.stop,
            AgentPlanBoundedPrefixStop::BlockingTiming { index: 1 }
        );
        let closed = agent.close().unwrap();
        assert_eq!(control_message_tags(&closed.transport.bytes), vec![2, 2]);
    }

    #[test]
    fn agent_try_queue_actions_bounded_prefix_rejects_malformed_suffix_without_dispatch() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
        let malformed = AgentAction::touch_frames_fixed(
            TOUCH_BATCH_FRAMES + 1,
            [AgentTouchFrame::EMPTY; TOUCH_BATCH_FRAMES],
        );

        let err = agent
            .try_queue_actions_bounded_prefix(&[AgentAction::tap(10, 20), malformed], usize::MAX)
            .unwrap_err();

        assert!(matches!(
            err,
            Error::SessionLifecycle("touch frame batch length overflow")
        ));
        let closed = agent.close().unwrap();
        assert!(closed.transport.bytes.is_empty());
    }

    #[test]
    fn agent_try_queue_actions_bounded_prefix_rejects_malformed_after_command_bound() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
        let malformed = AgentAction::key_batch_fixed(
            KEYBOARD_BATCH_FRAMES + 1,
            [KeyboardFrame::EMPTY; KEYBOARD_BATCH_FRAMES],
        );

        let err = agent
            .try_queue_actions_bounded_prefix(
                &[AgentAction::tap(10, 20), AgentAction::Flush, malformed],
                1,
            )
            .unwrap_err();

        assert!(matches!(
            err,
            Error::SessionLifecycle("keyboard batch length overflow")
        ));
        let closed = agent.close().unwrap();
        assert!(closed.transport.bytes.is_empty());
    }

    #[test]
    fn agent_try_queue_actions_bounded_prefix_rejects_malformed_after_blocking_boundary() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
        let malformed = AgentAction::key_batch_fixed(
            KEYBOARD_BATCH_FRAMES + 1,
            [KeyboardFrame::EMPTY; KEYBOARD_BATCH_FRAMES],
        );

        let err = agent
            .try_queue_actions_bounded_prefix(
                &[
                    AgentAction::tap(10, 20),
                    AgentAction::wait(Duration::from_millis(0)),
                    malformed,
                ],
                usize::MAX,
            )
            .unwrap_err();

        assert!(matches!(
            err,
            Error::SessionLifecycle("keyboard batch length overflow")
        ));
        let closed = agent.close().unwrap();
        assert!(closed.transport.bytes.is_empty());
    }

    #[test]
    fn agent_try_queue_actions_bounded_prefix_handles_blocking_first_without_dispatch() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

        let prefix = agent
            .try_queue_actions_bounded_prefix(
                &[
                    AgentAction::wait(Duration::from_millis(0)),
                    AgentAction::tap(10, 20),
                ],
                usize::MAX,
            )
            .unwrap();

        assert_eq!(prefix.accepted_actions, 0);
        assert!(prefix.is_empty());
        assert_eq!(
            prefix.stop,
            AgentPlanBoundedPrefixStop::BlockingTiming { index: 0 }
        );
        let closed = agent.close().unwrap();
        assert!(closed.transport.bytes.is_empty());
    }

    #[test]
    fn agent_try_queue_actions_preflights_timed_actions_without_dispatch() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

        let err = agent
            .try_queue_actions(&[
                AgentAction::tap(10, 20),
                AgentAction::wait(Duration::from_millis(0)),
            ])
            .unwrap_err();

        assert!(matches!(
            err,
            Error::SessionLifecycle("timed action requires queue_actions or run_actions")
        ));
        let closed = agent.close().unwrap();
        assert!(closed.transport.bytes.is_empty());
    }

    #[test]
    fn agent_queue_actions_preflights_structural_errors_without_dispatch() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

        let malformed = AgentAction::touch_frames_fixed(
            TOUCH_BATCH_FRAMES + 1,
            [AgentTouchFrame::EMPTY; TOUCH_BATCH_FRAMES],
        );
        let err = agent
            .queue_actions(&[AgentAction::tap(10, 20), malformed])
            .unwrap_err();

        assert!(matches!(
            err,
            Error::SessionLifecycle("touch frame batch length overflow")
        ));
        let closed = agent.close().unwrap();
        assert!(closed.transport.bytes.is_empty());
    }

    #[test]
    fn agent_try_queue_actions_preflights_structural_errors_without_dispatch() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

        let malformed = AgentAction::key_batch_fixed(
            KEYBOARD_BATCH_FRAMES + 1,
            [KeyboardFrame::EMPTY; KEYBOARD_BATCH_FRAMES],
        );
        let err = agent
            .try_queue_actions(&[AgentAction::tap(10, 20), malformed])
            .unwrap_err();

        assert!(matches!(
            err,
            Error::SessionLifecycle("keyboard batch length overflow")
        ));
        let closed = agent.close().unwrap();
        assert!(closed.transport.bytes.is_empty());
    }

    #[test]
    fn agent_queue_actions_preflights_oversized_launch_app_without_dispatch() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

        let err = agent
            .queue_actions(&[
                AgentAction::tap(10, 20),
                AgentAction::launch_app("a".repeat(256)),
            ])
            .unwrap_err();

        assert!(matches!(
            err,
            Error::SessionLifecycle(LAUNCH_APP_NAME_TOO_LONG)
        ));
        let closed = agent.close().unwrap();
        assert!(closed.transport.bytes.is_empty());
    }

    #[test]
    fn agent_try_run_actions_executes_plan_with_checked_boundary() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

        agent
            .try_run_actions(&[AgentAction::tap(10, 20), AgentAction::tap(30, 40)])
            .unwrap();

        let closed = agent.close().unwrap();
        assert_eq!(
            control_message_tags(&closed.transport.bytes),
            vec![2, 2, 2, 2]
        );
    }

    #[test]
    fn agent_try_run_actions_preflights_timed_actions_without_dispatch() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

        let err = agent
            .try_run_actions(&[
                AgentAction::tap(10, 20),
                AgentAction::wait(Duration::from_millis(0)),
            ])
            .unwrap_err();

        assert!(matches!(
            err,
            Error::SessionLifecycle("timed action requires queue_actions or run_actions")
        ));
        let closed = agent.close().unwrap();
        assert!(closed.transport.bytes.is_empty());
    }

    #[test]
    fn agent_try_run_actions_preflights_command_bound_without_dispatch() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts_with_bound(session, Cursor::new(Vec::new()), 1)
            .unwrap();

        let err = agent
            .try_run_actions(&[AgentAction::tap(10, 20)])
            .unwrap_err();

        assert!(matches!(
            err,
            Error::SessionLifecycle(TRY_RUN_EXCEEDS_COMMAND_BOUND)
        ));
        let closed = agent.close().unwrap();
        assert!(closed.transport.bytes.is_empty());
    }

    #[test]
    fn agent_try_run_actions_surfaces_dispatch_error_after_valid_work() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

        let err = agent
            .try_run_actions(&[
                AgentAction::GamepadButton {
                    button: GamepadButton::South,
                    pressed: true,
                },
                AgentAction::tap(10, 20),
            ])
            .unwrap_err();

        assert!(matches!(err, Error::SessionLifecycle("gamepad not open")));
        let closed = agent.close().unwrap();
        assert_eq!(control_message_tags(&closed.transport.bytes), vec![2, 2]);
    }

    #[test]
    fn agent_try_queue_actions_prefix_stops_before_blocking_action() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

        let actions = [
            AgentAction::tap(10, 20),
            AgentAction::PressBack,
            AgentAction::wait(Duration::from_millis(0)),
            AgentAction::tap(30, 40),
        ];
        let sent = agent.try_queue_actions_prefix(&actions).unwrap();

        assert_eq!(sent, 2);
        let closed = agent.close().unwrap();
        assert_eq!(control_message_tags(&closed.transport.bytes), vec![2, 2, 0]);
    }

    #[test]
    fn agent_try_queue_actions_prefix_handles_blocking_first_action() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

        let sent = agent
            .try_queue_actions_prefix(&[
                AgentAction::wait(Duration::from_millis(0)),
                AgentAction::tap(10, 20),
            ])
            .unwrap();

        assert_eq!(sent, 0);
        let closed = agent.close().unwrap();
        assert!(closed.transport.bytes.is_empty());
    }

    #[test]
    fn agent_try_queue_actions_prefix_rejects_structural_error_before_dispatch() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

        let malformed = AgentAction::key_batch_fixed(
            KEYBOARD_BATCH_FRAMES + 1,
            [KeyboardFrame::EMPTY; KEYBOARD_BATCH_FRAMES],
        );
        let err = agent
            .try_queue_actions_prefix(&[AgentAction::tap(10, 20), malformed])
            .unwrap_err();

        assert!(matches!(
            err,
            Error::SessionLifecycle("keyboard batch length overflow")
        ));
        let closed = agent.close().unwrap();
        assert!(closed.transport.bytes.is_empty());
    }

    #[test]
    fn agent_try_queue_actions_prefix_leaves_blocking_suffix_uninspected() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

        let malformed = AgentAction::key_batch_fixed(
            KEYBOARD_BATCH_FRAMES + 1,
            [KeyboardFrame::EMPTY; KEYBOARD_BATCH_FRAMES],
        );
        let sent = agent
            .try_queue_actions_prefix(&[
                AgentAction::tap(10, 20),
                AgentAction::wait(Duration::from_millis(0)),
                malformed,
            ])
            .unwrap();

        assert_eq!(sent, 1);
        let closed = agent.close().unwrap();
        assert_eq!(control_message_tags(&closed.transport.bytes), vec![2, 2]);
    }

    #[test]
    fn agent_try_run_actions_prefix_stops_before_blocking_action() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

        let actions = [
            AgentAction::tap(10, 20),
            AgentAction::PressBack,
            AgentAction::wait(Duration::from_millis(0)),
            AgentAction::tap(30, 40),
        ];
        let sent = agent.try_run_actions_prefix(&actions).unwrap();

        assert_eq!(sent, 2);
        let closed = agent.close().unwrap();
        assert_eq!(control_message_tags(&closed.transport.bytes), vec![2, 2, 0]);
    }

    #[test]
    fn agent_try_run_actions_prefix_handles_blocking_first_action() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

        let sent = agent
            .try_run_actions_prefix(&[
                AgentAction::wait(Duration::from_millis(0)),
                AgentAction::tap(10, 20),
            ])
            .unwrap();

        assert_eq!(sent, 0);
        let closed = agent.close().unwrap();
        assert!(closed.transport.bytes.is_empty());
    }

    #[test]
    fn agent_try_run_actions_prefix_rejects_structural_error_before_dispatch() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

        let malformed = AgentAction::key_batch_fixed(
            KEYBOARD_BATCH_FRAMES + 1,
            [KeyboardFrame::EMPTY; KEYBOARD_BATCH_FRAMES],
        );
        let err = agent
            .try_run_actions_prefix(&[AgentAction::tap(10, 20), malformed])
            .unwrap_err();

        assert!(matches!(
            err,
            Error::SessionLifecycle("keyboard batch length overflow")
        ));
        let closed = agent.close().unwrap();
        assert!(closed.transport.bytes.is_empty());
    }

    #[test]
    fn agent_try_run_actions_prefix_leaves_blocking_suffix_uninspected() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

        let malformed = AgentAction::key_batch_fixed(
            KEYBOARD_BATCH_FRAMES + 1,
            [KeyboardFrame::EMPTY; KEYBOARD_BATCH_FRAMES],
        );
        let sent = agent
            .try_run_actions_prefix(&[
                AgentAction::tap(10, 20),
                AgentAction::wait(Duration::from_millis(0)),
                malformed,
            ])
            .unwrap();

        assert_eq!(sent, 1);
        let closed = agent.close().unwrap();
        assert_eq!(control_message_tags(&closed.transport.bytes), vec![2, 2]);
    }

    #[test]
    fn agent_try_run_actions_prefix_preflights_command_bound_without_dispatch() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts_with_bound(session, Cursor::new(Vec::new()), 1)
            .unwrap();

        let err = agent
            .try_run_actions_prefix(&[
                AgentAction::tap(10, 20),
                AgentAction::wait(Duration::from_millis(0)),
            ])
            .unwrap_err();

        assert!(matches!(
            err,
            Error::SessionLifecycle(TRY_RUN_EXCEEDS_COMMAND_BOUND)
        ));
        let closed = agent.close().unwrap();
        assert!(closed.transport.bytes.is_empty());
    }

    #[test]
    fn agent_try_run_actions_prefix_surfaces_dispatch_error_after_prefix_work() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

        let err = agent
            .try_run_actions_prefix(&[
                AgentAction::GamepadButton {
                    button: GamepadButton::South,
                    pressed: true,
                },
                AgentAction::wait(Duration::from_millis(0)),
            ])
            .unwrap_err();

        assert!(matches!(err, Error::SessionLifecycle("gamepad not open")));
        let closed = agent.close().unwrap();
        assert!(closed.transport.bytes.is_empty());
    }

    #[test]
    fn agent_run_actions_surfaces_dispatch_error_after_valid_work() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

        let err = agent
            .run_actions(&[
                AgentAction::Key {
                    scancode: 0x04,
                    pressed: true,
                    mods: Modifiers::empty(),
                },
                AgentAction::tap(10, 20),
            ])
            .unwrap_err();

        assert!(matches!(err, Error::SessionLifecycle("keyboard not open")));
        let closed = agent.close().unwrap();
        assert_eq!(count_touch_events(&closed.transport.bytes), 2);
    }

    #[test]
    fn agent_run_actions_type_text_strict_preflights_unsupported_char_without_dispatch() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::kbd_only()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

        let err = agent
            .run_actions(&[
                AgentAction::type_text_strict("A中"),
                AgentAction::type_text("z"),
            ])
            .unwrap_err();

        assert!(matches!(
            err,
            Error::SessionLifecycle("unsupported char in type_text_strict")
        ));
        let closed = agent.close().unwrap();
        assert_eq!(count_uhid_inputs(&closed.transport.bytes), 0);
    }

    #[test]
    fn agent_actions_cover_typed_keyboard_tap_helpers() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::kbd_only()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

        agent
            .run_actions(&[
                AgentAction::tap_scancode(Scancode::A, Modifiers::LSHIFT),
                AgentAction::key_scancode(Scancode::B, true, Modifiers::empty()),
                AgentAction::key_scancode(Scancode::B, false, Modifiers::empty()),
            ])
            .unwrap();

        let closed = agent.close().unwrap();
        assert_eq!(count_uhid_inputs(&closed.transport.bytes), 4);
    }

    #[test]
    fn agent_run_actions_batches_consecutive_keyboard_actions() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::kbd_only()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

        agent
            .run_actions(&[
                AgentAction::tap_scancode(Scancode::A, Modifiers::LSHIFT),
                AgentAction::key_scancode(Scancode::B, true, Modifiers::empty()),
                AgentAction::key_scancode(Scancode::B, false, Modifiers::empty()),
            ])
            .unwrap();

        let closed = agent.close().unwrap();
        assert_eq!(count_uhid_inputs(&closed.transport.bytes), 4);
        assert_eq!(input_and_touch_tags(&closed.transport.bytes), vec![13; 4]);
    }

    #[test]
    fn agent_run_actions_flushes_keyboard_before_touch_actions() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::kbd_only()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

        agent
            .run_actions(&[
                AgentAction::tap_scancode(Scancode::A, Modifiers::LSHIFT),
                AgentAction::tap(10, 20),
                AgentAction::tap_scancode(Scancode::B, Modifiers::empty()),
            ])
            .unwrap();

        let closed = agent.close().unwrap();
        assert_eq!(
            input_and_touch_tags(&closed.transport.bytes),
            vec![13, 13, 2, 2, 13, 13]
        );
    }

    #[test]
    fn agent_try_queue_actions_batches_keyboard_with_tiny_bound() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::kbd_only()).unwrap();
        let agent = AgentControlSession::from_parts_with_bound(session, Cursor::new(Vec::new()), 1)
            .unwrap();

        agent
            .try_queue_actions(&[
                AgentAction::tap_scancode(Scancode::A, Modifiers::LSHIFT),
                AgentAction::key_scancode(Scancode::B, true, Modifiers::empty()),
                AgentAction::key_scancode(Scancode::B, false, Modifiers::empty()),
            ])
            .unwrap();

        let closed = agent.close().unwrap();
        assert_eq!(count_uhid_inputs(&closed.transport.bytes), 4);
    }

    #[test]
    fn agent_keyboard_frame_batch_action_dispatches_fixed_batch() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::kbd_only()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
        let frames = [
            KeyboardFrame::scancode_down(Scancode::A, Modifiers::LSHIFT),
            KeyboardFrame::scancode_up(Scancode::A),
            KeyboardFrame::scancode_down(Scancode::B, Modifiers::empty()),
            KeyboardFrame::scancode_up(Scancode::B),
        ];

        agent
            .run_actions(&[AgentAction::try_key_batch(&frames).unwrap()])
            .unwrap();

        let closed = agent.close().unwrap();
        assert_eq!(count_uhid_inputs(&closed.transport.bytes), frames.len());
    }

    #[test]
    fn agent_keyboard_chord_action_dispatches_fixed_batch() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::kbd_only()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

        agent
            .run_actions(&[
                AgentAction::ctrl_scancode(Scancode::C),
                AgentAction::ctrl_shift_scancode(Scancode::V),
            ])
            .unwrap();

        let closed = agent.close().unwrap();
        assert_eq!(count_uhid_inputs(&closed.transport.bytes), 4);
        assert_eq!(input_and_touch_tags(&closed.transport.bytes), vec![13; 4]);
    }

    #[test]
    fn agent_run_actions_batches_keyboard_chords_with_adjacent_keys() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::kbd_only()).unwrap();
        let agent = AgentControlSession::from_parts_with_bound(session, Cursor::new(Vec::new()), 1)
            .unwrap();

        agent
            .try_queue_actions(&[
                AgentAction::tap_scancode(Scancode::A, Modifiers::LSHIFT),
                AgentAction::try_scancode_chord(&[Scancode::K, Scancode::C], Modifiers::LCTRL)
                    .unwrap(),
                AgentAction::key_scancode(Scancode::B, true, Modifiers::empty()),
                AgentAction::key_scancode(Scancode::B, false, Modifiers::empty()),
            ])
            .unwrap();

        let closed = agent.close().unwrap();
        assert_eq!(count_uhid_inputs(&closed.transport.bytes), 8);
        assert_eq!(input_and_touch_tags(&closed.transport.bytes), vec![13; 8]);
    }

    #[test]
    fn agent_keyboard_batch_constructor_rejects_oversized_slices() {
        let frames = vec![KeyboardFrame::EMPTY; KEYBOARD_BATCH_FRAMES + 1];
        assert!(matches!(
            AgentAction::try_key_batch(&frames),
            Err(Error::SessionLifecycle("keyboard batch too large"))
        ));
    }

    #[test]
    fn agent_keyboard_chord_constructor_rejects_invalid_slices() {
        let frames = vec![Scancode::A; crate::client::KEYBOARD_CHORD_KEYS + 1];
        assert!(matches!(
            AgentAction::try_scancode_chord(&frames, Modifiers::LCTRL),
            Err(Error::SessionLifecycle("keyboard chord too large"))
        ));
        assert!(matches!(
            AgentAction::try_scancode_chord(&[Scancode::LeftCtrl], Modifiers::empty()),
            Err(Error::SessionLifecycle(
                "keyboard chord keys must be non-modifier scancodes"
            ))
        ));

        let malformed = AgentAction::keyboard_chord_fixed(KeyboardChordFrame::new(
            (crate::client::KEYBOARD_CHORD_KEYS + 1) as u8,
            [Scancode::A.to_u8(); crate::client::KEYBOARD_CHORD_KEYS],
            Modifiers::LCTRL,
        ));
        let session = HidSession::open(MockTransport::new(), OpenRequest::kbd_only()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
        assert!(matches!(
            agent.run_actions(&[malformed]),
            Err(Error::SessionLifecycle("keyboard chord length overflow"))
        ));
    }

    #[test]
    fn agent_queue_actions_defers_errors_until_checked_boundary() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

        agent
            .queue_actions(&[
                AgentAction::Key {
                    scancode: 0x04,
                    pressed: true,
                    mods: Modifiers::empty(),
                },
                AgentAction::tap(10, 20),
            ])
            .unwrap();
        let report = agent.close_checked().unwrap();

        assert!(matches!(
            report.command_result,
            Err(Error::SessionLifecycle("keyboard not open"))
        ));
        assert_eq!(count_touch_events(&report.closed.transport.bytes), 2);
    }

    #[test]
    fn agent_actions_cover_gamepad_and_clipboard_commands() {
        let session =
            HidSession::open(MockTransport::new(), OpenRequest::gamepad_only_realtime()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

        agent
            .run_actions(&[
                AgentAction::GamepadButton {
                    button: GamepadButton::South,
                    pressed: true,
                },
                AgentAction::GamepadButtons {
                    buttons: GamepadButton::South as u32 | GamepadButton::DpadUp as u32,
                },
                AgentAction::GamepadFrameUnchecked {
                    frame: GamepadFrameRaw::new(1, 2, 3, 4, 5, 6, 7),
                },
                AgentAction::set_clipboard("agent plan", false),
                AgentAction::request_clipboard_key(ClipboardCopyKey::COPY),
                AgentAction::configure_ai(
                    crate::control::AI_FLAG_KEYFRAMES | crate::control::AI_FLAG_OBJECTS,
                    16,
                    0,
                ),
                AgentAction::query_ai(0x0102_0304_0506_0708),
                AgentAction::pause_ai(),
            ])
            .unwrap();

        let closed = agent.close().unwrap();
        let bytes = closed.transport.bytes;
        assert_eq!(count_uhid_inputs(&bytes), 3);
        assert!(
            find_control_message(&bytes, 9).is_some(),
            "SET_CLIPBOARD tag should be present"
        );
        let get_clipboard = find_control_message(&bytes, 8).expect("GET_CLIPBOARD frame");
        assert_eq!(get_clipboard[1], 1);
        let ai_config = find_control_message(&bytes, 22).expect("AI_CONFIG frame");
        assert_eq!(
            ai_config,
            &[
                22,
                crate::control::AI_FLAG_KEYFRAMES | crate::control::AI_FLAG_OBJECTS,
                0,
                16,
                0,
                0
            ]
        );
        let ai_query = find_control_message(&bytes, 23).expect("AI_QUERY frame");
        assert_eq!(
            u64::from_be_bytes(ai_query[1..9].try_into().unwrap()),
            0x0102_0304_0506_0708
        );
        assert_eq!(find_control_message(&bytes, 24), Some(&[24][..]));
    }

    #[test]
    fn agent_gamepad_helpers_emit_uhid_reports() {
        let session =
            HidSession::open(MockTransport::new(), OpenRequest::gamepad_only_realtime()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

        agent.send_button(GamepadButton::South, true).unwrap();
        agent.send_button(GamepadButton::South, false).unwrap();
        agent
            .send_buttons(GamepadButton::South as u32 | GamepadButton::DpadUp as u32)
            .unwrap();
        agent.send_stick_raw(GamepadAxis::LeftX, 123).unwrap();
        agent.send_stick(GamepadAxis::RightY, 0.5).unwrap();
        agent.send_left_stick_raw(7, -7).unwrap();
        agent.send_right_stick_raw(-8, 8).unwrap();
        agent.send_triggers_raw(9, 10).unwrap();
        agent.send_sticks_raw(11, 12, 13, 14, 15, 16).unwrap();
        agent
            .send_frame_unchecked(GamepadFrameRaw::new(1, 2, 3, 4, 5, 6, 7))
            .unwrap();
        agent
            .send_frame_packed(GamepadFrameRaw::new(2, 3, 4, 5, 6, 7, 8).pack())
            .unwrap();

        let closed = agent.close().unwrap();
        assert_eq!(count_uhid_inputs(&closed.transport.bytes), 11);
    }

    #[test]
    fn agent_try_gamepad_helpers_use_nonblocking_checked_dispatch() {
        let session =
            HidSession::open(MockTransport::new(), OpenRequest::gamepad_only_realtime()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

        agent.try_send_button(GamepadButton::South, true).unwrap();
        agent.try_send_button(GamepadButton::South, false).unwrap();
        agent
            .try_send_buttons(GamepadButton::South as u32 | GamepadButton::DpadUp as u32)
            .unwrap();
        agent.try_send_stick_raw(GamepadAxis::LeftX, 123).unwrap();
        agent.try_send_stick(GamepadAxis::RightY, 0.5).unwrap();
        agent.try_send_left_stick_raw(7, -7).unwrap();
        agent.try_send_right_stick_raw(-8, 8).unwrap();
        agent.try_send_triggers_raw(9, 10).unwrap();
        agent.try_send_sticks_raw(11, 12, 13, 14, 15, 16).unwrap();
        agent
            .try_send_frame_unchecked(GamepadFrameRaw::new(1, 2, 3, 4, 5, 6, 7))
            .unwrap();
        agent
            .try_send_frame(GamepadFrameRaw::new(2, 3, 4, 5, 6, 7, 8))
            .unwrap();
        agent
            .try_send_frame_packed(GamepadFrameRaw::new(3, 4, 5, 6, 7, 8, 9).pack())
            .unwrap();

        let closed = agent.close().unwrap();
        assert_eq!(count_uhid_inputs(&closed.transport.bytes), 12);
    }

    #[test]
    fn agent_try_gamepad_fixed_batches_use_nonblocking_checked_dispatch() {
        let session =
            HidSession::open(MockTransport::new(), OpenRequest::gamepad_only_realtime()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
        let mut frames = [GamepadFrameRaw::new(0, 0, 0, 0, 0, 0, 0); GAMEPAD_BATCH_FRAMES];
        frames[0] = GamepadFrameRaw::new(1, 10, 20, 30, 40, 50, 60);
        frames[1] = GamepadFrameRaw::new(2, 11, 21, 31, 41, 51, 61);
        let mut packed = [[0u8; GAMEPAD_FRAME_BYTES]; GAMEPAD_BATCH_FRAMES];
        packed[0] = GamepadFrameRaw::new(3, 12, 22, 32, 42, 52, 62).pack();
        packed[1] = GamepadFrameRaw::new(4, 13, 23, 33, 43, 53, 63).pack();

        agent.try_send_frame_batch_fixed(2, frames).unwrap();
        agent
            .try_send_frame_batch_fixed_unchecked(2, frames)
            .unwrap();
        agent.try_send_frame_packed_batch_fixed(2, packed).unwrap();

        let closed = agent.close().unwrap();
        assert_eq!(count_uhid_inputs(&closed.transport.bytes), 6);
    }

    #[test]
    fn agent_try_gamepad_preflights_command_bound_without_dispatch() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts_with_bound(session, Cursor::new(Vec::new()), 1)
            .unwrap();

        let err = agent
            .try_send_button(GamepadButton::South, true)
            .unwrap_err();

        assert!(matches!(
            err,
            Error::SessionLifecycle(TRY_GAMEPAD_EXCEEDS_COMMAND_BOUND)
        ));
        let closed = agent.close().unwrap();
        assert!(closed.transport.bytes.is_empty());
    }

    #[test]
    fn agent_gamepad_frame_batch_action_dispatches_fixed_batch() {
        let session =
            HidSession::open(MockTransport::new(), OpenRequest::gamepad_only_realtime()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
        let frames = [
            GamepadFrameRaw::new(1, 10, 20, 30, 40, 50, 60),
            GamepadFrameRaw::new(2, 11, 21, 31, 41, 51, 61),
            GamepadFrameRaw::new(3, 12, 22, 32, 42, 52, 62),
        ];

        let action = AgentAction::try_gamepad_frame_batch(&frames).unwrap();
        agent.run_actions(&[action]).unwrap();

        let closed = agent.close().unwrap();
        assert_eq!(count_uhid_inputs(&closed.transport.bytes), 3);
    }

    #[test]
    fn agent_gamepad_unchecked_batch_preserves_duplicate_frames() {
        let session =
            HidSession::open(MockTransport::new(), OpenRequest::gamepad_only_realtime()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
        let frame = GamepadFrameRaw::new(1, 2, 3, 4, 5, 6, 7);
        let action = AgentAction::try_gamepad_frame_batch_unchecked(&[frame, frame]).unwrap();

        agent.run_actions(&[action]).unwrap();

        let closed = agent.close().unwrap();
        assert_eq!(count_uhid_inputs(&closed.transport.bytes), 2);
    }

    #[test]
    fn agent_gamepad_packed_batch_action_dispatches_fixed_batch() {
        let session =
            HidSession::open(MockTransport::new(), OpenRequest::gamepad_only_realtime()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
        let frames = [
            GamepadFrameRaw::new(1, 10, 20, 30, 40, 50, 60).pack(),
            GamepadFrameRaw::new(2, 11, 21, 31, 41, 51, 61).pack(),
        ];

        let action = AgentAction::try_gamepad_packed_frame_batch(&frames).unwrap();
        agent.run_actions(&[action]).unwrap();

        let closed = agent.close().unwrap();
        assert_eq!(count_uhid_inputs(&closed.transport.bytes), 2);
    }

    #[test]
    fn agent_run_actions_batches_consecutive_gamepad_unchecked_frames() {
        let session =
            HidSession::open(MockTransport::new(), OpenRequest::gamepad_only_realtime()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
        let batch = [
            GamepadFrameRaw::new(2, 11, 21, 31, 41, 51, 61),
            GamepadFrameRaw::new(3, 12, 22, 32, 42, 52, 62),
        ];

        agent
            .run_actions(&[
                AgentAction::gamepad_frame_unchecked(GamepadFrameRaw::new(
                    1, 10, 20, 30, 40, 50, 60,
                )),
                AgentAction::try_gamepad_frame_batch_unchecked(&batch).unwrap(),
                AgentAction::gamepad_frame_unchecked(GamepadFrameRaw::new(
                    4, 13, 23, 33, 43, 53, 63,
                )),
            ])
            .unwrap();

        let closed = agent.close().unwrap();
        assert_eq!(count_uhid_inputs(&closed.transport.bytes), 4);
        assert_eq!(input_and_touch_tags(&closed.transport.bytes), vec![13; 4]);
    }

    #[test]
    fn agent_run_actions_flushes_gamepad_before_touch_actions() {
        let session =
            HidSession::open(MockTransport::new(), OpenRequest::gamepad_only_realtime()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

        agent
            .run_actions(&[
                AgentAction::gamepad_frame_unchecked(GamepadFrameRaw::new(1, 2, 3, 4, 5, 6, 7)),
                AgentAction::tap(10, 20),
                AgentAction::gamepad_frame_unchecked(GamepadFrameRaw::new(
                    8, 9, 10, 11, 12, 13, 14,
                )),
            ])
            .unwrap();

        let closed = agent.close().unwrap();
        assert_eq!(
            input_and_touch_tags(&closed.transport.bytes),
            vec![13, 2, 2, 13]
        );
    }

    #[test]
    fn agent_try_queue_actions_batches_gamepad_with_tiny_bound() {
        let session =
            HidSession::open(MockTransport::new(), OpenRequest::gamepad_only_realtime()).unwrap();
        let agent = AgentControlSession::from_parts_with_bound(session, Cursor::new(Vec::new()), 1)
            .unwrap();

        agent
            .try_queue_actions(&[
                AgentAction::gamepad_frame_unchecked(GamepadFrameRaw::new(
                    1, 10, 20, 30, 40, 50, 60,
                )),
                AgentAction::gamepad_frame_unchecked(GamepadFrameRaw::new(
                    2, 11, 21, 31, 41, 51, 61,
                )),
                AgentAction::gamepad_frame_unchecked(GamepadFrameRaw::new(
                    3, 12, 22, 32, 42, 52, 62,
                )),
            ])
            .unwrap();

        let closed = agent.close().unwrap();
        assert_eq!(count_uhid_inputs(&closed.transport.bytes), 3);
    }

    #[test]
    fn agent_run_actions_flushes_gamepad_on_mode_switch() {
        let session =
            HidSession::open(MockTransport::new(), OpenRequest::gamepad_only_realtime()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
        let packed = GamepadFrameRaw::new(2, 11, 21, 31, 41, 51, 61).pack();

        agent
            .run_actions(&[
                AgentAction::gamepad_frame_unchecked(GamepadFrameRaw::new(
                    1, 10, 20, 30, 40, 50, 60,
                )),
                AgentAction::gamepad_packed_frame(packed),
                AgentAction::gamepad_frame_unchecked(GamepadFrameRaw::new(
                    3, 12, 22, 32, 42, 52, 62,
                )),
            ])
            .unwrap();

        let closed = agent.close().unwrap();
        assert_eq!(count_uhid_inputs(&closed.transport.bytes), 3);
        assert_eq!(input_and_touch_tags(&closed.transport.bytes), vec![13; 3]);
    }

    #[test]
    fn agent_gamepad_batch_constructors_reject_oversized_slices() {
        let frame = GamepadFrameRaw::new(1, 2, 3, 4, 5, 6, 7);
        let frames = vec![frame; GAMEPAD_BATCH_FRAMES + 1];
        assert!(matches!(
            AgentAction::try_gamepad_frame_batch(&frames),
            Err(Error::SessionLifecycle("gamepad frame batch too large"))
        ));
        assert!(matches!(
            AgentAction::try_gamepad_frame_batch_unchecked(&frames),
            Err(Error::SessionLifecycle("gamepad frame batch too large"))
        ));

        let packed = vec![[0u8; GAMEPAD_FRAME_BYTES]; GAMEPAD_BATCH_FRAMES + 1];
        assert!(matches!(
            AgentAction::try_gamepad_packed_frame_batch(&packed),
            Err(Error::SessionLifecycle(
                "gamepad packed frame batch too large"
            ))
        ));
    }

    #[test]
    fn agent_typed_clipboard_copy_key_helpers_emit_get_clipboard() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let mut agent =
            AgentControlSession::from_parts(session, Cursor::new(clipboard("typed"))).unwrap();

        agent.request_clipboard_key(ClipboardCopyKey::CUT).unwrap();
        let text = agent
            .get_clipboard_and_wait_key(ClipboardCopyKey::COPY)
            .unwrap();
        assert_eq!(text, "typed");

        let closed = agent.close().unwrap();
        assert_eq!(closed.transport.bytes, vec![8, 2, 8, 1]);
    }

    #[test]
    fn agent_scroll_helpers_emit_inject_scroll_with_screen_size() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

        agent.set_screen_size(720, 1280).unwrap();
        agent
            .scroll_with_buttons(100, 200, 8.0, -16.0, 0x11)
            .unwrap();
        agent
            .run_actions(&[AgentAction::scroll(300, 400, 0, 16)])
            .unwrap();

        let closed = agent.close().unwrap();
        let bytes = closed.transport.bytes;
        assert_eq!(control_message_tags(&bytes), vec![3, 3]);
        let first = &bytes[0..21];
        assert_eq!(i32::from_be_bytes(first[1..5].try_into().unwrap()), 100);
        assert_eq!(i32::from_be_bytes(first[5..9].try_into().unwrap()), 200);
        assert_eq!(u16::from_be_bytes(first[9..11].try_into().unwrap()), 720);
        assert_eq!(u16::from_be_bytes(first[11..13].try_into().unwrap()), 1280);
        assert_eq!(
            u16::from_be_bytes(first[13..15].try_into().unwrap()),
            0x4000
        );
        assert_eq!(
            u16::from_be_bytes(first[15..17].try_into().unwrap()),
            0x8000
        );
        assert_eq!(u32::from_be_bytes(first[17..21].try_into().unwrap()), 0x11);
        let second = &bytes[21..42];
        assert_eq!(i32::from_be_bytes(second[1..5].try_into().unwrap()), 300);
        assert_eq!(i32::from_be_bytes(second[5..9].try_into().unwrap()), 400);
        assert_eq!(u16::from_be_bytes(second[9..11].try_into().unwrap()), 720);
        assert_eq!(u16::from_be_bytes(second[11..13].try_into().unwrap()), 1280);
        assert_eq!(u16::from_be_bytes(second[13..15].try_into().unwrap()), 0);
        assert_eq!(
            u16::from_be_bytes(second[15..17].try_into().unwrap()),
            0x7FFF
        );
    }

    #[test]
    fn agent_scroll_batch_action_dispatches_fixed_batch() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
        let frames = [
            AgentScrollFrame::new(100, 200, 8, -16, 0x11),
            AgentScrollFrame::scroll(300, 400, 0, 16),
            AgentScrollFrame::new(500, 600, -1, 1, 0x22),
        ];

        agent
            .run_actions(&[AgentAction::try_scroll_batch(&frames).unwrap()])
            .unwrap();

        let closed = agent.close().unwrap();
        let bytes = closed.transport.bytes;
        assert_eq!(control_message_tags(&bytes), vec![3, 3, 3]);
        let first = &bytes[0..21];
        assert_eq!(i32::from_be_bytes(first[1..5].try_into().unwrap()), 100);
        assert_eq!(i32::from_be_bytes(first[5..9].try_into().unwrap()), 200);
        assert_eq!(u32::from_be_bytes(first[17..21].try_into().unwrap()), 0x11);
        let third = &bytes[42..63];
        assert_eq!(i32::from_be_bytes(third[1..5].try_into().unwrap()), 500);
        assert_eq!(u32::from_be_bytes(third[17..21].try_into().unwrap()), 0x22);
    }

    #[test]
    fn agent_run_actions_batches_consecutive_scroll_actions() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
        let frames = [
            AgentScrollFrame::scroll(300, 400, 0, 16),
            AgentScrollFrame::new(500, 600, -1, 1, 0x22),
        ];

        agent
            .run_actions(&[
                AgentAction::scroll(100, 200, 8, -16),
                AgentAction::scroll_with_buttons(150, 250, 1, -1, 0x11),
                AgentAction::try_scroll_batch(&frames).unwrap(),
            ])
            .unwrap();

        let closed = agent.close().unwrap();
        let bytes = closed.transport.bytes;
        assert_eq!(control_message_tags(&bytes), vec![3, 3, 3, 3]);
        let xs: Vec<_> = bytes
            .chunks_exact(21)
            .map(|frame| i32::from_be_bytes(frame[1..5].try_into().unwrap()))
            .collect();
        assert_eq!(xs, vec![100, 150, 300, 500]);
    }

    #[test]
    fn agent_run_actions_flushes_scroll_before_touch_actions() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

        agent
            .run_actions(&[
                AgentAction::scroll(100, 200, 0, 1),
                AgentAction::tap(10, 20),
                AgentAction::scroll(300, 400, 0, -1),
            ])
            .unwrap();

        let closed = agent.close().unwrap();
        assert_eq!(
            control_message_tags(&closed.transport.bytes),
            vec![3, 2, 2, 3]
        );
    }

    #[test]
    fn agent_try_queue_actions_batches_scroll_with_tiny_bound() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts_with_bound(session, Cursor::new(Vec::new()), 1)
            .unwrap();
        let frames = [
            AgentScrollFrame::scroll(300, 400, 0, 16),
            AgentScrollFrame::new(500, 600, -1, 1, 0x22),
        ];

        agent
            .try_queue_actions(&[
                AgentAction::scroll(100, 200, 8, -16),
                AgentAction::scroll_with_buttons(150, 250, 1, -1, 0x11),
                AgentAction::try_scroll_batch(&frames).unwrap(),
            ])
            .unwrap();

        let closed = agent.close().unwrap();
        assert_eq!(control_message_tags(&closed.transport.bytes), vec![3; 4]);
    }

    #[test]
    fn agent_scroll_batch_constructor_rejects_oversized_slices() {
        let frames = vec![AgentScrollFrame::EMPTY; SCROLL_BATCH_FRAMES + 1];
        assert!(matches!(
            AgentAction::try_scroll_batch(&frames),
            Err(Error::SessionLifecycle("scroll batch too large"))
        ));
    }

    #[test]
    fn set_clipboard_and_wait_ack_uses_matching_sequence() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let mut agent = AgentControlSession::from_parts(session, clipboard_ack_stream(1)).unwrap();

        let sequence = agent
            .set_clipboard_and_wait_ack("agent-copy", false)
            .unwrap();
        assert_eq!(sequence, 1);

        let closed = agent.close().unwrap();
        let bytes = closed.transport.bytes;
        let set_clipboard = bytes
            .iter()
            .position(|b| *b == 9)
            .expect("SET_CLIPBOARD frame");
        assert_eq!(
            u64::from_be_bytes(
                bytes[set_clipboard + 1..set_clipboard + 9]
                    .try_into()
                    .unwrap()
            ),
            1
        );
        assert_eq!(closed.reader.position(), 9);
    }

    #[test]
    fn wait_for_clipboard_ack_skips_unrelated_messages() {
        let mut stream = ack(7);
        stream.extend(ack(8));
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let mut agent = AgentControlSession::from_parts(session, Cursor::new(stream)).unwrap();

        agent.wait_for_clipboard_ack(8).unwrap();
        let closed = agent.close().unwrap();
        assert_eq!(closed.reader.position(), 18);
    }

    #[test]
    fn get_clipboard_and_wait_returns_clipboard_payload() {
        let mut stream = ack(99);
        stream.extend(clipboard("device text"));
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let mut agent = AgentControlSession::from_parts(session, Cursor::new(stream)).unwrap();

        let text = agent.get_clipboard_and_wait(1).unwrap();
        assert_eq!(text, "device text");

        let closed = agent.close().unwrap();
        let bytes = closed.transport.bytes;
        let get_clipboard = bytes
            .iter()
            .position(|b| *b == 8)
            .expect("GET_CLIPBOARD frame");
        assert_eq!(bytes[get_clipboard + 1], 1);
        assert_eq!(closed.reader.position(), 9 + 5 + "device text".len() as u64);
    }

    #[test]
    fn run_actions_and_get_clipboard_and_wait_key_flushes_then_reads() {
        let mut stream = ack(99);
        stream.extend(clipboard("copied text"));
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let mut agent = AgentControlSession::from_parts(session, Cursor::new(stream)).unwrap();

        let text = agent
            .run_actions_and_get_clipboard_and_wait_key(
                &[AgentAction::tap(10, 20)],
                ClipboardCopyKey::COPY,
            )
            .unwrap();
        assert_eq!(text, "copied text");

        let closed = agent.close().unwrap();
        let bytes = closed.transport.bytes;
        let tags = control_message_tags(&bytes);
        assert_eq!(tags, [2, 2, 8]);
        let get_clipboard = find_control_message(&bytes, 8).expect("GET_CLIPBOARD frame");
        assert_eq!(get_clipboard, &[8, ClipboardCopyKey::COPY.value()]);
        assert_eq!(closed.reader.position(), 9 + 5 + "copied text".len() as u64);
    }

    #[test]
    fn run_actions_and_set_clipboard_and_wait_ack_uses_matching_sequence() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let mut agent = AgentControlSession::from_parts(session, clipboard_ack_stream(1)).unwrap();

        let sequence = agent
            .run_actions_and_set_clipboard_and_wait_ack(
                &[AgentAction::tap(10, 20)],
                "queued clipboard",
                true,
            )
            .unwrap();
        assert_eq!(sequence, 1);

        let closed = agent.close().unwrap();
        let bytes = closed.transport.bytes;
        let tags = control_message_tags(&bytes);
        assert_eq!(tags, [2, 2, 9]);
        let set_clipboard = find_control_message(&bytes, 9).expect("SET_CLIPBOARD frame");
        assert_eq!(
            u64::from_be_bytes(set_clipboard[1..9].try_into().unwrap()),
            sequence
        );
        assert_eq!(set_clipboard[9], 1);
        assert_eq!(closed.reader.position(), 9);
    }

    #[test]
    fn wait_for_clipboard_skips_ack_messages() {
        let mut stream = ack(1);
        stream.extend(ack(2));
        stream.extend(clipboard("later"));
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let mut agent = AgentControlSession::from_parts(session, Cursor::new(stream)).unwrap();

        assert_eq!(agent.wait_for_clipboard().unwrap(), "later");
    }

    #[test]
    fn wait_for_clipboard_maps_reader_timeout_to_agent_timeout() {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let mut agent = AgentControlSession::from_parts(session, TimedOutReader).unwrap();

        assert!(matches!(
            agent.wait_for_clipboard().unwrap_err(),
            Error::AgentTimeout("clipboard")
        ));
    }

    #[test]
    fn tcp_tap_next_object_selector_at_timeout_emits_relative_touch_and_restores_timeout() {
        let stream = frame_summary_envelope_with(
            1,
            &[ObjectBox {
                x: 100,
                y: 200,
                w: 301,
                h: 101,
                class_id: 7,
                confidence: 230,
            }],
            &[],
        );
        let (mut agent, server) = tcp_agent_with_reader_bytes(stream);

        agent.set_screen_size(1000, 2000).unwrap();
        let rect = agent
            .tap_next_object_selector_at_timeout(
                AgentObjectSelector::class_min_confidence(7, 220),
                2_500,
                7_500,
                Duration::from_secs(1),
            )
            .unwrap();

        assert_eq!(rect.to_pixels(1000, 2000), (100, 200, 400, 300));
        let closed = agent.close().unwrap();
        server.join().unwrap();
        assert_eq!(closed.reader.read_timeout().unwrap(), None);
        let events = touch_events(&closed.transport.bytes);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0], (TouchAction::DOWN.value(), 0, 175, 275));
        assert_eq!(events[1], (TouchAction::UP.value(), 0, 175, 275));
    }

    #[test]
    fn tcp_tap_next_text_region_at_timeout_emits_relative_touch_and_restores_timeout() {
        let mut stream = Vec::new();
        stream.extend(frame_summary_envelope_with(
            1,
            &[],
            &[TextRegion {
                x: 700,
                y: 800,
                w: 101,
                h: 101,
            }],
        ));
        stream.extend(frame_summary_envelope_with(
            2,
            &[],
            &[
                TextRegion {
                    x: 10,
                    y: 20,
                    w: 11,
                    h: 21,
                },
                TextRegion {
                    x: 100,
                    y: 200,
                    w: 301,
                    h: 101,
                },
            ],
        ));
        let (mut agent, server) = tcp_agent_with_reader_bytes(stream);

        agent.set_screen_size(1000, 2000).unwrap();
        let indexed = agent
            .tap_next_text_region_at_timeout(0, 10_000, 0, Duration::from_secs(1))
            .unwrap();
        let largest = agent
            .tap_next_largest_text_region_at_timeout(0, 10_000, Duration::from_secs(1))
            .unwrap();

        assert_eq!(indexed.to_pixels(1000, 2000), (700, 800, 800, 900));
        assert_eq!(largest.to_pixels(1000, 2000), (100, 200, 400, 300));
        let closed = agent.close().unwrap();
        server.join().unwrap();
        assert_eq!(closed.reader.read_timeout().unwrap(), None);
        let events = touch_events(&closed.transport.bytes);
        assert_eq!(events.len(), 4);
        assert_eq!(events[0], (TouchAction::DOWN.value(), 0, 800, 800));
        assert_eq!(events[2], (TouchAction::DOWN.value(), 0, 100, 300));
    }

    #[test]
    fn tcp_tap_next_pointer_timeout_helpers_emit_typed_pointer_and_restore_timeout() {
        let mut stream = Vec::new();
        stream.extend(frame_summary_envelope_with(
            1,
            &[ObjectBox {
                x: 100,
                y: 200,
                w: 301,
                h: 101,
                class_id: 7,
                confidence: 230,
            }],
            &[],
        ));
        stream.extend(frame_summary_envelope_with(
            2,
            &[],
            &[TextRegion {
                x: 700,
                y: 800,
                w: 101,
                h: 101,
            }],
        ));
        let (mut agent, server) = tcp_agent_with_reader_bytes(stream);
        let pointer = TouchPointerId::VIRTUAL_FINGER;

        agent.set_screen_size(1000, 2000).unwrap();
        let object = agent
            .tap_next_object_selector_at_pointer_timeout(
                AgentObjectSelector::class_min_confidence(7, 220),
                pointer,
                2_500,
                7_500,
                Duration::from_secs(1),
            )
            .unwrap();
        let text = agent
            .tap_next_largest_text_region_pointer_timeout(pointer, Duration::from_secs(1))
            .unwrap();

        assert_eq!(object.to_pixels(1000, 2000), (100, 200, 400, 300));
        assert_eq!(text.center().to_pixels(1000, 2000), (750, 850));
        let closed = agent.close().unwrap();
        server.join().unwrap();
        assert_eq!(closed.reader.read_timeout().unwrap(), None);
        let events = touch_events(&closed.transport.bytes);
        assert_eq!(events.len(), 4);
        assert!(events
            .iter()
            .all(|(_, pointer_id, _, _)| *pointer_id == pointer.value()));
        assert_eq!(
            events[0],
            (TouchAction::DOWN.value(), pointer.value(), 175, 275)
        );
        assert_eq!(
            events[2],
            (TouchAction::DOWN.value(), pointer.value(), 750, 850)
        );
    }

    #[test]
    fn tcp_agent_target_selector_timeout_helpers_restore_timeout() {
        let mut stream = Vec::new();
        stream.extend(frame_summary_envelope_with(
            1,
            &[ObjectBox {
                x: 100,
                y: 200,
                w: 301,
                h: 101,
                class_id: 7,
                confidence: 230,
            }],
            &[],
        ));
        stream.extend(frame_summary_envelope_with(
            2,
            &[],
            &[TextRegion {
                x: 700,
                y: 800,
                w: 101,
                h: 101,
            }],
        ));
        stream.extend(frame_summary_envelope_with(
            3,
            &[],
            &[
                TextRegion {
                    x: 10,
                    y: 20,
                    w: 11,
                    h: 21,
                },
                TextRegion {
                    x: 100,
                    y: 200,
                    w: 301,
                    h: 101,
                },
            ],
        ));
        stream.extend(frame_summary_envelope_with(
            4,
            &[ObjectBox {
                x: 300,
                y: 400,
                w: 201,
                h: 101,
                class_id: 3,
                confidence: 240,
            }],
            &[],
        ));
        let (mut agent, server) = tcp_agent_with_reader_bytes(stream);
        let pointer = TouchPointerId::VIRTUAL_FINGER;

        agent.set_screen_size(1000, 2000).unwrap();
        let object = agent
            .wait_for_target_rect_timeout(
                AgentTargetSelector::object_class_min_confidence(7, 220),
                Duration::from_secs(1),
            )
            .unwrap();
        let text = agent
            .tap_next_target_at_pointer_timeout(
                AgentTargetSelector::text_region(0),
                pointer,
                10_000,
                0,
                Duration::from_secs(1),
            )
            .unwrap();
        let largest_text = agent
            .run_actions_and_wait_for_target_rect_timeout(
                &[AgentAction::tap(10, 20)],
                AgentTargetSelector::largest_text_region(),
                Duration::from_secs(1),
            )
            .unwrap();
        let best = agent
            .run_actions_and_tap_next_target_at_pointer_timeout(
                &[AgentAction::tap(30, 40)],
                AgentTargetSelector::best_object(),
                pointer,
                0,
                10_000,
                Duration::from_secs(1),
            )
            .unwrap();

        assert_eq!(object.to_pixels(1000, 2000), (100, 200, 400, 300));
        assert_eq!(text.to_pixels(1000, 2000), (700, 800, 800, 900));
        assert_eq!(largest_text.to_pixels(1000, 2000), (100, 200, 400, 300));
        assert_eq!(best.to_pixels(1000, 2000), (300, 400, 500, 500));
        let closed = agent.close().unwrap();
        server.join().unwrap();
        assert_eq!(closed.reader.read_timeout().unwrap(), None);
        let events = touch_events(&closed.transport.bytes);
        assert_eq!(events.len(), 8);
        assert_eq!(
            events[0],
            (TouchAction::DOWN.value(), pointer.value(), 800, 800)
        );
        assert_eq!(
            events[1],
            (TouchAction::UP.value(), pointer.value(), 800, 800)
        );
        assert_eq!(events[2], (TouchAction::DOWN.value(), 0, 10, 20));
        assert_eq!(events[3], (TouchAction::UP.value(), 0, 10, 20));
        assert_eq!(events[4], (TouchAction::DOWN.value(), 0, 30, 40));
        assert_eq!(events[5], (TouchAction::UP.value(), 0, 30, 40));
        assert_eq!(
            events[6],
            (TouchAction::DOWN.value(), pointer.value(), 300, 500)
        );
        assert_eq!(
            events[7],
            (TouchAction::UP.value(), pointer.value(), 300, 500)
        );
    }

    #[test]
    fn tcp_run_actions_and_tap_next_largest_text_region_at_timeout_taps_and_restores_timeout() {
        let stream = frame_summary_envelope_with(
            1,
            &[],
            &[TextRegion {
                x: 100,
                y: 200,
                w: 301,
                h: 101,
            }],
        );
        let (mut agent, server) = tcp_agent_with_reader_bytes(stream);

        agent.set_screen_size(1000, 2000).unwrap();
        let rect = agent
            .run_actions_and_tap_next_largest_text_region_at_timeout(
                &[AgentAction::tap(10, 20)],
                0,
                10_000,
                Duration::from_secs(1),
            )
            .unwrap();

        assert_eq!(rect.to_pixels(1000, 2000), (100, 200, 400, 300));
        let closed = agent.close().unwrap();
        server.join().unwrap();
        assert_eq!(closed.reader.read_timeout().unwrap(), None);
        let events = touch_events(&closed.transport.bytes);
        assert_eq!(events.len(), 4);
        assert_eq!(events[0], (TouchAction::DOWN.value(), 0, 10, 20));
        assert_eq!(events[1], (TouchAction::UP.value(), 0, 10, 20));
        assert_eq!(events[2], (TouchAction::DOWN.value(), 0, 100, 300));
        assert_eq!(events[3], (TouchAction::UP.value(), 0, 100, 300));
    }

    #[test]
    fn tcp_run_actions_and_tap_next_text_region_pointer_timeout_taps_and_restores_timeout() {
        let stream = frame_summary_envelope_with(
            1,
            &[],
            &[TextRegion {
                x: 700,
                y: 800,
                w: 101,
                h: 101,
            }],
        );
        let (mut agent, server) = tcp_agent_with_reader_bytes(stream);
        let pointer = TouchPointerId::VIRTUAL_FINGER;

        agent.set_screen_size(1000, 2000).unwrap();
        let rect = agent
            .run_actions_and_tap_next_text_region_pointer_timeout(
                &[AgentAction::tap(10, 20)],
                0,
                pointer,
                Duration::from_secs(1),
            )
            .unwrap();

        assert_eq!(rect.to_pixels(1000, 2000), (700, 800, 800, 900));
        let closed = agent.close().unwrap();
        server.join().unwrap();
        assert_eq!(closed.reader.read_timeout().unwrap(), None);
        let events = touch_events(&closed.transport.bytes);
        assert_eq!(events.len(), 4);
        assert_eq!(events[0], (TouchAction::DOWN.value(), 0, 10, 20));
        assert_eq!(events[1], (TouchAction::UP.value(), 0, 10, 20));
        assert_eq!(
            events[2],
            (TouchAction::DOWN.value(), pointer.value(), 750, 850)
        );
        assert_eq!(
            events[3],
            (TouchAction::UP.value(), pointer.value(), 750, 850)
        );
    }

    #[test]
    fn tcp_run_actions_and_wait_for_scene_change_timeout_restores_timeout() {
        let mut stream = Vec::new();
        stream.extend(frame_summary_envelope_full(1, 0, &[], &[], &[]));
        stream.extend(frame_summary_envelope_full(
            2,
            crate::ai::FLAG_SCENE_CHANGE,
            &[],
            &[],
            &[],
        ));
        let (mut agent, server) = tcp_agent_with_reader_bytes(stream);

        let summary = agent
            .run_actions_and_wait_for_scene_change_timeout(
                &[AgentAction::tap(10, 20)],
                Duration::from_secs(1),
            )
            .unwrap();

        assert_eq!(summary.frame_seq, 2);
        let closed = agent.close().unwrap();
        server.join().unwrap();
        assert_eq!(closed.reader.read_timeout().unwrap(), None);
        assert_eq!(count_touch_events(&closed.transport.bytes), 2);
    }

    #[test]
    fn tcp_run_actions_and_wait_for_fresh_frame_timeout_restores_timeout() {
        let mut stream = Vec::new();
        stream.extend(frame_summary_envelope_full_at(100, 4, 0, &[], &[], &[]));
        stream.extend(frame_summary_envelope_full_at(120, 6, 0, &[], &[], &[]));
        let (mut agent, server) = tcp_agent_with_reader_bytes(stream);

        let summary = agent
            .run_actions_and_wait_for_frame_summary_after_seq_timeout(
                &[AgentAction::tap(10, 20)],
                5,
                Duration::from_secs(1),
            )
            .unwrap();

        assert_eq!(summary.frame_seq, 6);
        let closed = agent.close().unwrap();
        server.join().unwrap();
        assert_eq!(closed.reader.read_timeout().unwrap(), None);
        assert_eq!(count_touch_events(&closed.transport.bytes), 2);
    }

    #[test]
    fn tcp_run_actions_and_wait_for_largest_text_region_timeout_restores_timeout() {
        let stream = frame_summary_envelope_with(
            1,
            &[],
            &[TextRegion {
                x: 100,
                y: 200,
                w: 301,
                h: 101,
            }],
        );
        let (mut agent, server) = tcp_agent_with_reader_bytes(stream);

        let rect = agent
            .run_actions_and_wait_for_largest_text_region_rect_timeout(
                &[AgentAction::tap(10, 20)],
                Duration::from_secs(1),
            )
            .unwrap();

        assert_eq!(rect.to_pixels(1000, 2000), (100, 200, 400, 300));
        assert_eq!(rect.center().to_pixels(1000, 2000), (250, 250));
        let closed = agent.close().unwrap();
        server.join().unwrap();
        assert_eq!(closed.reader.read_timeout().unwrap(), None);
        assert_eq!(count_touch_events(&closed.transport.bytes), 2);
    }

    #[test]
    fn tcp_wait_timeout_restores_previous_read_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (_sock, _addr) = listener.accept().unwrap();
            std::thread::sleep(Duration::from_millis(80));
        });

        let reader = TcpStream::connect(addr).unwrap();
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let mut agent = AgentControlSession::from_parts(session, reader).unwrap();

        assert!(matches!(
            agent
                .wait_for_clipboard_timeout(Duration::from_millis(5))
                .unwrap_err(),
            Error::AgentTimeout("clipboard")
        ));

        let closed = agent.close().unwrap();
        assert_eq!(closed.reader.read_timeout().unwrap(), None);
        server.join().unwrap();
    }

    #[test]
    fn tcp_run_actions_and_get_clipboard_timeout_taps_requests_and_restores_timeout() {
        let (mut agent, server) = tcp_agent_with_reader_bytes(clipboard("tcp copied"));

        let text = agent
            .run_actions_and_get_clipboard_and_wait_timeout(
                &[AgentAction::tap(10, 20)],
                ClipboardCopyKey::CUT.value(),
                Duration::from_secs(1),
            )
            .unwrap();

        assert_eq!(text, "tcp copied");
        let closed = agent.close().unwrap();
        assert_eq!(closed.reader.read_timeout().unwrap(), None);
        let tags = control_message_tags(&closed.transport.bytes);
        assert_eq!(tags, [2, 2, 8]);
        let get_clipboard =
            find_control_message(&closed.transport.bytes, 8).expect("GET_CLIPBOARD frame");
        assert_eq!(get_clipboard, &[8, ClipboardCopyKey::CUT.value()]);
        server.join().unwrap();
    }

    #[test]
    fn tcp_query_ai_and_wait_stats_timeout_restores_timeout() {
        let (mut agent, server) = tcp_agent_with_reader_bytes(ai_stats_envelope());

        let stats = agent
            .query_ai_and_wait_stats_timeout(0x0102_0304_0506_0708, Duration::from_secs(1))
            .unwrap();

        assert_eq!(stats.frames_sampled, 10);
        let closed = agent.close().unwrap();
        assert_eq!(closed.reader.read_timeout().unwrap(), None);
        let query = find_control_message(&closed.transport.bytes, 23).expect("AI_QUERY frame");
        assert_eq!(
            u64::from_be_bytes(query[1..9].try_into().unwrap()),
            0x0102_0304_0506_0708
        );
        server.join().unwrap();
    }

    #[test]
    fn tcp_run_actions_and_query_ai_and_wait_stats_timeout_restores_timeout() {
        let (mut agent, server) = tcp_agent_with_reader_bytes(ai_stats_envelope());

        let stats = agent
            .run_actions_and_query_ai_and_wait_stats_timeout(
                &[AgentAction::tap(10, 20)],
                0x2122_2324_2526_2728,
                Duration::from_secs(1),
            )
            .unwrap();

        assert_eq!(stats.frames_sampled, 10);
        let closed = agent.close().unwrap();
        assert_eq!(closed.reader.read_timeout().unwrap(), None);
        assert_eq!(control_message_tags(&closed.transport.bytes), [2, 2, 23]);
        let query = find_control_message(&closed.transport.bytes, 23).expect("AI_QUERY frame");
        assert_eq!(
            u64::from_be_bytes(query[1..9].try_into().unwrap()),
            0x2122_2324_2526_2728
        );
        server.join().unwrap();
    }
}
