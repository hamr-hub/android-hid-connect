use std::net::TcpStream;
use std::time::Duration;

use crate::ai::{AiStats, FrameSummary};
use crate::device::{read_scrcpy_control_prefix, ScrcpyControlPrefix};
use crate::error::{Error, Result, TransportWrite};
use crate::session::{HidSession, OpenRequest};
use crate::transport::open_tcp;
use crate::types::{ClipboardCopyKey, TouchPointerId};

use super::action::AgentAction;
use super::geometry::io_to_error;
use super::types::{AgentObjectSelector, AgentRect, AgentTargetSelector};
use super::AgentControlSession;

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
