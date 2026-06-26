//! Device-to-host messages emitted by scrcpy-server on the control socket.
//!
//! This is the reverse direction of [`crate::control`]: after the host sends
//! control messages, the device may reply with clipboard updates, clipboard
//! acknowledgements, or UHID output reports such as keyboard LED state.
//!
//! The native scrcpy wire format is **not** a generic envelope. Only clipboard
//! messages carry a `u32` text length:
//!
//! ```text
//! CLIPBOARD       type(1) + text_len(4 BE) + UTF-8 text
//! ACK_CLIPBOARD   type(1) + sequence(8 BE)
//! UHID_OUTPUT     type(1) + id(2 BE) + size(2 BE) + data
//! ```

use std::io::{self, Read};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::ai::{AiStats, FrameSummary, TYPE_AI_STATS, TYPE_FRAME_SUMMARY};

/// Maximum size of a native scrcpy device message.
pub const DEVICE_MSG_MAX_SIZE: usize = 1 << 18;

/// Maximum byte length for a clipboard text payload.
pub const DEVICE_MSG_TEXT_MAX_LENGTH: usize = DEVICE_MSG_MAX_SIZE - 5;

/// `DEVICE_MSG_TYPE_CLIPBOARD`.
pub const TYPE_CLIPBOARD: u8 = 0;

/// `DEVICE_MSG_TYPE_ACK_CLIPBOARD`.
pub const TYPE_ACK_CLIPBOARD: u8 = 1;

/// `DEVICE_MSG_TYPE_UHID_OUTPUT`.
pub const TYPE_UHID_OUTPUT: u8 = 2;

/// Default bound for the background device-message channel.
pub const DEFAULT_DEVICE_MESSAGE_BOUND: usize = 64;

/// Fixed scrcpy device-name field length in the control socket prefix.
pub const DEVICE_NAME_FIELD_LENGTH: usize = 64;

/// Out-of-band prefix sent by scrcpy-server before any `DeviceMessage`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScrcpyControlPrefix {
    /// The dummy byte sent when `send_dummy_byte=true`.
    pub dummy_byte: u8,
    /// Device name decoded from the fixed 64-byte, NUL-padded field.
    pub device_name: String,
    /// Raw fixed-size device name bytes.
    pub raw_device_name: [u8; DEVICE_NAME_FIELD_LENGTH],
}

/// One parsed native scrcpy device message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceMessage {
    /// Device clipboard content changed or was requested via `GET_CLIPBOARD`.
    Clipboard(String),

    /// Acknowledgement for `SET_CLIPBOARD`.
    AckClipboard { sequence: u64 },

    /// UHID output report from Android back to the virtual HID device.
    UhidOutput { id: u16, data: Vec<u8> },
}

/// One parsed server→host event from either native scrcpy or the AI extension.
#[derive(Debug, Clone, PartialEq)]
pub enum DeviceEvent {
    /// Native scrcpy device message: clipboard, ACK, or UHID output.
    Native(DeviceMessage),

    /// AI extension frame summary envelope.
    FrameSummary(FrameSummary),

    /// AI extension statistics envelope.
    AiStats(AiStats),

    /// Forward-compatible AI-style envelope with an unknown type tag.
    UnknownEnvelope { msg_type: u8, payload: Vec<u8> },
}

/// Cloneable snapshot of the latest AI frame summary observed by a
/// [`LatestFrameSummaryReceiver`].
#[derive(Debug, Clone, PartialEq)]
pub struct LatestFrameSummarySnapshot {
    /// Monotonic local version assigned by the background latest-frame pump.
    pub version: u64,
    /// Newest parsed frame summary at this version.
    pub summary: FrameSummary,
}

impl LatestFrameSummarySnapshot {
    /// Return a boundary token at this snapshot's local latest-frame version.
    pub const fn boundary(&self) -> LatestFrameSummaryBoundary {
        LatestFrameSummaryBoundary::new(self.version)
    }
}

/// Copyable marker for a latest-frame observation boundary.
///
/// AI loops can capture this before planning and later wait for any cached or
/// newly-pumped frame with a greater local latest-frame version, without
/// passing raw `u64` counters through planner code.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LatestFrameSummaryBoundary {
    version: u64,
}

impl LatestFrameSummaryBoundary {
    /// Create a boundary from a local latest-frame receiver version.
    pub const fn new(version: u64) -> Self {
        Self { version }
    }

    /// Create a boundary at `snapshot.version`.
    pub const fn from_snapshot(snapshot: &LatestFrameSummarySnapshot) -> Self {
        snapshot.boundary()
    }

    /// Return the local latest-frame version represented by this boundary.
    pub const fn version(self) -> u64 {
        self.version
    }

    /// Return whether `snapshot` is newer than this boundary.
    pub const fn accepts(self, snapshot: &LatestFrameSummarySnapshot) -> bool {
        snapshot.version > self.version
    }
}

/// Consistent one-read latest-frame observation.
///
/// [`LatestFrameSummaryReceiver::observe`] returns this after taking a single
/// receiver lock, so the boundary and optional snapshot describe the same pump
/// state. This is the low-overhead starting point for AI loops that observe,
/// plan, dispatch actions, then wait after the captured boundary.
#[derive(Debug, Clone, PartialEq)]
pub struct LatestFrameSummaryObservation {
    /// Boundary at the receiver version observed by this read.
    pub boundary: LatestFrameSummaryBoundary,
    /// Latest frame snapshot present at `boundary`, if any frame has arrived.
    pub snapshot: Option<LatestFrameSummarySnapshot>,
}

impl LatestFrameSummaryObservation {
    /// Create an observation from a captured boundary and optional cached
    /// snapshot.
    ///
    /// This is useful for receivers and tests that already hold both values
    /// from one logical read. If `snapshot` is present, its version should
    /// match `boundary.version()`.
    pub fn from_parts(
        boundary: LatestFrameSummaryBoundary,
        snapshot: Option<LatestFrameSummarySnapshot>,
    ) -> Self {
        if let Some(snapshot) = &snapshot {
            debug_assert_eq!(snapshot.version, boundary.version());
        }
        Self { boundary, snapshot }
    }

    /// Create an observation at `boundary` without a cached frame snapshot.
    pub const fn at_boundary(boundary: LatestFrameSummaryBoundary) -> Self {
        Self {
            boundary,
            snapshot: None,
        }
    }

    /// Create an observation at a raw local latest-frame receiver version
    /// without a cached frame snapshot.
    pub const fn at_version(version: u64) -> Self {
        Self::at_boundary(LatestFrameSummaryBoundary::new(version))
    }

    /// Create an observation whose boundary is `snapshot.version` and whose
    /// cached snapshot is `snapshot`.
    pub fn from_snapshot(snapshot: LatestFrameSummarySnapshot) -> Self {
        Self {
            boundary: snapshot.boundary(),
            snapshot: Some(snapshot),
        }
    }

