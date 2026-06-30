//! `CoalescingWriter<T>` — batch nearby `UhidInput` events into a
//! single `write_all` to amortize syscall + kernel-copy cost when an
//! AI agent is producing input at a high rate (e.g. 1 kHz stick jitter
//! or fast typing).
//!
//! Design (matches PRD-B's spec):
//!   - Droppable messages (`UhidInput`) are appended to an internal
//!     buffer. On the next `flush_now` call (window expiry, hard-limit
//!     hit, explicit call, or `Drop`) the buffer is sent to the
//!     underlying transport in a single `write_all`.
//!   - Critical messages (`UhidCreate` / `UhidDestroy`) bypass the
//!     buffer: the pending buffer is flushed first, then the critical
//!     message is sent immediately, mirroring scrcpy's
//!     `sc_control_msg_is_droppable` policy.
//!   - `Drop` flushes any remainder inside `catch_unwind` so a
//!     caller-panic never leaves bytes stuck in the buffer.

use std::time::{Duration, Instant};

use crate::control::message::{ControlMessage, ControlMsgType};
use crate::error::TransportWrite;
use crate::error::{Error, Result};
use crate::session::GamepadFrameRaw;
use crate::types::{dpad_hat_value, HID_MAX_SIZE};

/// Why a `CoalescingWriter::push` call actually wrote bytes to the
/// transport. Returned so callers can instrument the coalescing ratio.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FlushReason {
    /// The pending buffer was below the hard limit and the window
    /// had not expired; the message was buffered for a later flush.
    Pending,
    /// The pending buffer reached the hard byte limit and was force-
    /// flushed before the message (or together with it) was sent.
    Full,
    /// A `flush_now` call (explicit, window-expiry, or Drop) wrote the
    /// accumulated bytes, or an immediate direct-mode write.
    Window,
    /// A critical message triggered an immediate flush of the
    /// pending buffer; the critical message itself was then sent.
    Critical,
}

/// Hard byte limit that forces a flush regardless of timing. Sized to
/// hold ~200 `UhidInput` frames (each ≤ 20 bytes including tag), so a
/// bursty AI agent never holds more than that in memory before
/// pushing to the wire.
pub const DEFAULT_HARD_LIMIT: usize = 4096;

/// Default time window after which a buffered `push` triggers a flush.
pub const DEFAULT_WINDOW: Duration = Duration::from_millis(1);

const DEFAULT_PENDING_CAPACITY: usize = 4096;
const MIN_PENDING_CAPACITY: usize = 64;
const SCRATCH_CAPACITY: usize = 64;
const GAMEPAD_REPORT_SIZE_U16: u16 = 15;
const GAMEPAD_REPORT_SIZE: usize = GAMEPAD_REPORT_SIZE_U16 as usize;
pub(crate) const DIRECT_GAMEPAD_BATCH_FRAMES: usize = 32;
const DIRECT_GAMEPAD_BATCH_BYTES: usize = DIRECT_GAMEPAD_BATCH_FRAMES * (GAMEPAD_REPORT_SIZE + 5);

/// Batches `UhidInput` writes to amortize per-message syscall cost.
#[derive(Debug)]
pub struct CoalescingWriter<T: TransportWrite> {
    transport: T,
    pending: Vec<u8>,
    scratch: Vec<u8>,
    last_flush: Instant,
    window: Duration,
    hard_limit: usize,
    direct: bool,
    /// Stats: total `push` calls.
    pushed: u64,
    /// Stats: total bytes actually written to the transport.
    written: u64,
    /// Stats: number of actual transport writes.
    flushes: u64,
}

impl<T: TransportWrite> CoalescingWriter<T> {
    /// Wrap `transport`. The first message is buffered until the
    /// window expires, the hard limit is hit, or `flush_now` is called.
    pub fn new(transport: T) -> Self {
        Self::with_limits(transport, DEFAULT_WINDOW, DEFAULT_HARD_LIMIT)
    }

    /// Construct a writer that forwards every push immediately (no
    /// buffering). Critical messages are still flushed explicitly in the
    /// same order as before.
    pub fn direct(transport: T) -> Self {
        Self::with_limits(transport, Duration::from_millis(0), 0)
    }

    /// Same as `new` but with a custom window + hard limit. Mostly
    /// useful for tests that want deterministic flush behaviour.
    pub fn with_limits(transport: T, window: Duration, hard_limit: usize) -> Self {
        let pending_cap = hard_limit.clamp(MIN_PENDING_CAPACITY, DEFAULT_PENDING_CAPACITY);
        let direct = hard_limit == 0 && window.is_zero();
        Self {
            transport,
            pending: Vec::with_capacity(pending_cap),
            scratch: Vec::with_capacity(SCRATCH_CAPACITY),
            last_flush: Instant::now(),
            window,
            hard_limit,
            direct,
            pushed: 0,
            written: 0,
            flushes: 0,
        }
    }

