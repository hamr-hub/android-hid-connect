//! Tokio-based device-to-host message helpers.
//!
//! This module is available with the `tokio` feature. It mirrors
//! [`crate::device`] but works with [`tokio::io::AsyncRead`] streams and
//! `tokio::sync::mpsc`, so agent runtimes can consume scrcpy server replies
//! without blocking an async executor thread.

use std::io;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use crate::ai::{AiStats, FrameSummary, TYPE_AI_STATS, TYPE_FRAME_SUMMARY};
use crate::device::{
    DeviceEvent, DeviceMessage, DeviceReadError, LatestFrameSummaryBoundary,
    LatestFrameSummaryObservation, LatestFrameSummarySnapshot, ScrcpyControlPrefix,
    DEFAULT_DEVICE_MESSAGE_BOUND, DEVICE_MSG_MAX_SIZE, DEVICE_MSG_TEXT_MAX_LENGTH,
    DEVICE_NAME_FIELD_LENGTH, TYPE_ACK_CLIPBOARD, TYPE_CLIPBOARD, TYPE_UHID_OUTPUT,
};

/// Stateful async wrapper around any [`AsyncRead`] stream.
#[derive(Debug)]
pub struct AsyncDeviceMessageReceiver<R> {
    reader: R,
}

impl<R: AsyncRead + Unpin> AsyncDeviceMessageReceiver<R> {
    /// Wrap an async readable stream.
    pub fn new(reader: R) -> Self {
        Self { reader }
    }

    /// Read and parse the next native scrcpy device message.
    pub async fn read_next(&mut self) -> io::Result<DeviceMessage> {
        read_device_message_async(&mut self.reader).await
    }

    /// Read and parse the next native scrcpy message or AI extension event.
    pub async fn read_next_event(&mut self) -> io::Result<DeviceEvent> {
        read_device_event_async(&mut self.reader).await
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

/// Join handle for a background async device-message receiver task.
#[derive(Debug)]
pub struct AsyncDeviceMessagePump<R: AsyncRead + Unpin + Send + 'static> {
    join: JoinHandle<io::Result<R>>,
}

impl<R: AsyncRead + Unpin + Send + 'static> AsyncDeviceMessagePump<R> {
    /// Await the receiver task and recover the wrapped reader.
    pub async fn join(self) -> io::Result<R> {
        self.join
            .await
            .map_err(|_| io::Error::other("async device receiver task panicked"))?
    }
}

#[derive(Debug, Clone, Default)]
struct AsyncLatestFrameSummaryState {
    latest: Option<LatestFrameSummarySnapshot>,
    version: u64,
    terminal_error: Option<DeviceReadError>,
}

/// Async low-latency latest-frame view over a native-or-AI event stream.
///
/// This is the Tokio counterpart to
/// [`crate::device::LatestFrameSummaryReceiver`]. The background task drains
/// the mixed event stream, skips non-frame events, and publishes only the
/// newest [`FrameSummary`] through a `watch` channel.
#[derive(Debug, Clone)]
pub struct AsyncLatestFrameSummaryReceiver {
    rx: watch::Receiver<AsyncLatestFrameSummaryState>,
}

impl AsyncLatestFrameSummaryReceiver {
    /// Return the newest frame snapshot currently available.
    pub fn snapshot(&self) -> Option<LatestFrameSummarySnapshot> {
        self.rx.borrow().latest.clone()
    }

    /// Return a consistent current boundary plus optional newest snapshot.
    pub fn observe(&self) -> LatestFrameSummaryObservation {
        let state = self.rx.borrow();
        LatestFrameSummaryObservation::from_parts(
            LatestFrameSummaryBoundary::new(state.version),
            state.latest.clone(),
        )
    }

    /// Return the current local latest-frame version.
    pub fn version(&self) -> u64 {
        self.rx.borrow().version
    }

    /// Return a copyable marker for the current local latest-frame version.
    pub fn boundary(&self) -> LatestFrameSummaryBoundary {
        LatestFrameSummaryBoundary::new(self.version())
    }

    /// Return the terminal read/protocol error, if the pump has stopped.
    pub fn terminal_error(&self) -> Option<DeviceReadError> {
        self.rx.borrow().terminal_error.clone()
    }