    /// Return the boundary captured by this observation.
    pub const fn boundary(&self) -> LatestFrameSummaryBoundary {
        self.boundary
    }

    /// Return the local latest-frame version captured by this observation.
    pub const fn boundary_version(&self) -> u64 {
        self.boundary.version()
    }

    /// Return whether this observation contains a frame snapshot.
    pub const fn has_snapshot(&self) -> bool {
        self.snapshot.is_some()
    }

    /// Return the snapshot captured by this observation, if any.
    pub fn snapshot(&self) -> Option<&LatestFrameSummarySnapshot> {
        self.snapshot.as_ref()
    }

    /// Consume this observation and return its cached snapshot, if any.
    pub fn into_snapshot(self) -> Option<LatestFrameSummarySnapshot> {
        self.snapshot
    }

    /// Return the frame summary captured by this observation, if any.
    pub fn summary(&self) -> Option<&FrameSummary> {
        self.snapshot().map(|snapshot| &snapshot.summary)
    }

    /// Return whether `snapshot` is newer than this observation's boundary.
    pub const fn accepts(&self, snapshot: &LatestFrameSummarySnapshot) -> bool {
        self.boundary.accepts(snapshot)
    }
}

/// Cloneable representation of a terminal device read error.
///
/// `std::io::Error` is not `Clone`, but latest-frame waiters may need to see
/// the same terminal read/protocol error from multiple handles.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceReadError {
    /// Original [`io::ErrorKind`].
    pub kind: io::ErrorKind,
    /// Original error message.
    pub message: String,
}

impl DeviceReadError {
    /// Convert a non-cloneable [`io::Error`] into a cloneable device read error.
    pub fn from_io_error(err: io::Error) -> Self {
        Self {
            kind: err.kind(),
            message: err.to_string(),
        }
    }

    /// Recreate an [`io::Error`] with the captured kind and message.
    pub fn to_io_error(&self) -> io::Error {
        io::Error::new(self.kind, self.message.clone())
    }
}

#[derive(Debug, Default)]
struct LatestFrameSummaryState {
    latest: Option<LatestFrameSummarySnapshot>,
    version: u64,
    terminal_error: Option<DeviceReadError>,
}

#[derive(Debug, Default)]
struct LatestFrameSummaryInner {
    state: Mutex<LatestFrameSummaryState>,
    updated: Condvar,
}

/// Low-latency latest-frame view over a native-or-AI event stream.
///
/// Unlike [`spawn_device_event_receiver`], this receiver intentionally does not
/// preserve every event. Its background pump continuously drains the mixed
/// server→host stream, skips non-frame events, and overwrites the cached
/// [`FrameSummary`] so perception loops can read or wait on the newest frame
/// without replaying stale summaries.
#[derive(Debug, Clone, Default)]
pub struct LatestFrameSummaryReceiver {
    inner: Arc<LatestFrameSummaryInner>,
}

impl LatestFrameSummaryReceiver {
    /// Return the newest frame snapshot currently available.
    pub fn snapshot(&self) -> Option<LatestFrameSummarySnapshot> {
        self.inner.state.lock().unwrap().latest.clone()
    }

    /// Return a consistent current boundary plus optional newest snapshot.
    pub fn observe(&self) -> LatestFrameSummaryObservation {
        let state = self.inner.state.lock().unwrap();
        LatestFrameSummaryObservation::from_parts(
            LatestFrameSummaryBoundary::new(state.version),
            state.latest.clone(),
        )
    }

    /// Return the current local latest-frame version.
    pub fn version(&self) -> u64 {
        self.inner.state.lock().unwrap().version
    }

    /// Return a copyable marker for the current local latest-frame version.
    pub fn boundary(&self) -> LatestFrameSummaryBoundary {
        LatestFrameSummaryBoundary::new(self.version())
    }

    /// Return the terminal read/protocol error, if the pump has stopped.
    pub fn terminal_error(&self) -> Option<DeviceReadError> {
        self.inner.state.lock().unwrap().terminal_error.clone()
    }

    /// Return the current snapshot if it is newer than `after_version`.
    pub fn snapshot_after_version(&self, after_version: u64) -> Option<LatestFrameSummarySnapshot> {
        let state = self.inner.state.lock().unwrap();
        if state.version > after_version {
            state.latest.clone()
        } else {
            None
        }
    }

    /// Return the current snapshot if it is newer than `boundary`.
    pub fn snapshot_after_boundary(
        &self,
        boundary: LatestFrameSummaryBoundary,
    ) -> Option<LatestFrameSummarySnapshot> {
        self.snapshot_after_version(boundary.version())
    }

    /// Return the current snapshot if it is newer than
    /// `observation.boundary`.
    pub fn snapshot_after_observation(
        &self,
        observation: &LatestFrameSummaryObservation,
    ) -> Option<LatestFrameSummarySnapshot> {
        self.snapshot_after_boundary(observation.boundary())
    }

    /// Return the current snapshot if `frame_seq > min_frame_seq`.
    pub fn snapshot_after_frame_seq(
        &self,
        min_frame_seq: u32,
    ) -> Option<LatestFrameSummarySnapshot> {
        self.snapshot()
            .filter(|snapshot| snapshot.summary.frame_seq > min_frame_seq)
    }

    /// Return the current snapshot if `timestamp_ms > min_timestamp_ms`.
    pub fn snapshot_after_timestamp(
        &self,
        min_timestamp_ms: u64,
    ) -> Option<LatestFrameSummarySnapshot> {
        self.snapshot()
            .filter(|snapshot| snapshot.summary.timestamp_ms > min_timestamp_ms)
    }

    /// Return the current snapshot if it is accepted by `predicate`.
    pub fn snapshot_matching(
        &self,
        mut predicate: impl FnMut(&LatestFrameSummarySnapshot) -> bool,
    ) -> Option<LatestFrameSummarySnapshot> {
        self.snapshot().filter(|snapshot| predicate(snapshot))
    }

    /// Block until a frame snapshot newer than `after_version` is available.
    pub fn wait_next(&self, after_version: u64) -> io::Result<LatestFrameSummarySnapshot> {
        self.wait_next_matching(after_version, |_| true)
    }

    /// Block until a frame snapshot newer than `boundary` is available.
    pub fn wait_next_after_boundary(
        &self,
        boundary: LatestFrameSummaryBoundary,
    ) -> io::Result<LatestFrameSummarySnapshot> {
        self.wait_next(boundary.version())
    }

    /// Block until a frame snapshot newer than `observation.boundary` is
    /// available.
    pub fn wait_next_after_observation(
        &self,
        observation: &LatestFrameSummaryObservation,
    ) -> io::Result<LatestFrameSummarySnapshot> {
        self.wait_next_after_boundary(observation.boundary())
    }