    fn push_message_to_scratch(&mut self, msg: &ControlMessage) -> Result<()> {
        self.scratch.clear();
        msg.serialize_into(&mut self.scratch)?;
        Ok(())
    }

    #[inline]
    fn push_uhid_input_to_buf(buf: &mut Vec<u8>, id: u16, size: usize, data: &[u8]) -> Result<()> {
        if size > HID_MAX_SIZE || size > data.len() {
            return Err(Error::ControlMessageTooLarge {
                size,
                max: HID_MAX_SIZE + 5,
            });
        }
        buf.push(ControlMsgType::UhidInput as u8);
        buf.extend_from_slice(&id.to_be_bytes());
        buf.extend_from_slice(&(size as u16).to_be_bytes());
        buf.extend_from_slice(&data[..size]);
        Ok(())
    }

    #[inline]
    fn push_gamepad_input_to_buf(buf: &mut Vec<u8>, id: u16, data: &[u8; GAMEPAD_REPORT_SIZE]) {
        buf.push(ControlMsgType::UhidInput as u8);
        buf.extend_from_slice(&id.to_be_bytes());
        buf.extend_from_slice(&GAMEPAD_REPORT_SIZE_U16.to_be_bytes());
        buf.extend_from_slice(data);
    }

    /// Serialize one packed gamepad frame into a fixed-size scratch buffer
    /// without touching heap-backed buffers.
    #[inline]
    fn encode_gamepad_input_to_array(
        buf: &mut [u8; GAMEPAD_REPORT_SIZE + 5],
        id: u16,
        data: &[u8; GAMEPAD_REPORT_SIZE],
    ) {
        Self::encode_gamepad_input_slice(&mut buf[..], id, data);
    }

    #[inline]
    fn encode_gamepad_input_slice(buf: &mut [u8], id: u16, data: &[u8; GAMEPAD_REPORT_SIZE]) {
        debug_assert!(buf.len() >= GAMEPAD_REPORT_SIZE + 5);
        buf[0] = ControlMsgType::UhidInput as u8;
        let id_be = id.to_be_bytes();
        buf[1] = id_be[0];
        buf[2] = id_be[1];
        let size = GAMEPAD_REPORT_SIZE_U16.to_be_bytes();
        buf[3] = size[0];
        buf[4] = size[1];
        buf[5..].copy_from_slice(data);
    }

    #[inline]
    fn push_gamepad_input_fields_to_buf(buf: &mut Vec<u8>, id: u16, frame: &GamepadFrameRaw) {
        let buttons = frame.buttons;
        let left_x = frame.left_x;
        let left_y = frame.left_y;
        let right_x = frame.right_x;
        let right_y = frame.right_y;
        let left_trigger = frame.left_trigger;
        let right_trigger = frame.right_trigger;
        let left_x = (left_x as i32 + 0x8000) as u16;
        let left_y = (left_y as i32 + 0x8000) as u16;
        let right_x = (right_x as i32 + 0x8000) as u16;
        let right_y = (right_y as i32 + 0x8000) as u16;
        let left_trigger = (left_trigger.max(0) as u16).min(0x7FFF);
        let right_trigger = (right_trigger.max(0) as u16).min(0x7FFF);
        buf.push(ControlMsgType::UhidInput as u8);
        buf.extend_from_slice(&id.to_be_bytes());
        buf.extend_from_slice(&GAMEPAD_REPORT_SIZE_U16.to_be_bytes());
        buf.extend_from_slice(&left_x.to_le_bytes());
        buf.extend_from_slice(&left_y.to_le_bytes());
        buf.extend_from_slice(&right_x.to_le_bytes());
        buf.extend_from_slice(&right_y.to_le_bytes());
        buf.extend_from_slice(&left_trigger.to_le_bytes());
        buf.extend_from_slice(&right_trigger.to_le_bytes());
        buf.extend_from_slice(&(buttons as u16).to_le_bytes());
        buf.push(dpad_hat_value(buttons));
    }

    /// Serialize one gamepad frame from raw fields into a fixed-size
    /// scratch buffer without touching heap-backed buffers.
    #[inline]
    fn encode_gamepad_input_fields_to_array(
        buf: &mut [u8; GAMEPAD_REPORT_SIZE + 5],
        id: u16,
        frame: &GamepadFrameRaw,
    ) {
        let fields = &mut buf[..GAMEPAD_REPORT_SIZE + 5];
        Self::encode_gamepad_input_fields_slice(fields, id, frame);
    }