    /// Return the current snapshot if it is newer than `after_version`.
    pub fn snapshot_after_version(&self, after_version: u64) -> Option<LatestFrameSummarySnapshot> {
        let state = self.rx.borrow();
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

    /// Wait until a frame snapshot newer than `after_version` is available.
    pub async fn wait_next(
        &mut self,
        after_version: u64,
    ) -> io::Result<LatestFrameSummarySnapshot> {
        self.wait_next_matching(after_version, |_| true).await
    }

    /// Wait until a frame snapshot newer than `boundary` is available.
    pub async fn wait_next_after_boundary(
        &mut self,
        boundary: LatestFrameSummaryBoundary,
    ) -> io::Result<LatestFrameSummarySnapshot> {
        self.wait_next(boundary.version()).await
    }

    /// Wait until a frame snapshot newer than `observation.boundary` is
    /// available.
    pub async fn wait_next_after_observation(
        &mut self,
        observation: &LatestFrameSummaryObservation,
    ) -> io::Result<LatestFrameSummarySnapshot> {
        self.wait_next_after_boundary(observation.boundary()).await
    }

    /// Wait until a frame snapshot newer than `after_version` and accepted by
    /// `predicate` is available.
    pub async fn wait_next_matching(
        &mut self,
        after_version: u64,
        mut predicate: impl FnMut(&LatestFrameSummarySnapshot) -> bool,
    ) -> io::Result<LatestFrameSummarySnapshot> {
        self.wait_matching(move |snapshot| snapshot.version > after_version && predicate(snapshot))
            .await
    }

    /// Wait until a frame snapshot newer than `boundary` and accepted by
    /// `predicate` is available.
    pub async fn wait_next_matching_after_boundary(
        &mut self,
        boundary: LatestFrameSummaryBoundary,
        predicate: impl FnMut(&LatestFrameSummarySnapshot) -> bool,
    ) -> io::Result<LatestFrameSummarySnapshot> {
        self.wait_next_matching(boundary.version(), predicate).await
    }

    /// Wait until a frame snapshot newer than `observation.boundary` and
    /// accepted by `predicate` is available.
    pub async fn wait_next_matching_after_observation(
        &mut self,
        observation: &LatestFrameSummaryObservation,
        predicate: impl FnMut(&LatestFrameSummarySnapshot) -> bool,
    ) -> io::Result<LatestFrameSummarySnapshot> {
        self.wait_next_matching_after_boundary(observation.boundary(), predicate)
            .await
    }

    /// Wait until the first frame snapshot is available.
    pub async fn wait_first(&mut self) -> io::Result<LatestFrameSummarySnapshot> {
        self.wait_next(0).await
    }

    /// Wait until a frame snapshot accepted by `predicate` is available.
    pub async fn wait_matching(
        &mut self,
        mut predicate: impl FnMut(&LatestFrameSummarySnapshot) -> bool,
    ) -> io::Result<LatestFrameSummarySnapshot> {
        loop {
            {
                let state = self.rx.borrow();
                if let Some(snapshot) = &state.latest {
                    if predicate(snapshot) {
                        return Ok(snapshot.clone());
                    }
                }
                if let Some(err) = &state.terminal_error {
                    return Err(err.to_io_error());
                }
            }
            if self.rx.changed().await.is_err() {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "async latest frame receiver closed",
                ));
            }
        }
    }

    /// Wait until a frame snapshot newer than `after_version` is available,
    /// bounded by `timeout`.
    pub async fn wait_next_timeout(
        &mut self,
        after_version: u64,
        timeout: Duration,
    ) -> io::Result<LatestFrameSummarySnapshot> {
        self.wait_next_matching_timeout(after_version, timeout, |_| true)
            .await
    }

    /// Wait until a frame snapshot newer than `boundary` is available, bounded
    /// by `timeout`.
    pub async fn wait_next_after_boundary_timeout(
        &mut self,
        boundary: LatestFrameSummaryBoundary,
        timeout: Duration,
    ) -> io::Result<LatestFrameSummarySnapshot> {
        self.wait_next_timeout(boundary.version(), timeout).await
    }

    /// Wait until a frame snapshot newer than `observation.boundary` is
    /// available, bounded by `timeout`.
    pub async fn wait_next_after_observation_timeout(
        &mut self,
        observation: &LatestFrameSummaryObservation,
        timeout: Duration,
    ) -> io::Result<LatestFrameSummarySnapshot> {
        self.wait_next_after_boundary_timeout(observation.boundary(), timeout)
            .await
    }

    /// Wait until a frame snapshot newer than `after_version` and accepted by
    /// `predicate` is available, bounded by `timeout`.
    pub async fn wait_next_matching_timeout(
        &mut self,
        after_version: u64,
        timeout: Duration,
        mut predicate: impl FnMut(&LatestFrameSummarySnapshot) -> bool,
    ) -> io::Result<LatestFrameSummarySnapshot> {
        self.wait_matching_timeout(timeout, move |snapshot| {
            snapshot.version > after_version && predicate(snapshot)
        })
        .await
    }

    /// Wait until a frame snapshot newer than `boundary` and accepted by
    /// `predicate` is available, bounded by `timeout`.
    pub async fn wait_next_matching_after_boundary_timeout(
        &mut self,
        boundary: LatestFrameSummaryBoundary,
        timeout: Duration,
        predicate: impl FnMut(&LatestFrameSummarySnapshot) -> bool,
    ) -> io::Result<LatestFrameSummarySnapshot> {
        self.wait_next_matching_timeout(boundary.version(), timeout, predicate)
            .await
    }

    /// Wait until a frame snapshot newer than `observation.boundary` and
    /// accepted by `predicate` is available, bounded by `timeout`.
    pub async fn wait_next_matching_after_observation_timeout(
        &mut self,
        observation: &LatestFrameSummaryObservation,
        timeout: Duration,
        predicate: impl FnMut(&LatestFrameSummarySnapshot) -> bool,
    ) -> io::Result<LatestFrameSummarySnapshot> {
        self.wait_next_matching_after_boundary_timeout(observation.boundary(), timeout, predicate)
            .await
    }

    /// Wait until the first frame snapshot is available, bounded by `timeout`.
    pub async fn wait_first_timeout(
        &mut self,
        timeout: Duration,
    ) -> io::Result<LatestFrameSummarySnapshot> {
        self.wait_next_timeout(0, timeout).await
    }

    /// Wait until the latest cached frame has `frame_seq > min_frame_seq`.
    pub async fn wait_after_frame_seq(
        &mut self,
        min_frame_seq: u32,
    ) -> io::Result<LatestFrameSummarySnapshot> {
        self.wait_matching(|snapshot| snapshot.summary.frame_seq > min_frame_seq)
            .await
    }

    /// Wait until the latest cached frame has `frame_seq > min_frame_seq`,
    /// bounded by `timeout`.
    pub async fn wait_after_frame_seq_timeout(
        &mut self,
        min_frame_seq: u32,
        timeout: Duration,
    ) -> io::Result<LatestFrameSummarySnapshot> {
        self.wait_matching_timeout(timeout, |snapshot| {
            snapshot.summary.frame_seq > min_frame_seq
        })
        .await
    }

    /// Wait until the latest cached frame has `timestamp_ms > min_timestamp_ms`.
    pub async fn wait_after_timestamp(
        &mut self,
        min_timestamp_ms: u64,
    ) -> io::Result<LatestFrameSummarySnapshot> {
        self.wait_matching(|snapshot| snapshot.summary.timestamp_ms > min_timestamp_ms)
            .await
    }

    /// Wait until the latest cached frame has
    /// `timestamp_ms > min_timestamp_ms`, bounded by `timeout`.
    pub async fn wait_after_timestamp_timeout(
        &mut self,
        min_timestamp_ms: u64,
        timeout: Duration,
    ) -> io::Result<LatestFrameSummarySnapshot> {
        self.wait_matching_timeout(timeout, |snapshot| {
            snapshot.summary.timestamp_ms > min_timestamp_ms
        })
        .await
    }

    /// Wait until a frame snapshot accepted by `predicate` is available,
    /// bounded by `timeout`.
    pub async fn wait_matching_timeout(
        &mut self,
        timeout: Duration,
        predicate: impl FnMut(&LatestFrameSummarySnapshot) -> bool,
    ) -> io::Result<LatestFrameSummarySnapshot> {
        tokio::time::timeout(timeout, self.wait_matching(predicate))
            .await
            .map_err(|_| latest_frame_timeout_error())?
    }
}