    /// Block until a frame snapshot newer than `after_version` and accepted by
    /// `predicate` is available.
    pub fn wait_next_matching(
        &self,
        after_version: u64,
        mut predicate: impl FnMut(&LatestFrameSummarySnapshot) -> bool,
    ) -> io::Result<LatestFrameSummarySnapshot> {
        self.wait_matching(move |snapshot| snapshot.version > after_version && predicate(snapshot))
    }

    /// Block until a frame snapshot newer than `boundary` and accepted by
    /// `predicate` is available.
    pub fn wait_next_matching_after_boundary(
        &self,
        boundary: LatestFrameSummaryBoundary,
        predicate: impl FnMut(&LatestFrameSummarySnapshot) -> bool,
    ) -> io::Result<LatestFrameSummarySnapshot> {
        self.wait_next_matching(boundary.version(), predicate)
    }

    /// Block until a frame snapshot newer than `observation.boundary` and
    /// accepted by `predicate` is available.
    pub fn wait_next_matching_after_observation(
        &self,
        observation: &LatestFrameSummaryObservation,
        predicate: impl FnMut(&LatestFrameSummarySnapshot) -> bool,
    ) -> io::Result<LatestFrameSummarySnapshot> {
        self.wait_next_matching_after_boundary(observation.boundary(), predicate)
    }

    /// Block until the first frame snapshot is available.
    pub fn wait_first(&self) -> io::Result<LatestFrameSummarySnapshot> {
        self.wait_next(0)
    }

    /// Block until a frame snapshot accepted by `predicate` is available.
    pub fn wait_matching(
        &self,
        mut predicate: impl FnMut(&LatestFrameSummarySnapshot) -> bool,
    ) -> io::Result<LatestFrameSummarySnapshot> {
        let mut state = self.inner.state.lock().unwrap();
        loop {
            if let Some(snapshot) = &state.latest {
                if predicate(snapshot) {
                    return Ok(snapshot.clone());
                }
            }
            if let Some(err) = &state.terminal_error {
                return Err(err.to_io_error());
            }
            state = self.inner.updated.wait(state).unwrap();
        }
    }

    /// Block until a frame snapshot newer than `after_version` is available,
    /// bounded by `timeout`.
    pub fn wait_next_timeout(
        &self,
        after_version: u64,
        timeout: Duration,
    ) -> io::Result<LatestFrameSummarySnapshot> {
        self.wait_next_matching_timeout(after_version, timeout, |_| true)
    }

    /// Block until a frame snapshot newer than `boundary` is available, bounded
    /// by `timeout`.
    pub fn wait_next_after_boundary_timeout(
        &self,
        boundary: LatestFrameSummaryBoundary,
        timeout: Duration,
    ) -> io::Result<LatestFrameSummarySnapshot> {
        self.wait_next_timeout(boundary.version(), timeout)
    }

    /// Block until a frame snapshot newer than `observation.boundary` is
    /// available, bounded by `timeout`.
    pub fn wait_next_after_observation_timeout(
        &self,
        observation: &LatestFrameSummaryObservation,
        timeout: Duration,
    ) -> io::Result<LatestFrameSummarySnapshot> {
        self.wait_next_after_boundary_timeout(observation.boundary(), timeout)
    }

    /// Block until a frame snapshot newer than `after_version` and accepted by
    /// `predicate` is available, bounded by `timeout`.
    pub fn wait_next_matching_timeout(
        &self,
        after_version: u64,
        timeout: Duration,
        mut predicate: impl FnMut(&LatestFrameSummarySnapshot) -> bool,
    ) -> io::Result<LatestFrameSummarySnapshot> {
        self.wait_matching_timeout(timeout, move |snapshot| {
            snapshot.version > after_version && predicate(snapshot)
        })
    }

    /// Block until a frame snapshot newer than `boundary` and accepted by
    /// `predicate` is available, bounded by `timeout`.
    pub fn wait_next_matching_after_boundary_timeout(
        &self,
        boundary: LatestFrameSummaryBoundary,
        timeout: Duration,
        predicate: impl FnMut(&LatestFrameSummarySnapshot) -> bool,
    ) -> io::Result<LatestFrameSummarySnapshot> {
        self.wait_next_matching_timeout(boundary.version(), timeout, predicate)
    }

    /// Block until a frame snapshot newer than `observation.boundary` and
    /// accepted by `predicate` is available, bounded by `timeout`.
    pub fn wait_next_matching_after_observation_timeout(
        &self,
        observation: &LatestFrameSummaryObservation,
        timeout: Duration,
        predicate: impl FnMut(&LatestFrameSummarySnapshot) -> bool,
    ) -> io::Result<LatestFrameSummarySnapshot> {
        self.wait_next_matching_after_boundary_timeout(observation.boundary(), timeout, predicate)
    }

    /// Block until the first frame snapshot is available, bounded by
    /// `timeout`.
    pub fn wait_first_timeout(&self, timeout: Duration) -> io::Result<LatestFrameSummarySnapshot> {
        self.wait_next_timeout(0, timeout)
    }

    /// Block until the latest cached frame has `frame_seq > min_frame_seq`.
    pub fn wait_after_frame_seq(
        &self,
        min_frame_seq: u32,
    ) -> io::Result<LatestFrameSummarySnapshot> {
        self.wait_matching(|snapshot| snapshot.summary.frame_seq > min_frame_seq)
    }

    /// Block until the latest cached frame has `frame_seq > min_frame_seq`,
    /// bounded by `timeout`.
    pub fn wait_after_frame_seq_timeout(
        &self,
        min_frame_seq: u32,
        timeout: Duration,
    ) -> io::Result<LatestFrameSummarySnapshot> {
        self.wait_matching_timeout(timeout, |snapshot| {
            snapshot.summary.frame_seq > min_frame_seq
        })
    }

    /// Block until the latest cached frame has `timestamp_ms > min_timestamp_ms`.
    pub fn wait_after_timestamp(
        &self,
        min_timestamp_ms: u64,
    ) -> io::Result<LatestFrameSummarySnapshot> {
        self.wait_matching(|snapshot| snapshot.summary.timestamp_ms > min_timestamp_ms)
    }

    /// Block until the latest cached frame has
    /// `timestamp_ms > min_timestamp_ms`, bounded by `timeout`.
    pub fn wait_after_timestamp_timeout(
        &self,
        min_timestamp_ms: u64,
        timeout: Duration,
    ) -> io::Result<LatestFrameSummarySnapshot> {
        self.wait_matching_timeout(timeout, |snapshot| {
            snapshot.summary.timestamp_ms > min_timestamp_ms
        })
    }