    #[inline]
    fn encode_gamepad_input_fields_slice(buf: &mut [u8], id: u16, frame: &GamepadFrameRaw) {
        debug_assert!(buf.len() >= GAMEPAD_REPORT_SIZE + 5);
        buf[0] = ControlMsgType::UhidInput as u8;
        let id_be = id.to_be_bytes();
        buf[1] = id_be[0];
        buf[2] = id_be[1];
        let size = GAMEPAD_REPORT_SIZE_U16.to_be_bytes();
        buf[3] = size[0];
        buf[4] = size[1];

        let buttons = frame.buttons;
        let left_x = (frame.left_x as i32 + 0x8000) as u16;
        let left_y = (frame.left_y as i32 + 0x8000) as u16;
        let right_x = (frame.right_x as i32 + 0x8000) as u16;
        let right_y = (frame.right_y as i32 + 0x8000) as u16;
        let left_trigger = (frame.left_trigger.max(0) as u16).min(0x7FFF);
        let right_trigger = (frame.right_trigger.max(0) as u16).min(0x7FFF);

        buf[5..7].copy_from_slice(&left_x.to_le_bytes());
        buf[7..9].copy_from_slice(&left_y.to_le_bytes());
        buf[9..11].copy_from_slice(&right_x.to_le_bytes());
        buf[11..13].copy_from_slice(&right_y.to_le_bytes());
        buf[13..15].copy_from_slice(&left_trigger.to_le_bytes());
        buf[15..17].copy_from_slice(&right_trigger.to_le_bytes());
        buf[17..19].copy_from_slice(&(buttons as u16).to_le_bytes());
        buf[19] = dpad_hat_value(buttons);
    }

    /// Serialize one generic UhidInput message into a fixed-size scratch
    /// buffer without touching heap-backed buffers.
    #[inline]
    fn encode_uhid_input_to_array(
        buf: &mut [u8; HID_MAX_SIZE + 5],
        id: u16,
        size: usize,
        data: &[u8],
    ) -> Result<usize> {
        if size > HID_MAX_SIZE || size > data.len() {
            return Err(Error::ControlMessageTooLarge {
                size,
                max: HID_MAX_SIZE + 5,
            });
        }

        buf[0] = ControlMsgType::UhidInput as u8;
        let id_be = id.to_be_bytes();
        buf[1] = id_be[0];
        buf[2] = id_be[1];
        let size_u16 = size as u16;
        let size_be = size_u16.to_be_bytes();
        buf[3] = size_be[0];
        buf[4] = size_be[1];
        buf[5..(5 + size)].copy_from_slice(&data[..size]);
        Ok(5 + size)
    }

    #[inline]
    pub(crate) fn push_gamepad_input_fields(
        &mut self,
        id: u16,
        frame: &GamepadFrameRaw,
    ) -> Result<FlushReason> {
        if self.direct {
            self.pushed += 1;
            let mut raw = [0u8; GAMEPAD_REPORT_SIZE + 5];
            Self::encode_gamepad_input_fields_to_array(&mut raw, id, frame);
            let serialized_len = raw.len();
            self.transport.write_all(&raw)?;
            self.transport.flush()?;
            self.written += serialized_len as u64;
            self.flushes += 1;
            self.last_flush = Instant::now();
            return Ok(FlushReason::Window);
        }

        self.pushed += 1;
        Self::push_gamepad_input_fields_to_buf(&mut self.pending, id, frame);
        if self.pending.len() >= self.hard_limit {
            self.flush_now()?;
            return Ok(FlushReason::Full);
        }
        if self.last_flush.elapsed() >= self.window && !self.pending.is_empty() {
            self.flush_now()?;
            return Ok(FlushReason::Window);
        }
        Ok(FlushReason::Pending)
    }