/// Spawn a bounded Tokio task that reads server→host messages.
///
/// The task preserves ordering and does not drop messages. If the channel is
/// full, it waits for the consumer to catch up. It sends the first
/// read/protocol error as `Err(_)`, then exits.
pub fn spawn_async_device_message_receiver<R>(
    reader: R,
    bound: usize,
) -> (
    mpsc::Receiver<io::Result<DeviceMessage>>,
    AsyncDeviceMessagePump<R>,
)
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let (tx, rx) = mpsc::channel(bound);
    let join = tokio::spawn(async move { async_device_message_loop(reader, tx).await });
    (rx, AsyncDeviceMessagePump { join })
}

/// Spawn a background async receiver using [`DEFAULT_DEVICE_MESSAGE_BOUND`].
pub fn spawn_default_async_device_message_receiver<R>(
    reader: R,
) -> (
    mpsc::Receiver<io::Result<DeviceMessage>>,
    AsyncDeviceMessagePump<R>,
)
where
    R: AsyncRead + Unpin + Send + 'static,
{
    spawn_async_device_message_receiver(reader, DEFAULT_DEVICE_MESSAGE_BOUND)
}

/// Spawn a bounded Tokio task that reads native scrcpy messages and AI events.
///
/// This is the async counterpart to [`crate::device::read_device_event`].
pub fn spawn_async_device_event_receiver<R>(
    reader: R,
    bound: usize,
) -> (
    mpsc::Receiver<io::Result<DeviceEvent>>,
    AsyncDeviceMessagePump<R>,
)
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let (tx, rx) = mpsc::channel(bound);
    let join = tokio::spawn(async move { async_device_event_loop(reader, tx).await });
    (rx, AsyncDeviceMessagePump { join })
}

