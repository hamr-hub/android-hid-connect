//! `MultitouchHandle<T>` — high-level 10-point multi-touch facade
//! borrowed from a [`crate::session::HidSession`]. Each pointer is
//! identified by `pointer_id` in `0..MAX_POINTERS`.

use crate::error::{Error, Result, TransportWrite};
use crate::session::HidSession;

/// Maximum simultaneous touch pointers (matches USB HID Digitizer
/// Usage Tables' "Simultaneous Contacts" upper bound).
pub const MAX_POINTERS: u64 = 10;

pub const ACTION_DOWN:   u8 = 0;
pub const ACTION_UP:     u8 = 1;
pub const ACTION_MOVE:   u8 = 2;
pub const ACTION_CANCEL: u8 = 3;

#[derive(Debug)]
pub struct MultitouchHandle<'a, T: TransportWrite> {
    session: &'a mut HidSession<T>,
    active: [bool; MAX_POINTERS as usize],
}

impl<'a, T: TransportWrite> MultitouchHandle<'a, T> {
    pub(crate) fn new(session: &'a mut HidSession<T>) -> Self {
        Self { session, active: [false; MAX_POINTERS as usize] }
    }

    #[inline]
    pub fn active_count(&self) -> usize {
        self.active.iter().filter(|p| **p).count()
    }

    #[inline]
    pub fn is_active(&self, id: u64) -> bool {
        id < MAX_POINTERS && self.active[id as usize]
    }

    pub fn down(&mut self, id: u64, x: i32, y: i32, pressure: f32) -> Result<()> {
        self.check_id(id)?;
        if self.active[id as usize] {
            return Err(Error::PointerAlreadyDown(id));
        }
        self.session.inject_touch(ACTION_DOWN, id, x, y, pressure)?;
        self.active[id as usize] = true;
        Ok(())
    }

    pub fn move_to(&mut self, id: u64, x: i32, y: i32, pressure: f32) -> Result<()> {
        self.check_id(id)?;
        if !self.active[id as usize] {
            return Err(Error::PointerNotActive(id));
        }
        self.session.inject_touch(ACTION_MOVE, id, x, y, pressure)
    }

    pub fn up(&mut self, id: u64) -> Result<()> {
        self.check_id(id)?;
        if !self.active[id as usize] {
            return Err(Error::PointerNotActive(id));
        }
        self.session.inject_touch(ACTION_UP, id, 0, 0, 0.0)?;
        self.active[id as usize] = false;
        Ok(())
    }

    pub fn cancel(&mut self, id: u64) -> Result<()> {
        self.check_id(id)?;
        if !self.active[id as usize] {
            return Err(Error::PointerNotActive(id));
        }
        self.session.inject_touch(ACTION_CANCEL, id, 0, 0, 0.0)?;
        self.active[id as usize] = false;
        Ok(())
    }

    /// Two-pointer pinch. `p0` / `p1` are
    /// `(pointer_id, x_from, y_from, x_to, y_to)` 5-tuples.
    pub fn pinch(&mut self, p0: (u64, i32, i32, i32, i32),
                 p1: (u64, i32, i32, i32, i32),
                 steps: u32) -> Result<()> {
        let steps = steps.max(2);
        let (id0, x0_from, y0_from, x0_to, y0_to) = p0;
        let (id1, x1_from, y1_from, x1_to, y1_to) = p1;
        self.check_id(id0)?;
        self.check_id(id1)?;
        if !self.active[id0 as usize] || !self.active[id1 as usize] {
            return Err(Error::PointerNotActive(
                if !self.active[id0 as usize] { id0 } else { id1 }));
        }
        for i in 1..=steps {
            let t = i as f32 / steps as f32;
            let x0 = (x0_from as f32 + (x0_to - x0_from) as f32 * t).round() as i32;
            let y0 = (y0_from as f32 + (y0_to - y0_from) as f32 * t).round() as i32;
            let x1 = (x1_from as f32 + (x1_to - x1_from) as f32 * t).round() as i32;
            let y1 = (y1_from as f32 + (y1_to - y1_from) as f32 * t).round() as i32;
            self.session.inject_touch(ACTION_MOVE, id0, x0, y0, 1.0)?;
            self.session.inject_touch(ACTION_MOVE, id1, x1, y1, 1.0)?;
        }
        Ok(())
    }

    pub fn release_all(&mut self) -> Result<()> {
        for id in 0..MAX_POINTERS {
            if self.active[id as usize] {
                self.session.inject_touch(ACTION_UP, id, 0, 0, 0.0)?;
                self.active[id as usize] = false;
            }
        }
        Ok(())
    }

    #[inline]
    pub fn flush_now(&mut self) -> Result<usize> {
        self.session.flush_now()
    }

    #[inline]
    fn check_id(&self, id: u64) -> Result<()> {
        if id >= MAX_POINTERS {
            Err(Error::PointerIdOutOfRange(id, MAX_POINTERS - 1))
        } else {
            Ok(())
        }
    }
}