    /// Encode a single UHID_INPUT message bypassing the coalescing
    /// accumulator. Reserved for callers that need to flush exactly
    /// one event outside the normal batching window (e.g. when
    /// dropping the writer with one frame still pending). Not used
    /// by the production hot path today — kept for future use.
    #[allow(dead_code)]
    #[inline]
    pub(crate) fn push_uhid_input(
        &mut self,
        id: u16,
        size: u16,
        data: &[u8],
    ) -> Result<FlushReason> {
        let size = size as usize;
        if self.direct {
            self.pushed += 1;
            let mut raw = [0u8; HID_MAX_SIZE + 5];
            let serialized_len = Self::encode_uhid_input_to_array(&mut raw, id, size, data)?;
            self.transport.write_all(&raw[..serialized_len])?;
            self.transport.flush()?;
            self.written += serialized_len as u64;
            self.flushes += 1;
            self.last_flush = Instant::now();
            return Ok(FlushReason::Window);
        }

        self.pushed += 1;
        Self::push_uhid_input_to_buf(&mut self.pending, id, size, data)?;
        if self.pending.len() >= self.hard_limit {
            self.flush_now()?;
            return Ok(FlushReason::Full);
        }
        if self.last_flush.elapsed() >= self.window && !self.pending.is_empty() {
            self.flush_now()?;
            return Ok(FlushReason::Window);
        }
        Ok(FlushReason::Pending)
    }

    #[inline]
    pub(crate) fn push_gamepad_input(
        &mut self,
        id: u16,
        data: &[u8; GAMEPAD_REPORT_SIZE],
    ) -> Result<FlushReason> {
        if self.direct {
            self.pushed += 1;
            let mut raw = [0u8; GAMEPAD_REPORT_SIZE + 5];
            Self::encode_gamepad_input_to_array(&mut raw, id, data);
            let serialized_len = raw.len();
            self.transport.write_all(&raw)?;
            self.transport.flush()?;
            self.written += serialized_len as u64;
            self.flushes += 1;
            self.last_flush = Instant::now();
            return Ok(FlushReason::Window);
        }

        self.pushed += 1;
        Self::push_gamepad_input_to_buf(&mut self.pending, id, data);
        if self.pending.len() >= self.hard_limit {
            self.flush_now()?;
            return Ok(FlushReason::Full);
        }
        if self.last_flush.elapsed() >= self.window && !self.pending.is_empty() {
            self.flush_now()?;
            return Ok(FlushReason::Window);
        }
        Ok(FlushReason::Pending)
    }

    /// Fast path for a batch of fixed-size gamepad reports (no per-item
    /// window check while iterating; window/flush checks happen only once).
    ///
    /// Used by high-rate full-frame gamepad loops to avoid repeated
    /// `Instant::elapsed` work under batched dispatch.
    #[inline]
    pub(crate) fn push_gamepad_input_batch(
        &mut self,
        id: u16,
        data: &[[u8; GAMEPAD_REPORT_SIZE]],
    ) -> Result<FlushReason> {
        if data.is_empty() {
            return Ok(FlushReason::Pending);
        }
        if data.len() == 1 {
            return self.push_gamepad_input(id, &data[0]);
        }
        if self.direct {
            self.pushed += data.len() as u64;
            let mut serialized = 0usize;
            let mut raw = [0u8; DIRECT_GAMEPAD_BATCH_BYTES];
            for chunk in data.chunks(DIRECT_GAMEPAD_BATCH_FRAMES) {
                let mut offset = 0usize;
                for payload in chunk {
                    Self::encode_gamepad_input_slice(
                        &mut raw[offset..offset + GAMEPAD_REPORT_SIZE + 5],
                        id,
                        payload,
                    );
                    offset += GAMEPAD_REPORT_SIZE + 5;
                }
                self.transport.write_all(&raw[..offset])?;
                serialized += offset;
            }
            self.transport.flush()?;
            self.written += serialized as u64;
            self.flushes += 1;
            self.last_flush = Instant::now();
            return Ok(FlushReason::Window);
        }

        let mut reason = FlushReason::Pending;
        self.pushed += data.len() as u64;
        let frame_bytes = data.len() * (GAMEPAD_REPORT_SIZE + 5);
        if self.pending.len() + frame_bytes > self.pending.capacity() {
            self.pending.reserve(frame_bytes);
        }
        if self.pending.len() + frame_bytes <= self.hard_limit {
            for payload in data {
                Self::push_gamepad_input_to_buf(&mut self.pending, id, payload);
            }
            if self.last_flush.elapsed() >= self.window && !self.pending.is_empty() {
                self.flush_now()?;
                return Ok(FlushReason::Window);
            }
            return Ok(FlushReason::Pending);
        }
        for payload in data {
            Self::push_gamepad_input_to_buf(&mut self.pending, id, payload);
            if self.pending.len() >= self.hard_limit {
                self.flush_now()?;
                reason = FlushReason::Full;
            }
        }
        if self.last_flush.elapsed() >= self.window && !self.pending.is_empty() {
            self.flush_now()?;
            reason = FlushReason::Window;
        }
        Ok(reason)
    }