/// Spawn an async native-or-AI event receiver using [`DEFAULT_DEVICE_MESSAGE_BOUND`].
pub fn spawn_default_async_device_event_receiver<R>(
    reader: R,
) -> (
    mpsc::Receiver<io::Result<DeviceEvent>>,
    AsyncDeviceMessagePump<R>,
)
where
    R: AsyncRead + Unpin + Send + 'static,
{
    spawn_async_device_event_receiver(reader, DEFAULT_DEVICE_MESSAGE_BOUND)
}

/// Spawn a Tokio latest-frame receiver for AI frame summaries.
///
/// The background task drains native scrcpy messages and AI extension events
/// from `reader`, skips non-frame events, and publishes only the newest
/// [`FrameSummary`]. This avoids ordered-channel backlog for async perception
/// loops that need current UI state rather than replay.
pub fn spawn_async_latest_frame_summary_receiver<R>(
    reader: R,
) -> (AsyncLatestFrameSummaryReceiver, AsyncDeviceMessagePump<R>)
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let (tx, rx) = watch::channel(AsyncLatestFrameSummaryState::default());
    let join = tokio::spawn(async move { async_latest_frame_summary_loop(reader, tx).await });
    (
        AsyncLatestFrameSummaryReceiver { rx },
        AsyncDeviceMessagePump { join },
    )
}

/// Read scrcpy's out-of-band control socket prefix from an async stream.
pub async fn read_scrcpy_control_prefix_async<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> io::Result<ScrcpyControlPrefix> {
    let dummy_byte = read_u8(reader).await?;
    let mut raw_device_name = [0u8; DEVICE_NAME_FIELD_LENGTH];
    reader.read_exact(&mut raw_device_name).await?;
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

/// Read and parse one native scrcpy device message from an async stream.
pub async fn read_device_message_async<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> io::Result<DeviceMessage> {
    let ty = read_u8(reader).await?;
    match ty {
        TYPE_CLIPBOARD => read_clipboard(reader).await,
        TYPE_ACK_CLIPBOARD => read_ack_clipboard(reader).await,
        TYPE_UHID_OUTPUT => read_uhid_output(reader).await,
        _ => Err(invalid_data(format!("unknown device message type {ty}"))),
    }
}

/// Read and parse one native scrcpy message or AI extension envelope from an
/// async stream.
pub async fn read_device_event_async<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> io::Result<DeviceEvent> {
    let ty = read_u8(reader).await?;
    match ty {
        TYPE_CLIPBOARD => read_clipboard(reader).await.map(DeviceEvent::Native),
        TYPE_ACK_CLIPBOARD => read_ack_clipboard(reader).await.map(DeviceEvent::Native),
        TYPE_UHID_OUTPUT => read_uhid_output(reader).await.map(DeviceEvent::Native),
        _ => read_ai_or_unknown_envelope(reader, ty).await,
    }
}

async fn async_device_message_loop<R>(
    mut reader: R,
    tx: mpsc::Sender<io::Result<DeviceMessage>>,
) -> io::Result<R>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    loop {
        match read_device_message_async(&mut reader).await {
            Ok(msg) => {
                if tx.send(Ok(msg)).await.is_err() {
                    return Ok(reader);
                }
            }
            Err(e) => {
                let _ = tx.send(Err(e)).await;
                return Ok(reader);
            }
        }
    }
}