    /// Block until a frame snapshot accepted by `predicate` is available,
    /// bounded by `timeout`.
    pub fn wait_matching_timeout(
        &self,
        timeout: Duration,
        mut predicate: impl FnMut(&LatestFrameSummarySnapshot) -> bool,
    ) -> io::Result<LatestFrameSummarySnapshot> {
        let deadline = Instant::now() + timeout;
        let mut state = self.inner.state.lock().unwrap();
        loop {
            if let Some(snapshot) = &state.latest {
                if predicate(snapshot) {
                    return Ok(snapshot.clone());
                }
            }
            if let Some(err) = &state.terminal_error {
                return Err(err.to_io_error());
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "latest frame summary timeout",
                ));
            }
            let remaining = deadline.saturating_duration_since(now);
            let (next_state, _) = self.inner.updated.wait_timeout(state, remaining).unwrap();
            state = next_state;
        }
    }

    fn store_summary(&self, summary: FrameSummary) {
        let mut state = self.inner.state.lock().unwrap();
        state.version = state.version.saturating_add(1);
        let version = state.version;
        state.latest = Some(LatestFrameSummarySnapshot { version, summary });
        self.inner.updated.notify_all();
    }

    fn store_terminal_error(&self, err: io::Error) {
        let mut state = self.inner.state.lock().unwrap();
        state.terminal_error = Some(DeviceReadError::from_io_error(err));
        self.inner.updated.notify_all();
    }

    fn is_abandoned(&self) -> bool {
        Arc::strong_count(&self.inner) == 1
    }
}

impl DeviceEvent {
    /// Server→host event type tag.
    #[inline]
    pub fn msg_type(&self) -> u8 {
        match self {
            Self::Native(message) => message.msg_type(),
            Self::FrameSummary(_) => TYPE_FRAME_SUMMARY,
            Self::AiStats(_) => TYPE_AI_STATS,
            Self::UnknownEnvelope { msg_type, .. } => *msg_type,
        }
    }

    /// One-line representation suitable for logs and agent traces.
    pub fn describe(&self) -> String {
        match self {
            Self::Native(message) => message.describe(),
            Self::FrameSummary(summary) => {
                format!(
                    "DEVICE_EVT_FRAME_SUMMARY frame#{} {}x{} objects={} text={}",
                    summary.frame_seq,
                    summary.width,
                    summary.height,
                    summary.objects.len(),
                    summary.text_regions.len()
                )
            }
            Self::AiStats(stats) => {
                format!(
                    "DEVICE_EVT_AI_STATS sampled={} fps={:.2} latency_ms={:.2}",
                    stats.frames_sampled, stats.current_fps, stats.avg_latency_ms
                )
            }
            Self::UnknownEnvelope { msg_type, payload } => {
                format!(
                    "DEVICE_EVT_UNKNOWN_ENVELOPE type={msg_type} len={}",
                    payload.len()
                )
            }
        }
    }
}

impl From<DeviceMessage> for DeviceEvent {
    fn from(value: DeviceMessage) -> Self {
        Self::Native(value)
    }
}

impl DeviceMessage {
    /// Native scrcpy device message type tag.
    #[inline]
    pub fn msg_type(&self) -> u8 {
        match self {
            Self::Clipboard(_) => TYPE_CLIPBOARD,
            Self::AckClipboard { .. } => TYPE_ACK_CLIPBOARD,
            Self::UhidOutput { .. } => TYPE_UHID_OUTPUT,
        }
    }

    /// One-line representation suitable for logs and agent traces.
    pub fn describe(&self) -> String {
        match self {
            Self::Clipboard(text) => format!("DEVICE_MSG_CLIPBOARD len={}", text.len()),
            Self::AckClipboard { sequence } => {
                format!("DEVICE_MSG_ACK_CLIPBOARD seq={sequence}")
            }
            Self::UhidOutput { id, data } => {
                format!("DEVICE_MSG_UHID_OUTPUT id={id} size={}", data.len())
            }
        }
    }
}

/// Stateful wrapper around any `Read` stream.
#[derive(Debug)]
pub struct DeviceMessageReceiver<R> {
    reader: R,
}

impl<R: Read> DeviceMessageReceiver<R> {
    /// Wrap a readable stream.
    pub fn new(reader: R) -> Self {
        Self { reader }
    }

    /// Read and parse the next native scrcpy device message.
    pub fn read_next(&mut self) -> io::Result<DeviceMessage> {
        read_device_message(&mut self.reader)
    }

    /// Read and parse the next native-or-AI server→host event.
    pub fn read_next_event(&mut self) -> io::Result<DeviceEvent> {
        read_device_event(&mut self.reader)
    }

    /// Borrow the wrapped reader.
    pub fn get_ref(&self) -> &R {
        &self.reader
    }

    /// Mutably borrow the wrapped reader.
    pub fn get_mut(&mut self) -> &mut R {
        &mut self.reader
    }

    /// Recover the wrapped reader.
    pub fn into_inner(self) -> R {
        self.reader
    }
}

/// Join handle for a background device-message receiver thread.
///
/// Dropping the paired `Receiver` returned by
/// [`spawn_device_message_receiver`] stops the thread after its current
/// read/send step and lets `join()` recover the underlying reader.
#[derive(Debug)]
pub struct DeviceMessagePump<R: Read + Send + 'static> {
    join: Option<JoinHandle<io::Result<R>>>,
}

impl<R: Read + Send + 'static> DeviceMessagePump<R> {
    /// Join the background receiver and recover the wrapped reader.
    pub fn join(mut self) -> io::Result<R> {
        let join = self.join.take().ok_or_else(|| {
            io::Error::new(io::ErrorKind::BrokenPipe, "device receiver already joined")
        })?;
        join.join()
            .map_err(|_| io::Error::other("device receiver thread panicked"))?
    }
}

/// Spawn a bounded, std-only background receiver for server→host messages.
///
/// The reader thread preserves ordering and does not drop messages: if the
/// channel is full, it blocks until the consumer catches up. It sends the first
/// read/protocol error as `Err(_)`, then exits so callers do not get repeated
/// timeout/error spam.
pub fn spawn_device_message_receiver<R>(
    reader: R,
    bound: usize,
) -> io::Result<(Receiver<io::Result<DeviceMessage>>, DeviceMessagePump<R>)>
where
    R: Read + Send + 'static,
{
    let (tx, rx) = mpsc::sync_channel(bound);
    let join = thread::Builder::new()
        .name("android-hid-device-receiver".into())
        .spawn(move || device_message_loop(reader, tx))?;
    Ok((rx, DeviceMessagePump { join: Some(join) }))
}

/// Spawn a background receiver using [`DEFAULT_DEVICE_MESSAGE_BOUND`].
pub fn spawn_default_device_message_receiver<R>(
    reader: R,
) -> io::Result<(Receiver<io::Result<DeviceMessage>>, DeviceMessagePump<R>)>
where
    R: Read + Send + 'static,
{
    spawn_device_message_receiver(reader, DEFAULT_DEVICE_MESSAGE_BOUND)
}