    /// Batch push normalized frame fields without temporary packed
    /// allocations. Keeps coalescing behavior identical while avoiding
    /// per-frame `Vec` churn in unchecked full-state loops.
    #[inline]
    pub(crate) fn push_gamepad_input_batch_from_fields(
        &mut self,
        id: u16,
        frames: &[GamepadFrameRaw],
    ) -> Result<FlushReason> {
        if frames.is_empty() {
            return Ok(FlushReason::Pending);
        }
        if frames.len() == 1 {
            return self.push_gamepad_input_fields(id, &frames[0]);
        }

        if self.direct {
            self.pushed += frames.len() as u64;
            let mut serialized = 0usize;
            let mut raw = [0u8; DIRECT_GAMEPAD_BATCH_BYTES];
            for chunk in frames.chunks(DIRECT_GAMEPAD_BATCH_FRAMES) {
                let mut offset = 0usize;
                for frame in chunk {
                    Self::encode_gamepad_input_fields_slice(
                        &mut raw[offset..offset + GAMEPAD_REPORT_SIZE + 5],
                        id,
                        frame,
                    );
                    offset += GAMEPAD_REPORT_SIZE + 5;
                }
                self.transport.write_all(&raw[..offset])?;
                serialized += offset;
            }
            self.transport.flush()?;
            self.written += serialized as u64;
            self.flushes += 1;
            self.last_flush = Instant::now();
            return Ok(FlushReason::Window);
        }

        let mut reason = FlushReason::Pending;
        self.pushed += frames.len() as u64;
        let frame_bytes = frames.len() * (GAMEPAD_REPORT_SIZE + 5);
        if self.pending.len() + frame_bytes > self.pending.capacity() {
            self.pending.reserve(frame_bytes);
        }
        if self.pending.len() + frame_bytes <= self.hard_limit {
            for frame in frames {
                Self::push_gamepad_input_fields_to_buf(&mut self.pending, id, frame);
            }
            if self.last_flush.elapsed() >= self.window && !self.pending.is_empty() {
                self.flush_now()?;
                return Ok(FlushReason::Window);
            }
            return Ok(FlushReason::Pending);
        }
        for frame in frames {
            Self::push_gamepad_input_fields_to_buf(&mut self.pending, id, frame);
            if self.pending.len() >= self.hard_limit {
                self.flush_now()?;
                reason = FlushReason::Full;
            }
        }
        if self.last_flush.elapsed() >= self.window && !self.pending.is_empty() {
            self.flush_now()?;
            reason = FlushReason::Window;
        }
        Ok(reason)
    }

    #[inline]
    fn push_direct(&mut self, msg: &ControlMessage) -> Result<FlushReason> {
        self.pushed += 1;
        match msg {
            ControlMessage::UhidInput(input) => {
                let mut raw = [0u8; HID_MAX_SIZE + 5];
                let serialized_len = Self::encode_uhid_input_to_array(
                    &mut raw,
                    input.id,
                    input.size as usize,
                    &input.data,
                )?;
                self.transport.write_all(&raw[..serialized_len])?;
                self.transport.flush()?;
                self.written += serialized_len as u64;
                self.flushes += 1;
                self.last_flush = Instant::now();
                Ok(if msg.is_critical() {
                    FlushReason::Critical
                } else {
                    FlushReason::Window
                })
            }
            _ => {
                self.push_message_to_scratch(msg)?;
                // take-restore: move scratch out for write_all, then put it
                // back (preserving any capacity growth) and clear for next push.
                let serialized = std::mem::take(&mut self.scratch);
                let serialized_len = serialized.len();
                self.transport.write_all(&serialized)?;
                self.transport.flush()?;
                self.written += serialized_len as u64;
                self.flushes += 1;
                self.last_flush = Instant::now();
                self.scratch = serialized;
                self.scratch.clear();
                Ok(if msg.is_critical() {
                    FlushReason::Critical
                } else {
                    FlushReason::Window
                })
            }
        }
    }

