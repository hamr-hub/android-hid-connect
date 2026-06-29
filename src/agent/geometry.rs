use std::io;

use crate::ai::{FrameSummary, ObjectBox, TextRegion};
use crate::error::{Error, Result};

pub(super) fn io_to_error(e: io::Error) -> Error {
    Error::Transport(format!("{e}"))
}

pub(super) fn io_to_wait_error(e: io::Error, operation: &'static str) -> Error {
    match e.kind() {
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => Error::AgentTimeout(operation),
        _ => io_to_error(e),
    }
}

#[inline]
pub(super) fn basis_points_to_unit(value: u16) -> u16 {
    (((value as u32) * (u16::MAX as u32) + 5_000) / 10_000) as u16
}

#[inline]
pub(super) fn normalized_rect_axis_at_basis_points(a: u16, b: u16, value: u16) -> u16 {
    let start = a.min(b) as u32;
    let end = a.max(b) as u32;
    (start + (((end - start) * (value as u32) + 5_000) / 10_000)) as u16
}

#[inline]
pub(super) fn normalized_axis_to_pixel(value: u16, extent: u16) -> i32 {
    if extent <= 1 {
        return 0;
    }
    (((value as u64) * ((extent - 1) as u64) + ((u16::MAX as u64) / 2)) / (u16::MAX as u64)) as i32
}

#[inline]
pub(super) fn pixel_axis_to_unit(value: i32, extent: u16) -> Result<u16> {
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
pub(super) fn pixel_rect_axis_to_unit(start: i32, len: i32, extent: u16) -> Result<(u16, u16)> {
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

pub(super) fn best_object(objects: impl IntoIterator<Item = ObjectBox>) -> Option<ObjectBox> {
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
pub(super) fn frame_summary_is_stable(summary: &FrameSummary) -> bool {
    !summary.is_scene_change() && !summary.is_moving()
}

#[inline]
pub(super) fn object_score(object: ObjectBox) -> (u8, u32) {
    (object.confidence, object_area(object))
}

#[inline]
pub(super) fn object_area(object: ObjectBox) -> u32 {
    object.w as u32 * object.h as u32
}

#[inline]
pub(super) fn text_region_area(region: &TextRegion) -> u32 {
    region.w as u32 * region.h as u32
}

#[inline]
pub(super) fn lerp_i32(a: i32, b: i32, t: f32) -> i32 {
    (a as f32 + (b - a) as f32 * t).round() as i32
}
