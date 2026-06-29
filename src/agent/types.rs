use crate::ai::{FrameSummary, ObjectBox, TextRegion};
use crate::client::{ScrollFrame, TouchFrame};
use crate::error::{Error, Result};
use crate::types::{TouchAction, TouchPointerId};

use super::geometry::{
    basis_points_to_unit, best_object, normalized_axis_to_pixel,
    normalized_rect_axis_at_basis_points, pixel_rect_axis_to_unit, text_region_area,
};

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

    pub(super) fn into_touch_frame(self) -> TouchFrame {
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

    pub(super) fn into_scroll_frame(self) -> ScrollFrame {
        ScrollFrame::new(
            self.x,
            self.y,
            self.hscroll as f32,
            self.vscroll as f32,
            self.buttons,
        )
    }
}