async fn async_device_event_loop<R>(
    mut reader: R,
    tx: mpsc::Sender<io::Result<DeviceEvent>>,
) -> io::Result<R>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    loop {
        match read_device_event_async(&mut reader).await {
            Ok(event) => {
                if tx.send(Ok(event)).await.is_err() {
                    return Ok(reader);
                }
            }
            Err(e) => {
                let _ = tx.send(Err(e)).await;
                return Ok(reader);
            }
        }
    }
}

async fn async_latest_frame_summary_loop<R>(
    mut reader: R,
    tx: watch::Sender<AsyncLatestFrameSummaryState>,
) -> io::Result<R>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let mut state = AsyncLatestFrameSummaryState::default();
    loop {
        if tx.receiver_count() == 0 {
            return Ok(reader);
        }
        match read_device_event_async(&mut reader).await {
            Ok(DeviceEvent::FrameSummary(summary)) => {
                state.version = state.version.saturating_add(1);
                state.latest = Some(LatestFrameSummarySnapshot {
                    version: state.version,
                    summary,
                });
                tx.send_replace(state.clone());
            }
            Ok(_) => {}
            Err(e) => {
                state.terminal_error = Some(DeviceReadError::from_io_error(e));
                let _ = tx.send(state);
                return Ok(reader);
            }
        }
    }
}

async fn read_clipboard<R: AsyncRead + Unpin>(reader: &mut R) -> io::Result<DeviceMessage> {
    let len = read_u32_be(reader).await? as usize;
    if len > DEVICE_MSG_TEXT_MAX_LENGTH {
        return Err(invalid_data(format!(
            "clipboard payload too large: {len} bytes (max {DEVICE_MSG_TEXT_MAX_LENGTH})"
        )));
    }

    let mut payload = vec![0u8; len];
    if len > 0 {
        reader.read_exact(&mut payload).await?;
    }
    let text = String::from_utf8(payload).map_err(|e| invalid_data(e.to_string()))?;
    Ok(DeviceMessage::Clipboard(text))
}

async fn read_ack_clipboard<R: AsyncRead + Unpin>(reader: &mut R) -> io::Result<DeviceMessage> {
    Ok(DeviceMessage::AckClipboard {
        sequence: read_u64_be(reader).await?,
    })
}

async fn read_uhid_output<R: AsyncRead + Unpin>(reader: &mut R) -> io::Result<DeviceMessage> {
    let id = read_u16_be(reader).await?;
    let size = read_u16_be(reader).await? as usize;
    let max_payload = DEVICE_MSG_MAX_SIZE - 5;
    if size > max_payload {
        return Err(invalid_data(format!(
            "uhid output payload too large: {size} bytes (max {max_payload})"
        )));
    }

    let mut data = vec![0u8; size];
    if size > 0 {
        reader.read_exact(&mut data).await?;
    }
    Ok(DeviceMessage::UhidOutput { id, data })
}