/// Spawn a bounded, std-only background receiver for native scrcpy messages
/// and AI extension events.
///
/// This mirrors [`spawn_device_message_receiver`] but parses
/// [`DeviceEvent`], so AI-enabled agent runtimes can consume frame summaries
/// without blocking command producers or desynchronizing on unknown extension
/// envelopes.
pub fn spawn_device_event_receiver<R>(
    reader: R,
    bound: usize,
) -> io::Result<(Receiver<io::Result<DeviceEvent>>, DeviceMessagePump<R>)>
where
    R: Read + Send + 'static,
{
    let (tx, rx) = mpsc::sync_channel(bound);
    let join = thread::Builder::new()
        .name("android-hid-device-event-receiver".into())
        .spawn(move || device_event_loop(reader, tx))?;
    Ok((rx, DeviceMessagePump { join: Some(join) }))
}

/// Spawn a native-or-AI event receiver using [`DEFAULT_DEVICE_MESSAGE_BOUND`].
pub fn spawn_default_device_event_receiver<R>(
    reader: R,
) -> io::Result<(Receiver<io::Result<DeviceEvent>>, DeviceMessagePump<R>)>
where
    R: Read + Send + 'static,
{
    spawn_device_event_receiver(reader, DEFAULT_DEVICE_MESSAGE_BOUND)
}

/// Spawn a std-only latest-frame receiver for AI frame summaries.
///
/// The background thread drains [`DeviceEvent`] values from `reader`, skips
/// native scrcpy messages, AI stats, and unknown envelopes, and stores only the
/// newest [`FrameSummary`]. This avoids channel backpressure and stale-frame
/// replay in low-latency agent perception loops. Use
/// [`DeviceMessagePump::join`] to recover the reader when the stream ends or
/// the latest-frame handle is dropped.
pub fn spawn_latest_frame_summary_receiver<R>(
    reader: R,
) -> io::Result<(LatestFrameSummaryReceiver, DeviceMessagePump<R>)>
where
    R: Read + Send + 'static,
{
    let latest = LatestFrameSummaryReceiver::default();
    let pump_latest = latest.clone();
    let join = thread::Builder::new()
        .name("android-hid-latest-frame-receiver".into())
        .spawn(move || latest_frame_summary_loop(reader, pump_latest))?;
    Ok((latest, DeviceMessagePump { join: Some(join) }))
}

/// Read scrcpy's out-of-band control socket prefix.
///
/// With the standard `tunnel_forward=true send_dummy_byte=true` launch,
/// scrcpy-server writes one dummy byte followed by a 64-byte NUL-padded device
/// name before any device-message frames. Agent code should consume this prefix
/// before calling [`read_device_message`].
pub fn read_scrcpy_control_prefix<R: Read>(reader: &mut R) -> io::Result<ScrcpyControlPrefix> {
    let dummy_byte = read_u8(reader)?;
    let mut raw_device_name = [0u8; DEVICE_NAME_FIELD_LENGTH];
    reader.read_exact(&mut raw_device_name)?;
    let name_len = raw_device_name
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(DEVICE_NAME_FIELD_LENGTH);
    let device_name = String::from_utf8_lossy(&raw_device_name[..name_len]).to_string();
    Ok(ScrcpyControlPrefix {
        dummy_byte,
        device_name,
        raw_device_name,
    })
}

/// Read and parse one native scrcpy device message from `reader`.
///
/// Timeout, `WouldBlock`, and EOF behavior is inherited from the underlying
/// reader as `std::io::Error`, so live callers can distinguish socket timeout
/// from protocol errors via `ErrorKind`.
pub fn read_device_message<R: Read>(reader: &mut R) -> io::Result<DeviceMessage> {
    let ty = read_u8(reader)?;
    match ty {
        TYPE_CLIPBOARD => read_clipboard(reader),
        TYPE_ACK_CLIPBOARD => read_ack_clipboard(reader),
        TYPE_UHID_OUTPUT => read_uhid_output(reader),
        _ => Err(invalid_data(format!("unknown device message type {ty}"))),
    }
}

/// Read and parse one native scrcpy message or AI extension envelope.
///
/// Native scrcpy messages keep their original layouts for type tags `0..=2`.
/// AI extension events use `type(1) + length(4 BE) + payload` for tags `3+`.
/// Unknown extension tags are read and returned as [`DeviceEvent::UnknownEnvelope`]
/// so forward-compatible agent loops can skip them without stream desync.
pub fn read_device_event<R: Read>(reader: &mut R) -> io::Result<DeviceEvent> {
    let ty = read_u8(reader)?;
    match ty {
        TYPE_CLIPBOARD => read_clipboard(reader).map(DeviceEvent::Native),
        TYPE_ACK_CLIPBOARD => read_ack_clipboard(reader).map(DeviceEvent::Native),
        TYPE_UHID_OUTPUT => read_uhid_output(reader).map(DeviceEvent::Native),
        _ => read_ai_or_unknown_envelope(reader, ty),
    }
}

fn device_message_loop<R: Read>(
    mut reader: R,
    tx: SyncSender<io::Result<DeviceMessage>>,
) -> io::Result<R> {
    loop {
        match read_device_message(&mut reader) {
            Ok(msg) => {
                if tx.send(Ok(msg)).is_err() {
                    return Ok(reader);
                }
            }
            Err(e) => {
                let _ = tx.send(Err(e));
                return Ok(reader);
            }
        }
    }
}

fn device_event_loop<R: Read>(
    mut reader: R,
    tx: SyncSender<io::Result<DeviceEvent>>,
) -> io::Result<R> {
    loop {
        match read_device_event(&mut reader) {
            Ok(event) => {
                if tx.send(Ok(event)).is_err() {
                    return Ok(reader);
                }
            }
            Err(e) => {
                let _ = tx.send(Err(e));
                return Ok(reader);
            }
        }
    }
}

fn latest_frame_summary_loop<R: Read>(
    mut reader: R,
    latest: LatestFrameSummaryReceiver,
) -> io::Result<R> {
    loop {
        if latest.is_abandoned() {
            return Ok(reader);
        }
        match read_device_event(&mut reader) {
            Ok(DeviceEvent::FrameSummary(summary)) => latest.store_summary(summary),
            Ok(_) => {}
            Err(e) => {
                latest.store_terminal_error(e);
                return Ok(reader);
            }
        }
    }
}

fn read_clipboard<R: Read>(reader: &mut R) -> io::Result<DeviceMessage> {
    let len = read_u32_be(reader)? as usize;
    if len > DEVICE_MSG_TEXT_MAX_LENGTH {
        return Err(invalid_data(format!(
            "clipboard payload too large: {len} bytes (max {DEVICE_MSG_TEXT_MAX_LENGTH})"
        )));
    }

    let mut payload = vec![0u8; len];
    if len > 0 {
        reader.read_exact(&mut payload)?;
    }
    let text = String::from_utf8(payload).map_err(|e| invalid_data(e.to_string()))?;
    Ok(DeviceMessage::Clipboard(text))
}

fn read_ack_clipboard<R: Read>(reader: &mut R) -> io::Result<DeviceMessage> {
    Ok(DeviceMessage::AckClipboard {
        sequence: read_u64_be(reader)?,
    })
}

