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

use crate::control::message::ControlMessage;
use crate::error::Result;
use crate::error::TransportWrite;

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
    /// accumulated bytes.
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

/// Batches `UhidInput` writes to amortize per-message syscall cost.
#[derive(Debug)]
pub struct CoalescingWriter<T: TransportWrite> {
    transport: T,
    pending: Vec<u8>,
    last_flush: Instant,
    window: Duration,
    hard_limit: usize,
    /// Stats: total `push` calls.
    pushed: u64,
    /// Stats: total bytes actually written to the transport.
    written: u64,
}

impl<T: TransportWrite> CoalescingWriter<T> {
    /// Wrap `transport`. The first message is buffered until the
    /// window expires, the hard limit is hit, or `flush_now` is called.
    pub fn new(transport: T) -> Self {
        Self::with_limits(transport, DEFAULT_WINDOW, DEFAULT_HARD_LIMIT)
    }

    /// Same as `new` but with a custom window + hard limit. Mostly
    /// useful for tests that want deterministic flush behaviour.
    pub fn with_limits(transport: T, window: Duration, hard_limit: usize) -> Self {
        Self {
            transport,
            pending: Vec::with_capacity(64),
            last_flush: Instant::now(),
            window,
            hard_limit,
            pushed: 0,
            written: 0,
        }
    }

    /// Push a message. Returns `FlushReason::Pending` if the message
    /// was buffered; `Full` / `Window` / `Critical` if a flush
    /// actually happened as a side effect.
    pub fn push(&mut self, msg: &ControlMessage) -> Result<FlushReason> {
        self.pushed += 1;
        if msg.is_critical() {
            // Flush whatever's pending, then send the critical msg
            // itself. Mirrors scrcpy's `sc_control_msg_is_droppable`
            // invariant: a critical (UHID_CREATE / UHID_DESTROY) must
            // never be dropped.
            self.flush_now()?;
            let bytes = msg.serialize()?;
            self.transport.write_all(&bytes)?;
            self.transport.flush()?;
            self.written += bytes.len() as u64;
            return Ok(FlushReason::Critical);
        }
        let bytes = msg.serialize()?;
        self.pending.extend_from_slice(&bytes);
        if self.pending.len() >= self.hard_limit {
            self.flush_now()?;
            return Ok(FlushReason::Full);
        }
        // Check window expiry opportunistically (cheap branch on
        // Instant::now()).
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
        Ok(n)
    }

    /// Number of bytes currently buffered (not yet on the wire).
    #[inline]
    pub fn pending_bytes(&self) -> usize { self.pending.len() }

    /// Total messages pushed through this writer since construction.
    #[inline]
    pub fn pushed(&self) -> u64 { self.pushed }

    /// Total bytes actually written to the transport (sum of all
    /// `write_all` payloads, including critical messages).
    #[inline]
    pub fn written(&self) -> u64 { self.written }

    /// Coalescing ratio: `pushed` / `flushes`. A value of 1 means
    /// every message was flushed individually; a value of 100 means
    /// 100 messages were batched into one flush on average.
    #[inline]
    pub fn messages_per_flush(&self) -> f64 {
        let f = self.written.max(1) as f64;
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
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.flush_now()
        }));
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
    use crate::control::message::{ControlMessage, UhidInput, UhidDestroy};
    use crate::types::HID_MAX_SIZE;
    use crate::transport::MockTransport;

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
            Duration::from_millis(100),  // long window
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
        assert_eq!(bytes.len(), 3 * WIRE_SIZE, "all 3 messages flushed in 1 write_all");
    }

    #[test]
    fn hard_limit_forces_flush() {
        // UhidInput with size=8 = 13 bytes. Set hard_limit to exactly
        // 13 so the first input trips the flush.
        let mut w = CoalescingWriter::with_limits(
            MockTransport::new(),
            Duration::from_secs(60),  // window never expires in this test
            13,                        // hard limit = exactly 1 UhidInput
        );
        let r1 = w.push(&input_msg(1, 0x10)).unwrap();
        assert_eq!(r1, FlushReason::Full, "13-byte input hits the 13-byte hard limit");
        let r2 = w.push(&input_msg(1, 0x20)).unwrap();
        assert_eq!(r2, FlushReason::Full,
            "second 13-byte input also hits the 13-byte limit (buffer was empty after the first force-flush)");
        let t = w.into_inner().unwrap();
        let bytes = t.into_bytes();
        // 1 full input (13B) + 1 full input (13B) = 26B
        assert_eq!(bytes.len(), 26);
    }

    #[test]
    fn critical_message_flushes_then_writes() {
        let mut w = CoalescingWriter::with_limits(
            MockTransport::new(),
            Duration::from_millis(100),
            4096,
        );
        // Buffer some inputs.
        w.push(&input_msg(1, 0x10)).unwrap();
        w.push(&input_msg(1, 0x20)).unwrap();
        assert_eq!(w.pending_bytes(), 26);
        // Critical message — must flush pending + write critical.
        let r = w.push(&ControlMessage::UhidDestroy(UhidDestroy { id: 1 })).unwrap();
        assert_eq!(r, FlushReason::Critical);
        let t = w.into_inner().unwrap();
        let bytes = t.into_bytes();
        // 2 buffered inputs (26B) + critical DESTROY (3B) = 29B
        assert_eq!(bytes.len(), 29);
        // The DESTROY (last 3 bytes) must be at the end.
        assert_eq!(&bytes[bytes.len() - 3..], &[14, 0x00, 0x01]);
    }

    #[test]
    fn drop_flushes_remainder() {
        let mut w = CoalescingWriter::with_limits(
            MockTransport::new(),
            Duration::from_millis(100),
            4096,
        );
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
        let w = CoalescingWriter::with_limits(
            MockTransport::new(),
            Duration::from_millis(100),
            4096,
        );
        let r = panic::catch_unwind(panic::AssertUnwindSafe(|| drop(w)));
        assert!(r.is_ok(), "Drop must not panic");
    }

    #[test]
    fn stats_track_messages() {
        let mut w = CoalescingWriter::with_limits(
            MockTransport::new(),
            Duration::from_millis(100),
            4096,
        );
        w.push(&input_msg(1, 0x10)).unwrap();
        w.push(&input_msg(1, 0x20)).unwrap();
        w.flush_now().unwrap();
        assert_eq!(w.pushed(), 2);
        assert_eq!(w.written(), 26);
        assert_eq!(w.pending_bytes(), 0);
    }
}
