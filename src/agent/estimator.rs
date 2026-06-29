use crate::client::{
    AndroidKeyFrameBatcher, GamepadFrameBatcher, HidClient, KeyboardFrameBatcher,
    MouseFrameBatcher, PackedGamepadFrameBatcher, ScrollFrameBatcher, TouchFrameBatcher,
    ANDROID_KEY_BATCH_FRAMES, GAMEPAD_BATCH_FRAMES, KEYBOARD_BATCH_FRAMES, MOUSE_BATCH_FRAMES,
    SCROLL_BATCH_FRAMES, TOUCH_BATCH_FRAMES,
};
use crate::error::{Error, Result};
use crate::session::{GamepadFrameRaw, GAMEPAD_FRAME_BYTES};

use super::action::{AgentAction, AgentPlanBoundedPrefix, AgentPlanBoundedPrefixStop};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanGamepadBatchMode {
    Empty,
    Dedupe,
    Unchecked,
    Packed,
}

#[derive(Debug)]
pub struct PlanGamepadBatcher<'a> {
    mode: PlanGamepadBatchMode,
    dedupe: GamepadFrameBatcher<'a>,
    unchecked: GamepadFrameBatcher<'a>,
    packed: PackedGamepadFrameBatcher<'a>,
}

pub type PlanBatchers<'a, 'b> = (
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
    pub(super) const fn new(capacity: usize) -> Self {
        Self { len: 0, capacity }
    }

    pub(super) fn push_frames(&mut self, frames: usize) -> usize {
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

    pub(super) fn flush(&mut self) -> usize {
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
    pub(super) const fn new() -> Self {
        Self {
            mode: PlanGamepadBatchMode::Empty,
            batch: EstimatedFrameBatch::new(GAMEPAD_BATCH_FRAMES),
        }
    }

    pub(super) fn push_frames(&mut self, mode: PlanGamepadBatchMode, frames: usize) -> usize {
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

    pub(super) fn flush(&mut self) -> usize {
        let sends = self.batch.flush();
        self.mode = PlanGamepadBatchMode::Empty;
        sends
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlanCommandEstimator {
    commands: usize,
    touch: EstimatedFrameBatch,
    key: EstimatedFrameBatch,
    android_key: EstimatedFrameBatch,
    mouse: EstimatedFrameBatch,
    scroll: EstimatedFrameBatch,
    gamepad: EstimatedGamepadBatch,
}

impl PlanCommandEstimator {
    pub(super) const fn new() -> Self {
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

    pub(super) fn estimate_queue_actions(actions: &[AgentAction]) -> usize {
        let mut estimator = Self::new();
        for action in actions {
            estimator.push_action(action);
        }
        estimator.commands_after_final_flush()
    }

    pub(super) fn bounded_try_queue_prefix(
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

    pub(super) fn bounded_try_run_prefix(
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

    pub(super) fn commands_after_final_flush(mut self) -> usize {
        self.flush_all();
        self.commands
    }

    pub(super) fn add_commands(&mut self, commands: usize) {
        self.commands = self.commands.saturating_add(commands);
    }

    pub(super) fn flush_touch(&mut self) {
        let commands = self.touch.flush();
        self.add_commands(commands);
    }

    pub(super) fn flush_key(&mut self) {
        let commands = self.key.flush();
        self.add_commands(commands);
    }

    pub(super) fn flush_android_key(&mut self) {
        let commands = self.android_key.flush();
        self.add_commands(commands);
    }

    pub(super) fn flush_mouse(&mut self) {
        let commands = self.mouse.flush();
        self.add_commands(commands);
    }

    pub(super) fn flush_scroll(&mut self) {
        let commands = self.scroll.flush();
        self.add_commands(commands);
    }

    pub(super) fn flush_gamepad(&mut self) {
        let commands = self.gamepad.flush();
        self.add_commands(commands);
    }

    pub(super) fn flush_all(&mut self) {
        self.flush_touch();
        self.flush_key();
        self.flush_android_key();
        self.flush_mouse();
        self.flush_scroll();
        self.flush_gamepad();
    }

    pub(super) fn flush_non_touch(&mut self) {
        self.flush_key();
        self.flush_android_key();
        self.flush_mouse();
        self.flush_gamepad();
    }

    pub(super) fn flush_non_key(&mut self) {
        self.flush_touch();
        self.flush_android_key();
        self.flush_mouse();
        self.flush_gamepad();
    }

    pub(super) fn flush_non_android_key(&mut self) {
        self.flush_touch();
        self.flush_key();
        self.flush_mouse();
        self.flush_gamepad();
    }

    pub(super) fn flush_non_mouse(&mut self) {
        self.flush_touch();
        self.flush_key();
        self.flush_android_key();
        self.flush_gamepad();
    }

    pub(super) fn flush_non_scroll(&mut self) {
        self.flush_touch();
        self.flush_key();
        self.flush_android_key();
        self.flush_mouse();
        self.flush_gamepad();
    }

    pub(super) fn flush_non_gamepad(&mut self) {
        self.flush_touch();
        self.flush_key();
        self.flush_android_key();
        self.flush_mouse();
    }

    pub(super) fn push_touch(&mut self, frames: usize) {
        let commands = self.touch.push_frames(frames);
        self.add_commands(commands);
    }

    pub(super) fn push_key(&mut self, frames: usize) {
        let commands = self.key.push_frames(frames);
        self.add_commands(commands);
    }

    pub(super) fn push_android_key(&mut self, frames: usize) {
        let commands = self.android_key.push_frames(frames);
        self.add_commands(commands);
    }

    pub(super) fn push_mouse(&mut self, frames: usize) {
        let commands = self.mouse.push_frames(frames);
        self.add_commands(commands);
    }

    pub(super) fn push_scroll(&mut self, frames: usize) {
        let commands = self.scroll.push_frames(frames);
        self.add_commands(commands);
    }

    pub(super) fn push_gamepad(&mut self, mode: PlanGamepadBatchMode, frames: usize) {
        let commands = self.gamepad.push_frames(mode, frames);
        self.add_commands(commands);
    }

    pub(super) fn push_action(&mut self, action: &AgentAction) {
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

    pub(super) fn is_scroll_action(action: &AgentAction) -> bool {
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
    pub(super) fn new(client: &'a HidClient) -> Self {
        Self {
            mode: PlanGamepadBatchMode::Empty,
            dedupe: GamepadFrameBatcher::dedupe(client, GAMEPAD_BATCH_FRAMES),
            unchecked: GamepadFrameBatcher::unchecked(client, GAMEPAD_BATCH_FRAMES),
            packed: PackedGamepadFrameBatcher::new(client, GAMEPAD_BATCH_FRAMES),
        }
    }

    pub(super) fn push_dedupe(&mut self, frame: GamepadFrameRaw) -> Result<()> {
        self.ensure_mode(PlanGamepadBatchMode::Dedupe)?;
        self.dedupe.push(frame)
    }

    pub(super) fn try_push_dedupe(&mut self, frame: GamepadFrameRaw) -> Result<()> {
        self.try_ensure_mode(PlanGamepadBatchMode::Dedupe)?;
        self.dedupe.try_push(frame)
    }

    pub(super) fn push_unchecked(&mut self, frame: GamepadFrameRaw) -> Result<()> {
        self.ensure_mode(PlanGamepadBatchMode::Unchecked)?;
        self.unchecked.push(frame)
    }

    pub(super) fn try_push_unchecked(&mut self, frame: GamepadFrameRaw) -> Result<()> {
        self.try_ensure_mode(PlanGamepadBatchMode::Unchecked)?;
        self.unchecked.try_push(frame)
    }

    pub(super) fn push_packed(&mut self, frame: [u8; GAMEPAD_FRAME_BYTES]) -> Result<()> {
        self.ensure_mode(PlanGamepadBatchMode::Packed)?;
        self.packed.push(frame)
    }

    pub(super) fn try_push_packed(&mut self, frame: [u8; GAMEPAD_FRAME_BYTES]) -> Result<()> {
        self.try_ensure_mode(PlanGamepadBatchMode::Packed)?;
        self.packed.try_push(frame)
    }

    pub(super) fn push_dedupe_slice(
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

    pub(super) fn try_push_dedupe_slice(
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

    pub(super) fn push_unchecked_slice(
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

    pub(super) fn try_push_unchecked_slice(
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

    pub(super) fn push_packed_slice(
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

    pub(super) fn try_push_packed_slice(
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

    pub(super) fn flush(&mut self) -> Result<()> {
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

    pub(super) fn try_flush(&mut self) -> Result<()> {
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

    pub(super) fn ensure_mode(&mut self, mode: PlanGamepadBatchMode) -> Result<()> {
        if self.mode != PlanGamepadBatchMode::Empty && self.mode != mode {
            self.flush()?;
        }
        if self.mode == PlanGamepadBatchMode::Empty {
            self.mode = mode;
        }
        Ok(())
    }

    pub(super) fn try_ensure_mode(&mut self, mode: PlanGamepadBatchMode) -> Result<()> {
        if self.mode != PlanGamepadBatchMode::Empty && self.mode != mode {
            self.try_flush()?;
        }
        if self.mode == PlanGamepadBatchMode::Empty {
            self.mode = mode;
        }
        Ok(())
    }
}