    /// Push a message. Returns `FlushReason::Pending` if the message
    /// was buffered; `Full` / `Window` / `Critical` if a flush
    /// actually happened as a side effect.
    pub fn push(&mut self, msg: &ControlMessage) -> Result<FlushReason> {
        if self.direct {
            return self.push_direct(msg);
        }
        self.pushed += 1;
        if msg.is_critical() {
            // Flush whatever's pending, then send the critical msg
            // itself. Mirrors scrcpy's `sc_control_msg_is_droppable`
            // invariant: a critical (UHID_CREATE / UHID_DESTROY) must
            // never be dropped.
            self.flush_now()?;
            self.push_message_to_scratch(msg)?;
            // take-restore: move scratch out for write_all, then put it
            // back (preserving any capacity growth) and clear for next push.
            let serialized = std::mem::take(&mut self.scratch);
            self.transport.write_all(&serialized)?;
            self.transport.flush()?;
            self.written += serialized.len() as u64;
            self.flushes += 1;
            self.scratch = serialized;
            self.scratch.clear();
            return Ok(FlushReason::Critical);
        }

        match msg {
            ControlMessage::UhidInput(input) => {
                Self::push_uhid_input_to_buf(
                    &mut self.pending,
                    input.id,
                    input.size as usize,
                    &input.data,
                )?;
            }
            _ => {
                self.push_message_to_scratch(msg)?;
                self.pending.extend_from_slice(&self.scratch);
            }
        }
        if self.pending.len() >= self.hard_limit {
            self.flush_now()?;
            return Ok(FlushReason::Full);
        }
        if self.last_flush.elapsed() >= self.window && !self.pending.is_empty() {
            self.flush_now()?;
            return Ok(FlushReason::Window);
        }
        Ok(FlushReason::Pending)
    }

    /// Force any buffered bytes to the transport. Returns the number
    /// of bytes flushed (0 if nothing was buffered).
    pub fn flush_now(&mut self) -> Result<usize> {
        if self.pending.is_empty() {
            self.last_flush = Instant::now();
            return Ok(0);
        }
        let n = self.pending.len();
        self.transport.write_all(&self.pending)?;
        self.transport.flush()?;
        self.pending.clear();
        self.last_flush = Instant::now();
        self.written += n as u64;
        self.flushes += 1;
        Ok(n)
    }

    /// Number of bytes currently buffered (not yet on the wire).
    #[inline]
    pub fn pending_bytes(&self) -> usize {
        self.pending.len()
    }

    /// Total messages pushed through this writer since construction.
    #[inline]
    pub fn pushed(&self) -> u64 {
        self.pushed
    }

    /// Total bytes actually written to the transport (sum of all
    /// `write_all` payloads, including critical messages).
    #[inline]
    pub fn written(&self) -> u64 {
        self.written
    }

    /// Total number of actual transport writes performed.
    #[inline]
    pub fn flushes(&self) -> u64 {
        self.flushes
    }

    #[inline]
    #[allow(dead_code)]
    pub(crate) fn is_direct(&self) -> bool {
        self.direct
    }

    /// Coalescing ratio: `pushed` / `flushes`. A value of 1 means
    /// every message was written individually; a value of 100 means
    /// 100 messages were batched into one flush on average.
    #[inline]
    pub fn messages_per_flush(&self) -> f64 {
        let f = self.flushes.max(1) as f64;
        self.pushed as f64 / f
    }

    /// Recover the underlying transport after all pending bytes have
    /// been flushed. Disables the `Drop` flush.
    pub fn into_inner(mut self) -> Result<T> {
        self.flush_now()?;
        // We just flushed, so Drop is a no-op. Skip the destructor.
        let transport = unsafe { std::ptr::read(&self.transport) };
        std::mem::forget(self);
        Ok(transport)
    }
}