async fn read_ai_or_unknown_envelope<R: AsyncRead + Unpin>(
    reader: &mut R,
    ty: u8,
) -> io::Result<DeviceEvent> {
    let len = read_u32_be(reader).await? as usize;
    let max_payload = DEVICE_MSG_MAX_SIZE - 5;
    if len > max_payload {
        return Err(invalid_data(format!(
            "device event payload too large: {len} bytes (max {max_payload})"
        )));
    }
    let mut payload = vec![0u8; len];
    if len > 0 {
        reader.read_exact(&mut payload).await?;
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

async fn read_u8<R: AsyncRead + Unpin>(reader: &mut R) -> io::Result<u8> {
    let mut b = [0u8; 1];
    reader.read_exact(&mut b).await?;
    Ok(b[0])
}

async fn read_u16_be<R: AsyncRead + Unpin>(reader: &mut R) -> io::Result<u16> {
    let mut b = [0u8; 2];
    reader.read_exact(&mut b).await?;
    Ok(u16::from_be_bytes(b))
}

async fn read_u32_be<R: AsyncRead + Unpin>(reader: &mut R) -> io::Result<u32> {
    let mut b = [0u8; 4];
    reader.read_exact(&mut b).await?;
    Ok(u32::from_be_bytes(b))
}

async fn read_u64_be<R: AsyncRead + Unpin>(reader: &mut R) -> io::Result<u64> {
    let mut b = [0u8; 8];
    reader.read_exact(&mut b).await?;
    Ok(u64::from_be_bytes(b))
}

fn invalid_data(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

fn latest_frame_timeout_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        "async latest frame summary timeout",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use tokio::io::{AsyncWriteExt, BufReader};

    fn ack(sequence: u64) -> Vec<u8> {
        let mut bytes = vec![TYPE_ACK_CLIPBOARD];
        bytes.extend(sequence.to_be_bytes());
        bytes
    }

    fn clipboard(text: &str) -> Vec<u8> {
        let mut bytes = vec![TYPE_CLIPBOARD];
        bytes.extend((text.len() as u32).to_be_bytes());
        bytes.extend(text.as_bytes());
        bytes
    }

    fn uhid_output(id: u16, data: &[u8]) -> Vec<u8> {
        let mut bytes = vec![TYPE_UHID_OUTPUT];
        bytes.extend(id.to_be_bytes());
        bytes.extend((data.len() as u16).to_be_bytes());
        bytes.extend(data);
        bytes
    }

    fn frame_summary_envelope(frame_seq: u32) -> Vec<u8> {
        frame_summary_envelope_at(100, frame_seq)
    }

    fn frame_summary_envelope_at(timestamp_ms: u64, frame_seq: u32) -> Vec<u8> {
        let mut payload = Vec::new();
        payload.extend(timestamp_ms.to_be_bytes());
        payload.extend(frame_seq.to_be_bytes());
        payload.extend(1000u16.to_be_bytes());
        payload.extend(2000u16.to_be_bytes());
        payload.push(crate::ai::FLAG_KEYFRAME | crate::ai::FLAG_OBJECTS);
        payload.extend(0u16.to_be_bytes());
        payload.extend(0u16.to_be_bytes());
        payload.extend(1u16.to_be_bytes());
        payload.extend(100u16.to_be_bytes());
        payload.extend(200u16.to_be_bytes());
        payload.extend(301u16.to_be_bytes());
        payload.extend(101u16.to_be_bytes());
        payload.push(7);
        payload.push(220);
        payload.push(0);

        let mut bytes = vec![TYPE_FRAME_SUMMARY];
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

        let mut bytes = vec![TYPE_AI_STATS];
        bytes.extend((payload.len() as u32).to_be_bytes());
        bytes.extend(payload);
        bytes
    }

    async fn frame_summary_from_envelope_at(timestamp_ms: u64, frame_seq: u32) -> FrameSummary {
        let bytes = frame_summary_envelope_at(timestamp_ms, frame_seq);
        let mut reader = BufReader::new(bytes.as_slice());
        match read_device_event_async(&mut reader).await.unwrap() {
            DeviceEvent::FrameSummary(summary) => summary,
            other => panic!("expected frame summary, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn async_receiver_reads_consecutive_mixed_messages() {
        let mut bytes = Vec::new();
        bytes.extend(ack(7));
        bytes.extend(uhid_output(2, &[0xaa]));
        bytes.extend(clipboard("ok"));
        let mut rx = AsyncDeviceMessageReceiver::new(BufReader::new(bytes.as_slice()));

        assert_eq!(
            rx.read_next().await.unwrap(),
            DeviceMessage::AckClipboard { sequence: 7 }
        );
        assert_eq!(
            rx.read_next().await.unwrap(),
            DeviceMessage::UhidOutput {
                id: 2,
                data: vec![0xaa],
            }
        );
        assert_eq!(
            rx.read_next().await.unwrap(),
            DeviceMessage::Clipboard("ok".to_string())
        );
    }

    #[tokio::test]
    async fn async_receiver_reads_native_ai_and_unknown_events() {
        let mut bytes = Vec::new();
        bytes.extend(ack(7));
        bytes.extend(frame_summary_envelope(42));
        bytes.extend(ai_stats_envelope());
        bytes.push(99);
        bytes.extend(3u32.to_be_bytes());
        bytes.extend([1, 2, 3]);
        let mut rx = AsyncDeviceMessageReceiver::new(BufReader::new(bytes.as_slice()));

        assert_eq!(
            rx.read_next_event().await.unwrap(),
            DeviceEvent::Native(DeviceMessage::AckClipboard { sequence: 7 })
        );
        match rx.read_next_event().await.unwrap() {
            DeviceEvent::FrameSummary(summary) => {
                assert_eq!(summary.frame_seq, 42);
                assert_eq!(summary.objects[0].class_id, 7);
            }
            other => panic!("expected frame summary, got {other:?}"),
        }
        match rx.read_next_event().await.unwrap() {
            DeviceEvent::AiStats(stats) => assert_eq!(stats.frames_sampled, 10),
            other => panic!("expected ai stats, got {other:?}"),
        }
        assert_eq!(
            rx.read_next_event().await.unwrap(),
            DeviceEvent::UnknownEnvelope {
                msg_type: 99,
                payload: vec![1, 2, 3],
            }
        );
    }

    #[tokio::test]
    async fn async_prefix_reader_consumes_dummy_and_device_name() {
        let mut bytes = vec![0x00];
        let mut name = [0u8; DEVICE_NAME_FIELD_LENGTH];
        name[..7].copy_from_slice(b"SM-G991");
        bytes.extend(name);
        bytes.extend(ack(9));

        let mut reader = BufReader::new(bytes.as_slice());
        let prefix = read_scrcpy_control_prefix_async(&mut reader).await.unwrap();
        assert_eq!(prefix.dummy_byte, 0);
        assert_eq!(prefix.device_name, "SM-G991");
        assert_eq!(
            read_device_message_async(&mut reader).await.unwrap(),
            DeviceMessage::AckClipboard { sequence: 9 }
        );
    }

    #[tokio::test]
    async fn async_background_receiver_streams_messages_then_reports_eof() {
        let mut bytes = Vec::new();
        bytes.extend(ack(1));
        bytes.extend(clipboard("ok"));
        let (mut rx, pump) =
            spawn_async_device_message_receiver(BufReader::new(Cursor::new(bytes.clone())), 1);

        assert_eq!(
            rx.recv().await.unwrap().unwrap(),
            DeviceMessage::AckClipboard { sequence: 1 }
        );
        assert_eq!(
            rx.recv().await.unwrap().unwrap(),
            DeviceMessage::Clipboard("ok".to_string())
        );
        let err = rx.recv().await.unwrap().unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);

        let reader = pump.join().await.unwrap();
        assert_eq!(reader.get_ref().get_ref().len(), bytes.len());
    }

    #[tokio::test]
    async fn async_background_event_receiver_streams_mixed_events_then_reports_eof() {
        let mut bytes = Vec::new();
        bytes.extend(ack(1));
        bytes.extend(frame_summary_envelope(2));
        bytes.extend(ai_stats_envelope());
        let (mut rx, pump) =
            spawn_async_device_event_receiver(BufReader::new(Cursor::new(bytes.clone())), 1);

        assert_eq!(
            rx.recv().await.unwrap().unwrap(),
            DeviceEvent::Native(DeviceMessage::AckClipboard { sequence: 1 })
        );
        match rx.recv().await.unwrap().unwrap() {
            DeviceEvent::FrameSummary(summary) => assert_eq!(summary.frame_seq, 2),
            other => panic!("expected frame summary, got {other:?}"),
        }
        match rx.recv().await.unwrap().unwrap() {
            DeviceEvent::AiStats(stats) => assert_eq!(stats.frames_sampled, 10),
            other => panic!("expected ai stats, got {other:?}"),
        }
        let err = rx.recv().await.unwrap().unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);

        let reader = pump.join().await.unwrap();
        assert_eq!(reader.get_ref().get_ref().len(), bytes.len());
    }

    #[tokio::test]
    async fn async_latest_frame_summary_receiver_keeps_newest_frame_and_reports_terminal_error() {
        let mut bytes = Vec::new();
        bytes.extend(ack(1));
        bytes.extend(frame_summary_envelope_at(100, 2));
        bytes.extend(ai_stats_envelope());
        bytes.extend(frame_summary_envelope_at(200, 3));
        bytes.push(99);
        bytes.extend(3u32.to_be_bytes());
        bytes.extend([1, 2, 3]);
        bytes.extend(frame_summary_envelope_at(250, 4));
        let (mut latest, pump) =
            spawn_async_latest_frame_summary_receiver(BufReader::new(Cursor::new(bytes.clone())));

        let reader = pump.join().await.unwrap();
        assert_eq!(reader.get_ref().get_ref().len(), bytes.len());

        let snapshot = latest.wait_first().await.unwrap();
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
                .await
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
                .await
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
                .await
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
                .await
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
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
        assert_eq!(
            latest.terminal_error().unwrap().kind,
            io::ErrorKind::UnexpectedEof
        );
    }

    #[tokio::test]
    async fn async_latest_frame_summary_receiver_reports_eof_without_frames() {
        let mut bytes = Vec::new();
        bytes.extend(ack(1));
        bytes.extend(ai_stats_envelope());
        let (mut latest, pump) =
            spawn_async_latest_frame_summary_receiver(BufReader::new(Cursor::new(bytes.clone())));

        let reader = pump.join().await.unwrap();
        assert_eq!(reader.get_ref().get_ref().len(), bytes.len());

        assert!(latest.snapshot().is_none());
        let err = latest.wait_first().await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[tokio::test]
    async fn async_latest_frame_summary_timeout_bounds_empty_wait() {
        let (_tx, rx) = watch::channel(AsyncLatestFrameSummaryState::default());
        let mut latest = AsyncLatestFrameSummaryReceiver { rx };

        let err = latest
            .wait_first_timeout(Duration::from_millis(1))
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
    }

    #[tokio::test]
    async fn async_latest_frame_summary_timeout_returns_cached_match() {
        let bytes = frame_summary_envelope_at(250, 7);
        let mut reader = BufReader::new(bytes.as_slice());
        let summary = match read_device_event_async(&mut reader).await.unwrap() {
            DeviceEvent::FrameSummary(summary) => summary,
            other => panic!("expected frame summary, got {other:?}"),
        };
        let (_tx, rx) = watch::channel(AsyncLatestFrameSummaryState {
            latest: Some(LatestFrameSummarySnapshot {
                version: 1,
                summary,
            }),
            version: 1,
            terminal_error: None,
        });
        let mut latest = AsyncLatestFrameSummaryReceiver { rx };

        let snapshot = latest
            .wait_after_frame_seq_timeout(6, Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(snapshot.version, 1);
        assert_eq!(snapshot.summary.frame_seq, 7);
    }

    #[tokio::test]
    async fn async_latest_frame_summary_matching_waits_skip_cached_miss() {
        let (tx, rx) = watch::channel(AsyncLatestFrameSummaryState::default());
        let mut latest = AsyncLatestFrameSummaryReceiver { rx };
        tx.send(AsyncLatestFrameSummaryState {
            latest: Some(LatestFrameSummarySnapshot {
                version: 1,
                summary: frame_summary_from_envelope_at(100, 1).await,
            }),
            version: 1,
            terminal_error: None,
        })
        .unwrap();

        assert!(latest
            .snapshot_matching(|snapshot| snapshot.summary.frame_seq > 1)
            .is_none());
        let err = latest
            .wait_next_matching_timeout(0, Duration::from_millis(1), |snapshot| {
                snapshot.summary.frame_seq > 1
            })
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);

        tx.send(AsyncLatestFrameSummaryState {
            latest: Some(LatestFrameSummarySnapshot {
                version: 2,
                summary: frame_summary_from_envelope_at(180, 2).await,
            }),
            version: 2,
            terminal_error: None,
        })
        .unwrap();
        let snapshot = latest
            .wait_next_matching(0, |snapshot| snapshot.summary.frame_seq > 1)
            .await
            .unwrap();
        assert_eq!(snapshot.version, 2);
        assert_eq!(snapshot.summary.frame_seq, 2);
        assert_eq!(
            latest
                .wait_matching_timeout(Duration::from_secs(1), |snapshot| {
                    snapshot.summary.timestamp_ms > 150
                })
                .await
                .unwrap()
                .summary
                .timestamp_ms,
            180
        );
    }

    #[tokio::test]
    async fn async_parser_handles_streaming_duplex_input() {
        let (mut writer, reader) = tokio::io::duplex(64);
        let write = tokio::spawn(async move {
            writer.write_all(&ack(5)).await.unwrap();
            writer.write_all(&clipboard("duplex")).await.unwrap();
        });
        let mut rx = AsyncDeviceMessageReceiver::new(reader);

        assert_eq!(
            rx.read_next().await.unwrap(),
            DeviceMessage::AckClipboard { sequence: 5 }
        );
        assert_eq!(
            rx.read_next().await.unwrap(),
            DeviceMessage::Clipboard("duplex".to_string())
        );
        write.await.unwrap();
    }
}
