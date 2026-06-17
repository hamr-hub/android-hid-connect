//! Error types for android-hid-connect.

/// Errors that can occur when constructing or transmitting HID control messages.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A scancode exceeds the HID keyboard range (0..=0x65) and was not a
    /// modifier; the event was dropped.
    #[error("scancode {0:#x} is out of HID keyboard range")]
    ScancodeOutOfRange(u16),

    /// A control message payload exceeds the 256 KiB hard cap that
    /// scrcpy-server enforces on the control socket.
    #[error("control message too large: {size} bytes (max {max})")]
    ControlMessageTooLarge { size: usize, max: usize },

    /// A name string for UHID_CREATE exceeded 127 bytes (the scrcpy hard cap
    /// for the tiny-string field).
    #[error("device name too long: {size} bytes (max 127)")]
    NameTooLong { size: usize },

    /// A report descriptor exceeded 65535 bytes (the maximum representable
    /// in the u16 length field of UHID_CREATE).
    #[error("report descriptor too long: {size} bytes (max 65535)")]
    ReportDescTooLong { size: usize },

    /// Tried to operate on a gamepad slot that has not been opened via
    /// `GamepadHid::open()`.
    #[error("unknown gamepad id {0}")]
    UnknownGamepad(u32),

    /// No free gamepad slot remaining (scrcpy allows at most 8 concurrent
    /// gamepads).
    #[error("no free gamepad slot (max 8)")]
    NoGamepadSlot,

    /// The underlying transport (TCP / mock) failed.
    #[error("transport error: {0}")]
    Transport(String),

    /// The control buffer is full and a non-droppable message
    /// (UHID_CREATE / UHID_DESTROY) could not be enqueued.
    #[error("control buffer full; cannot drop non-droppable message")]
    BufferFullCritical,

    /// A `HidSession` lifecycle operation failed (open / close / Drop).
    #[error("session lifecycle error: {0}")]
    SessionLifecycle(&'static str),
}

/// Convenience alias for `Result<T, Error>`.
pub type Result<T> = std::result::Result<T, Error>;

/// Lightweight transport error trait so callers can plug custom transports
/// without depending on `std::io` (the library core is `no_std`-friendly).
pub trait TransportWrite {
    /// Write all bytes; partial writes are not allowed.
    fn write_all(&mut self, buf: &[u8]) -> Result<()>;

    /// Flush any buffered bytes; the default impl is a no-op.
    fn flush(&mut self) -> Result<()> {
        Ok(())
    }
}

/// Blanket impl for `std::io::Write` types so users can pass `TcpStream`,
/// `Vec<u8>`, etc. without wrapping.
impl<T: std::io::Write> TransportWrite for T {
    fn write_all(&mut self, buf: &[u8]) -> Result<()> {
        std::io::Write::write_all(self, buf)
            .map_err(|e| Error::Transport(format!("{e}")))
    }

    fn flush(&mut self) -> Result<()> {
        std::io::Write::flush(self).map_err(|e| Error::Transport(format!("{e}")))
    }
}