impl<T: TransportWrite> Drop for CoalescingWriter<T> {
    /// Flush any remainder, swallowing panics. A failure during drop
    /// is logged to stderr so the process isn't aborted by a
    /// double-panic.
    fn drop(&mut self) {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.flush_now()));
        if let Err(panic) = result {
            eprintln!(
                "CoalescingWriter::drop: flush failed during unwind: {:?}",
                panic
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::message::{ControlMessage, UhidDestroy, UhidInput};
    use crate::transport::MockTransport;
    use crate::types::HID_MAX_SIZE;

    fn input_msg(id: u16, byte: u8) -> ControlMessage {
        let mut data = [0u8; HID_MAX_SIZE];
        data[0] = byte;
        ControlMessage::UhidInput(UhidInput { id, size: 8, data })
    }

    #[test]
    fn buffers_droppable_messages() {
        // A UhidInput with size=8 is 5+8=13 bytes on the wire.
        const WIRE_SIZE: usize = 13;
        let mut w = CoalescingWriter::with_limits(
            MockTransport::new(),
            Duration::from_millis(100), // long window
            4096,
        );
        let r1 = w.push(&input_msg(1, 0x10)).unwrap();
        let r2 = w.push(&input_msg(1, 0x20)).unwrap();
        let r3 = w.push(&input_msg(1, 0x30)).unwrap();
        assert_eq!(r1, FlushReason::Pending);
        assert_eq!(r2, FlushReason::Pending);
        assert_eq!(r3, FlushReason::Pending);
        assert_eq!(w.pending_bytes(), 3 * WIRE_SIZE);
        let t = w.into_inner().unwrap();
        let bytes = t.into_bytes();
        assert_eq!(
            bytes.len(),
            3 * WIRE_SIZE,
            "all 3 messages flushed in 1 write_all"
        );
    }

    #[test]
    fn hard_limit_forces_flush() {
        // UhidInput with size=8 = 13 bytes. Set hard_limit to exactly
        // 13 so the first input trips the flush.
        let mut w = CoalescingWriter::with_limits(
            MockTransport::new(),
            Duration::from_secs(60), // window never expires in this test
            13,                      // hard limit = exactly 1 UhidInput
        );
        let r1 = w.push(&input_msg(1, 0x10)).unwrap();
        assert_eq!(
            r1,
            FlushReason::Full,
            "13-byte input hits the 13-byte hard limit"
        );
        let r2 = w.push(&input_msg(1, 0x20)).unwrap();
        assert_eq!(
            r2,
            FlushReason::Full,
            "second 13-byte input also hits the 13-byte limit (buffer was empty after the first force-flush)"
        );
        let t = w.into_inner().unwrap();
        let bytes = t.into_bytes();
        // 1 full input (13B) + 1 full input (13B) = 26B
        assert_eq!(bytes.len(), 26);
    }

    #[test]
    fn critical_message_flushes_then_writes() {
        let mut w =
            CoalescingWriter::with_limits(MockTransport::new(), Duration::from_millis(100), 4096);
        // Buffer some inputs.
        w.push(&input_msg(1, 0x10)).unwrap();
        w.push(&input_msg(1, 0x20)).unwrap();
        assert_eq!(w.pending_bytes(), 26);
        // Critical message — must flush pending + write critical.
        let r = w
            .push(&ControlMessage::UhidDestroy(UhidDestroy { id: 1 }))
            .unwrap();
        assert_eq!(r, FlushReason::Critical);
        let t = w.into_inner().unwrap();
        let bytes = t.into_bytes();
        // 2 buffered inputs (26B) + critical DESTROY (3B) = 29B
        assert_eq!(bytes.len(), 29);
        // The DESTROY (last 3 bytes) must be at the end.
        assert_eq!(&bytes[bytes.len() - 3..], &[14, 0x00, 0x01]);
    }

    #[test]
    fn direct_writer_flushes_immediately() {
        let mut w = CoalescingWriter::direct(MockTransport::new());
        let r1 = w.push(&input_msg(1, 0x10)).unwrap();
        let r2 = w.push(&input_msg(1, 0x20)).unwrap();
        assert_eq!(r1, FlushReason::Window);
        assert_eq!(r2, FlushReason::Window);
        assert_eq!(w.pending_bytes(), 0);
        assert_eq!(w.flushes(), 2);
        assert_eq!(w.pushed(), 2);
        let t = w.into_inner().unwrap();
        let bytes = t.into_bytes();
        // 2 inputs, 13 bytes each.
        assert_eq!(bytes.len(), 26);
    }

    #[test]
    fn direct_gamepad_batch_flushes_once() {
        let mut w = CoalescingWriter::direct(MockTransport::new());
        let payload = [0u8; GAMEPAD_REPORT_SIZE];
        w.push_gamepad_input_batch(3, &[payload, payload]).unwrap();
        assert_eq!(w.flushes(), 1);
        assert_eq!(w.pending_bytes(), 0);
        let t = w.into_inner().unwrap();
        let bytes = t.into_bytes();
        assert_eq!(bytes.len(), 2 * (GAMEPAD_REPORT_SIZE + 5));
    }

    #[test]
    fn direct_fields_batch_flushes_once() {
        let mut w = CoalescingWriter::direct(MockTransport::new());
        let frames = vec![
            GamepadFrameRaw::new(1u32, 0i16, 0i16, 0i16, 0i16, 0i16, 0i16),
            GamepadFrameRaw::new(2u32, 1i16, -1i16, 2i16, -2i16, 10i16, 20i16),
        ];
        w.push_gamepad_input_batch_from_fields(3, &frames).unwrap();
        assert_eq!(w.flushes(), 1);
        assert_eq!(w.pending_bytes(), 0);
        let t = w.into_inner().unwrap();
        let bytes = t.into_bytes();
        assert_eq!(bytes.len(), 2 * (GAMEPAD_REPORT_SIZE + 5));
    }

    #[test]
    fn direct_gamepad_batch_len_one_flushes_once() {
        let mut w = CoalescingWriter::direct(MockTransport::new());
        let payload = [0u8; GAMEPAD_REPORT_SIZE];
        w.push_gamepad_input_batch(3, &[payload]).unwrap();
        assert_eq!(w.flushes(), 1);
        assert_eq!(w.pending_bytes(), 0);
        let t = w.into_inner().unwrap();
        let bytes = t.into_bytes();
        assert_eq!(bytes.len(), GAMEPAD_REPORT_SIZE + 5);
    }

    #[test]
    fn direct_fields_batch_len_one_flushes_once() {
        let mut w = CoalescingWriter::direct(MockTransport::new());
        let frame = GamepadFrameRaw::new(3u32, 0i16, 0i16, 0i16, 0i16, 0i16, 0i16);
        w.push_gamepad_input_batch_from_fields(3, &[frame]).unwrap();
        assert_eq!(w.flushes(), 1);
        assert_eq!(w.pending_bytes(), 0);
        let t = w.into_inner().unwrap();
        let bytes = t.into_bytes();
        assert_eq!(bytes.len(), GAMEPAD_REPORT_SIZE + 5);
    }

    #[test]
    fn direct_fields_single_flushes_once() {
        let mut w = CoalescingWriter::direct(MockTransport::new());
        let frame = GamepadFrameRaw::new(1u32, 0i16, 0i16, 0i16, 0i16, 0i16, 0i16);
        w.push_gamepad_input_fields(3, &frame).unwrap();
        assert_eq!(w.flushes(), 1);
        assert_eq!(w.pending_bytes(), 0);
        let t = w.into_inner().unwrap();
        let bytes = t.into_bytes();
        assert_eq!(bytes.len(), GAMEPAD_REPORT_SIZE + 5);
    }

    #[test]
    fn drop_flushes_remainder() {
        let mut w =
            CoalescingWriter::with_limits(MockTransport::new(), Duration::from_millis(100), 4096);
        w.push(&input_msg(1, 0x10)).unwrap();
        w.push(&input_msg(1, 0x20)).unwrap();
        assert_eq!(w.pending_bytes(), 26);
        // Drop without explicit flush — the bytes should still hit the
        // transport (via the Drop impl).
        let t = w; // moves, then drops at end of scope
        drop(t);
        // Note: we can't observe the bytes here because the transport
        // is also dropped. The next test ("drop_panic_safe") verifies
        // that no panic happens.
    }

    #[test]
    fn drop_panic_safe() {
        use std::panic;
        let w =
            CoalescingWriter::with_limits(MockTransport::new(), Duration::from_millis(100), 4096);
        let r = panic::catch_unwind(panic::AssertUnwindSafe(|| drop(w)));
        assert!(r.is_ok(), "Drop must not panic");
    }

    #[test]
    fn stats_track_messages() {
        let mut w =
            CoalescingWriter::with_limits(MockTransport::new(), Duration::from_millis(100), 4096);
        w.push(&input_msg(1, 0x10)).unwrap();
        w.push(&input_msg(1, 0x20)).unwrap();
        w.flush_now().unwrap();
        assert_eq!(w.pushed(), 2);
        assert_eq!(w.written(), 26);
        assert_eq!(w.pending_bytes(), 0);
        assert!(w.flushes() >= 1);
        assert!((w.messages_per_flush() - 2.0).abs() < f64::EPSILON);
    }

    /// Regression coverage for OPT-2 + OPT-3: the scratch-buffer
    /// take-restore pattern (and extend_from_slice reuse on the
    /// droppable path) must keep functioning under heavy mixed
    /// non-UHID + UHID traffic. Alloc-tracking is implicit — this
    /// test enforces the functional invariant only.
    #[test]
    fn test_scratch_reuse_no_realloc() {
        let mut w =
            CoalescingWriter::with_limits(MockTransport::new(), Duration::from_millis(100), 4096);
        for i in 0..1000u16 {
            let r = w.push(&input_msg(i, (i & 0xFF) as u8)).unwrap();
            // UHID_INPUT is droppable; after a flush-triggering number
            // of pushes the reason must cycle between Pending and
            // Full/Window, never be Critical (these are non-critical).
            if r != FlushReason::Pending {
                assert_eq!(r, FlushReason::Full);
            }
        }
        assert_eq!(w.pushed(), 1000);
        assert!(w.written() > 0);
        assert!(w.flushes() > 0);
        // Round-trip the bytes through MockTransport to make sure the
        // scratch-restored buffer at the critical-path / direct-path
        // sites keeps producing well-formed frames.
        let t = w.into_inner().unwrap();
        let bytes = t.into_bytes();
        assert!(!bytes.is_empty());
    }
}