fn read_uhid_output<R: Read>(reader: &mut R) -> io::Result<DeviceMessage> {
    let id = read_u16_be(reader)?;
    let size = read_u16_be(reader)? as usize;
    let max_payload = DEVICE_MSG_MAX_SIZE - 5;
    if size > max_payload {
        return Err(invalid_data(format!(
            "uhid output payload too large: {size} bytes (max {max_payload})"
        )));
    }

    let mut data = vec![0u8; size];
    if size > 0 {
        reader.read_exact(&mut data)?;
    }
    Ok(DeviceMessage::UhidOutput { id, data })
}

fn read_ai_or_unknown_envelope<R: Read>(reader: &mut R, ty: u8) -> io::Result<DeviceEvent> {
    let len = read_u32_be(reader)? as usize;
    let max_payload = DEVICE_MSG_MAX_SIZE - 5;
    if len > max_payload {
        return Err(invalid_data(format!(
            "device event payload too large: {len} bytes (max {max_payload})"
        )));
    }
    let mut payload = vec![0u8; len];
    if len > 0 {
        reader.read_exact(&mut payload)?;
    }
    match ty {
        TYPE_FRAME_SUMMARY => FrameSummary::parse(&payload)
            .map(DeviceEvent::FrameSummary)
            .map_err(ai_error_to_io),
        TYPE_AI_STATS => AiStats::parse(&payload)
            .map(DeviceEvent::AiStats)
            .map_err(ai_error_to_io),
        _ => Ok(DeviceEvent::UnknownEnvelope {
            msg_type: ty,
            payload,
        }),
    }
}

fn ai_error_to_io(err: crate::error::Error) -> io::Error {
    invalid_data(err.to_string())
}

fn read_u8<R: Read>(reader: &mut R) -> io::Result<u8> {
    let mut b = [0u8; 1];
    reader.read_exact(&mut b)?;
    Ok(b[0])
}

fn read_u16_be<R: Read>(reader: &mut R) -> io::Result<u16> {
    let mut b = [0u8; 2];
    reader.read_exact(&mut b)?;
    Ok(u16::from_be_bytes(b))
}

fn read_u32_be<R: Read>(reader: &mut R) -> io::Result<u32> {
    let mut b = [0u8; 4];
    reader.read_exact(&mut b)?;
    Ok(u32::from_be_bytes(b))
}

fn read_u64_be<R: Read>(reader: &mut R) -> io::Result<u64> {
    let mut b = [0u8; 8];
    reader.read_exact(&mut b)?;
    Ok(u64::from_be_bytes(b))
}

fn invalid_data(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::time::Duration;

    fn be_u16(v: u16) -> [u8; 2] {
        v.to_be_bytes()
    }

    fn be_u32(v: u32) -> [u8; 4] {
        v.to_be_bytes()
    }

    fn be_u64(v: u64) -> [u8; 8] {
        v.to_be_bytes()
    }

    fn be_f32(v: f32) -> [u8; 4] {
        v.to_be_bytes()
    }

    fn ack(sequence: u64) -> Vec<u8> {
        let mut bytes = vec![TYPE_ACK_CLIPBOARD];
        bytes.extend(be_u64(sequence));
        bytes
    }

    fn clipboard(text: &str) -> Vec<u8> {
        let mut bytes = vec![TYPE_CLIPBOARD];
        bytes.extend(be_u32(text.len() as u32));
        bytes.extend(text.as_bytes());
        bytes
    }

    fn frame_summary_envelope(frame_seq: u32) -> Vec<u8> {
        frame_summary_envelope_at(100, frame_seq)
    }

    fn frame_summary_envelope_at(timestamp_ms: u64, frame_seq: u32) -> Vec<u8> {
        let mut payload = Vec::new();
        payload.extend(be_u64(timestamp_ms));
        payload.extend(be_u32(frame_seq));
        payload.extend(be_u16(1000));
        payload.extend(be_u16(2000));
        payload.push(crate::ai::FLAG_KEYFRAME | crate::ai::FLAG_OBJECTS);
        payload.extend(be_u16(1));
        payload.extend(be_f32(0.25));
        payload.extend(be_u16(0));
        payload.extend(be_u16(1));
        payload.extend(be_u16(100));
        payload.extend(be_u16(200));
        payload.extend(be_u16(301));
        payload.extend(be_u16(101));
        payload.push(7);
        payload.push(220);
        payload.push(0);

        let mut bytes = vec![TYPE_FRAME_SUMMARY];
        bytes.extend(be_u32(payload.len() as u32));
        bytes.extend(payload);
        bytes
    }

    fn ai_stats_envelope() -> Vec<u8> {
        let mut payload = Vec::new();
        payload.extend(be_u64(1_000));
        payload.extend(be_u32(10));
        payload.extend(be_u32(1));
        payload.extend(be_u32(2));
        payload.extend(be_u64(300));
        payload.extend(be_f32(4.5));
        payload.extend(be_f32(60.0));

        let mut bytes = vec![TYPE_AI_STATS];
        bytes.extend(be_u32(payload.len() as u32));
        bytes.extend(payload);
        bytes
    }

    fn frame_summary_from_envelope_at(timestamp_ms: u64, frame_seq: u32) -> FrameSummary {
        match read_device_event(&mut Cursor::new(frame_summary_envelope_at(
            timestamp_ms,
            frame_seq,
        )))
        .unwrap()
        {
            DeviceEvent::FrameSummary(summary) => summary,
            other => panic!("expected frame summary, got {other:?}"),
        }
    }

    #[test]
    fn clipboard_message_uses_text_length_prefix() {
        let mut bytes = vec![TYPE_CLIPBOARD];
        bytes.extend(be_u32(5));
        bytes.extend(b"hello");

        let msg = read_device_message(&mut Cursor::new(bytes)).unwrap();
        assert_eq!(msg, DeviceMessage::Clipboard("hello".to_string()));
        assert_eq!(msg.msg_type(), TYPE_CLIPBOARD);
    }

    #[test]
    fn ack_clipboard_has_no_generic_length_prefix() {
        let mut bytes = vec![TYPE_ACK_CLIPBOARD];
        bytes.extend(be_u64(42));

        let msg = read_device_message(&mut Cursor::new(bytes)).unwrap();
        assert_eq!(msg, DeviceMessage::AckClipboard { sequence: 42 });
        assert_eq!(msg.describe(), "DEVICE_MSG_ACK_CLIPBOARD seq=42");
    }

    #[test]
    fn uhid_output_has_id_size_and_data_without_generic_length_prefix() {
        let mut bytes = vec![TYPE_UHID_OUTPUT];
        bytes.extend(be_u16(7));
        bytes.extend(be_u16(3));
        bytes.extend([0x10, 0x20, 0x30]);

        let msg = read_device_message(&mut Cursor::new(bytes)).unwrap();
        assert_eq!(
            msg,
            DeviceMessage::UhidOutput {
                id: 7,
                data: vec![0x10, 0x20, 0x30],
            }
        );
    }

    #[test]
    fn receiver_reads_consecutive_mixed_messages_without_desync() {
        let mut bytes = vec![TYPE_ACK_CLIPBOARD];
        bytes.extend(be_u64(7));
        bytes.push(TYPE_UHID_OUTPUT);
        bytes.extend(be_u16(2));
        bytes.extend(be_u16(1));
        bytes.push(0xaa);
        bytes.push(TYPE_CLIPBOARD);
        bytes.extend(be_u32(2));
        bytes.extend(b"ok");

        let mut rx = DeviceMessageReceiver::new(Cursor::new(bytes));
        assert_eq!(
            rx.read_next().unwrap(),
            DeviceMessage::AckClipboard { sequence: 7 }
        );
        assert_eq!(
            rx.read_next().unwrap(),
            DeviceMessage::UhidOutput {
                id: 2,
                data: vec![0xaa],
            }
        );
        assert_eq!(
            rx.read_next().unwrap(),
            DeviceMessage::Clipboard("ok".to_string())
        );
    }

    #[test]
    fn device_event_reads_native_ai_and_unknown_envelopes_in_order() {
        let mut bytes = Vec::new();
        bytes.extend(ack(7));
        bytes.extend(frame_summary_envelope(42));
        bytes.extend(ai_stats_envelope());
        bytes.push(99);
        bytes.extend(be_u32(3));
        bytes.extend([1, 2, 3]);

        let mut cur = Cursor::new(bytes);
        assert_eq!(
            read_device_event(&mut cur).unwrap(),
            DeviceEvent::Native(DeviceMessage::AckClipboard { sequence: 7 })
        );
        match read_device_event(&mut cur).unwrap() {
            DeviceEvent::FrameSummary(summary) => {
                assert_eq!(summary.frame_seq, 42);
                assert_eq!(summary.objects[0].class_id, 7);
            }
            other => panic!("expected frame summary, got {other:?}"),
        }
        match read_device_event(&mut cur).unwrap() {
            DeviceEvent::AiStats(stats) => {
                assert_eq!(stats.frames_sampled, 10);
                assert!((stats.current_fps - 60.0).abs() < f32::EPSILON);
            }
            other => panic!("expected ai stats, got {other:?}"),
        }
        assert_eq!(
            read_device_event(&mut cur).unwrap(),
            DeviceEvent::UnknownEnvelope {
                msg_type: 99,
                payload: vec![1, 2, 3],
            }
        );
    }

    #[test]
    fn unknown_type_is_invalid_data() {
        let err = read_device_message(&mut Cursor::new([99u8])).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn oversized_clipboard_is_rejected_before_allocation() {
        let mut bytes = vec![TYPE_CLIPBOARD];
        bytes.extend(be_u32((DEVICE_MSG_TEXT_MAX_LENGTH + 1) as u32));

        let err = read_device_message(&mut Cursor::new(bytes)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn invalid_clipboard_utf8_is_invalid_data() {
        let mut bytes = vec![TYPE_CLIPBOARD];
        bytes.extend(be_u32(1));
        bytes.push(0xff);

        let err = read_device_message(&mut Cursor::new(bytes)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn scrcpy_control_prefix_reads_dummy_and_trimmed_name() {
        let mut bytes = vec![0x00];
        let mut name = [0u8; DEVICE_NAME_FIELD_LENGTH];
        name[..7].copy_from_slice(b"SM-G991");
        bytes.extend(name);
        bytes.extend(ack(9));

        let mut cur = Cursor::new(bytes);
        let prefix = read_scrcpy_control_prefix(&mut cur).unwrap();
        assert_eq!(prefix.dummy_byte, 0);
        assert_eq!(prefix.device_name, "SM-G991");
        assert_eq!(
            read_device_message(&mut cur).unwrap(),
            DeviceMessage::AckClipboard { sequence: 9 }
        );
    }

    #[test]
    fn short_ack_returns_unexpected_eof() {
        let mut bytes = vec![TYPE_ACK_CLIPBOARD];
        bytes.extend([0, 0, 0]);

        let err = read_device_message(&mut Cursor::new(bytes)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn background_receiver_streams_messages_in_order_then_reports_eof() {
        let mut bytes = Vec::new();
        bytes.extend(ack(1));
        bytes.extend(clipboard("ok"));

        let (rx, pump) = spawn_device_message_receiver(Cursor::new(bytes), 1).unwrap();

        assert_eq!(
            rx.recv().unwrap().unwrap(),
            DeviceMessage::AckClipboard { sequence: 1 }
        );
        assert_eq!(
            rx.recv().unwrap().unwrap(),
            DeviceMessage::Clipboard("ok".to_string())
        );
        let err = rx.recv().unwrap().unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);

        let reader = pump.join().unwrap();
        assert_eq!(reader.position(), reader.get_ref().len() as u64);
    }

    #[test]
    fn background_event_receiver_streams_events_in_order_then_reports_eof() {
        let mut bytes = Vec::new();
        bytes.extend(ack(1));
        bytes.extend(frame_summary_envelope(2));
        bytes.extend(ai_stats_envelope());

        let (rx, pump) = spawn_device_event_receiver(Cursor::new(bytes), 1).unwrap();

        assert_eq!(
            rx.recv().unwrap().unwrap(),
            DeviceEvent::Native(DeviceMessage::AckClipboard { sequence: 1 })
        );
        match rx.recv().unwrap().unwrap() {
            DeviceEvent::FrameSummary(summary) => assert_eq!(summary.frame_seq, 2),
            other => panic!("expected frame summary, got {other:?}"),
        }
        match rx.recv().unwrap().unwrap() {
            DeviceEvent::AiStats(stats) => assert_eq!(stats.frames_sampled, 10),
            other => panic!("expected ai stats, got {other:?}"),
        }
        let err = rx.recv().unwrap().unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);

        let reader = pump.join().unwrap();
        assert_eq!(reader.position(), reader.get_ref().len() as u64);
    }

    #[test]
    fn latest_frame_summary_receiver_keeps_newest_frame_and_reports_terminal_error() {
        let mut bytes = Vec::new();
        bytes.extend(ack(1));
        bytes.extend(frame_summary_envelope_at(100, 2));
        bytes.extend(ai_stats_envelope());
        bytes.extend(frame_summary_envelope_at(200, 3));
        bytes.push(99);
        bytes.extend(be_u32(3));
        bytes.extend([1, 2, 3]);
        bytes.extend(frame_summary_envelope_at(250, 4));

        let (latest, pump) = spawn_latest_frame_summary_receiver(Cursor::new(bytes)).unwrap();
        let reader = pump.join().unwrap();
        assert_eq!(reader.position(), reader.get_ref().len() as u64);

        let snapshot = latest.wait_first().unwrap();
        assert_eq!(snapshot.version, 3);
        assert_eq!(snapshot.summary.frame_seq, 4);
        assert_eq!(snapshot.summary.timestamp_ms, 250);

        assert_eq!(
            latest.snapshot_after_version(2).unwrap().summary.frame_seq,
            4
        );
        assert!(latest.snapshot_after_version(3).is_none());
        let boundary = latest.boundary();
        let previous_boundary = LatestFrameSummaryBoundary::new(2);
        assert_eq!(boundary.version(), 3);
        assert_eq!(snapshot.boundary(), boundary);
        assert_eq!(
            LatestFrameSummaryBoundary::from_snapshot(&snapshot),
            boundary
        );
        assert!(previous_boundary.accepts(&snapshot));
        assert!(!boundary.accepts(&snapshot));
        let observation = latest.observe();
        assert!(observation.has_snapshot());
        assert_eq!(observation.boundary(), boundary);
        assert_eq!(observation.boundary_version(), 3);
        assert_eq!(
            observation.snapshot().unwrap().summary.frame_seq,
            snapshot.summary.frame_seq
        );
        assert_eq!(observation.summary().unwrap().timestamp_ms, 250);
        assert!(!observation.accepts(&snapshot));
        let rebuilt_observation =
            LatestFrameSummaryObservation::from_parts(boundary, Some(snapshot.clone()));
        assert_eq!(rebuilt_observation.boundary(), boundary);
        assert_eq!(
            rebuilt_observation.summary().unwrap().frame_seq,
            snapshot.summary.frame_seq
        );
        assert_eq!(
            LatestFrameSummaryObservation::from_snapshot(snapshot.clone())
                .into_snapshot()
                .unwrap()
                .summary
                .frame_seq,
            4
        );
        assert_eq!(
            LatestFrameSummaryObservation::at_version(3).boundary(),
            boundary
        );
        let previous_observation = LatestFrameSummaryObservation::at_boundary(previous_boundary);
        assert_eq!(
            latest
                .snapshot_after_boundary(previous_boundary)
                .unwrap()
                .summary
                .frame_seq,
            4
        );
        assert_eq!(
            latest
                .snapshot_after_observation(&previous_observation)
                .unwrap()
                .summary
                .frame_seq,
            4
        );
        assert!(latest.snapshot_after_boundary(boundary).is_none());
        assert!(latest.snapshot_after_observation(&observation).is_none());
        assert_eq!(
            latest
                .wait_next_matching_after_boundary(previous_boundary, |snapshot| {
                    snapshot.summary.frame_seq == 4
                })
                .unwrap()
                .summary
                .frame_seq,
            4
        );
        assert_eq!(
            latest
                .wait_next_matching_after_observation(&previous_observation, |snapshot| {
                    snapshot.summary.frame_seq == 4
                })
                .unwrap()
                .summary
                .frame_seq,
            4
        );
        assert_eq!(
            latest
                .wait_next_matching_after_boundary_timeout(
                    previous_boundary,
                    Duration::from_secs(1),
                    |snapshot| snapshot.summary.timestamp_ms == 250,
                )
                .unwrap()
                .summary
                .timestamp_ms,
            250
        );
        assert_eq!(
            latest
                .wait_next_matching_after_observation_timeout(
                    &previous_observation,
                    Duration::from_secs(1),
                    |snapshot| snapshot.summary.timestamp_ms == 250,
                )
                .unwrap()
                .summary
                .timestamp_ms,
            250
        );
        assert_eq!(
            latest
                .snapshot_after_frame_seq(3)
                .unwrap()
                .summary
                .frame_seq,
            4
        );
        assert!(latest.snapshot_after_frame_seq(4).is_none());
        assert_eq!(
            latest
                .snapshot_after_timestamp(200)
                .unwrap()
                .summary
                .timestamp_ms,
            250
        );
        assert!(latest.snapshot_after_timestamp(250).is_none());

        let err = latest
            .wait_next_after_observation(&observation)
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
        assert_eq!(
            latest.terminal_error().unwrap().kind,
            io::ErrorKind::UnexpectedEof
        );
    }

    #[test]
    fn latest_frame_summary_receiver_reports_eof_without_frames() {
        let mut bytes = Vec::new();
        bytes.extend(ack(1));
        bytes.extend(ai_stats_envelope());

        let (latest, pump) = spawn_latest_frame_summary_receiver(Cursor::new(bytes)).unwrap();
        let reader = pump.join().unwrap();
        assert_eq!(reader.position(), reader.get_ref().len() as u64);

        assert!(latest.snapshot().is_none());
        let err = latest.wait_first().unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn latest_frame_summary_timeout_bounds_empty_wait() {
        let latest = LatestFrameSummaryReceiver::default();

        let err = latest
            .wait_first_timeout(Duration::from_millis(1))
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
    }

    #[test]
    fn latest_frame_summary_matching_waits_skip_cached_miss() {
        let latest = LatestFrameSummaryReceiver::default();
        latest.store_summary(frame_summary_from_envelope_at(100, 1));

        assert!(latest
            .snapshot_matching(|snapshot| snapshot.summary.frame_seq > 1)
            .is_none());
        let err = latest
            .wait_next_matching_timeout(0, Duration::from_millis(1), |snapshot| {
                snapshot.summary.frame_seq > 1
            })
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);

        latest.store_summary(frame_summary_from_envelope_at(180, 2));
        let snapshot = latest
            .wait_next_matching(0, |snapshot| snapshot.summary.frame_seq > 1)
            .unwrap();
        assert_eq!(snapshot.version, 2);
        assert_eq!(snapshot.summary.frame_seq, 2);
        assert_eq!(
            latest
                .wait_matching_timeout(Duration::from_secs(1), |snapshot| {
                    snapshot.summary.timestamp_ms > 150
                })
                .unwrap()
                .summary
                .timestamp_ms,
            180
        );
    }

    #[test]
    fn background_receiver_stops_when_consumer_drops_channel() {
        let mut bytes = Vec::new();
        bytes.extend(ack(1));
        bytes.extend(ack(2));
        bytes.extend(ack(3));

        let (rx, pump) = spawn_device_message_receiver(Cursor::new(bytes), 1).unwrap();
        assert_eq!(
            rx.recv().unwrap().unwrap(),
            DeviceMessage::AckClipboard { sequence: 1 }
        );
        drop(rx);

        let reader = pump.join().unwrap();
        assert!(reader.position() >= 9);
    }

    #[test]
    fn background_event_receiver_stops_when_consumer_drops_channel() {
        let mut bytes = Vec::new();
        bytes.extend(ack(1));
        bytes.extend(frame_summary_envelope(2));
        bytes.extend(frame_summary_envelope(3));

        let (rx, pump) = spawn_device_event_receiver(Cursor::new(bytes), 1).unwrap();
        assert_eq!(
            rx.recv().unwrap().unwrap(),
            DeviceEvent::Native(DeviceMessage::AckClipboard { sequence: 1 })
        );
        drop(rx);

        let reader = pump.join().unwrap();
        assert!(reader.position() >= 9);
    }
}
