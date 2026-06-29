use std::io::{self, Read};
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::Duration;

use crate::ai::{AiStats, FrameSummary};
use crate::client::{
    AndroidKeyFrame, AndroidKeyFrameBatcher, HidClient, HidCommand, KeyboardChordFrame,
    KeyboardFrame, KeyboardFrameBatcher, MouseFrame, MouseFrameBatcher, ScrollFrame,
    ScrollFrameBatcher, TouchFrame, TouchFrameBatcher, ANDROID_KEY_BATCH_FRAMES,
    GAMEPAD_BATCH_FRAMES, KEYBOARD_BATCH_FRAMES, MOUSE_BATCH_FRAMES, SCROLL_BATCH_FRAMES,
    TOUCH_BATCH_FRAMES,
};
use crate::device::{
    spawn_latest_frame_summary_receiver, DeviceEvent, DeviceMessage, DeviceMessagePump,
    DeviceMessageReceiver, LatestFrameSummaryBoundary, LatestFrameSummaryObservation,
    LatestFrameSummaryReceiver, LatestFrameSummarySnapshot,
};
use crate::error::{Error, Result, TransportWrite};
use crate::session::{GamepadFrameRaw, HidSession, GAMEPAD_FRAME_BYTES};
use crate::types::{
    AndroidKeyAction, AndroidKeycode, ClipboardCopyKey, GamepadAxis, GamepadButton, Modifiers,
    MouseButton, Scancode, TouchPointerId,
};

use super::action::{AgentAction, AgentPlanBoundedPrefix, AgentPlanBoundedPrefixStop};
use super::estimator::{PlanBatchers, PlanGamepadBatcher};
use super::geometry::{frame_summary_is_stable, io_to_error, io_to_wait_error, lerp_i32};
use super::types::{
    AgentObjectSelector, AgentPoint, AgentRect, AgentScrollFrame, AgentTargetSelector,
    AgentTouchFrame,
};
use super::{
    AgentControlCloseReport, AgentControlClosed, AgentControlCommandCloseReport,
    AgentControlSession, DEFAULT_AGENT_COMMAND_BOUND, DEFAULT_AGENT_SCREEN_HEIGHT,
    DEFAULT_AGENT_SCREEN_WIDTH, LAUNCH_APP_NAME_TOO_LONG, TIMED_ACTION_REQUIRES_BLOCKING,
    TRY_AI_EXCEEDS_COMMAND_BOUND, TRY_ANDROID_KEY_EXCEEDS_COMMAND_BOUND,
    TRY_CLIPBOARD_EXCEEDS_COMMAND_BOUND, TRY_CONTROL_EXCEEDS_COMMAND_BOUND,
    TRY_DOUBLE_TAP_EXCEEDS_COMMAND_BOUND, TRY_GAMEPAD_EXCEEDS_COMMAND_BOUND,
    TRY_KEY_EXCEEDS_COMMAND_BOUND, TRY_MOUSE_EXCEEDS_COMMAND_BOUND, TRY_RUN_EXCEEDS_COMMAND_BOUND,
    TRY_SCROLL_EXCEEDS_COMMAND_BOUND, TRY_TAP_EXCEEDS_COMMAND_BOUND,
};

impl<T, R> AgentControlSession<T, R>
where
    T: TransportWrite + Send + 'static,
    R: Read,
{
    /// Build an agent session from an already-opened `HidSession` and a
    /// byte-aligned device-message reader.
    pub fn from_parts(session: HidSession<T>, reader: R) -> Result<Self> {
        Self::from_parts_with_bound(session, reader, DEFAULT_AGENT_COMMAND_BOUND)
    }

    /// Same as [`Self::from_parts`], but with an explicit command-channel
    /// bound for high-rate producer loops.
    pub fn from_parts_with_bound(
        session: HidSession<T>,
        reader: R,
        command_bound: usize,
    ) -> Result<Self> {
        let (client, dispatcher) = session.into_client_with_bound(command_bound)?;
        Ok(Self {
            client,
            dispatcher: Some(dispatcher),
            receiver: Some(DeviceMessageReceiver::new(reader)),
            command_bound,
            next_clipboard_sequence: 1,
            screen_width: AtomicU16::new(DEFAULT_AGENT_SCREEN_WIDTH),
            screen_height: AtomicU16::new(DEFAULT_AGENT_SCREEN_HEIGHT),
        })
    }

    /// Producer handle for sending control commands.
    pub fn client(&self) -> &HidClient {
        &self.client
    }

    /// Cloneable producer handle for worker threads or agent tools.
    pub fn clone_client(&self) -> HidClient {
        self.client.clone()
    }

    /// Configured dispatcher command-channel bound for this session.
    pub const fn command_bound(&self) -> usize {
        self.command_bound
    }

    /// Analyze the longest safe non-blocking prefix using this session's
    /// configured dispatcher command-channel bound, without dispatching it.
    ///
    /// Use this when a scheduler needs to split or route a plan before touching
    /// the bounded producer queue. Use
    /// [`Self::try_queue_actions_bounded_prefix_with_session_bound`] when the
    /// accepted prefix should be dispatched immediately.
    pub fn bounded_try_queue_prefix_with_session_bound(
        &self,
        actions: &[AgentAction],
    ) -> AgentPlanBoundedPrefix {
        AgentAction::bounded_try_queue_prefix(actions, self.command_bound)
    }

    /// Analyze the longest checked non-blocking prefix using this session's
    /// configured dispatcher command-channel bound, without dispatching it.
    ///
    /// Unlike [`Self::bounded_try_queue_prefix_with_session_bound`], this
    /// reserves one command slot for the final checked barrier used by
    /// [`Self::try_run_actions_bounded_prefix_with_session_bound`].
    pub fn bounded_try_run_prefix_with_session_bound(
        &self,
        actions: &[AgentAction],
    ) -> AgentPlanBoundedPrefix {
        AgentAction::bounded_try_run_prefix(actions, self.command_bound)
    }

    /// Update screen dimensions used by subsequent touch injection.
    pub fn set_screen_size(&self, width: u16, height: u16) -> Result<()> {
        self.client.set_screen_size(width, height)?;
        self.screen_width.store(width, Ordering::Relaxed);
        self.screen_height.store(height, Ordering::Relaxed);
        Ok(())
    }

    /// Update screen dimensions using non-blocking dispatcher send, then
    /// enqueue one checked dispatcher barrier before updating agent-local
    /// coordinate metadata.
    pub fn try_set_screen_size(&self, width: u16, height: u16) -> Result<()> {
        self.ensure_direct_try_capacity(TRY_CONTROL_EXCEEDS_COMMAND_BOUND)?;
        self.client.try_set_screen_size(width, height)?;
        self.client.try_flush_wait()?;
        self.screen_width.store(width, Ordering::Relaxed);
        self.screen_height.store(height, Ordering::Relaxed);
        Ok(())
    }

    /// Current agent-local screen size used for generated gesture paths.
    pub fn screen_size(&self) -> (u16, u16) {
        (
            self.screen_width.load(Ordering::Relaxed),
            self.screen_height.load(Ordering::Relaxed),
        )
    }

    fn ensure_direct_try_capacity(&self, msg: &'static str) -> Result<()> {
        if self.command_bound < 2 {
            return Err(Error::SessionLifecycle(msg));
        }
        Ok(())
    }

    fn try_finish_direct_gamepad_command(&self) -> Result<()> {
        self.client.try_flush_wait().map(|_| ())
    }

    fn try_run_direct_command(&self, cmd: HidCommand, msg: &'static str) -> Result<()> {
        self.ensure_direct_try_capacity(msg)?;
        self.client.try_send(cmd)?;
        self.client.try_flush_wait().map(|_| ())
    }

    /// Read the next server→host message from the byte-aligned receiver.
    pub fn recv_device_message(&mut self) -> io::Result<DeviceMessage> {
        self.receiver_mut()?.read_next()
    }

    /// Read the next native scrcpy message or AI extension event.
    pub fn recv_device_event(&mut self) -> io::Result<DeviceEvent> {
        self.receiver_mut()?.read_next_event()
    }

    /// Move the session's byte-aligned reader into a latest-frame background
    /// pump.
    ///
    /// This is an explicit mode switch for low-latency perception loops:
    /// the returned [`LatestFrameSummaryReceiver`] continuously drains the
    /// mixed server→host stream and keeps only the newest AI frame summary.
    /// After detaching, ordered read helpers such as clipboard ACK waits are no
    /// longer available on this agent; use [`Self::close_transport`] or
    /// [`Self::close_transport_checked`] to close the write side, and join the
    /// returned pump to recover the reader.
    pub fn detach_latest_frame_summary_receiver(
        &mut self,
    ) -> Result<(LatestFrameSummaryReceiver, DeviceMessagePump<R>)>
    where
        R: Send + 'static,
    {
        let reader = self
            .receiver
            .take()
            .ok_or(Error::DispatcherDown("agent receiver already taken"))?
            .into_inner();
        spawn_latest_frame_summary_receiver(reader).map_err(io_to_error)
    }

    /// Type text into the focused field using the dispatcher thread.
    pub fn type_text(&self, text: impl Into<String>) -> Result<()> {
        self.client.type_text(text)
    }

    /// Type text into the focused field and fail at the next checked
    /// dispatcher boundary if any character cannot be represented as a USB HID
    /// keyboard scancode.
    pub fn type_text_strict(&self, text: impl Into<String>) -> Result<()> {
        self.client.type_text_strict(text)
    }

    /// Send one raw USB HID keyboard scancode edge.
    pub fn key(&self, scancode: u8, pressed: bool, mods: Modifiers) -> Result<()> {
        self.client.key(scancode, pressed, mods)
    }

    /// Send one raw USB HID keyboard scancode edge using non-blocking
    /// dispatcher send, then enqueue one checked dispatcher barrier.
    pub fn try_key(&self, scancode: u8, pressed: bool, mods: Modifiers) -> Result<()> {
        self.ensure_direct_try_capacity(TRY_KEY_EXCEEDS_COMMAND_BOUND)?;
        self.client.try_key(scancode, pressed, mods)?;
        self.client.try_flush_wait().map(|_| ())
    }

    /// Send one typed USB HID keyboard scancode edge.
    pub fn key_scancode(&self, scancode: Scancode, pressed: bool, mods: Modifiers) -> Result<()> {
        self.client.key_scancode(scancode, pressed, mods)
    }

    /// Send one typed USB HID keyboard scancode edge using non-blocking
    /// dispatcher send, then enqueue one checked dispatcher barrier.
    pub fn try_key_scancode(
        &self,
        scancode: Scancode,
        pressed: bool,
        mods: Modifiers,
    ) -> Result<()> {
        self.try_key(scancode.to_u8(), pressed, mods)
    }

    /// Press and release one raw USB HID keyboard scancode through one
    /// dispatcher command.
    pub fn tap_key(&self, scancode: u8, mods: Modifiers) -> Result<()> {
        self.client.tap_key(scancode, mods)
    }

    /// Press and release one raw USB HID keyboard scancode using non-blocking
    /// dispatcher send, then enqueue one checked dispatcher barrier.
    pub fn try_tap_key(&self, scancode: u8, mods: Modifiers) -> Result<()> {
        self.ensure_direct_try_capacity(TRY_KEY_EXCEEDS_COMMAND_BOUND)?;
        self.client.try_tap_key(scancode, mods)?;
        self.client.try_flush_wait().map(|_| ())
    }

    /// Press and release one typed USB HID keyboard scancode through one
    /// dispatcher command.
    pub fn tap_scancode(&self, scancode: Scancode, mods: Modifiers) -> Result<()> {
        self.client.tap_scancode(scancode, mods)
    }

    /// Press and release one typed USB HID keyboard scancode using
    /// non-blocking dispatcher send, then enqueue one checked dispatcher
    /// barrier.
    pub fn try_tap_scancode(&self, scancode: Scancode, mods: Modifiers) -> Result<()> {
        self.try_tap_key(scancode.to_u8(), mods)
    }

    /// Send one keyboard chord as a fixed-buffer edge batch.
    pub fn key_chord(&self, chord: KeyboardChordFrame) -> Result<()> {
        self.client.key_chord(chord)
    }

    /// Send one keyboard chord as a fixed-buffer edge batch using
    /// non-blocking dispatcher send, then enqueue one checked dispatcher
    /// barrier.
    pub fn try_key_chord(&self, chord: KeyboardChordFrame) -> Result<()> {
        self.ensure_direct_try_capacity(TRY_KEY_EXCEEDS_COMMAND_BOUND)?;
        self.client.try_key_chord(chord)?;
        self.client.try_flush_wait().map(|_| ())
    }

    /// Send one keyboard chord from typed scancodes.
    pub fn scancode_chord(&self, scancodes: &[Scancode], mods: Modifiers) -> Result<()> {
        self.client.scancode_chord(scancodes, mods)
    }

    /// Send one keyboard chord from typed scancodes using non-blocking
    /// dispatcher send, then enqueue one checked dispatcher barrier.
    pub fn try_scancode_chord(&self, scancodes: &[Scancode], mods: Modifiers) -> Result<()> {
        self.ensure_direct_try_capacity(TRY_KEY_EXCEEDS_COMMAND_BOUND)?;
        self.client.try_scancode_chord(scancodes, mods)?;
        self.client.try_flush_wait().map(|_| ())
    }

    /// Inject a raw Android `KeyEvent.KEYCODE_*` control message.
    pub fn inject_keycode(
        &self,
        action: u8,
        keycode: u32,
        repeat: u32,
        metastate: u32,
    ) -> Result<()> {
        self.client
            .inject_keycode(action, keycode, repeat, metastate)
    }

    /// Inject a raw Android `KeyEvent.KEYCODE_*` control message using
    /// non-blocking dispatcher send, then enqueue one checked dispatcher
    /// barrier.
    pub fn try_inject_keycode(
        &self,
        action: u8,
        keycode: u32,
        repeat: u32,
        metastate: u32,
    ) -> Result<()> {
        self.ensure_direct_try_capacity(TRY_ANDROID_KEY_EXCEEDS_COMMAND_BOUND)?;
        self.client
            .try_inject_keycode(action, keycode, repeat, metastate)?;
        self.client.try_flush_wait().map(|_| ())
    }

    /// Inject a typed Android `KeyEvent.KEYCODE_*` control message.
    pub fn inject_android_keycode(
        &self,
        action: u8,
        keycode: AndroidKeycode,
        repeat: u32,
        metastate: u32,
    ) -> Result<()> {
        self.client
            .inject_android_keycode(action, keycode, repeat, metastate)
    }

    /// Inject a typed Android `KeyEvent.KEYCODE_*` control message using
    /// non-blocking dispatcher send, then enqueue one checked dispatcher
    /// barrier.
    pub fn try_inject_android_keycode(
        &self,
        action: u8,
        keycode: AndroidKeycode,
        repeat: u32,
        metastate: u32,
    ) -> Result<()> {
        self.try_inject_keycode(action, keycode.value(), repeat, metastate)
    }

    /// Inject a fully typed Android key event.
    pub fn inject_android_key_event(
        &self,
        action: AndroidKeyAction,
        keycode: AndroidKeycode,
        repeat: u32,
        metastate: u32,
    ) -> Result<()> {
        self.client
            .inject_android_key_event(action, keycode, repeat, metastate)
    }

    /// Inject a fully typed Android key event using non-blocking dispatcher
    /// send, then enqueue one checked dispatcher barrier.
    pub fn try_inject_android_key_event(
        &self,
        action: AndroidKeyAction,
        keycode: AndroidKeycode,
        repeat: u32,
        metastate: u32,
    ) -> Result<()> {
        self.try_inject_android_keycode(action.value(), keycode, repeat, metastate)
    }

    /// Press one typed Android keycode with action DOWN.
    pub fn press_android_key(&self, keycode: AndroidKeycode) -> Result<()> {
        self.client.press_android_key(keycode)
    }

    /// Press one typed Android keycode with action DOWN using non-blocking
    /// dispatcher send, then enqueue one checked dispatcher barrier.
    pub fn try_press_android_key(&self, keycode: AndroidKeycode) -> Result<()> {
        self.ensure_direct_try_capacity(TRY_ANDROID_KEY_EXCEEDS_COMMAND_BOUND)?;
        self.client.try_press_android_key(keycode)?;
        self.client.try_flush_wait().map(|_| ())
    }

    /// Release one typed Android keycode with action UP.
    pub fn release_android_key(&self, keycode: AndroidKeycode) -> Result<()> {
        self.client.release_android_key(keycode)
    }

    /// Release one typed Android keycode with action UP using non-blocking
    /// dispatcher send, then enqueue one checked dispatcher barrier.
    pub fn try_release_android_key(&self, keycode: AndroidKeycode) -> Result<()> {
        self.ensure_direct_try_capacity(TRY_ANDROID_KEY_EXCEEDS_COMMAND_BOUND)?;
        self.client.try_release_android_key(keycode)?;
        self.client.try_flush_wait().map(|_| ())
    }

    /// Press and release one raw Android `KeyEvent.KEYCODE_*` through one
    /// dispatcher command.
    pub fn tap_android_keycode(&self, keycode: u32, metastate: u32) -> Result<()> {
        self.client.tap_android_keycode(keycode, metastate)
    }

    /// Press and release one raw Android `KeyEvent.KEYCODE_*` using
    /// non-blocking dispatcher send, then enqueue one checked dispatcher
    /// barrier.
    pub fn try_tap_android_keycode(&self, keycode: u32, metastate: u32) -> Result<()> {
        self.ensure_direct_try_capacity(TRY_ANDROID_KEY_EXCEEDS_COMMAND_BOUND)?;
        self.client.try_tap_android_keycode(keycode, metastate)?;
        self.client.try_flush_wait().map(|_| ())
    }

    /// Press and release one typed Android keycode through one dispatcher
    /// command.
    pub fn tap_android_key(&self, keycode: AndroidKeycode) -> Result<()> {
        self.client.tap_android_key(keycode)
    }

    /// Press and release one typed Android keycode using non-blocking
    /// dispatcher send, then enqueue one checked dispatcher barrier.
    pub fn try_tap_android_key(&self, keycode: AndroidKeycode) -> Result<()> {
        self.try_tap_android_keycode(keycode.value(), 0)
    }

    /// Press and release one typed Android keycode with a metastate.
    pub fn tap_android_key_with_metastate(
        &self,
        keycode: AndroidKeycode,
        metastate: u32,
    ) -> Result<()> {
        self.client
            .tap_android_key_with_metastate(keycode, metastate)
    }

    /// Press and release one typed Android keycode with a metastate using
    /// non-blocking dispatcher send, then enqueue one checked dispatcher
    /// barrier.
    pub fn try_tap_android_key_with_metastate(
        &self,
        keycode: AndroidKeycode,
        metastate: u32,
    ) -> Result<()> {
        self.try_tap_android_keycode(keycode.value(), metastate)
    }

    /// Send scrcpy BACK_OR_SCREEN_ON. If the screen is off, scrcpy wakes it;
    /// otherwise it behaves like Back for the supplied key action.
    pub fn back_or_screen_on(&self, action: AndroidKeyAction) -> Result<()> {
        self.client.back_or_screen_on(action)
    }

    /// Send scrcpy BACK_OR_SCREEN_ON using non-blocking dispatcher send, then
    /// enqueue one checked dispatcher barrier.
    pub fn try_back_or_screen_on(&self, action: AndroidKeyAction) -> Result<()> {
        self.ensure_direct_try_capacity(TRY_ANDROID_KEY_EXCEEDS_COMMAND_BOUND)?;
        self.client.try_back_or_screen_on(action)?;
        self.client.try_flush_wait().map(|_| ())
    }

    /// Press the Home key.
    pub fn press_home(&self) -> Result<()> {
        self.client.press_home()
    }

    /// Press the Home key using non-blocking dispatcher send, then enqueue one
    /// checked dispatcher barrier.
    pub fn try_press_home(&self) -> Result<()> {
        self.try_press_android_key(AndroidKeycode::HOME)
    }

    /// Press the Back key.
    pub fn press_back(&self) -> Result<()> {
        self.client.press_back()
    }

    /// Press the Back key using non-blocking dispatcher send, then enqueue one
    /// checked dispatcher barrier.
    pub fn try_press_back(&self) -> Result<()> {
        self.try_press_android_key(AndroidKeycode::BACK)
    }

    /// Open the Android recents / app switcher.
    pub fn open_recents(&self) -> Result<()> {
        self.client.open_recents()
    }

    /// Open the Android recents / app switcher using non-blocking dispatcher
    /// send, then enqueue one checked dispatcher barrier.
    pub fn try_open_recents(&self) -> Result<()> {
        self.try_press_android_key(AndroidKeycode::APP_SWITCH)
    }

    /// Press Volume Up.
    pub fn volume_up(&self) -> Result<()> {
        self.client.volume_up()
    }

    /// Press Volume Up using non-blocking dispatcher send, then enqueue one
    /// checked dispatcher barrier.
    pub fn try_volume_up(&self) -> Result<()> {
        self.try_press_android_key(AndroidKeycode::VOLUME_UP)
    }

    /// Press Volume Down.
    pub fn volume_down(&self) -> Result<()> {
        self.client.volume_down()
    }

    /// Press Volume Down using non-blocking dispatcher send, then enqueue one
    /// checked dispatcher barrier.
    pub fn try_volume_down(&self) -> Result<()> {
        self.try_press_android_key(AndroidKeycode::VOLUME_DOWN)
    }

    /// Press Volume Mute.
    pub fn volume_mute(&self) -> Result<()> {
        self.client.volume_mute()
    }

    /// Press Volume Mute using non-blocking dispatcher send, then enqueue one
    /// checked dispatcher barrier.
    pub fn try_volume_mute(&self) -> Result<()> {
        self.try_press_android_key(AndroidKeycode::VOLUME_MUTE)
    }

    /// Send one relative UHID mouse motion report.
    pub fn mouse_motion(&self, dx: i32, dy: i32, buttons: u8) -> Result<()> {
        self.client.mouse_motion(dx, dy, buttons)
    }

    /// Send one relative UHID mouse motion report using non-blocking dispatcher
    /// send, then enqueue one checked dispatcher barrier.
    pub fn try_mouse_motion(&self, dx: i32, dy: i32, buttons: u8) -> Result<()> {
        if self.command_bound < 2 {
            return Err(Error::SessionLifecycle(TRY_MOUSE_EXCEEDS_COMMAND_BOUND));
        }
        self.client.try_mouse_motion(dx, dy, buttons)?;
        self.client.try_flush_wait().map(|_| ())
    }

    /// Send one relative UHID mouse motion report with typed buttons.
    pub fn mouse_motion_buttons(&self, dx: i32, dy: i32, buttons: &[MouseButton]) -> Result<()> {
        self.client.mouse_motion_buttons(dx, dy, buttons)
    }

    /// Send one relative UHID mouse motion report with typed buttons using
    /// non-blocking dispatcher send, then enqueue one checked dispatcher
    /// barrier.
    pub fn try_mouse_motion_buttons(
        &self,
        dx: i32,
        dy: i32,
        buttons: &[MouseButton],
    ) -> Result<()> {
        self.try_mouse_motion(dx, dy, MouseButton::state(buttons))
    }

    /// Send one UHID mouse button-state report.
    pub fn mouse_buttons(&self, buttons: u8) -> Result<()> {
        self.client.mouse_buttons(buttons)
    }

    /// Send one UHID mouse button-state report using non-blocking dispatcher
    /// send, then enqueue one checked dispatcher barrier.
    pub fn try_mouse_buttons(&self, buttons: u8) -> Result<()> {
        if self.command_bound < 2 {
            return Err(Error::SessionLifecycle(TRY_MOUSE_EXCEEDS_COMMAND_BOUND));
        }
        self.client.try_mouse_buttons(buttons)?;
        self.client.try_flush_wait().map(|_| ())
    }

    /// Send one UHID mouse button-state report with typed buttons.
    pub fn mouse_button_state(&self, buttons: &[MouseButton]) -> Result<()> {
        self.client.mouse_button_state(buttons)
    }

    /// Send one UHID mouse button-state report with typed buttons using
    /// non-blocking dispatcher send, then enqueue one checked dispatcher
    /// barrier.
    pub fn try_mouse_button_state(&self, buttons: &[MouseButton]) -> Result<()> {
        self.try_mouse_buttons(MouseButton::state(buttons))
    }

    /// Send one UHID mouse scroll sample.
    pub fn mouse_scroll(&self, hscroll: f32, vscroll: f32) -> Result<()> {
        self.client.mouse_scroll(hscroll, vscroll)
    }

    /// Send one UHID mouse scroll sample using non-blocking dispatcher send,
    /// then enqueue one checked dispatcher barrier.
    pub fn try_mouse_scroll(&self, hscroll: f32, vscroll: f32) -> Result<()> {
        if self.command_bound < 2 {
            return Err(Error::SessionLifecycle(TRY_MOUSE_EXCEEDS_COMMAND_BOUND));
        }
        self.client.try_mouse_scroll(hscroll, vscroll)?;
        self.client.try_flush_wait().map(|_| ())
    }

    /// Send one gamepad button edge.
    pub fn send_button(&self, button: GamepadButton, pressed: bool) -> Result<()> {
        self.client.send_button(button, pressed)
    }

    /// Send one gamepad button edge using non-blocking dispatcher send, then
    /// enqueue one checked dispatcher barrier.
    pub fn try_send_button(&self, button: GamepadButton, pressed: bool) -> Result<()> {
        self.ensure_direct_try_capacity(TRY_GAMEPAD_EXCEEDS_COMMAND_BOUND)?;
        self.client.try_send_button(button, pressed)?;
        self.try_finish_direct_gamepad_command()
    }

    /// Replace all gamepad buttons from a single bitframe.
    pub fn send_buttons(&self, buttons: u32) -> Result<()> {
        self.client.send_buttons(buttons)
    }

    /// Replace all gamepad buttons from a single bitframe using non-blocking
    /// dispatcher send, then enqueue one checked dispatcher barrier.
    pub fn try_send_buttons(&self, buttons: u32) -> Result<()> {
        self.ensure_direct_try_capacity(TRY_GAMEPAD_EXCEEDS_COMMAND_BOUND)?;
        self.client.try_send_buttons(buttons)?;
        self.try_finish_direct_gamepad_command()
    }

    /// Send one normalized gamepad stick/trigger axis update.
    pub fn send_stick(&self, axis: GamepadAxis, value: f32) -> Result<()> {
        self.client.send_stick(axis, value)
    }

    /// Send one normalized gamepad stick/trigger axis update using
    /// non-blocking dispatcher send, then enqueue one checked dispatcher
    /// barrier.
    pub fn try_send_stick(&self, axis: GamepadAxis, value: f32) -> Result<()> {
        self.ensure_direct_try_capacity(TRY_GAMEPAD_EXCEEDS_COMMAND_BOUND)?;
        self.client.try_send_stick(axis, value)?;
        self.try_finish_direct_gamepad_command()
    }

    /// Send one raw gamepad stick/trigger axis update.
    pub fn send_stick_raw(&self, axis: GamepadAxis, value: i16) -> Result<()> {
        self.client.send_stick_raw(axis, value)
    }

    /// Send one raw gamepad stick/trigger axis update using non-blocking
    /// dispatcher send, then enqueue one checked dispatcher barrier.
    pub fn try_send_stick_raw(&self, axis: GamepadAxis, value: i16) -> Result<()> {
        self.ensure_direct_try_capacity(TRY_GAMEPAD_EXCEEDS_COMMAND_BOUND)?;
        self.client.try_send_stick_raw(axis, value)?;
        self.try_finish_direct_gamepad_command()
    }

    /// Send a raw left-stick pair update.
    pub fn send_left_stick_raw(&self, x: i16, y: i16) -> Result<()> {
        self.client.send_left_stick_raw(x, y)
    }

    /// Send a raw left-stick pair update using non-blocking dispatcher send,
    /// then enqueue one checked dispatcher barrier.
    pub fn try_send_left_stick_raw(&self, x: i16, y: i16) -> Result<()> {
        self.ensure_direct_try_capacity(TRY_GAMEPAD_EXCEEDS_COMMAND_BOUND)?;
        self.client.try_send_left_stick_raw(x, y)?;
        self.try_finish_direct_gamepad_command()
    }

    /// Send a raw right-stick pair update.
    pub fn send_right_stick_raw(&self, x: i16, y: i16) -> Result<()> {
        self.client.send_right_stick_raw(x, y)
    }

    /// Send a raw right-stick pair update using non-blocking dispatcher send,
    /// then enqueue one checked dispatcher barrier.
    pub fn try_send_right_stick_raw(&self, x: i16, y: i16) -> Result<()> {
        self.ensure_direct_try_capacity(TRY_GAMEPAD_EXCEEDS_COMMAND_BOUND)?;
        self.client.try_send_right_stick_raw(x, y)?;
        self.try_finish_direct_gamepad_command()
    }

    /// Send raw left/right trigger updates.
    pub fn send_triggers_raw(&self, left: i16, right: i16) -> Result<()> {
        self.client.send_triggers_raw(left, right)
    }

    /// Send raw left/right trigger updates using non-blocking dispatcher send,
    /// then enqueue one checked dispatcher barrier.
    pub fn try_send_triggers_raw(&self, left: i16, right: i16) -> Result<()> {
        self.ensure_direct_try_capacity(TRY_GAMEPAD_EXCEEDS_COMMAND_BOUND)?;
        self.client.try_send_triggers_raw(left, right)?;
        self.try_finish_direct_gamepad_command()
    }

    /// Send raw left/right stick and trigger updates in one command.
    pub fn send_sticks_raw(
        &self,
        left_x: i16,
        left_y: i16,
        right_x: i16,
        right_y: i16,
        left_trigger: i16,
        right_trigger: i16,
    ) -> Result<()> {
        self.client.send_sticks_raw(
            left_x,
            left_y,
            right_x,
            right_y,
            left_trigger,
            right_trigger,
        )
    }

    /// Send raw left/right stick and trigger updates in one command using
    /// non-blocking dispatcher send, then enqueue one checked dispatcher
    /// barrier.
    pub fn try_send_sticks_raw(
        &self,
        left_x: i16,
        left_y: i16,
        right_x: i16,
        right_y: i16,
        left_trigger: i16,
        right_trigger: i16,
    ) -> Result<()> {
        self.ensure_direct_try_capacity(TRY_GAMEPAD_EXCEEDS_COMMAND_BOUND)?;
        self.client.try_send_sticks_raw(
            left_x,
            left_y,
            right_x,
            right_y,
            left_trigger,
            right_trigger,
        )?;
        self.try_finish_direct_gamepad_command()
    }

    /// Send one full gamepad frame with server-side state dedupe.
    pub fn send_frame(&self, frame: GamepadFrameRaw) -> Result<()> {
        self.client.send_frame(frame)
    }

    /// Send one full gamepad frame with server-side state dedupe using
    /// non-blocking dispatcher send, then enqueue one checked dispatcher
    /// barrier.
    pub fn try_send_frame(&self, frame: GamepadFrameRaw) -> Result<()> {
        self.ensure_direct_try_capacity(TRY_GAMEPAD_EXCEEDS_COMMAND_BOUND)?;
        self.client.try_send_frame(frame)?;
        self.try_finish_direct_gamepad_command()
    }

    /// Send one full gamepad frame without state dedupe.
    pub fn send_frame_unchecked(&self, frame: GamepadFrameRaw) -> Result<()> {
        self.client.send_frame_unchecked(frame)
    }

    /// Send one full gamepad frame without state dedupe using non-blocking
    /// dispatcher send, then enqueue one checked dispatcher barrier.
    pub fn try_send_frame_unchecked(&self, frame: GamepadFrameRaw) -> Result<()> {
        self.ensure_direct_try_capacity(TRY_GAMEPAD_EXCEEDS_COMMAND_BOUND)?;
        self.client.try_send_frame_unchecked(frame)?;
        self.try_finish_direct_gamepad_command()
    }

    /// Send full gamepad frames with a fixed stack buffer and state dedupe.
    pub fn send_frame_batch_fixed(
        &self,
        len: usize,
        frames: [GamepadFrameRaw; GAMEPAD_BATCH_FRAMES],
    ) -> Result<()> {
        self.client.send_frame_batch_fixed(len, frames)
    }

    /// Send full gamepad frames with a fixed stack buffer and state dedupe using
    /// non-blocking dispatcher send, then enqueue one checked dispatcher
    /// barrier.
    pub fn try_send_frame_batch_fixed(
        &self,
        len: usize,
        frames: [GamepadFrameRaw; GAMEPAD_BATCH_FRAMES],
    ) -> Result<()> {
        if len == 0 {
            return Ok(());
        }
        self.ensure_direct_try_capacity(TRY_GAMEPAD_EXCEEDS_COMMAND_BOUND)?;
        self.client.try_send_frame_batch_fixed(len, frames)?;
        self.try_finish_direct_gamepad_command()
    }

    /// Send full gamepad frames with a fixed stack buffer and no state dedupe.
    pub fn send_frame_batch_fixed_unchecked(
        &self,
        len: usize,
        frames: [GamepadFrameRaw; GAMEPAD_BATCH_FRAMES],
    ) -> Result<()> {
        self.client.send_frame_batch_fixed_unchecked(len, frames)
    }

    /// Send full gamepad frames with a fixed stack buffer and no state dedupe
    /// using non-blocking dispatcher send, then enqueue one checked dispatcher
    /// barrier.
    pub fn try_send_frame_batch_fixed_unchecked(
        &self,
        len: usize,
        frames: [GamepadFrameRaw; GAMEPAD_BATCH_FRAMES],
    ) -> Result<()> {
        if len == 0 {
            return Ok(());
        }
        self.ensure_direct_try_capacity(TRY_GAMEPAD_EXCEEDS_COMMAND_BOUND)?;
        self.client
            .try_send_frame_batch_fixed_unchecked(len, frames)?;
        self.try_finish_direct_gamepad_command()
    }

    /// Send one packed 15-byte gamepad report.
    pub fn send_frame_packed(&self, frame: [u8; GAMEPAD_FRAME_BYTES]) -> Result<()> {
        self.client.send_frame_packed(frame)
    }

    /// Send one packed 15-byte gamepad report using non-blocking dispatcher
    /// send, then enqueue one checked dispatcher barrier.
    pub fn try_send_frame_packed(&self, frame: [u8; GAMEPAD_FRAME_BYTES]) -> Result<()> {
        self.ensure_direct_try_capacity(TRY_GAMEPAD_EXCEEDS_COMMAND_BOUND)?;
        self.client.try_send_frame_packed(frame)?;
        self.try_finish_direct_gamepad_command()
    }

    /// Send packed 15-byte gamepad frames with a fixed stack buffer.
    pub fn send_frame_packed_batch_fixed(
        &self,
        len: usize,
        frames: [[u8; GAMEPAD_FRAME_BYTES]; GAMEPAD_BATCH_FRAMES],
    ) -> Result<()> {
        self.client.send_frame_packed_batch_fixed(len, frames)
    }

    /// Send packed 15-byte gamepad frames with a fixed stack buffer using
    /// non-blocking dispatcher send, then enqueue one checked dispatcher
    /// barrier.
    pub fn try_send_frame_packed_batch_fixed(
        &self,
        len: usize,
        frames: [[u8; GAMEPAD_FRAME_BYTES]; GAMEPAD_BATCH_FRAMES],
    ) -> Result<()> {
        if len == 0 {
            return Ok(());
        }
        self.ensure_direct_try_capacity(TRY_GAMEPAD_EXCEEDS_COMMAND_BOUND)?;
        self.client.try_send_frame_packed_batch_fixed(len, frames)?;
        self.try_finish_direct_gamepad_command()
    }

    /// Tap one screen coordinate using touch down/up control messages.
    pub fn tap(&self, x: i32, y: i32) -> Result<()> {
        self.client.tap(x, y)
    }

    /// Tap one screen coordinate using non-blocking dispatcher sends, then
    /// enqueue one checked dispatcher barrier.
    pub fn try_tap(&self, x: i32, y: i32) -> Result<()> {
        self.try_tap_pointer(TouchPointerId::finger(0), x, y)
    }

    /// Tap one screen coordinate with a typed scrcpy pointer id.
    pub fn tap_pointer(&self, pointer_id: TouchPointerId, x: i32, y: i32) -> Result<()> {
        self.client.tap_pointer(pointer_id, x, y)
    }

    /// Tap one screen coordinate with a typed scrcpy pointer id using
    /// non-blocking dispatcher sends, then enqueue one checked dispatcher
    /// barrier.
    pub fn try_tap_pointer(&self, pointer_id: TouchPointerId, x: i32, y: i32) -> Result<()> {
        if self.command_bound < 2 {
            return Err(Error::SessionLifecycle(TRY_TAP_EXCEEDS_COMMAND_BOUND));
        }
        self.try_queue_tap_pointer(pointer_id, x, y)?;
        self.client.try_flush_wait().map(|_| ())
    }

    /// Tap one normalized screen point using the tracked screen size.
    pub fn tap_point(&self, point: AgentPoint) -> Result<()> {
        let (x, y) = self.point_to_pixels(point);
        self.tap(x, y)
    }

    /// Tap one normalized screen point using non-blocking dispatcher sends,
    /// then enqueue one checked dispatcher barrier.
    pub fn try_tap_point(&self, point: AgentPoint) -> Result<()> {
        let (x, y) = self.point_to_pixels(point);
        self.try_tap(x, y)
    }

    /// Tap one normalized screen point with a typed scrcpy pointer id.
    pub fn tap_point_pointer(&self, pointer_id: TouchPointerId, point: AgentPoint) -> Result<()> {
        let (x, y) = self.point_to_pixels(point);
        self.tap_pointer(pointer_id, x, y)
    }

    /// Tap one normalized screen point with a typed scrcpy pointer id using
    /// non-blocking dispatcher sends, then enqueue one checked dispatcher
    /// barrier.
    pub fn try_tap_point_pointer(
        &self,
        pointer_id: TouchPointerId,
        point: AgentPoint,
    ) -> Result<()> {
        let (x, y) = self.point_to_pixels(point);
        self.try_tap_pointer(pointer_id, x, y)
    }

    /// Tap the center of one normalized screen rectangle.
    pub fn tap_rect(&self, rect: AgentRect) -> Result<()> {
        self.tap_point(rect.center())
    }

    /// Tap the center of one normalized screen rectangle using non-blocking
    /// dispatcher sends, then enqueue one checked dispatcher barrier.
    pub fn try_tap_rect(&self, rect: AgentRect) -> Result<()> {
        self.try_tap_point(rect.center())
    }

    /// Tap a relative point inside one normalized screen rectangle.
    ///
    /// `x_bp` and `y_bp` are basis points from the rectangle's top-left edge
    /// to bottom-right edge.
    pub fn tap_rect_at(&self, rect: AgentRect, x_bp: u16, y_bp: u16) -> Result<()> {
        self.tap_point(rect.try_point_at_basis_points(x_bp, y_bp)?)
    }

    /// Tap a relative point inside one normalized screen rectangle using
    /// non-blocking dispatcher sends, then enqueue one checked dispatcher
    /// barrier.
    pub fn try_tap_rect_at(&self, rect: AgentRect, x_bp: u16, y_bp: u16) -> Result<()> {
        self.try_tap_point(rect.try_point_at_basis_points(x_bp, y_bp)?)
    }

    /// Tap the center of one normalized screen rectangle with a typed scrcpy
    /// pointer id.
    pub fn tap_rect_pointer(&self, pointer_id: TouchPointerId, rect: AgentRect) -> Result<()> {
        self.tap_point_pointer(pointer_id, rect.center())
    }

    /// Tap the center of one normalized screen rectangle with a typed scrcpy
    /// pointer id using non-blocking dispatcher sends, then enqueue one checked
    /// dispatcher barrier.
    pub fn try_tap_rect_pointer(&self, pointer_id: TouchPointerId, rect: AgentRect) -> Result<()> {
        self.try_tap_point_pointer(pointer_id, rect.center())
    }

    /// Tap a relative point inside one normalized screen rectangle with a typed
    /// scrcpy pointer id.
    pub fn tap_rect_at_pointer(
        &self,
        pointer_id: TouchPointerId,
        rect: AgentRect,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<()> {
        self.tap_point_pointer(pointer_id, rect.try_point_at_basis_points(x_bp, y_bp)?)
    }

    /// Tap a relative point inside one normalized screen rectangle with a typed
    /// scrcpy pointer id using non-blocking dispatcher sends, then enqueue one
    /// checked dispatcher barrier.
    pub fn try_tap_rect_at_pointer(
        &self,
        pointer_id: TouchPointerId,
        rect: AgentRect,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<()> {
        self.try_tap_point_pointer(pointer_id, rect.try_point_at_basis_points(x_bp, y_bp)?)
    }

    /// Two quick taps at one coordinate.
    pub fn double_tap(&self, x: i32, y: i32) -> Result<()> {
        self.client.double_tap(x, y)
    }

    /// Two quick taps at one coordinate using non-blocking dispatcher sends,
    /// then enqueue one checked dispatcher barrier.
    pub fn try_double_tap(&self, x: i32, y: i32) -> Result<()> {
        self.try_double_tap_pointer(TouchPointerId::finger(0), x, y)
    }

    /// Two quick taps at one coordinate with a typed scrcpy pointer id.
    pub fn double_tap_pointer(&self, pointer_id: TouchPointerId, x: i32, y: i32) -> Result<()> {
        self.client.double_tap_pointer(pointer_id, x, y)
    }

    /// Two quick taps at one coordinate with a typed scrcpy pointer id using
    /// non-blocking dispatcher sends, then enqueue one checked dispatcher
    /// barrier.
    pub fn try_double_tap_pointer(&self, pointer_id: TouchPointerId, x: i32, y: i32) -> Result<()> {
        if self.command_bound < 2 {
            return Err(Error::SessionLifecycle(
                TRY_DOUBLE_TAP_EXCEEDS_COMMAND_BOUND,
            ));
        }
        self.try_queue_double_tap_pointer(pointer_id, x, y)?;
        self.client.try_flush_wait().map(|_| ())
    }

    /// Two quick taps at one normalized screen point.
    pub fn double_tap_point(&self, point: AgentPoint) -> Result<()> {
        let (x, y) = self.point_to_pixels(point);
        self.double_tap(x, y)
    }

    /// Two quick taps at one normalized screen point using non-blocking
    /// dispatcher sends, then enqueue one checked dispatcher barrier.
    pub fn try_double_tap_point(&self, point: AgentPoint) -> Result<()> {
        let (x, y) = self.point_to_pixels(point);
        self.try_double_tap(x, y)
    }

    /// Two quick taps at one normalized screen point with a typed scrcpy pointer
    /// id.
    pub fn double_tap_point_pointer(
        &self,
        pointer_id: TouchPointerId,
        point: AgentPoint,
    ) -> Result<()> {
        let (x, y) = self.point_to_pixels(point);
        self.double_tap_pointer(pointer_id, x, y)
    }

    /// Two quick taps at one normalized screen point with a typed scrcpy pointer
    /// id using non-blocking dispatcher sends, then enqueue one checked
    /// dispatcher barrier.
    pub fn try_double_tap_point_pointer(
        &self,
        pointer_id: TouchPointerId,
        point: AgentPoint,
    ) -> Result<()> {
        let (x, y) = self.point_to_pixels(point);
        self.try_double_tap_pointer(pointer_id, x, y)
    }

    /// Two quick taps at the center of one normalized screen rectangle.
    pub fn double_tap_rect(&self, rect: AgentRect) -> Result<()> {
        self.double_tap_point(rect.center())
    }

    /// Two quick taps at the center of one normalized screen rectangle using
    /// non-blocking dispatcher sends, then enqueue one checked dispatcher
    /// barrier.
    pub fn try_double_tap_rect(&self, rect: AgentRect) -> Result<()> {
        self.try_double_tap_point(rect.center())
    }

    /// Two quick taps at a relative point inside one normalized screen
    /// rectangle.
    pub fn double_tap_rect_at(&self, rect: AgentRect, x_bp: u16, y_bp: u16) -> Result<()> {
        self.double_tap_point(rect.try_point_at_basis_points(x_bp, y_bp)?)
    }

    /// Two quick taps at a relative point inside one normalized screen
    /// rectangle using non-blocking dispatcher sends, then enqueue one checked
    /// dispatcher barrier.
    pub fn try_double_tap_rect_at(&self, rect: AgentRect, x_bp: u16, y_bp: u16) -> Result<()> {
        self.try_double_tap_point(rect.try_point_at_basis_points(x_bp, y_bp)?)
    }

    /// Two quick taps at the center of one normalized screen rectangle with a
    /// typed scrcpy pointer id.
    pub fn double_tap_rect_pointer(
        &self,
        pointer_id: TouchPointerId,
        rect: AgentRect,
    ) -> Result<()> {
        self.double_tap_point_pointer(pointer_id, rect.center())
    }

    /// Two quick taps at the center of one normalized screen rectangle with a
    /// typed scrcpy pointer id using non-blocking dispatcher sends, then enqueue
    /// one checked dispatcher barrier.
    pub fn try_double_tap_rect_pointer(
        &self,
        pointer_id: TouchPointerId,
        rect: AgentRect,
    ) -> Result<()> {
        self.try_double_tap_point_pointer(pointer_id, rect.center())
    }

    /// Two quick taps at a relative point inside one normalized screen
    /// rectangle with a typed scrcpy pointer id.
    pub fn double_tap_rect_at_pointer(
        &self,
        pointer_id: TouchPointerId,
        rect: AgentRect,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<()> {
        self.double_tap_point_pointer(pointer_id, rect.try_point_at_basis_points(x_bp, y_bp)?)
    }

    /// Two quick taps at a relative point inside one normalized screen
    /// rectangle with a typed scrcpy pointer id using non-blocking dispatcher
    /// sends, then enqueue one checked dispatcher barrier.
    pub fn try_double_tap_rect_at_pointer(
        &self,
        pointer_id: TouchPointerId,
        rect: AgentRect,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<()> {
        self.try_double_tap_point_pointer(pointer_id, rect.try_point_at_basis_points(x_bp, y_bp)?)
    }

    /// Press, hold for `dur`, then release.
    pub fn long_press(&self, x: i32, y: i32, dur: Duration) -> Result<()> {
        self.client.long_press(x, y, dur)
    }

    /// Press, hold, then release with a typed scrcpy pointer id.
    pub fn long_press_pointer(
        &self,
        pointer_id: TouchPointerId,
        x: i32,
        y: i32,
        dur: Duration,
    ) -> Result<()> {
        self.client.long_press_pointer(pointer_id, x, y, dur)
    }

    /// Press, hold, then release at one normalized screen point.
    pub fn long_press_point(&self, point: AgentPoint, dur: Duration) -> Result<()> {
        let (x, y) = self.point_to_pixels(point);
        self.long_press(x, y, dur)
    }

    /// Press, hold, then release at one normalized screen point with a typed
    /// scrcpy pointer id.
    pub fn long_press_point_pointer(
        &self,
        pointer_id: TouchPointerId,
        point: AgentPoint,
        dur: Duration,
    ) -> Result<()> {
        let (x, y) = self.point_to_pixels(point);
        self.long_press_pointer(pointer_id, x, y, dur)
    }

    /// Press and hold the center of one normalized screen rectangle.
    pub fn long_press_rect(&self, rect: AgentRect, dur: Duration) -> Result<()> {
        self.long_press_point(rect.center(), dur)
    }

    /// Press and hold a relative point inside one normalized screen rectangle.
    pub fn long_press_rect_at(
        &self,
        rect: AgentRect,
        x_bp: u16,
        y_bp: u16,
        dur: Duration,
    ) -> Result<()> {
        self.long_press_point(rect.try_point_at_basis_points(x_bp, y_bp)?, dur)
    }

    /// Press and hold the center of one normalized screen rectangle with a typed
    /// scrcpy pointer id.
    pub fn long_press_rect_pointer(
        &self,
        pointer_id: TouchPointerId,
        rect: AgentRect,
        dur: Duration,
    ) -> Result<()> {
        self.long_press_point_pointer(pointer_id, rect.center(), dur)
    }

    /// Press and hold a relative point inside one normalized screen rectangle
    /// with a typed scrcpy pointer id.
    pub fn long_press_rect_at_pointer(
        &self,
        pointer_id: TouchPointerId,
        rect: AgentRect,
        x_bp: u16,
        y_bp: u16,
        dur: Duration,
    ) -> Result<()> {
        self.long_press_point_pointer(pointer_id, rect.try_point_at_basis_points(x_bp, y_bp)?, dur)
    }

    /// Swipe from one coordinate to another in `steps` intermediate samples.
    pub fn swipe(&self, from: (i32, i32), to: (i32, i32), steps: usize) -> Result<()> {
        self.client.swipe(from, to, steps)
    }

    /// Swipe between two coordinates with a typed scrcpy pointer id.
    pub fn swipe_pointer(
        &self,
        pointer_id: TouchPointerId,
        from: (i32, i32),
        to: (i32, i32),
        steps: usize,
    ) -> Result<()> {
        self.client.swipe_pointer(pointer_id, from, to, steps)
    }

    /// Swipe between two normalized screen points.
    pub fn swipe_points(&self, from: AgentPoint, to: AgentPoint, steps: usize) -> Result<()> {
        self.swipe(self.point_to_pixels(from), self.point_to_pixels(to), steps)
    }

    /// Swipe between two normalized screen points with a typed scrcpy pointer
    /// id.
    pub fn swipe_points_pointer(
        &self,
        pointer_id: TouchPointerId,
        from: AgentPoint,
        to: AgentPoint,
        steps: usize,
    ) -> Result<()> {
        self.swipe_pointer(
            pointer_id,
            self.point_to_pixels(from),
            self.point_to_pixels(to),
            steps,
        )
    }

    /// Swipe between two relative points inside one normalized screen rectangle.
    pub fn swipe_rect(
        &self,
        rect: AgentRect,
        from: (u16, u16),
        to: (u16, u16),
        steps: usize,
    ) -> Result<()> {
        self.swipe_points(
            rect.try_point_at_basis_points(from.0, from.1)?,
            rect.try_point_at_basis_points(to.0, to.1)?,
            steps,
        )
    }

    /// Swipe between two relative points inside one normalized screen rectangle
    /// with a typed scrcpy pointer id.
    pub fn swipe_rect_pointer(
        &self,
        pointer_id: TouchPointerId,
        rect: AgentRect,
        from: (u16, u16),
        to: (u16, u16),
        steps: usize,
    ) -> Result<()> {
        self.swipe_points_pointer(
            pointer_id,
            rect.try_point_at_basis_points(from.0, from.1)?,
            rect.try_point_at_basis_points(to.0, to.1)?,
            steps,
        )
    }

    /// Two-pointer pinch/spread using raw pixel endpoints.
    ///
    /// Pointer ids `0` and `1` are pressed, moved in alternating samples, and
    /// released. Moving endpoints closer performs pinch-in; farther performs
    /// spread/zoom-out.
    pub fn pinch(
        &self,
        first_from: (i32, i32),
        first_to: (i32, i32),
        second_from: (i32, i32),
        second_to: (i32, i32),
        steps: usize,
    ) -> Result<()> {
        self.queue_pinch(first_from, first_to, second_from, second_to, steps)
    }

    /// Two-pointer pinch/spread using normalized screen points.
    pub fn pinch_points(
        &self,
        first_from: AgentPoint,
        first_to: AgentPoint,
        second_from: AgentPoint,
        second_to: AgentPoint,
        steps: usize,
    ) -> Result<()> {
        self.pinch(
            self.point_to_pixels(first_from),
            self.point_to_pixels(first_to),
            self.point_to_pixels(second_from),
            self.point_to_pixels(second_to),
            steps,
        )
    }

    /// Absolute scroll with no pressed mouse buttons.
    pub fn scroll(&self, x: i32, y: i32, hscroll: f32, vscroll: f32) -> Result<()> {
        self.client.scroll(x, y, hscroll, vscroll)
    }

    /// Absolute scroll using non-blocking dispatcher send, then enqueue one
    /// checked dispatcher barrier.
    pub fn try_scroll(&self, x: i32, y: i32, hscroll: f32, vscroll: f32) -> Result<()> {
        self.try_scroll_with_buttons(x, y, hscroll, vscroll, 0)
    }

    /// Absolute scroll at one normalized screen point.
    pub fn scroll_point(&self, point: AgentPoint, hscroll: f32, vscroll: f32) -> Result<()> {
        let (x, y) = self.point_to_pixels(point);
        self.scroll(x, y, hscroll, vscroll)
    }

    /// Absolute scroll at one normalized screen point using non-blocking
    /// dispatcher send, then enqueue one checked dispatcher barrier.
    pub fn try_scroll_point(&self, point: AgentPoint, hscroll: f32, vscroll: f32) -> Result<()> {
        let (x, y) = self.point_to_pixels(point);
        self.try_scroll(x, y, hscroll, vscroll)
    }

    /// Absolute scroll at the center of one normalized screen rectangle.
    pub fn scroll_rect(&self, rect: AgentRect, hscroll: f32, vscroll: f32) -> Result<()> {
        self.scroll_point(rect.center(), hscroll, vscroll)
    }

    /// Absolute scroll at the center of one normalized screen rectangle using
    /// non-blocking dispatcher send, then enqueue one checked dispatcher
    /// barrier.
    pub fn try_scroll_rect(&self, rect: AgentRect, hscroll: f32, vscroll: f32) -> Result<()> {
        self.try_scroll_point(rect.center(), hscroll, vscroll)
    }

    /// Absolute scroll at a relative point inside one normalized screen
    /// rectangle.
    pub fn scroll_rect_at(
        &self,
        rect: AgentRect,
        x_bp: u16,
        y_bp: u16,
        hscroll: f32,
        vscroll: f32,
    ) -> Result<()> {
        self.scroll_point(
            rect.try_point_at_basis_points(x_bp, y_bp)?,
            hscroll,
            vscroll,
        )
    }

    /// Absolute scroll at a relative point inside one normalized screen
    /// rectangle using non-blocking dispatcher send, then enqueue one checked
    /// dispatcher barrier.
    pub fn try_scroll_rect_at(
        &self,
        rect: AgentRect,
        x_bp: u16,
        y_bp: u16,
        hscroll: f32,
        vscroll: f32,
    ) -> Result<()> {
        self.try_scroll_point(
            rect.try_point_at_basis_points(x_bp, y_bp)?,
            hscroll,
            vscroll,
        )
    }

    /// Absolute scroll with an explicit Android mouse-button bitmask.
    pub fn scroll_with_buttons(
        &self,
        x: i32,
        y: i32,
        hscroll: f32,
        vscroll: f32,
        buttons: u32,
    ) -> Result<()> {
        self.client
            .scroll_with_buttons(x, y, hscroll, vscroll, buttons)
    }

    /// Absolute scroll with an explicit Android mouse-button bitmask using
    /// non-blocking dispatcher send, then enqueue one checked dispatcher
    /// barrier.
    pub fn try_scroll_with_buttons(
        &self,
        x: i32,
        y: i32,
        hscroll: f32,
        vscroll: f32,
        buttons: u32,
    ) -> Result<()> {
        if self.command_bound < 2 {
            return Err(Error::SessionLifecycle(TRY_SCROLL_EXCEEDS_COMMAND_BOUND));
        }
        self.client
            .try_scroll_with_buttons(x, y, hscroll, vscroll, buttons)?;
        self.client.try_flush_wait().map(|_| ())
    }

    /// Absolute scroll at one normalized screen point with a button bitmask.
    pub fn scroll_point_with_buttons(
        &self,
        point: AgentPoint,
        hscroll: f32,
        vscroll: f32,
        buttons: u32,
    ) -> Result<()> {
        let (x, y) = self.point_to_pixels(point);
        self.scroll_with_buttons(x, y, hscroll, vscroll, buttons)
    }

    /// Absolute scroll at one normalized screen point with a button bitmask
    /// using non-blocking dispatcher send, then enqueue one checked dispatcher
    /// barrier.
    pub fn try_scroll_point_with_buttons(
        &self,
        point: AgentPoint,
        hscroll: f32,
        vscroll: f32,
        buttons: u32,
    ) -> Result<()> {
        let (x, y) = self.point_to_pixels(point);
        self.try_scroll_with_buttons(x, y, hscroll, vscroll, buttons)
    }

    /// Absolute scroll at a normalized rectangle center with a button bitmask.
    pub fn scroll_rect_with_buttons(
        &self,
        rect: AgentRect,
        hscroll: f32,
        vscroll: f32,
        buttons: u32,
    ) -> Result<()> {
        self.scroll_point_with_buttons(rect.center(), hscroll, vscroll, buttons)
    }

    /// Absolute scroll at a normalized rectangle center with a button bitmask
    /// using non-blocking dispatcher send, then enqueue one checked dispatcher
    /// barrier.
    pub fn try_scroll_rect_with_buttons(
        &self,
        rect: AgentRect,
        hscroll: f32,
        vscroll: f32,
        buttons: u32,
    ) -> Result<()> {
        self.try_scroll_point_with_buttons(rect.center(), hscroll, vscroll, buttons)
    }

    /// Absolute scroll at a relative point inside a normalized rectangle with a
    /// button bitmask.
    pub fn scroll_rect_at_with_buttons(
        &self,
        rect: AgentRect,
        x_bp: u16,
        y_bp: u16,
        hscroll: f32,
        vscroll: f32,
        buttons: u32,
    ) -> Result<()> {
        self.scroll_point_with_buttons(
            rect.try_point_at_basis_points(x_bp, y_bp)?,
            hscroll,
            vscroll,
            buttons,
        )
    }

    /// Absolute scroll at a relative point inside a normalized rectangle with a
    /// button bitmask using non-blocking dispatcher send, then enqueue one
    /// checked dispatcher barrier.
    pub fn try_scroll_rect_at_with_buttons(
        &self,
        rect: AgentRect,
        x_bp: u16,
        y_bp: u16,
        hscroll: f32,
        vscroll: f32,
        buttons: u32,
    ) -> Result<()> {
        self.try_scroll_point_with_buttons(
            rect.try_point_at_basis_points(x_bp, y_bp)?,
            hscroll,
            vscroll,
            buttons,
        )
    }

    /// Cancel one active touch pointer.
    pub fn cancel_touch(&self, pointer_id: u64) -> Result<()> {
        self.client.cancel_touch(pointer_id)
    }

    /// Cancel one active typed scrcpy touch pointer.
    pub fn cancel_touch_pointer(&self, pointer_id: TouchPointerId) -> Result<()> {
        self.client.cancel_touch_pointer(pointer_id)
    }

    /// Three-finger swipe down using the current agent-local screen size.
    pub fn three_finger_screenshot(&self) -> Result<()> {
        let (width, height) = self.screen_size();
        self.client.three_finger_screenshot(width, height)
    }

    /// Launch an app by Android package name.
    pub fn launch_app(&self, name: impl Into<String>) -> Result<()> {
        self.client.launch_app(name)
    }

    /// Launch an app by Android package name using non-blocking dispatcher
    /// send, then enqueue one checked dispatcher barrier.
    pub fn try_launch_app(&self, name: impl Into<String>) -> Result<()> {
        let name = name.into();
        if name.len() > 255 {
            return Err(Error::SessionLifecycle(LAUNCH_APP_NAME_TOO_LONG));
        }
        self.try_run_direct_command(
            HidCommand::LaunchApp { name },
            TRY_CONTROL_EXCEEDS_COMMAND_BOUND,
        )
    }

    /// Turn the display on/off through scrcpy control.
    pub fn set_screen_power(&self, on: bool) -> Result<()> {
        self.client.set_screen_power(on)
    }

    /// Turn the display on/off through scrcpy control using non-blocking
    /// dispatcher send, then enqueue one checked dispatcher barrier.
    pub fn try_set_screen_power(&self, on: bool) -> Result<()> {
        self.try_run_direct_command(
            HidCommand::SetScreenPower { on },
            TRY_CONTROL_EXCEEDS_COMMAND_BOUND,
        )
    }

    /// Expand the notification panel.
    pub fn show_notifications(&self) -> Result<()> {
        self.client.show_notifications()
    }

    /// Expand the notification panel using non-blocking dispatcher send, then
    /// enqueue one checked dispatcher barrier.
    pub fn try_show_notifications(&self) -> Result<()> {
        self.try_run_direct_command(
            HidCommand::ShowNotifications,
            TRY_CONTROL_EXCEEDS_COMMAND_BOUND,
        )
    }

    /// Expand the quick-settings panel.
    pub fn show_quick_settings(&self) -> Result<()> {
        self.client.show_quick_settings()
    }

    /// Expand the quick-settings panel using non-blocking dispatcher send, then
    /// enqueue one checked dispatcher barrier.
    pub fn try_show_quick_settings(&self) -> Result<()> {
        self.try_run_direct_command(
            HidCommand::ShowQuickSettings,
            TRY_CONTROL_EXCEEDS_COMMAND_BOUND,
        )
    }

    /// Collapse notification and quick-settings panels.
    pub fn collapse_panels(&self) -> Result<()> {
        self.client.collapse_panels()
    }

    /// Collapse notification and quick-settings panels using non-blocking
    /// dispatcher send, then enqueue one checked dispatcher barrier.
    pub fn try_collapse_panels(&self) -> Result<()> {
        self.try_run_direct_command(
            HidCommand::CollapsePanels,
            TRY_CONTROL_EXCEEDS_COMMAND_BOUND,
        )
    }

    /// Rotate the device display.
    pub fn rotate_device(&self) -> Result<()> {
        self.client.rotate_device()
    }

    /// Rotate the device display using non-blocking dispatcher send, then
    /// enqueue one checked dispatcher barrier.
    pub fn try_rotate_device(&self) -> Result<()> {
        self.try_run_direct_command(HidCommand::RotateDevice, TRY_CONTROL_EXCEEDS_COMMAND_BOUND)
    }

    /// Ask the device/server to resize its display.
    ///
    /// This emits scrcpy `RESIZE_DISPLAY`. Use [`Self::set_screen_size`] when
    /// you only need to update local touch-coordinate metadata.
    pub fn resize_display(&self, width: u16, height: u16) -> Result<()> {
        self.client.resize_display(width, height)
    }

    /// Ask the device/server to resize its display using non-blocking
    /// dispatcher send, then enqueue one checked dispatcher barrier.
    pub fn try_resize_display(&self, width: u16, height: u16) -> Result<()> {
        self.try_run_direct_command(
            HidCommand::ResizeDisplay { width, height },
            TRY_CONTROL_EXCEEDS_COMMAND_BOUND,
        )
    }

    /// Toggle the camera torch.
    pub fn set_torch(&self, on: bool) -> Result<()> {
        self.client.set_torch(on)
    }

    /// Toggle the camera torch using non-blocking dispatcher send, then enqueue
    /// one checked dispatcher barrier.
    pub fn try_set_torch(&self, on: bool) -> Result<()> {
        self.try_run_direct_command(
            HidCommand::SetTorch { on },
            TRY_CONTROL_EXCEEDS_COMMAND_BOUND,
        )
    }

    /// Camera zoom in.
    pub fn camera_zoom_in(&self) -> Result<()> {
        self.client.camera_zoom_in()
    }

    /// Camera zoom in using non-blocking dispatcher send, then enqueue one
    /// checked dispatcher barrier.
    pub fn try_camera_zoom_in(&self) -> Result<()> {
        self.try_run_direct_command(HidCommand::CameraZoomIn, TRY_CONTROL_EXCEEDS_COMMAND_BOUND)
    }

    /// Camera zoom out.
    pub fn camera_zoom_out(&self) -> Result<()> {
        self.client.camera_zoom_out()
    }

    /// Camera zoom out using non-blocking dispatcher send, then enqueue one
    /// checked dispatcher barrier.
    pub fn try_camera_zoom_out(&self) -> Result<()> {
        self.try_run_direct_command(HidCommand::CameraZoomOut, TRY_CONTROL_EXCEEDS_COMMAND_BOUND)
    }

    /// Open the physical-keyboard settings activity.
    pub fn open_hard_keyboard_settings(&self) -> Result<()> {
        self.client.open_hard_keyboard_settings()
    }

    /// Open the physical-keyboard settings activity using non-blocking
    /// dispatcher send, then enqueue one checked dispatcher barrier.
    pub fn try_open_hard_keyboard_settings(&self) -> Result<()> {
        self.try_run_direct_command(
            HidCommand::OpenHardKeyboardSettings,
            TRY_CONTROL_EXCEEDS_COMMAND_BOUND,
        )
    }

    /// Reset the scrcpy video stream.
    pub fn reset_video(&self) -> Result<()> {
        self.client.reset_video()
    }

    /// Reset the scrcpy video stream using non-blocking dispatcher send, then
    /// enqueue one checked dispatcher barrier.
    pub fn try_reset_video(&self) -> Result<()> {
        self.try_run_direct_command(HidCommand::ResetVideo, TRY_CONTROL_EXCEEDS_COMMAND_BOUND)
    }

    /// Configure the AI summary pipeline on an AI-enabled scrcpy server.
    pub fn configure_ai(&self, flags: u8, sample_interval_ms: u16, feature_dim: u16) -> Result<()> {
        self.client
            .configure_ai(flags, sample_interval_ms, feature_dim)
    }

    /// Configure the AI summary pipeline using non-blocking dispatcher send,
    /// then enqueue one checked dispatcher barrier.
    pub fn try_configure_ai(
        &self,
        flags: u8,
        sample_interval_ms: u16,
        feature_dim: u16,
    ) -> Result<()> {
        self.try_run_direct_command(
            HidCommand::AiConfig {
                flags,
                sample_interval_ms,
                feature_dim,
            },
            TRY_AI_EXCEEDS_COMMAND_BOUND,
        )
    }

    /// Query the AI extension for summaries or stats since a timestamp.
    pub fn query_ai(&self, since_timestamp_ms: u64) -> Result<()> {
        self.client.query_ai(since_timestamp_ms)
    }

    /// Query the AI extension using non-blocking dispatcher send, then enqueue
    /// one checked dispatcher barrier.
    pub fn try_query_ai(&self, since_timestamp_ms: u64) -> Result<()> {
        self.try_run_direct_command(
            HidCommand::AiQuery { since_timestamp_ms },
            TRY_AI_EXCEEDS_COMMAND_BOUND,
        )
    }

    /// Query the AI extension and wait for the next AI stats envelope.
    pub fn query_ai_and_wait_stats(&mut self, since_timestamp_ms: u64) -> Result<AiStats> {
        self.query_ai(since_timestamp_ms)?;
        self.flush()?;
        self.wait_for_ai_stats()
    }

    /// Run an action plan, query the AI extension, and wait for the next AI
    /// stats envelope.
    ///
    /// The action plan and AI_QUERY command share one checked dispatcher
    /// boundary before the stats wait.
    pub fn run_actions_and_query_ai_and_wait_stats(
        &mut self,
        actions: &[AgentAction],
        since_timestamp_ms: u64,
    ) -> Result<AiStats> {
        self.queue_actions(actions)?;
        self.query_ai(since_timestamp_ms)?;
        self.flush()?;
        self.wait_for_ai_stats()
    }

    /// Pause the AI summary pipeline on an AI-enabled scrcpy server.
    pub fn pause_ai(&self) -> Result<()> {
        self.client.pause_ai()
    }

    /// Pause the AI summary pipeline using non-blocking dispatcher send, then
    /// enqueue one checked dispatcher barrier.
    pub fn try_pause_ai(&self) -> Result<()> {
        self.try_run_direct_command(HidCommand::AiPause, TRY_AI_EXCEEDS_COMMAND_BOUND)
    }

    /// Set the device clipboard without waiting for an ACK.
    pub fn set_clipboard(&self, text: impl Into<String>, paste: bool) -> Result<()> {
        self.client.set_clipboard(text, paste)
    }

    /// Set the device clipboard without waiting for an ACK using non-blocking
    /// dispatcher send, then enqueue one checked dispatcher barrier.
    pub fn try_set_clipboard(&self, text: impl Into<String>, paste: bool) -> Result<()> {
        self.try_run_direct_command(
            HidCommand::SetClipboard {
                text: text.into(),
                paste,
            },
            TRY_CLIPBOARD_EXCEEDS_COMMAND_BOUND,
        )
    }

    /// Set the device clipboard with a specific sequence number.
    pub fn set_clipboard_sequenced(
        &self,
        sequence: u64,
        text: impl Into<String>,
        paste: bool,
    ) -> Result<()> {
        self.client.set_clipboard_sequenced(sequence, text, paste)
    }

    /// Set the device clipboard with a specific sequence number using
    /// non-blocking dispatcher send, then enqueue one checked dispatcher
    /// barrier.
    pub fn try_set_clipboard_sequenced(
        &self,
        sequence: u64,
        text: impl Into<String>,
        paste: bool,
    ) -> Result<()> {
        self.try_run_direct_command(
            HidCommand::SetClipboardSequenced {
                sequence,
                text: text.into(),
                paste,
            },
            TRY_CLIPBOARD_EXCEEDS_COMMAND_BOUND,
        )
    }

    /// Request the current device clipboard. `copy_key` follows scrcpy:
    /// `0 = none`, `1 = copy`, `2 = cut`.
    pub fn request_clipboard(&self, copy_key: u8) -> Result<()> {
        self.client.request_clipboard(copy_key)
    }

    /// Request the current device clipboard using non-blocking dispatcher send,
    /// then enqueue one checked dispatcher barrier.
    pub fn try_request_clipboard(&self, copy_key: u8) -> Result<()> {
        self.try_run_direct_command(
            HidCommand::GetClipboard { copy_key },
            TRY_CLIPBOARD_EXCEEDS_COMMAND_BOUND,
        )
    }

    /// Request the current device clipboard with a typed scrcpy copy-key.
    pub fn request_clipboard_key(&self, copy_key: ClipboardCopyKey) -> Result<()> {
        self.client.request_clipboard_key(copy_key)
    }

    /// Request the current device clipboard with a typed scrcpy copy-key using
    /// non-blocking dispatcher send, then enqueue one checked dispatcher
    /// barrier.
    pub fn try_request_clipboard_key(&self, copy_key: ClipboardCopyKey) -> Result<()> {
        self.try_request_clipboard(copy_key.value())
    }

    /// Queue a typed agent action plan without waiting for a dispatcher
    /// acknowledgement.
    ///
    /// Use this when the caller already owns a wider flush/close boundary.
    /// For most agent workflows, prefer [`Self::run_actions`].
    pub fn queue_actions(&self, actions: &[AgentAction]) -> Result<()> {
        AgentAction::validate_plan_structure(actions)?;
        let mut touch_batch = self.client.touch_frame_batcher();
        let mut key_batch = self.client.keyboard_frame_batcher();
        let mut android_key_batch = self.client.android_key_frame_batcher();
        let mut mouse_batch = self.client.mouse_frame_batcher();
        let mut scroll_batch = self.client.scroll_frame_batcher();
        let mut gamepad_batch = PlanGamepadBatcher::new(&self.client);
        for action in actions {
            self.queue_planned_action(
                action,
                (
                    &mut touch_batch,
                    &mut key_batch,
                    &mut android_key_batch,
                    &mut mouse_batch,
                    &mut scroll_batch,
                    &mut gamepad_batch,
                ),
            )?;
        }
        touch_batch.flush()?;
        key_batch.flush()?;
        android_key_batch.flush()?;
        mouse_batch.flush()?;
        scroll_batch.flush()?;
        gamepad_batch.flush()
    }

    /// Queue a typed agent action plan using non-blocking dispatcher sends.
    ///
    /// This is the back-pressure aware variant for high-contention agent
    /// schedulers. It returns `SessionLifecycle("channel full ...")` when the
    /// bounded command queue is full. Timing-dependent actions (`Wait` and
    /// `LongPress`) require [`Self::queue_actions`] or [`Self::run_actions`].
    pub fn try_queue_actions(&self, actions: &[AgentAction]) -> Result<()> {
        AgentAction::validate_try_queue_plan(actions)?;
        let mut touch_batch = self.client.touch_frame_batcher();
        let mut key_batch = self.client.keyboard_frame_batcher();
        let mut android_key_batch = self.client.android_key_frame_batcher();
        let mut mouse_batch = self.client.mouse_frame_batcher();
        let mut scroll_batch = self.client.scroll_frame_batcher();
        let mut gamepad_batch = PlanGamepadBatcher::new(&self.client);
        for action in actions {
            self.try_queue_planned_action(
                action,
                (
                    &mut touch_batch,
                    &mut key_batch,
                    &mut android_key_batch,
                    &mut mouse_batch,
                    &mut scroll_batch,
                    &mut gamepad_batch,
                ),
            )?;
        }
        touch_batch.try_flush()?;
        key_batch.try_flush()?;
        android_key_batch.try_flush()?;
        mouse_batch.try_flush()?;
        scroll_batch.try_flush()?;
        gamepad_batch.try_flush()
    }

    /// Queue the longest non-blocking prefix of a typed agent action plan.
    ///
    /// This is useful for schedulers that accept mixed plans: the returned
    /// count is the number of leading actions sent through
    /// [`Self::try_queue_actions`]. Only a blocking timing/barrier requirement
    /// can produce a short successful prefix; malformed fixed-buffer/chord or
    /// rect-anchor metadata before that timing barrier is rejected before any
    /// prefix action is dispatched. Runtime back-pressure can still return an
    /// error while sending the prefix.
    pub fn try_queue_actions_prefix(&self, actions: &[AgentAction]) -> Result<usize> {
        let len = AgentAction::blocking_timing_prefix_len(actions);
        self.try_queue_actions(&actions[..len])?;
        Ok(len)
    }

    /// Queue the longest non-blocking prefix of a typed agent action plan, then
    /// enqueue one checked dispatcher barrier without blocking on a full command
    /// queue.
    ///
    /// This is the checked-barrier companion to
    /// [`Self::try_queue_actions_prefix`]. It validates and dispatches only the
    /// leading non-blocking prefix, leaves the blocking suffix for a scheduler
    /// handoff, and still reports dispatcher-side command errors once the final
    /// barrier is accepted. The accepted prefix plus barrier is preflighted
    /// against this session's configured command bound before dispatch.
    pub fn try_run_actions_prefix(&self, actions: &[AgentAction]) -> Result<usize> {
        let len = AgentAction::blocking_timing_prefix_len(actions);
        self.try_run_actions(&actions[..len])?;
        Ok(len)
    }

    /// Queue the longest statically safe non-blocking prefix that fits an
    /// estimated dispatcher-command budget.
    ///
    /// This combines full-plan structural preflight,
    /// [`AgentAction::bounded_try_queue_prefix`], and [`Self::try_queue_actions`].
    /// A command-bound or blocking-timing stop queues the accepted prefix and
    /// returns the stop metadata. Malformed metadata anywhere in the supplied
    /// plan returns an error without dispatching any accepted prefix.
    pub fn try_queue_actions_bounded_prefix(
        &self,
        actions: &[AgentAction],
        command_bound: usize,
    ) -> Result<AgentPlanBoundedPrefix> {
        AgentAction::validate_plan_structure(actions)?;
        let prefix = AgentAction::bounded_try_queue_prefix(actions, command_bound);
        if let AgentPlanBoundedPrefixStop::TryQueueError { error, .. } = prefix.stop {
            return Err(Error::SessionLifecycle(error));
        }
        self.try_queue_actions(&actions[..prefix.accepted_actions])?;
        Ok(prefix)
    }

    /// Queue a bounded non-blocking prefix using this session's configured
    /// dispatcher command-channel bound.
    ///
    /// This avoids planning against a caller-supplied bound that differs from the
    /// actual bound passed to [`Self::from_parts_with_bound`].
    pub fn try_queue_actions_bounded_prefix_with_session_bound(
        &self,
        actions: &[AgentAction],
    ) -> Result<AgentPlanBoundedPrefix> {
        self.try_queue_actions_bounded_prefix(actions, self.command_bound)
    }

    /// Queue the longest statically safe non-blocking prefix that fits an
    /// estimated dispatcher-command budget while reserving one command for a
    /// checked final barrier.
    ///
    /// This is the bounded-prefix companion to [`Self::try_run_actions`].
    /// Malformed metadata anywhere in the supplied plan returns an error before
    /// dispatching any accepted prefix. A command-bound or blocking-timing stop
    /// queues the accepted prefix, enqueues one checked barrier, and returns the
    /// stop metadata.
    pub fn try_run_actions_bounded_prefix(
        &self,
        actions: &[AgentAction],
        command_bound: usize,
    ) -> Result<AgentPlanBoundedPrefix> {
        AgentAction::validate_plan_structure(actions)?;
        let prefix = AgentAction::bounded_try_run_prefix(actions, command_bound);
        if !prefix.checked_dispatch_fits_bound() {
            return Err(Error::SessionLifecycle(TRY_RUN_EXCEEDS_COMMAND_BOUND));
        }
        if let AgentPlanBoundedPrefixStop::TryQueueError { error, .. } = prefix.stop {
            return Err(Error::SessionLifecycle(error));
        }
        self.try_queue_actions(&actions[..prefix.accepted_actions])?;
        self.client.try_flush_wait()?;
        Ok(prefix)
    }

    /// Queue a checked bounded non-blocking prefix using this session's
    /// configured dispatcher command-channel bound.
    pub fn try_run_actions_bounded_prefix_with_session_bound(
        &self,
        actions: &[AgentAction],
    ) -> Result<AgentPlanBoundedPrefix> {
        self.try_run_actions_bounded_prefix(actions, self.command_bound)
    }

    /// Queue a typed agent action plan and wait for one checked dispatcher
    /// barrier after the final action.
    ///
    /// Touch, low-level keyboard, relative mouse, and full-frame gamepad
    /// actions are internally batched across compatible adjacent plan steps,
    /// while still reporting dispatcher-side command errors at the end.
    pub fn run_actions(&self, actions: &[AgentAction]) -> Result<()> {
        self.queue_actions(actions)?;
        self.flush()
    }

    /// Queue a typed agent action plan using non-blocking dispatcher sends,
    /// then enqueue one checked dispatcher barrier without blocking on a full
    /// command queue.
    ///
    /// This is the checked-barrier companion to [`Self::try_queue_actions`] for
    /// high-contention schedulers. It rejects timing-dependent actions like
    /// `Wait` / `LongPress` before dispatch, rejects plans that cannot fit this
    /// session's empty command queue with the final barrier, returns
    /// back-pressure if the live queue cannot accept the plan or final barrier,
    /// and surfaces dispatcher-side command errors once the barrier is accepted.
    pub fn try_run_actions(&self, actions: &[AgentAction]) -> Result<()> {
        let summary = AgentAction::plan_summary(actions);
        if let Some((_, error)) = summary.first_try_queue_error {
            return Err(Error::SessionLifecycle(error));
        }
        if !summary.try_run_dispatch_fits_bound(self.command_bound) {
            return Err(Error::SessionLifecycle(TRY_RUN_EXCEEDS_COMMAND_BOUND));
        }
        self.try_queue_actions(actions)?;
        self.client.try_flush_wait().map(|_| ())
    }

    /// Queue an action plan through [`Self::try_run_actions`], then wait for the
    /// next newest-only frame snapshot observed after the checked barrier.
    pub fn try_run_actions_and_wait_for_next_latest_frame(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
    ) -> Result<LatestFrameSummarySnapshot> {
        self.try_run_actions_and_wait_for_next_latest_frame_matching(actions, latest, |_| true)
    }

    /// Queue an action plan through [`Self::try_run_actions`], then wait for the
    /// next newest-only frame snapshot accepted by `predicate`.
    pub fn try_run_actions_and_wait_for_next_latest_frame_matching(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        mut predicate: impl FnMut(&FrameSummary) -> bool,
    ) -> Result<LatestFrameSummarySnapshot> {
        self.try_run_actions(actions)?;
        let after_version = latest.version();
        latest
            .wait_next_matching(after_version, |snapshot| predicate(&snapshot.summary))
            .map_err(|e| io_to_wait_error(e, "latest frame summary"))
    }

    /// Queue an action plan through [`Self::try_run_actions`], then wait up to
    /// `timeout` for the next newest-only frame snapshot.
    pub fn try_run_actions_and_wait_for_next_latest_frame_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        timeout: Duration,
    ) -> Result<LatestFrameSummarySnapshot> {
        self.try_run_actions_and_wait_for_next_latest_frame_matching_timeout(
            actions,
            latest,
            timeout,
            |_| true,
        )
    }

    /// Queue an action plan through [`Self::try_run_actions`], then wait up to
    /// `timeout` for the next newest-only frame accepted by `predicate`.
    pub fn try_run_actions_and_wait_for_next_latest_frame_matching_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        timeout: Duration,
        mut predicate: impl FnMut(&FrameSummary) -> bool,
    ) -> Result<LatestFrameSummarySnapshot> {
        self.try_run_actions(actions)?;
        let after_version = latest.version();
        latest
            .wait_next_matching_timeout(after_version, timeout, |snapshot| {
                predicate(&snapshot.summary)
            })
            .map_err(|e| io_to_wait_error(e, "latest frame summary"))
    }

    /// Queue an action plan through [`Self::try_run_actions`], then wait for a
    /// newest-only frame snapshot with `version > after_version`.
    pub fn try_run_actions_and_wait_for_next_latest_frame_after_version(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        after_version: u64,
    ) -> Result<LatestFrameSummarySnapshot> {
        self.try_run_actions_and_wait_for_next_latest_frame_matching_after_version(
            actions,
            latest,
            after_version,
            |_| true,
        )
    }

    /// Queue an action plan through [`Self::try_run_actions`], then wait for a
    /// newest-only frame snapshot newer than `boundary`.
    pub fn try_run_actions_and_wait_for_next_latest_frame_after_boundary(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        boundary: LatestFrameSummaryBoundary,
    ) -> Result<LatestFrameSummarySnapshot> {
        self.try_run_actions_and_wait_for_next_latest_frame_after_version(
            actions,
            latest,
            boundary.version(),
        )
    }

    /// Queue an action plan through [`Self::try_run_actions`], then wait for a
    /// newest-only frame snapshot newer than `observation.boundary`.
    pub fn try_run_actions_and_wait_for_next_latest_frame_after_observation(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        observation: &LatestFrameSummaryObservation,
    ) -> Result<LatestFrameSummarySnapshot> {
        self.try_run_actions_and_wait_for_next_latest_frame_after_boundary(
            actions,
            latest,
            observation.boundary(),
        )
    }

    /// Queue an action plan through [`Self::try_run_actions`], then wait for a
    /// newest-only frame snapshot newer than `after_version` and accepted by
    /// `predicate`.
    pub fn try_run_actions_and_wait_for_next_latest_frame_matching_after_version(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        after_version: u64,
        mut predicate: impl FnMut(&FrameSummary) -> bool,
    ) -> Result<LatestFrameSummarySnapshot> {
        self.try_run_actions(actions)?;
        latest
            .wait_next_matching(after_version, |snapshot| predicate(&snapshot.summary))
            .map_err(|e| io_to_wait_error(e, "latest frame summary"))
    }

    /// Queue an action plan through [`Self::try_run_actions`], then wait for a
    /// newest-only frame snapshot newer than `boundary` and accepted by
    /// `predicate`.
    pub fn try_run_actions_and_wait_for_next_latest_frame_matching_after_boundary(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        boundary: LatestFrameSummaryBoundary,
        predicate: impl FnMut(&FrameSummary) -> bool,
    ) -> Result<LatestFrameSummarySnapshot> {
        self.try_run_actions_and_wait_for_next_latest_frame_matching_after_version(
            actions,
            latest,
            boundary.version(),
            predicate,
        )
    }

    /// Queue an action plan through [`Self::try_run_actions`], then wait for a
    /// newest-only frame snapshot newer than `observation.boundary` and accepted
    /// by `predicate`.
    pub fn try_run_actions_and_wait_for_next_latest_frame_matching_after_observation(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        observation: &LatestFrameSummaryObservation,
        predicate: impl FnMut(&FrameSummary) -> bool,
    ) -> Result<LatestFrameSummarySnapshot> {
        self.try_run_actions_and_wait_for_next_latest_frame_matching_after_boundary(
            actions,
            latest,
            observation.boundary(),
            predicate,
        )
    }

    /// Queue an action plan through [`Self::try_run_actions`], then wait up to
    /// `timeout` for a newest-only frame with `version > after_version`.
    pub fn try_run_actions_and_wait_for_next_latest_frame_after_version_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        after_version: u64,
        timeout: Duration,
    ) -> Result<LatestFrameSummarySnapshot> {
        self.try_run_actions_and_wait_for_next_latest_frame_matching_after_version_timeout(
            actions,
            latest,
            after_version,
            timeout,
            |_| true,
        )
    }

    /// Queue an action plan through [`Self::try_run_actions`], then wait up to
    /// `timeout` for a newest-only frame snapshot newer than `boundary`.
    pub fn try_run_actions_and_wait_for_next_latest_frame_after_boundary_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        boundary: LatestFrameSummaryBoundary,
        timeout: Duration,
    ) -> Result<LatestFrameSummarySnapshot> {
        self.try_run_actions_and_wait_for_next_latest_frame_after_version_timeout(
            actions,
            latest,
            boundary.version(),
            timeout,
        )
    }

    /// Queue an action plan through [`Self::try_run_actions`], then wait up to
    /// `timeout` for a newest-only frame snapshot newer than
    /// `observation.boundary`.
    pub fn try_run_actions_and_wait_for_next_latest_frame_after_observation_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        observation: &LatestFrameSummaryObservation,
        timeout: Duration,
    ) -> Result<LatestFrameSummarySnapshot> {
        self.try_run_actions_and_wait_for_next_latest_frame_after_boundary_timeout(
            actions,
            latest,
            observation.boundary(),
            timeout,
        )
    }

    /// Queue an action plan through [`Self::try_run_actions`], then wait up to
    /// `timeout` for a newest-only frame with `version > after_version` accepted
    /// by `predicate`.
    pub fn try_run_actions_and_wait_for_next_latest_frame_matching_after_version_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        after_version: u64,
        timeout: Duration,
        mut predicate: impl FnMut(&FrameSummary) -> bool,
    ) -> Result<LatestFrameSummarySnapshot> {
        self.try_run_actions(actions)?;
        latest
            .wait_next_matching_timeout(after_version, timeout, |snapshot| {
                predicate(&snapshot.summary)
            })
            .map_err(|e| io_to_wait_error(e, "latest frame summary"))
    }

    /// Queue an action plan through [`Self::try_run_actions`], then wait up to
    /// `timeout` for a newest-only frame snapshot newer than `boundary` and
    /// accepted by `predicate`.
    pub fn try_run_actions_and_wait_for_next_latest_frame_matching_after_boundary_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        boundary: LatestFrameSummaryBoundary,
        timeout: Duration,
        predicate: impl FnMut(&FrameSummary) -> bool,
    ) -> Result<LatestFrameSummarySnapshot> {
        self.try_run_actions_and_wait_for_next_latest_frame_matching_after_version_timeout(
            actions,
            latest,
            boundary.version(),
            timeout,
            predicate,
        )
    }

    /// Queue an action plan through [`Self::try_run_actions`], then wait up to
    /// `timeout` for a newest-only frame snapshot newer than
    /// `observation.boundary` and accepted by `predicate`.
    pub fn try_run_actions_and_wait_for_next_latest_frame_matching_after_observation_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        observation: &LatestFrameSummaryObservation,
        timeout: Duration,
        predicate: impl FnMut(&FrameSummary) -> bool,
    ) -> Result<LatestFrameSummarySnapshot> {
        self.try_run_actions_and_wait_for_next_latest_frame_matching_after_boundary_timeout(
            actions,
            latest,
            observation.boundary(),
            timeout,
            predicate,
        )
    }

    /// Queue an action plan through [`Self::try_run_actions`], then wait for the
    /// next newest-only frame with `frame_seq > min_frame_seq`.
    pub fn try_run_actions_and_wait_for_next_latest_frame_after_seq(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        min_frame_seq: u32,
    ) -> Result<LatestFrameSummarySnapshot> {
        self.try_run_actions_and_wait_for_next_latest_frame_matching(actions, latest, |summary| {
            summary.frame_seq > min_frame_seq
        })
    }

    /// Queue an action plan through [`Self::try_run_actions`], then wait up to
    /// `timeout` for the next newest-only frame with
    /// `frame_seq > min_frame_seq`.
    pub fn try_run_actions_and_wait_for_next_latest_frame_after_seq_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        min_frame_seq: u32,
        timeout: Duration,
    ) -> Result<LatestFrameSummarySnapshot> {
        self.try_run_actions_and_wait_for_next_latest_frame_matching_timeout(
            actions,
            latest,
            timeout,
            |summary| summary.frame_seq > min_frame_seq,
        )
    }

    /// Queue an action plan through [`Self::try_run_actions`], then wait for the
    /// next newest-only frame with `timestamp_ms > min_timestamp_ms`.
    pub fn try_run_actions_and_wait_for_next_latest_frame_after_timestamp(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        min_timestamp_ms: u64,
    ) -> Result<LatestFrameSummarySnapshot> {
        self.try_run_actions_and_wait_for_next_latest_frame_matching(actions, latest, |summary| {
            summary.timestamp_ms > min_timestamp_ms
        })
    }

    /// Queue an action plan through [`Self::try_run_actions`], then wait up to
    /// `timeout` for the next newest-only frame with
    /// `timestamp_ms > min_timestamp_ms`.
    pub fn try_run_actions_and_wait_for_next_latest_frame_after_timestamp_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        min_timestamp_ms: u64,
        timeout: Duration,
    ) -> Result<LatestFrameSummarySnapshot> {
        self.try_run_actions_and_wait_for_next_latest_frame_matching_timeout(
            actions,
            latest,
            timeout,
            |summary| summary.timestamp_ms > min_timestamp_ms,
        )
    }

    /// Run an action plan, wait for one checked dispatcher barrier, then wait
    /// for the next newest-only frame snapshot observed after that barrier.
    pub fn run_actions_and_wait_for_next_latest_frame(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
    ) -> Result<LatestFrameSummarySnapshot> {
        self.run_actions_and_wait_for_next_latest_frame_matching(actions, latest, |_| true)
    }

    /// Run an action plan, wait for one checked dispatcher barrier, then wait
    /// for the next newest-only frame snapshot accepted by `predicate`.
    pub fn run_actions_and_wait_for_next_latest_frame_matching(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        mut predicate: impl FnMut(&FrameSummary) -> bool,
    ) -> Result<LatestFrameSummarySnapshot> {
        self.run_actions(actions)?;
        let after_version = latest.version();
        latest
            .wait_next_matching(after_version, |snapshot| predicate(&snapshot.summary))
            .map_err(|e| io_to_wait_error(e, "latest frame summary"))
    }

    /// Run an action plan, wait for one checked dispatcher barrier, then wait
    /// for the next newest-only frame snapshot within `timeout`.
    pub fn run_actions_and_wait_for_next_latest_frame_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        timeout: Duration,
    ) -> Result<LatestFrameSummarySnapshot> {
        self.run_actions_and_wait_for_next_latest_frame_matching_timeout(
            actions,
            latest,
            timeout,
            |_| true,
        )
    }

    /// Run an action plan, wait for one checked dispatcher barrier, then wait
    /// up to `timeout` for the next newest-only frame accepted by `predicate`.
    pub fn run_actions_and_wait_for_next_latest_frame_matching_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        timeout: Duration,
        mut predicate: impl FnMut(&FrameSummary) -> bool,
    ) -> Result<LatestFrameSummarySnapshot> {
        self.run_actions(actions)?;
        let after_version = latest.version();
        latest
            .wait_next_matching_timeout(after_version, timeout, |snapshot| {
                predicate(&snapshot.summary)
            })
            .map_err(|e| io_to_wait_error(e, "latest frame summary"))
    }

    /// Run an action plan, wait for one checked dispatcher barrier, then wait
    /// for a newest-only frame snapshot with `version > after_version`.
    ///
    /// Use this when the caller already captured a latest-frame boundary before
    /// deciding which actions to send and wants to accept any cached/newer frame
    /// observed since that boundary.
    pub fn run_actions_and_wait_for_next_latest_frame_after_version(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        after_version: u64,
    ) -> Result<LatestFrameSummarySnapshot> {
        self.run_actions_and_wait_for_next_latest_frame_matching_after_version(
            actions,
            latest,
            after_version,
            |_| true,
        )
    }

    /// Run an action plan, wait for one checked dispatcher barrier, then wait
    /// for a newest-only frame snapshot newer than `boundary`.
    pub fn run_actions_and_wait_for_next_latest_frame_after_boundary(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        boundary: LatestFrameSummaryBoundary,
    ) -> Result<LatestFrameSummarySnapshot> {
        self.run_actions_and_wait_for_next_latest_frame_after_version(
            actions,
            latest,
            boundary.version(),
        )
    }

    /// Run an action plan, wait for one checked dispatcher barrier, then wait
    /// for a newest-only frame snapshot newer than `observation.boundary`.
    pub fn run_actions_and_wait_for_next_latest_frame_after_observation(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        observation: &LatestFrameSummaryObservation,
    ) -> Result<LatestFrameSummarySnapshot> {
        self.run_actions_and_wait_for_next_latest_frame_after_boundary(
            actions,
            latest,
            observation.boundary(),
        )
    }

    /// Run an action plan, wait for one checked dispatcher barrier, then wait
    /// for a newest-only frame snapshot with `version > after_version` accepted
    /// by `predicate`.
    pub fn run_actions_and_wait_for_next_latest_frame_matching_after_version(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        after_version: u64,
        mut predicate: impl FnMut(&FrameSummary) -> bool,
    ) -> Result<LatestFrameSummarySnapshot> {
        self.run_actions(actions)?;
        latest
            .wait_next_matching(after_version, |snapshot| predicate(&snapshot.summary))
            .map_err(|e| io_to_wait_error(e, "latest frame summary"))
    }

    /// Run an action plan, wait for one checked dispatcher barrier, then wait
    /// for a newest-only frame snapshot newer than `boundary` and accepted by
    /// `predicate`.
    pub fn run_actions_and_wait_for_next_latest_frame_matching_after_boundary(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        boundary: LatestFrameSummaryBoundary,
        predicate: impl FnMut(&FrameSummary) -> bool,
    ) -> Result<LatestFrameSummarySnapshot> {
        self.run_actions_and_wait_for_next_latest_frame_matching_after_version(
            actions,
            latest,
            boundary.version(),
            predicate,
        )
    }

    /// Run an action plan, wait for one checked dispatcher barrier, then wait
    /// for a newest-only frame snapshot newer than `observation.boundary` and
    /// accepted by `predicate`.
    pub fn run_actions_and_wait_for_next_latest_frame_matching_after_observation(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        observation: &LatestFrameSummaryObservation,
        predicate: impl FnMut(&FrameSummary) -> bool,
    ) -> Result<LatestFrameSummarySnapshot> {
        self.run_actions_and_wait_for_next_latest_frame_matching_after_boundary(
            actions,
            latest,
            observation.boundary(),
            predicate,
        )
    }

    /// Run an action plan, wait for one checked dispatcher barrier, then wait
    /// up to `timeout` for a newest-only frame with `version > after_version`.
    pub fn run_actions_and_wait_for_next_latest_frame_after_version_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        after_version: u64,
        timeout: Duration,
    ) -> Result<LatestFrameSummarySnapshot> {
        self.run_actions_and_wait_for_next_latest_frame_matching_after_version_timeout(
            actions,
            latest,
            after_version,
            timeout,
            |_| true,
        )
    }

    /// Run an action plan, wait for one checked dispatcher barrier, then wait
    /// up to `timeout` for a newest-only frame snapshot newer than `boundary`.
    pub fn run_actions_and_wait_for_next_latest_frame_after_boundary_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        boundary: LatestFrameSummaryBoundary,
        timeout: Duration,
    ) -> Result<LatestFrameSummarySnapshot> {
        self.run_actions_and_wait_for_next_latest_frame_after_version_timeout(
            actions,
            latest,
            boundary.version(),
            timeout,
        )
    }

    /// Run an action plan, wait for one checked dispatcher barrier, then wait up
    /// to `timeout` for a newest-only frame snapshot newer than
    /// `observation.boundary`.
    pub fn run_actions_and_wait_for_next_latest_frame_after_observation_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        observation: &LatestFrameSummaryObservation,
        timeout: Duration,
    ) -> Result<LatestFrameSummarySnapshot> {
        self.run_actions_and_wait_for_next_latest_frame_after_boundary_timeout(
            actions,
            latest,
            observation.boundary(),
            timeout,
        )
    }

    /// Run an action plan, wait for one checked dispatcher barrier, then wait
    /// up to `timeout` for a newest-only frame with `version > after_version`
    /// accepted by `predicate`.
    pub fn run_actions_and_wait_for_next_latest_frame_matching_after_version_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        after_version: u64,
        timeout: Duration,
        mut predicate: impl FnMut(&FrameSummary) -> bool,
    ) -> Result<LatestFrameSummarySnapshot> {
        self.run_actions(actions)?;
        latest
            .wait_next_matching_timeout(after_version, timeout, |snapshot| {
                predicate(&snapshot.summary)
            })
            .map_err(|e| io_to_wait_error(e, "latest frame summary"))
    }

    /// Run an action plan, wait for one checked dispatcher barrier, then wait
    /// up to `timeout` for a newest-only frame snapshot newer than `boundary`
    /// and accepted by `predicate`.
    pub fn run_actions_and_wait_for_next_latest_frame_matching_after_boundary_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        boundary: LatestFrameSummaryBoundary,
        timeout: Duration,
        predicate: impl FnMut(&FrameSummary) -> bool,
    ) -> Result<LatestFrameSummarySnapshot> {
        self.run_actions_and_wait_for_next_latest_frame_matching_after_version_timeout(
            actions,
            latest,
            boundary.version(),
            timeout,
            predicate,
        )
    }

    /// Run an action plan, wait for one checked dispatcher barrier, then wait up
    /// to `timeout` for a newest-only frame snapshot newer than
    /// `observation.boundary` and accepted by `predicate`.
    pub fn run_actions_and_wait_for_next_latest_frame_matching_after_observation_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        observation: &LatestFrameSummaryObservation,
        timeout: Duration,
        predicate: impl FnMut(&FrameSummary) -> bool,
    ) -> Result<LatestFrameSummarySnapshot> {
        self.run_actions_and_wait_for_next_latest_frame_matching_after_boundary_timeout(
            actions,
            latest,
            observation.boundary(),
            timeout,
            predicate,
        )
    }

    /// Run an action plan, wait for one checked dispatcher barrier, then wait
    /// for the next newest-only frame with `frame_seq > min_frame_seq`.
    pub fn run_actions_and_wait_for_next_latest_frame_after_seq(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        min_frame_seq: u32,
    ) -> Result<LatestFrameSummarySnapshot> {
        self.run_actions_and_wait_for_next_latest_frame_matching(actions, latest, |summary| {
            summary.frame_seq > min_frame_seq
        })
    }

    /// Run an action plan, wait for one checked dispatcher barrier, then wait
    /// up to `timeout` for the next newest-only frame with
    /// `frame_seq > min_frame_seq`.
    pub fn run_actions_and_wait_for_next_latest_frame_after_seq_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        min_frame_seq: u32,
        timeout: Duration,
    ) -> Result<LatestFrameSummarySnapshot> {
        self.run_actions_and_wait_for_next_latest_frame_matching_timeout(
            actions,
            latest,
            timeout,
            |summary| summary.frame_seq > min_frame_seq,
        )
    }

    /// Run an action plan, wait for one checked dispatcher barrier, then wait
    /// for the next newest-only frame with
    /// `timestamp_ms > min_timestamp_ms`.
    pub fn run_actions_and_wait_for_next_latest_frame_after_timestamp(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        min_timestamp_ms: u64,
    ) -> Result<LatestFrameSummarySnapshot> {
        self.run_actions_and_wait_for_next_latest_frame_matching(actions, latest, |summary| {
            summary.timestamp_ms > min_timestamp_ms
        })
    }

    /// Run an action plan, wait for one checked dispatcher barrier, then wait
    /// up to `timeout` for the next newest-only frame with
    /// `timestamp_ms > min_timestamp_ms`.
    pub fn run_actions_and_wait_for_next_latest_frame_after_timestamp_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        min_timestamp_ms: u64,
        timeout: Duration,
    ) -> Result<LatestFrameSummarySnapshot> {
        self.run_actions_and_wait_for_next_latest_frame_matching_timeout(
            actions,
            latest,
            timeout,
            |summary| summary.timestamp_ms > min_timestamp_ms,
        )
    }

    /// Queue an action plan through [`Self::try_run_actions`], wait for the
    /// next newest-only frame containing `target`, and return its rectangle.
    pub fn try_run_actions_and_wait_for_next_latest_target_rect(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        target: AgentTargetSelector,
    ) -> Result<AgentRect> {
        self.try_run_actions_and_select_next_latest_target(actions, latest, target)
    }

    /// Queue an action plan through [`Self::try_run_actions`], wait up to
    /// `timeout` for the next newest-only frame containing `target`, and return
    /// its rectangle.
    pub fn try_run_actions_and_wait_for_next_latest_target_rect_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        target: AgentTargetSelector,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.try_run_actions_and_select_next_latest_target_timeout(actions, latest, target, timeout)
    }

    /// Queue an action plan through [`Self::try_run_actions`], wait for the
    /// next newest-only frame newer than `after_version` and containing
    /// `target`, and return its rectangle.
    pub fn try_run_actions_and_wait_for_next_latest_target_rect_after_version(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        target: AgentTargetSelector,
        after_version: u64,
    ) -> Result<AgentRect> {
        self.try_run_actions_and_select_next_latest_target_after_version(
            actions,
            latest,
            target,
            after_version,
        )
    }

    /// Queue an action plan through [`Self::try_run_actions`], wait for the
    /// next newest-only frame newer than `boundary` and containing `target`,
    /// and return its rectangle.
    pub fn try_run_actions_and_wait_for_next_latest_target_rect_after_boundary(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        target: AgentTargetSelector,
        boundary: LatestFrameSummaryBoundary,
    ) -> Result<AgentRect> {
        self.try_run_actions_and_wait_for_next_latest_target_rect_after_version(
            actions,
            latest,
            target,
            boundary.version(),
        )
    }

    /// Queue an action plan through [`Self::try_run_actions`], wait for the
    /// next newest-only frame newer than `observation.boundary` and containing
    /// `target`, and return its rectangle.
    pub fn try_run_actions_and_wait_for_next_latest_target_rect_after_observation(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        target: AgentTargetSelector,
        observation: &LatestFrameSummaryObservation,
    ) -> Result<AgentRect> {
        self.try_run_actions_and_wait_for_next_latest_target_rect_after_boundary(
            actions,
            latest,
            target,
            observation.boundary(),
        )
    }

    /// Queue an action plan through [`Self::try_run_actions`], wait up to
    /// `timeout` for the next newest-only frame newer than `after_version` and
    /// containing `target`, and return its rectangle.
    pub fn try_run_actions_and_wait_for_next_latest_target_rect_after_version_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        target: AgentTargetSelector,
        after_version: u64,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.try_run_actions_and_select_next_latest_target_after_version_timeout(
            actions,
            latest,
            target,
            after_version,
            timeout,
        )
    }

    /// Queue an action plan through [`Self::try_run_actions`], wait up to
    /// `timeout` for the next newest-only frame newer than `boundary` and
    /// containing `target`, and return its rectangle.
    pub fn try_run_actions_and_wait_for_next_latest_target_rect_after_boundary_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        target: AgentTargetSelector,
        boundary: LatestFrameSummaryBoundary,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.try_run_actions_and_wait_for_next_latest_target_rect_after_version_timeout(
            actions,
            latest,
            target,
            boundary.version(),
            timeout,
        )
    }

    /// Queue an action plan through [`Self::try_run_actions`], wait up to
    /// `timeout` for the next newest-only frame newer than
    /// `observation.boundary` and containing `target`, and return its rectangle.
    pub fn try_run_actions_and_wait_for_next_latest_target_rect_after_observation_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        target: AgentTargetSelector,
        observation: &LatestFrameSummaryObservation,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.try_run_actions_and_wait_for_next_latest_target_rect_after_boundary_timeout(
            actions,
            latest,
            target,
            observation.boundary(),
            timeout,
        )
    }

    /// Queue an action plan through [`Self::try_run_actions`], wait up to
    /// `timeout` for the next newest-only frame containing `target`, tap a
    /// relative point inside it with a typed scrcpy pointer id, and return it.
    pub fn try_run_actions_and_tap_next_latest_target_at_pointer_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        target: AgentTargetSelector,
        pointer_id: TouchPointerId,
        anchor_bp: (u16, u16),
        timeout: Duration,
    ) -> Result<AgentRect> {
        let rect = self.try_run_actions_and_select_next_latest_target_timeout(
            actions, latest, target, timeout,
        )?;
        self.try_tap_rect_at_pointer(pointer_id, rect, anchor_bp.0, anchor_bp.1)?;
        Ok(rect)
    }

    /// Run an action plan, wait for the next newest-only frame containing
    /// `target`, and return its rectangle.
    pub fn run_actions_and_wait_for_next_latest_target_rect(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        target: AgentTargetSelector,
    ) -> Result<AgentRect> {
        self.run_actions_and_select_next_latest_target(actions, latest, target)
    }

    /// Run an action plan, wait up to `timeout` for the next newest-only frame
    /// containing `target`, and return its rectangle.
    pub fn run_actions_and_wait_for_next_latest_target_rect_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        target: AgentTargetSelector,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.run_actions_and_select_next_latest_target_timeout(actions, latest, target, timeout)
    }

    /// Run an action plan, wait for the next newest-only frame newer than
    /// `after_version` and containing `target`, and return its rectangle.
    pub fn run_actions_and_wait_for_next_latest_target_rect_after_version(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        target: AgentTargetSelector,
        after_version: u64,
    ) -> Result<AgentRect> {
        self.run_actions_and_select_next_latest_target_after_version(
            actions,
            latest,
            target,
            after_version,
        )
    }

    /// Run an action plan, wait for the next newest-only frame newer than
    /// `boundary` and containing `target`, and return its rectangle.
    pub fn run_actions_and_wait_for_next_latest_target_rect_after_boundary(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        target: AgentTargetSelector,
        boundary: LatestFrameSummaryBoundary,
    ) -> Result<AgentRect> {
        self.run_actions_and_wait_for_next_latest_target_rect_after_version(
            actions,
            latest,
            target,
            boundary.version(),
        )
    }

    /// Run an action plan, wait for the next newest-only frame newer than
    /// `observation.boundary` and containing `target`, and return its rectangle.
    pub fn run_actions_and_wait_for_next_latest_target_rect_after_observation(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        target: AgentTargetSelector,
        observation: &LatestFrameSummaryObservation,
    ) -> Result<AgentRect> {
        self.run_actions_and_wait_for_next_latest_target_rect_after_boundary(
            actions,
            latest,
            target,
            observation.boundary(),
        )
    }

    /// Run an action plan, wait up to `timeout` for the next newest-only frame
    /// newer than `after_version` and containing `target`, and return its
    /// rectangle.
    pub fn run_actions_and_wait_for_next_latest_target_rect_after_version_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        target: AgentTargetSelector,
        after_version: u64,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.run_actions_and_select_next_latest_target_after_version_timeout(
            actions,
            latest,
            target,
            after_version,
            timeout,
        )
    }

    /// Run an action plan, wait up to `timeout` for the next newest-only frame
    /// newer than `boundary` and containing `target`, and return its rectangle.
    pub fn run_actions_and_wait_for_next_latest_target_rect_after_boundary_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        target: AgentTargetSelector,
        boundary: LatestFrameSummaryBoundary,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.run_actions_and_wait_for_next_latest_target_rect_after_version_timeout(
            actions,
            latest,
            target,
            boundary.version(),
            timeout,
        )
    }

    /// Run an action plan, wait up to `timeout` for the next newest-only frame
    /// newer than `observation.boundary` and containing `target`, and return its
    /// rectangle.
    pub fn run_actions_and_wait_for_next_latest_target_rect_after_observation_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        target: AgentTargetSelector,
        observation: &LatestFrameSummaryObservation,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.run_actions_and_wait_for_next_latest_target_rect_after_boundary_timeout(
            actions,
            latest,
            target,
            observation.boundary(),
            timeout,
        )
    }

    /// Run an action plan, wait for the next newest-only frame containing
    /// `target`, tap its center, and return it.
    pub fn run_actions_and_tap_next_latest_target(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        target: AgentTargetSelector,
    ) -> Result<AgentRect> {
        let rect = self.run_actions_and_select_next_latest_target(actions, latest, target)?;
        self.tap_rect(rect)?;
        Ok(rect)
    }

    /// Run an action plan, wait for the next newest-only frame newer than
    /// `after_version` and containing `target`, tap its center, and return it.
    pub fn run_actions_and_tap_next_latest_target_after_version(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        target: AgentTargetSelector,
        after_version: u64,
    ) -> Result<AgentRect> {
        let rect = self.run_actions_and_select_next_latest_target_after_version(
            actions,
            latest,
            target,
            after_version,
        )?;
        self.tap_rect(rect)?;
        Ok(rect)
    }

    /// Run an action plan, wait for the next newest-only frame newer than
    /// `boundary` and containing `target`, tap its center, and return it.
    pub fn run_actions_and_tap_next_latest_target_after_boundary(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        target: AgentTargetSelector,
        boundary: LatestFrameSummaryBoundary,
    ) -> Result<AgentRect> {
        self.run_actions_and_tap_next_latest_target_after_version(
            actions,
            latest,
            target,
            boundary.version(),
        )
    }

    /// Run an action plan, wait for the next newest-only frame newer than
    /// `observation.boundary` and containing `target`, tap its center, and
    /// return it.
    pub fn run_actions_and_tap_next_latest_target_after_observation(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        target: AgentTargetSelector,
        observation: &LatestFrameSummaryObservation,
    ) -> Result<AgentRect> {
        self.run_actions_and_tap_next_latest_target_after_boundary(
            actions,
            latest,
            target,
            observation.boundary(),
        )
    }

    /// Run an action plan, wait up to `timeout` for the next newest-only frame
    /// newer than `after_version` and containing `target`, tap its center, and
    /// return it.
    pub fn run_actions_and_tap_next_latest_target_after_version_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        target: AgentTargetSelector,
        after_version: u64,
        timeout: Duration,
    ) -> Result<AgentRect> {
        let rect = self.run_actions_and_select_next_latest_target_after_version_timeout(
            actions,
            latest,
            target,
            after_version,
            timeout,
        )?;
        self.tap_rect(rect)?;
        Ok(rect)
    }

    /// Run an action plan, wait up to `timeout` for the next newest-only frame
    /// newer than `boundary` and containing `target`, tap its center, and return
    /// it.
    pub fn run_actions_and_tap_next_latest_target_after_boundary_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        target: AgentTargetSelector,
        boundary: LatestFrameSummaryBoundary,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.run_actions_and_tap_next_latest_target_after_version_timeout(
            actions,
            latest,
            target,
            boundary.version(),
            timeout,
        )
    }

    /// Run an action plan, wait up to `timeout` for the next newest-only frame
    /// newer than `observation.boundary` and containing `target`, tap its center,
    /// and return it.
    pub fn run_actions_and_tap_next_latest_target_after_observation_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        target: AgentTargetSelector,
        observation: &LatestFrameSummaryObservation,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.run_actions_and_tap_next_latest_target_after_boundary_timeout(
            actions,
            latest,
            target,
            observation.boundary(),
            timeout,
        )
    }

    /// Run an action plan, wait up to `timeout` for the next newest-only frame
    /// containing `target`, tap its center, and return it.
    pub fn run_actions_and_tap_next_latest_target_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        target: AgentTargetSelector,
        timeout: Duration,
    ) -> Result<AgentRect> {
        let rect = self
            .run_actions_and_select_next_latest_target_timeout(actions, latest, target, timeout)?;
        self.tap_rect(rect)?;
        Ok(rect)
    }

    /// Run an action plan, wait for the next newest-only frame containing
    /// `target`, tap its center with a typed scrcpy pointer id, and return it.
    pub fn run_actions_and_tap_next_latest_target_pointer(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        target: AgentTargetSelector,
        pointer_id: TouchPointerId,
    ) -> Result<AgentRect> {
        let rect = self.run_actions_and_select_next_latest_target(actions, latest, target)?;
        self.tap_rect_pointer(pointer_id, rect)?;
        Ok(rect)
    }

    /// Run an action plan, wait up to `timeout` for the next newest-only frame
    /// containing `target`, tap its center with a typed scrcpy pointer id, and
    /// return it.
    pub fn run_actions_and_tap_next_latest_target_pointer_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        target: AgentTargetSelector,
        pointer_id: TouchPointerId,
        timeout: Duration,
    ) -> Result<AgentRect> {
        let rect = self
            .run_actions_and_select_next_latest_target_timeout(actions, latest, target, timeout)?;
        self.tap_rect_pointer(pointer_id, rect)?;
        Ok(rect)
    }

    /// Run an action plan, wait for the next newest-only frame containing
    /// `target`, tap a relative point inside it, and return it.
    pub fn run_actions_and_tap_next_latest_target_at(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        target: AgentTargetSelector,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<AgentRect> {
        let rect = self.run_actions_and_select_next_latest_target(actions, latest, target)?;
        self.tap_rect_at(rect, x_bp, y_bp)?;
        Ok(rect)
    }

    /// Run an action plan, wait up to `timeout` for the next newest-only frame
    /// containing `target`, tap a relative point inside it, and return it.
    pub fn run_actions_and_tap_next_latest_target_at_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        target: AgentTargetSelector,
        x_bp: u16,
        y_bp: u16,
        timeout: Duration,
    ) -> Result<AgentRect> {
        let rect = self
            .run_actions_and_select_next_latest_target_timeout(actions, latest, target, timeout)?;
        self.tap_rect_at(rect, x_bp, y_bp)?;
        Ok(rect)
    }

    /// Run an action plan, wait for the next newest-only frame containing
    /// `target`, tap a relative point inside it with a typed scrcpy pointer id,
    /// and return it.
    pub fn run_actions_and_tap_next_latest_target_at_pointer(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        target: AgentTargetSelector,
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<AgentRect> {
        let rect = self.run_actions_and_select_next_latest_target(actions, latest, target)?;
        self.tap_rect_at_pointer(pointer_id, rect, x_bp, y_bp)?;
        Ok(rect)
    }

    /// Run an action plan, wait up to `timeout` for the next newest-only frame
    /// containing `target`, tap a relative point inside it with a typed scrcpy
    /// pointer id, and return it.
    pub fn run_actions_and_tap_next_latest_target_at_pointer_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        target: AgentTargetSelector,
        pointer_id: TouchPointerId,
        anchor_bp: (u16, u16),
        timeout: Duration,
    ) -> Result<AgentRect> {
        let rect = self
            .run_actions_and_select_next_latest_target_timeout(actions, latest, target, timeout)?;
        self.tap_rect_at_pointer(pointer_id, rect, anchor_bp.0, anchor_bp.1)?;
        Ok(rect)
    }

    /// Run an action plan, wait for the next newest-only frame containing an
    /// object matching `selector`, tap its center, and return it.
    pub fn run_actions_and_tap_next_latest_object_selector(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        selector: AgentObjectSelector,
    ) -> Result<AgentRect> {
        let rect =
            self.run_actions_and_select_next_latest_object_selector(actions, latest, selector)?;
        self.tap_rect(rect)?;
        Ok(rect)
    }

    /// Run an action plan, wait up to `timeout` for the next newest-only frame
    /// containing an object matching `selector`, tap its center, and return it.
    pub fn run_actions_and_tap_next_latest_object_selector_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        selector: AgentObjectSelector,
        timeout: Duration,
    ) -> Result<AgentRect> {
        let rect = self.run_actions_and_select_next_latest_object_selector_timeout(
            actions, latest, selector, timeout,
        )?;
        self.tap_rect(rect)?;
        Ok(rect)
    }

    /// Run an action plan, wait for the next newest-only frame containing an
    /// object matching `selector`, tap its center with a typed scrcpy pointer
    /// id, and return it.
    pub fn run_actions_and_tap_next_latest_object_selector_pointer(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        selector: AgentObjectSelector,
        pointer_id: TouchPointerId,
    ) -> Result<AgentRect> {
        let rect =
            self.run_actions_and_select_next_latest_object_selector(actions, latest, selector)?;
        self.tap_rect_pointer(pointer_id, rect)?;
        Ok(rect)
    }

    /// Run an action plan, wait up to `timeout` for the next newest-only frame
    /// containing an object matching `selector`, tap its center with a typed
    /// scrcpy pointer id, and return it.
    pub fn run_actions_and_tap_next_latest_object_selector_pointer_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        selector: AgentObjectSelector,
        pointer_id: TouchPointerId,
        timeout: Duration,
    ) -> Result<AgentRect> {
        let rect = self.run_actions_and_select_next_latest_object_selector_timeout(
            actions, latest, selector, timeout,
        )?;
        self.tap_rect_pointer(pointer_id, rect)?;
        Ok(rect)
    }

    /// Run an action plan, wait for the next newest-only frame containing an
    /// object matching `selector`, tap a relative point inside it, and return
    /// it.
    pub fn run_actions_and_tap_next_latest_object_selector_at(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        selector: AgentObjectSelector,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<AgentRect> {
        let rect =
            self.run_actions_and_select_next_latest_object_selector(actions, latest, selector)?;
        self.tap_rect_at(rect, x_bp, y_bp)?;
        Ok(rect)
    }

    /// Run an action plan, wait up to `timeout` for the next newest-only frame
    /// containing an object matching `selector`, tap a relative point inside
    /// it, and return it.
    pub fn run_actions_and_tap_next_latest_object_selector_at_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        selector: AgentObjectSelector,
        x_bp: u16,
        y_bp: u16,
        timeout: Duration,
    ) -> Result<AgentRect> {
        let rect = self.run_actions_and_select_next_latest_object_selector_timeout(
            actions, latest, selector, timeout,
        )?;
        self.tap_rect_at(rect, x_bp, y_bp)?;
        Ok(rect)
    }

    /// Run an action plan, wait for the next newest-only frame containing an
    /// object matching `selector`, tap a relative point inside it with a typed
    /// scrcpy pointer id, and return it.
    pub fn run_actions_and_tap_next_latest_object_selector_at_pointer(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        selector: AgentObjectSelector,
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<AgentRect> {
        let rect =
            self.run_actions_and_select_next_latest_object_selector(actions, latest, selector)?;
        self.tap_rect_at_pointer(pointer_id, rect, x_bp, y_bp)?;
        Ok(rect)
    }

    /// Run an action plan, wait up to `timeout` for the next newest-only frame
    /// containing an object matching `selector`, tap a relative point inside
    /// it with a typed scrcpy pointer id, and return it.
    pub fn run_actions_and_tap_next_latest_object_selector_at_pointer_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        selector: AgentObjectSelector,
        pointer_id: TouchPointerId,
        anchor_bp: (u16, u16),
        timeout: Duration,
    ) -> Result<AgentRect> {
        let rect = self.run_actions_and_select_next_latest_object_selector_timeout(
            actions, latest, selector, timeout,
        )?;
        self.tap_rect_at_pointer(pointer_id, rect, anchor_bp.0, anchor_bp.1)?;
        Ok(rect)
    }

    /// Run an action plan, wait for the next newest-only frame containing at
    /// least one text region, tap the largest region's center, and return it.
    pub fn run_actions_and_tap_next_latest_largest_text_region(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
    ) -> Result<AgentRect> {
        let rect = self.run_actions_and_select_next_latest_largest_text_region(actions, latest)?;
        self.tap_rect(rect)?;
        Ok(rect)
    }

    /// Run an action plan, wait up to `timeout` for the next newest-only frame
    /// containing at least one text region, tap the largest region's center,
    /// and return it.
    pub fn run_actions_and_tap_next_latest_largest_text_region_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        timeout: Duration,
    ) -> Result<AgentRect> {
        let rect = self.run_actions_and_select_next_latest_largest_text_region_timeout(
            actions, latest, timeout,
        )?;
        self.tap_rect(rect)?;
        Ok(rect)
    }

    /// Run an action plan, wait for the next newest-only frame containing at
    /// least one text region, tap the largest region's center with a typed
    /// scrcpy pointer id, and return it.
    pub fn run_actions_and_tap_next_latest_largest_text_region_pointer(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        pointer_id: TouchPointerId,
    ) -> Result<AgentRect> {
        let rect = self.run_actions_and_select_next_latest_largest_text_region(actions, latest)?;
        self.tap_rect_pointer(pointer_id, rect)?;
        Ok(rect)
    }

    /// Run an action plan, wait up to `timeout` for the next newest-only frame
    /// containing at least one text region, tap the largest region's center
    /// with a typed scrcpy pointer id, and return it.
    pub fn run_actions_and_tap_next_latest_largest_text_region_pointer_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        pointer_id: TouchPointerId,
        timeout: Duration,
    ) -> Result<AgentRect> {
        let rect = self.run_actions_and_select_next_latest_largest_text_region_timeout(
            actions, latest, timeout,
        )?;
        self.tap_rect_pointer(pointer_id, rect)?;
        Ok(rect)
    }

    /// Run an action plan, wait for the next newest-only frame containing at
    /// least one text region, tap a relative point inside the largest region,
    /// and return it.
    pub fn run_actions_and_tap_next_latest_largest_text_region_at(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<AgentRect> {
        let rect = self.run_actions_and_select_next_latest_largest_text_region(actions, latest)?;
        self.tap_rect_at(rect, x_bp, y_bp)?;
        Ok(rect)
    }

    /// Run an action plan, wait up to `timeout` for the next newest-only frame
    /// containing at least one text region, tap a relative point inside the
    /// largest region, and return it.
    pub fn run_actions_and_tap_next_latest_largest_text_region_at_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        x_bp: u16,
        y_bp: u16,
        timeout: Duration,
    ) -> Result<AgentRect> {
        let rect = self.run_actions_and_select_next_latest_largest_text_region_timeout(
            actions, latest, timeout,
        )?;
        self.tap_rect_at(rect, x_bp, y_bp)?;
        Ok(rect)
    }

    /// Run an action plan, wait for the next newest-only frame containing at
    /// least one text region, tap a relative point inside the largest region
    /// with a typed scrcpy pointer id, and return it.
    pub fn run_actions_and_tap_next_latest_largest_text_region_at_pointer(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<AgentRect> {
        let rect = self.run_actions_and_select_next_latest_largest_text_region(actions, latest)?;
        self.tap_rect_at_pointer(pointer_id, rect, x_bp, y_bp)?;
        Ok(rect)
    }

    /// Run an action plan, wait up to `timeout` for the next newest-only frame
    /// containing at least one text region, tap a relative point inside the
    /// largest region with a typed scrcpy pointer id, and return it.
    pub fn run_actions_and_tap_next_latest_largest_text_region_at_pointer_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        pointer_id: TouchPointerId,
        anchor_bp: (u16, u16),
        timeout: Duration,
    ) -> Result<AgentRect> {
        let rect = self.run_actions_and_select_next_latest_largest_text_region_timeout(
            actions, latest, timeout,
        )?;
        self.tap_rect_at_pointer(pointer_id, rect, anchor_bp.0, anchor_bp.1)?;
        Ok(rect)
    }

    fn run_actions_and_select_next_latest_object_selector(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        selector: AgentObjectSelector,
    ) -> Result<AgentRect> {
        self.run_actions_and_select_next_latest_target(
            actions,
            latest,
            AgentTargetSelector::object_matching(selector),
        )
    }

    fn run_actions_and_select_next_latest_object_selector_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        selector: AgentObjectSelector,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.run_actions_and_select_next_latest_target_timeout(
            actions,
            latest,
            AgentTargetSelector::object_matching(selector),
            timeout,
        )
    }

    fn run_actions_and_select_next_latest_largest_text_region(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
    ) -> Result<AgentRect> {
        self.run_actions_and_select_next_latest_target(
            actions,
            latest,
            AgentTargetSelector::largest_text_region(),
        )
    }

    fn run_actions_and_select_next_latest_largest_text_region_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        timeout: Duration,
    ) -> Result<AgentRect> {
        self.run_actions_and_select_next_latest_target_timeout(
            actions,
            latest,
            AgentTargetSelector::largest_text_region(),
            timeout,
        )
    }

    fn try_run_actions_and_select_next_latest_target(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        target: AgentTargetSelector,
    ) -> Result<AgentRect> {
        let snapshot = self.try_run_actions_and_wait_for_next_latest_frame_matching(
            actions,
            latest,
            |summary| target.is_present(summary),
        )?;
        self.latest_target_rect(&snapshot, target)?
            .ok_or(Error::SessionLifecycle("latest target disappeared"))
    }

    fn try_run_actions_and_select_next_latest_target_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        target: AgentTargetSelector,
        timeout: Duration,
    ) -> Result<AgentRect> {
        let snapshot = self.try_run_actions_and_wait_for_next_latest_frame_matching_timeout(
            actions,
            latest,
            timeout,
            |summary| target.is_present(summary),
        )?;
        self.latest_target_rect(&snapshot, target)?
            .ok_or(Error::SessionLifecycle("latest target disappeared"))
    }

    fn try_run_actions_and_select_next_latest_target_after_version(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        target: AgentTargetSelector,
        after_version: u64,
    ) -> Result<AgentRect> {
        let snapshot = self.try_run_actions_and_wait_for_next_latest_frame_matching_after_version(
            actions,
            latest,
            after_version,
            |summary| target.is_present(summary),
        )?;
        self.latest_target_rect(&snapshot, target)?
            .ok_or(Error::SessionLifecycle("latest target disappeared"))
    }

    fn try_run_actions_and_select_next_latest_target_after_version_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        target: AgentTargetSelector,
        after_version: u64,
        timeout: Duration,
    ) -> Result<AgentRect> {
        let snapshot = self
            .try_run_actions_and_wait_for_next_latest_frame_matching_after_version_timeout(
                actions,
                latest,
                after_version,
                timeout,
                |summary| target.is_present(summary),
            )?;
        self.latest_target_rect(&snapshot, target)?
            .ok_or(Error::SessionLifecycle("latest target disappeared"))
    }

    fn run_actions_and_select_next_latest_target(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        target: AgentTargetSelector,
    ) -> Result<AgentRect> {
        let snapshot =
            self.run_actions_and_wait_for_next_latest_frame_matching(actions, latest, |summary| {
                target.is_present(summary)
            })?;
        self.latest_target_rect(&snapshot, target)?
            .ok_or(Error::SessionLifecycle("latest target disappeared"))
    }

    fn run_actions_and_select_next_latest_target_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        target: AgentTargetSelector,
        timeout: Duration,
    ) -> Result<AgentRect> {
        let snapshot = self.run_actions_and_wait_for_next_latest_frame_matching_timeout(
            actions,
            latest,
            timeout,
            |summary| target.is_present(summary),
        )?;
        self.latest_target_rect(&snapshot, target)?
            .ok_or(Error::SessionLifecycle("latest target disappeared"))
    }

    fn run_actions_and_select_next_latest_target_after_version(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        target: AgentTargetSelector,
        after_version: u64,
    ) -> Result<AgentRect> {
        let snapshot = self.run_actions_and_wait_for_next_latest_frame_matching_after_version(
            actions,
            latest,
            after_version,
            |summary| target.is_present(summary),
        )?;
        self.latest_target_rect(&snapshot, target)?
            .ok_or(Error::SessionLifecycle("latest target disappeared"))
    }

    fn run_actions_and_select_next_latest_target_after_version_timeout(
        &self,
        actions: &[AgentAction],
        latest: &LatestFrameSummaryReceiver,
        target: AgentTargetSelector,
        after_version: u64,
        timeout: Duration,
    ) -> Result<AgentRect> {
        let snapshot = self
            .run_actions_and_wait_for_next_latest_frame_matching_after_version_timeout(
                actions,
                latest,
                after_version,
                timeout,
                |summary| target.is_present(summary),
            )?;
        self.latest_target_rect(&snapshot, target)?
            .ok_or(Error::SessionLifecycle("latest target disappeared"))
    }

    /// Run an action plan, wait for one checked dispatcher barrier, then read
    /// the next AI frame summary.
    pub fn run_actions_and_wait_for_frame_summary(
        &mut self,
        actions: &[AgentAction],
    ) -> Result<FrameSummary> {
        self.run_actions(actions)?;
        self.wait_for_frame_summary()
    }

    /// Run an action plan, then wait for the next scene-change frame.
    pub fn run_actions_and_wait_for_scene_change(
        &mut self,
        actions: &[AgentAction],
    ) -> Result<FrameSummary> {
        self.run_actions(actions)?;
        self.wait_for_scene_change()
    }

    /// Run an action plan, then wait for the next frame with motion vectors.
    pub fn run_actions_and_wait_for_motion(
        &mut self,
        actions: &[AgentAction],
    ) -> Result<FrameSummary> {
        self.run_actions(actions)?;
        self.wait_for_motion()
    }

    /// Run an action plan, then wait for one stable frame.
    pub fn run_actions_and_wait_for_stable_frame(
        &mut self,
        actions: &[AgentAction],
    ) -> Result<FrameSummary> {
        self.run_actions_and_wait_for_stable_frames(actions, 1)
    }

    /// Run an action plan, then wait for `consecutive` stable frames.
    pub fn run_actions_and_wait_for_stable_frames(
        &mut self,
        actions: &[AgentAction],
        consecutive: usize,
    ) -> Result<FrameSummary> {
        self.run_actions(actions)?;
        self.wait_for_stable_frames(consecutive)
    }

    /// Run an action plan, wait for one checked dispatcher barrier, then read
    /// at most `max_summaries` frame summaries until `predicate` accepts one.
    pub fn run_actions_and_wait_for_frame_summary_matching_with_limit(
        &mut self,
        actions: &[AgentAction],
        max_summaries: usize,
        predicate: impl FnMut(&FrameSummary) -> bool,
    ) -> Result<Option<FrameSummary>> {
        self.run_actions(actions)?;
        self.wait_for_frame_summary_matching_with_limit(max_summaries, predicate)
    }

    /// Run an action plan, then wait for a frame with
    /// `frame_seq > min_frame_seq`.
    pub fn run_actions_and_wait_for_frame_summary_after_seq(
        &mut self,
        actions: &[AgentAction],
        min_frame_seq: u32,
    ) -> Result<FrameSummary> {
        self.run_actions(actions)?;
        self.wait_for_frame_summary_after_seq(min_frame_seq)
    }

    /// Run an action plan, then inspect at most `max_summaries` frame summaries
    /// until one has `frame_seq > min_frame_seq`.
    pub fn run_actions_and_wait_for_frame_summary_after_seq_with_limit(
        &mut self,
        actions: &[AgentAction],
        min_frame_seq: u32,
        max_summaries: usize,
    ) -> Result<Option<FrameSummary>> {
        self.run_actions(actions)?;
        self.wait_for_frame_summary_after_seq_with_limit(min_frame_seq, max_summaries)
    }

    /// Run an action plan, then wait for a frame with
    /// `timestamp_ms > min_timestamp_ms`.
    pub fn run_actions_and_wait_for_frame_summary_after_timestamp(
        &mut self,
        actions: &[AgentAction],
        min_timestamp_ms: u64,
    ) -> Result<FrameSummary> {
        self.run_actions(actions)?;
        self.wait_for_frame_summary_after_timestamp(min_timestamp_ms)
    }

    /// Run an action plan, then inspect at most `max_summaries` frame summaries
    /// until one has `timestamp_ms > min_timestamp_ms`.
    pub fn run_actions_and_wait_for_frame_summary_after_timestamp_with_limit(
        &mut self,
        actions: &[AgentAction],
        min_timestamp_ms: u64,
        max_summaries: usize,
    ) -> Result<Option<FrameSummary>> {
        self.run_actions(actions)?;
        self.wait_for_frame_summary_after_timestamp_with_limit(min_timestamp_ms, max_summaries)
    }

    /// Run an action plan, then read at most `max_summaries` frame summaries
    /// until the next scene-change frame is observed.
    pub fn run_actions_and_wait_for_scene_change_with_limit(
        &mut self,
        actions: &[AgentAction],
        max_summaries: usize,
    ) -> Result<Option<FrameSummary>> {
        self.run_actions(actions)?;
        self.wait_for_scene_change_with_limit(max_summaries)
    }

    /// Run an action plan, then read at most `max_summaries` frame summaries
    /// until a frame with motion vectors is observed.
    pub fn run_actions_and_wait_for_motion_with_limit(
        &mut self,
        actions: &[AgentAction],
        max_summaries: usize,
    ) -> Result<Option<FrameSummary>> {
        self.run_actions(actions)?;
        self.wait_for_motion_with_limit(max_summaries)
    }

    /// Run an action plan, then read at most `max_summaries` frame summaries
    /// until one stable frame is observed.
    pub fn run_actions_and_wait_for_stable_frame_with_limit(
        &mut self,
        actions: &[AgentAction],
        max_summaries: usize,
    ) -> Result<Option<FrameSummary>> {
        self.run_actions_and_wait_for_stable_frames_with_limit(actions, 1, max_summaries)
    }

    /// Run an action plan, then read at most `max_summaries` frame summaries
    /// until `consecutive` stable frames are observed.
    pub fn run_actions_and_wait_for_stable_frames_with_limit(
        &mut self,
        actions: &[AgentAction],
        consecutive: usize,
        max_summaries: usize,
    ) -> Result<Option<FrameSummary>> {
        self.run_actions(actions)?;
        self.wait_for_stable_frames_with_limit(consecutive, max_summaries)
    }

    /// Read messages until the next clipboard payload is observed.
    ///
    /// Live callers should configure a read timeout on the underlying stream if
    /// they need a bounded wait.
    pub fn wait_for_clipboard(&mut self) -> Result<String> {
        loop {
            if let DeviceEvent::Native(DeviceMessage::Clipboard(text)) = self
                .recv_device_event()
                .map_err(|e| io_to_wait_error(e, "clipboard"))?
            {
                return Ok(text);
            }
        }
    }

    /// Request the current device clipboard and wait for the clipboard payload.
    pub fn get_clipboard_and_wait(&mut self, copy_key: u8) -> Result<String> {
        self.request_clipboard(copy_key)?;
        self.flush()?;
        self.wait_for_clipboard()
    }

    /// Request the current device clipboard with a typed copy-key and wait.
    pub fn get_clipboard_and_wait_key(&mut self, copy_key: ClipboardCopyKey) -> Result<String> {
        self.get_clipboard_and_wait(copy_key.value())
    }

    /// Run an action plan, then wait for the next clipboard payload.
    pub fn run_actions_and_wait_for_clipboard(
        &mut self,
        actions: &[AgentAction],
    ) -> Result<String> {
        self.run_actions(actions)?;
        self.wait_for_clipboard()
    }

    /// Run an action plan, request the current device clipboard, then wait for
    /// the clipboard payload.
    ///
    /// The action plan and request share one checked dispatcher barrier.
    pub fn run_actions_and_get_clipboard_and_wait(
        &mut self,
        actions: &[AgentAction],
        copy_key: u8,
    ) -> Result<String> {
        self.queue_actions(actions)?;
        self.request_clipboard(copy_key)?;
        self.flush()?;
        self.wait_for_clipboard()
    }

    /// Run an action plan, request the current device clipboard with a typed
    /// copy-key, then wait for the clipboard payload.
    pub fn run_actions_and_get_clipboard_and_wait_key(
        &mut self,
        actions: &[AgentAction],
        copy_key: ClipboardCopyKey,
    ) -> Result<String> {
        self.run_actions_and_get_clipboard_and_wait(actions, copy_key.value())
    }

    /// Read messages until the matching clipboard ACK sequence is observed.
    ///
    /// Live callers should configure a read timeout on the underlying stream if
    /// they need a bounded wait.
    pub fn wait_for_clipboard_ack(&mut self, sequence: u64) -> Result<()> {
        loop {
            match self
                .recv_device_event()
                .map_err(|e| io_to_wait_error(e, "clipboard ack"))?
            {
                DeviceEvent::Native(DeviceMessage::AckClipboard { sequence: got })
                    if got == sequence =>
                {
                    return Ok(())
                }
                _ => {}
            }
        }
    }

    /// Run an action plan, then wait for the matching clipboard ACK sequence.
    pub fn run_actions_and_wait_for_clipboard_ack(
        &mut self,
        actions: &[AgentAction],
        sequence: u64,
    ) -> Result<()> {
        self.run_actions(actions)?;
        self.wait_for_clipboard_ack(sequence)
    }

    /// Read events until the next AI frame summary is observed.
    ///
    /// Native scrcpy messages and unknown extension envelopes are skipped.
    pub fn wait_for_frame_summary(&mut self) -> Result<FrameSummary> {
        loop {
            if let DeviceEvent::FrameSummary(summary) = self
                .recv_device_event()
                .map_err(|e| io_to_wait_error(e, "frame summary"))?
            {
                return Ok(summary);
            }
        }
    }

    /// Read frame summaries until `predicate` accepts one.
    ///
    /// Native scrcpy messages, AI stats, and unknown extension envelopes are
    /// skipped by the underlying mixed-event reader.
    pub fn wait_for_frame_summary_matching(
        &mut self,
        mut predicate: impl FnMut(&FrameSummary) -> bool,
    ) -> Result<FrameSummary> {
        loop {
            let summary = self.wait_for_frame_summary()?;
            if predicate(&summary) {
                return Ok(summary);
            }
        }
    }

    /// Read at most `max_summaries` frame summaries until `predicate` accepts
    /// one.
    ///
    /// This bounds the number of AI frame summaries inspected, not wall-clock
    /// time. Native scrcpy messages, AI stats, and unknown extension envelopes
    /// are still skipped by the underlying mixed-event reader.
    pub fn wait_for_frame_summary_matching_with_limit(
        &mut self,
        max_summaries: usize,
        mut predicate: impl FnMut(&FrameSummary) -> bool,
    ) -> Result<Option<FrameSummary>> {
        for _ in 0..max_summaries {
            let summary = self.wait_for_frame_summary()?;
            if predicate(&summary) {
                return Ok(Some(summary));
            }
        }
        Ok(None)
    }

    /// Read frame summaries until one has `frame_seq > min_frame_seq`.
    ///
    /// This is useful after an agent already observed a frame and needs to skip
    /// stale summaries still buffered in the event stream.
    pub fn wait_for_frame_summary_after_seq(&mut self, min_frame_seq: u32) -> Result<FrameSummary> {
        self.wait_for_frame_summary_matching(|summary| summary.frame_seq > min_frame_seq)
    }

    /// Read at most `max_summaries` frame summaries until one has
    /// `frame_seq > min_frame_seq`.
    pub fn wait_for_frame_summary_after_seq_with_limit(
        &mut self,
        min_frame_seq: u32,
        max_summaries: usize,
    ) -> Result<Option<FrameSummary>> {
        self.wait_for_frame_summary_matching_with_limit(max_summaries, |summary| {
            summary.frame_seq > min_frame_seq
        })
    }

    /// Read frame summaries until one has `timestamp_ms > min_timestamp_ms`.
    pub fn wait_for_frame_summary_after_timestamp(
        &mut self,
        min_timestamp_ms: u64,
    ) -> Result<FrameSummary> {
        self.wait_for_frame_summary_matching(|summary| summary.timestamp_ms > min_timestamp_ms)
    }

    /// Read at most `max_summaries` frame summaries until one has
    /// `timestamp_ms > min_timestamp_ms`.
    pub fn wait_for_frame_summary_after_timestamp_with_limit(
        &mut self,
        min_timestamp_ms: u64,
        max_summaries: usize,
    ) -> Result<Option<FrameSummary>> {
        self.wait_for_frame_summary_matching_with_limit(max_summaries, |summary| {
            summary.timestamp_ms > min_timestamp_ms
        })
    }

    /// Read frame summaries until the next scene-change frame is observed.
    pub fn wait_for_scene_change(&mut self) -> Result<FrameSummary> {
        self.wait_for_frame_summary_matching(FrameSummary::is_scene_change)
    }

    /// Read at most `max_summaries` frame summaries until the next
    /// scene-change frame is observed.
    pub fn wait_for_scene_change_with_limit(
        &mut self,
        max_summaries: usize,
    ) -> Result<Option<FrameSummary>> {
        self.wait_for_frame_summary_matching_with_limit(
            max_summaries,
            FrameSummary::is_scene_change,
        )
    }

    /// Read frame summaries until the next frame with motion vectors is
    /// observed.
    pub fn wait_for_motion(&mut self) -> Result<FrameSummary> {
        self.wait_for_frame_summary_matching(FrameSummary::is_moving)
    }

    /// Read at most `max_summaries` frame summaries until the next frame with
    /// motion vectors is observed.
    pub fn wait_for_motion_with_limit(
        &mut self,
        max_summaries: usize,
    ) -> Result<Option<FrameSummary>> {
        self.wait_for_frame_summary_matching_with_limit(max_summaries, FrameSummary::is_moving)
    }

    /// Read frame summaries until the next stable frame is observed.
    ///
    /// A stable frame has no scene-change flag and no motion vectors.
    pub fn wait_for_stable_frame(&mut self) -> Result<FrameSummary> {
        self.wait_for_stable_frames(1)
    }

    /// Read at most `max_summaries` frame summaries until the next stable frame
    /// is observed.
    pub fn wait_for_stable_frame_with_limit(
        &mut self,
        max_summaries: usize,
    ) -> Result<Option<FrameSummary>> {
        self.wait_for_stable_frames_with_limit(1, max_summaries)
    }

    /// Read frame summaries until `consecutive` stable frames have been
    /// observed, then return the final stable frame in that run.
    pub fn wait_for_stable_frames(&mut self, consecutive: usize) -> Result<FrameSummary> {
        if consecutive == 0 {
            return Err(Error::SessionLifecycle(
                "stable frame count must be nonzero",
            ));
        }

        let mut stable_frames = 0usize;
        loop {
            let summary = self.wait_for_frame_summary()?;
            if frame_summary_is_stable(&summary) {
                stable_frames += 1;
                if stable_frames >= consecutive {
                    return Ok(summary);
                }
            } else {
                stable_frames = 0;
            }
        }
    }

    /// Read at most `max_summaries` frame summaries until `consecutive` stable
    /// frames have been observed, then return the final stable frame in that
    /// run.
    pub fn wait_for_stable_frames_with_limit(
        &mut self,
        consecutive: usize,
        max_summaries: usize,
    ) -> Result<Option<FrameSummary>> {
        if consecutive == 0 {
            return Err(Error::SessionLifecycle(
                "stable frame count must be nonzero",
            ));
        }

        let mut stable_frames = 0usize;
        for _ in 0..max_summaries {
            let summary = self.wait_for_frame_summary()?;
            if frame_summary_is_stable(&summary) {
                stable_frames += 1;
                if stable_frames >= consecutive {
                    return Ok(Some(summary));
                }
            } else {
                stable_frames = 0;
            }
        }
        Ok(None)
    }

    /// Read frame summaries until the indexed object detection is present.
    pub fn wait_for_object_rect(&mut self, index: usize) -> Result<AgentRect> {
        loop {
            let summary = self.wait_for_frame_summary()?;
            if let Some(rect) = AgentRect::try_from_frame_object(&summary, index)? {
                return Ok(rect);
            }
        }
    }

    /// Read at most `max_summaries` frame summaries until the indexed object
    /// detection is present.
    pub fn wait_for_object_rect_with_limit(
        &mut self,
        index: usize,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.wait_for_rect_with_limit(max_summaries, |summary| {
            AgentRect::try_from_frame_object(summary, index)
        })
    }

    /// Read frame summaries until any object detection is present, then return
    /// the highest-confidence target. Ties prefer the larger box.
    pub fn wait_for_best_object_rect(&mut self) -> Result<AgentRect> {
        self.wait_for_object_selector_rect(AgentObjectSelector::ANY)
    }

    /// Read at most `max_summaries` frame summaries until any object detection
    /// is present, then return the highest-confidence target. Ties prefer the
    /// larger box.
    pub fn wait_for_best_object_rect_with_limit(
        &mut self,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.wait_for_object_selector_rect_with_limit(AgentObjectSelector::ANY, max_summaries)
    }

    /// Read frame summaries until the requested object class is present, then
    /// return the highest-confidence target for that class.
    pub fn wait_for_best_object_class_rect(&mut self, class_id: u8) -> Result<AgentRect> {
        self.wait_for_object_selector_rect(AgentObjectSelector::class_id(class_id))
    }

    /// Read at most `max_summaries` frame summaries until the requested object
    /// class is present, then return the highest-confidence target for that
    /// class.
    pub fn wait_for_best_object_class_rect_with_limit(
        &mut self,
        class_id: u8,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.wait_for_object_selector_rect_with_limit(
            AgentObjectSelector::class_id(class_id),
            max_summaries,
        )
    }

    /// Read frame summaries until an object matching `selector` is present,
    /// then return the highest-confidence matching target. Ties prefer the
    /// larger box.
    pub fn wait_for_object_selector_rect(
        &mut self,
        selector: AgentObjectSelector,
    ) -> Result<AgentRect> {
        loop {
            let summary = self.wait_for_frame_summary()?;
            if let Some(rect) = AgentRect::try_from_best_object_matching(&summary, selector)? {
                return Ok(rect);
            }
        }
    }

    /// Read at most `max_summaries` frame summaries until an object matching
    /// `selector` is present, then return the highest-confidence matching
    /// target. Ties prefer the larger box.
    pub fn wait_for_object_selector_rect_with_limit(
        &mut self,
        selector: AgentObjectSelector,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.wait_for_rect_with_limit(max_summaries, |summary| {
            AgentRect::try_from_best_object_matching(summary, selector)
        })
    }

    /// Read frame summaries until the indexed text region is present.
    pub fn wait_for_text_region_rect(&mut self, index: usize) -> Result<AgentRect> {
        loop {
            let summary = self.wait_for_frame_summary()?;
            if let Some(rect) = AgentRect::try_from_frame_text_region(&summary, index)? {
                return Ok(rect);
            }
        }
    }

    /// Read at most `max_summaries` frame summaries until the indexed text
    /// region is present.
    pub fn wait_for_text_region_rect_with_limit(
        &mut self,
        index: usize,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.wait_for_rect_with_limit(max_summaries, |summary| {
            AgentRect::try_from_frame_text_region(summary, index)
        })
    }

    /// Read frame summaries until at least one text region is present, then
    /// return the largest target.
    pub fn wait_for_largest_text_region_rect(&mut self) -> Result<AgentRect> {
        loop {
            let summary = self.wait_for_frame_summary()?;
            if let Some(rect) = AgentRect::try_from_largest_text_region(&summary)? {
                return Ok(rect);
            }
        }
    }

    /// Read at most `max_summaries` frame summaries until at least one text
    /// region is present, then return the largest target.
    pub fn wait_for_largest_text_region_rect_with_limit(
        &mut self,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.wait_for_rect_with_limit(max_summaries, AgentRect::try_from_largest_text_region)
    }

    /// Read frame summaries until any supported object/text target selected by
    /// `target` is present.
    pub fn wait_for_target_rect(&mut self, target: AgentTargetSelector) -> Result<AgentRect> {
        loop {
            let summary = self.wait_for_frame_summary()?;
            if let Some(rect) = target.select_rect(&summary)? {
                return Ok(rect);
            }
        }
    }

    /// Read at most `max_summaries` frame summaries until any supported
    /// object/text target selected by `target` is present.
    pub fn wait_for_target_rect_with_limit(
        &mut self,
        target: AgentTargetSelector,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.wait_for_rect_with_limit(max_summaries, |summary| target.select_rect(summary))
    }

    fn wait_for_rect_with_limit(
        &mut self,
        max_summaries: usize,
        mut select: impl FnMut(&FrameSummary) -> Result<Option<AgentRect>>,
    ) -> Result<Option<AgentRect>> {
        for _ in 0..max_summaries {
            let summary = self.wait_for_frame_summary()?;
            if let Some(rect) = select(&summary)? {
                return Ok(Some(rect));
            }
        }
        Ok(None)
    }

    /// Select the indexed object target from a newest-only frame snapshot.
    pub fn latest_object_rect(
        &self,
        snapshot: &LatestFrameSummarySnapshot,
        index: usize,
    ) -> Result<Option<AgentRect>> {
        AgentRect::try_from_frame_object(&snapshot.summary, index)
    }

    /// Select the highest-confidence object target from a newest-only frame
    /// snapshot.
    pub fn latest_best_object_rect(
        &self,
        snapshot: &LatestFrameSummarySnapshot,
    ) -> Result<Option<AgentRect>> {
        AgentRect::try_from_best_object(&snapshot.summary)
    }

    /// Select the highest-confidence object target matching `selector` from a
    /// newest-only frame snapshot.
    pub fn latest_object_selector_rect(
        &self,
        snapshot: &LatestFrameSummarySnapshot,
        selector: AgentObjectSelector,
    ) -> Result<Option<AgentRect>> {
        AgentRect::try_from_best_object_matching(&snapshot.summary, selector)
    }

    /// Select the indexed text target from a newest-only frame snapshot.
    pub fn latest_text_region_rect(
        &self,
        snapshot: &LatestFrameSummarySnapshot,
        index: usize,
    ) -> Result<Option<AgentRect>> {
        AgentRect::try_from_frame_text_region(&snapshot.summary, index)
    }

    /// Select the largest text target from a newest-only frame snapshot.
    pub fn latest_largest_text_region_rect(
        &self,
        snapshot: &LatestFrameSummarySnapshot,
    ) -> Result<Option<AgentRect>> {
        AgentRect::try_from_largest_text_region(&snapshot.summary)
    }

    /// Select any supported object/text target from a newest-only frame
    /// snapshot.
    pub fn latest_target_rect(
        &self,
        snapshot: &LatestFrameSummarySnapshot,
        target: AgentTargetSelector,
    ) -> Result<Option<AgentRect>> {
        target.select_rect(&snapshot.summary)
    }

    /// Tap the center of any supported object/text target from a newest-only
    /// frame snapshot, if present.
    pub fn tap_latest_target(
        &self,
        snapshot: &LatestFrameSummarySnapshot,
        target: AgentTargetSelector,
    ) -> Result<Option<AgentRect>> {
        let rect = self.latest_target_rect(snapshot, target)?;
        self.tap_latest_optional_rect(rect)
    }

    /// Tap a relative point inside any supported object/text target from a
    /// newest-only frame snapshot, if present.
    pub fn tap_latest_target_at(
        &self,
        snapshot: &LatestFrameSummarySnapshot,
        target: AgentTargetSelector,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<Option<AgentRect>> {
        let rect = self.latest_target_rect(snapshot, target)?;
        self.tap_latest_optional_rect_at(rect, x_bp, y_bp)
    }

    /// Tap the center of any supported object/text target from a newest-only
    /// frame snapshot with a typed scrcpy pointer id, if present.
    pub fn tap_latest_target_pointer(
        &self,
        snapshot: &LatestFrameSummarySnapshot,
        target: AgentTargetSelector,
        pointer_id: TouchPointerId,
    ) -> Result<Option<AgentRect>> {
        let rect = self.latest_target_rect(snapshot, target)?;
        self.tap_latest_optional_rect_pointer(rect, pointer_id)
    }

    /// Tap a relative point inside any supported object/text target from a
    /// newest-only frame snapshot with a typed scrcpy pointer id, if present.
    pub fn tap_latest_target_at_pointer(
        &self,
        snapshot: &LatestFrameSummarySnapshot,
        target: AgentTargetSelector,
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<Option<AgentRect>> {
        let rect = self.latest_target_rect(snapshot, target)?;
        self.tap_latest_optional_rect_at_pointer(rect, pointer_id, x_bp, y_bp)
    }

    /// Tap the center of the highest-confidence object matching `selector`
    /// from a newest-only frame snapshot, if present.
    pub fn tap_latest_object_selector(
        &self,
        snapshot: &LatestFrameSummarySnapshot,
        selector: AgentObjectSelector,
    ) -> Result<Option<AgentRect>> {
        let rect = self.latest_object_selector_rect(snapshot, selector)?;
        self.tap_latest_optional_rect(rect)
    }

    /// Tap a relative point inside the highest-confidence object matching
    /// `selector` from a newest-only frame snapshot, if present.
    pub fn tap_latest_object_selector_at(
        &self,
        snapshot: &LatestFrameSummarySnapshot,
        selector: AgentObjectSelector,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<Option<AgentRect>> {
        let rect = self.latest_object_selector_rect(snapshot, selector)?;
        self.tap_latest_optional_rect_at(rect, x_bp, y_bp)
    }

    /// Tap a relative point inside the highest-confidence object matching
    /// `selector` from a newest-only frame snapshot with a typed scrcpy pointer
    /// id, if present.
    pub fn tap_latest_object_selector_at_pointer(
        &self,
        snapshot: &LatestFrameSummarySnapshot,
        selector: AgentObjectSelector,
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<Option<AgentRect>> {
        let rect = self.latest_object_selector_rect(snapshot, selector)?;
        self.tap_latest_optional_rect_at_pointer(rect, pointer_id, x_bp, y_bp)
    }

    /// Tap the center of the largest text region from a newest-only frame
    /// snapshot, if present.
    pub fn tap_latest_largest_text_region(
        &self,
        snapshot: &LatestFrameSummarySnapshot,
    ) -> Result<Option<AgentRect>> {
        let rect = self.latest_largest_text_region_rect(snapshot)?;
        self.tap_latest_optional_rect(rect)
    }

    /// Tap a relative point inside the largest text region from a newest-only
    /// frame snapshot, if present.
    pub fn tap_latest_largest_text_region_at(
        &self,
        snapshot: &LatestFrameSummarySnapshot,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<Option<AgentRect>> {
        let rect = self.latest_largest_text_region_rect(snapshot)?;
        self.tap_latest_optional_rect_at(rect, x_bp, y_bp)
    }

    /// Tap a relative point inside the largest text region from a newest-only
    /// frame snapshot with a typed scrcpy pointer id, if present.
    pub fn tap_latest_largest_text_region_at_pointer(
        &self,
        snapshot: &LatestFrameSummarySnapshot,
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<Option<AgentRect>> {
        let rect = self.latest_largest_text_region_rect(snapshot)?;
        self.tap_latest_optional_rect_at_pointer(rect, pointer_id, x_bp, y_bp)
    }

    /// Select any supported object/text target from a one-read latest-frame
    /// observation. Returns `None` if the observation has no snapshot or the
    /// target is absent.
    pub fn latest_observation_target_rect(
        &self,
        observation: &LatestFrameSummaryObservation,
        target: AgentTargetSelector,
    ) -> Result<Option<AgentRect>> {
        let Some(snapshot) = observation.snapshot() else {
            return Ok(None);
        };
        self.latest_target_rect(snapshot, target)
    }

    /// Tap the center of any supported object/text target from a one-read
    /// latest-frame observation, if present.
    pub fn tap_latest_observation_target(
        &self,
        observation: &LatestFrameSummaryObservation,
        target: AgentTargetSelector,
    ) -> Result<Option<AgentRect>> {
        let rect = self.latest_observation_target_rect(observation, target)?;
        self.tap_latest_optional_rect(rect)
    }

    /// Tap a relative point inside any supported object/text target from a
    /// one-read latest-frame observation, if present.
    pub fn tap_latest_observation_target_at(
        &self,
        observation: &LatestFrameSummaryObservation,
        target: AgentTargetSelector,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<Option<AgentRect>> {
        let rect = self.latest_observation_target_rect(observation, target)?;
        self.tap_latest_optional_rect_at(rect, x_bp, y_bp)
    }

    /// Tap the center of any supported object/text target from a one-read
    /// latest-frame observation with a typed scrcpy pointer id, if present.
    pub fn tap_latest_observation_target_pointer(
        &self,
        observation: &LatestFrameSummaryObservation,
        target: AgentTargetSelector,
        pointer_id: TouchPointerId,
    ) -> Result<Option<AgentRect>> {
        let rect = self.latest_observation_target_rect(observation, target)?;
        self.tap_latest_optional_rect_pointer(rect, pointer_id)
    }

    /// Tap a relative point inside any supported object/text target from a
    /// one-read latest-frame observation with a typed scrcpy pointer id, if
    /// present.
    pub fn tap_latest_observation_target_at_pointer(
        &self,
        observation: &LatestFrameSummaryObservation,
        target: AgentTargetSelector,
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<Option<AgentRect>> {
        let rect = self.latest_observation_target_rect(observation, target)?;
        self.tap_latest_optional_rect_at_pointer(rect, pointer_id, x_bp, y_bp)
    }

    fn tap_latest_optional_rect(&self, rect: Option<AgentRect>) -> Result<Option<AgentRect>> {
        let Some(rect) = rect else {
            return Ok(None);
        };
        self.tap_rect(rect)?;
        Ok(Some(rect))
    }

    fn tap_latest_optional_rect_at(
        &self,
        rect: Option<AgentRect>,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<Option<AgentRect>> {
        let Some(rect) = rect else {
            return Ok(None);
        };
        self.tap_rect_at(rect, x_bp, y_bp)?;
        Ok(Some(rect))
    }

    fn tap_latest_optional_rect_pointer(
        &self,
        rect: Option<AgentRect>,
        pointer_id: TouchPointerId,
    ) -> Result<Option<AgentRect>> {
        let Some(rect) = rect else {
            return Ok(None);
        };
        self.tap_rect_pointer(pointer_id, rect)?;
        Ok(Some(rect))
    }

    fn tap_latest_optional_rect_at_pointer(
        &self,
        rect: Option<AgentRect>,
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<Option<AgentRect>> {
        let Some(rect) = rect else {
            return Ok(None);
        };
        self.tap_rect_at_pointer(pointer_id, rect, x_bp, y_bp)?;
        Ok(Some(rect))
    }

    fn tap_optional_rect(&mut self, rect: Option<AgentRect>) -> Result<Option<AgentRect>> {
        let Some(rect) = rect else {
            return Ok(None);
        };
        self.tap_rect(rect)?;
        Ok(Some(rect))
    }

    fn tap_optional_rect_pointer(
        &mut self,
        rect: Option<AgentRect>,
        pointer_id: TouchPointerId,
    ) -> Result<Option<AgentRect>> {
        let Some(rect) = rect else {
            return Ok(None);
        };
        self.tap_rect_pointer(pointer_id, rect)?;
        Ok(Some(rect))
    }

    fn tap_optional_rect_at(
        &mut self,
        rect: Option<AgentRect>,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<Option<AgentRect>> {
        let Some(rect) = rect else {
            return Ok(None);
        };
        self.tap_rect_at(rect, x_bp, y_bp)?;
        Ok(Some(rect))
    }

    fn tap_optional_rect_at_pointer(
        &mut self,
        rect: Option<AgentRect>,
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<Option<AgentRect>> {
        let Some(rect) = rect else {
            return Ok(None);
        };
        self.tap_rect_at_pointer(pointer_id, rect, x_bp, y_bp)?;
        Ok(Some(rect))
    }

    /// Wait for any supported object/text target selected by `target`, tap its
    /// center, and return it.
    pub fn tap_next_target(&mut self, target: AgentTargetSelector) -> Result<AgentRect> {
        let rect = self.wait_for_target_rect(target)?;
        self.tap_rect(rect)?;
        Ok(rect)
    }

    /// Wait for any supported object/text target selected by `target`, tap its
    /// center with a typed scrcpy pointer id, and return it.
    pub fn tap_next_target_pointer(
        &mut self,
        target: AgentTargetSelector,
        pointer_id: TouchPointerId,
    ) -> Result<AgentRect> {
        let rect = self.wait_for_target_rect(target)?;
        self.tap_rect_pointer(pointer_id, rect)?;
        Ok(rect)
    }

    /// Wait for any supported object/text target selected by `target`, tap a
    /// relative point inside it, and return it.
    pub fn tap_next_target_at(
        &mut self,
        target: AgentTargetSelector,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<AgentRect> {
        let rect = self.wait_for_target_rect(target)?;
        self.tap_rect_at(rect, x_bp, y_bp)?;
        Ok(rect)
    }

    /// Wait for any supported object/text target selected by `target`, tap a
    /// relative point inside it with a typed scrcpy pointer id, and return it.
    pub fn tap_next_target_at_pointer(
        &mut self,
        target: AgentTargetSelector,
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<AgentRect> {
        let rect = self.wait_for_target_rect(target)?;
        self.tap_rect_at_pointer(pointer_id, rect, x_bp, y_bp)?;
        Ok(rect)
    }

    /// Wait for any supported object/text target selected by `target` within
    /// `max_summaries` frame summaries, tap its center if found, and return it.
    pub fn tap_next_target_with_limit(
        &mut self,
        target: AgentTargetSelector,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        let rect = self.wait_for_target_rect_with_limit(target, max_summaries)?;
        self.tap_optional_rect(rect)
    }

    /// Wait for any supported object/text target selected by `target` within
    /// `max_summaries` frame summaries, tap its center with a typed scrcpy
    /// pointer id if found, and return it.
    pub fn tap_next_target_pointer_with_limit(
        &mut self,
        target: AgentTargetSelector,
        pointer_id: TouchPointerId,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        let rect = self.wait_for_target_rect_with_limit(target, max_summaries)?;
        self.tap_optional_rect_pointer(rect, pointer_id)
    }

    /// Wait for any supported object/text target selected by `target` within
    /// `max_summaries` frame summaries, tap a relative point inside it if
    /// found, and return it.
    pub fn tap_next_target_at_with_limit(
        &mut self,
        target: AgentTargetSelector,
        x_bp: u16,
        y_bp: u16,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        let rect = self.wait_for_target_rect_with_limit(target, max_summaries)?;
        self.tap_optional_rect_at(rect, x_bp, y_bp)
    }

    /// Wait for any supported object/text target selected by `target` within
    /// `max_summaries` frame summaries, tap a relative point inside it with a
    /// typed scrcpy pointer id if found, and return it.
    pub fn tap_next_target_at_pointer_with_limit(
        &mut self,
        target: AgentTargetSelector,
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        let rect = self.wait_for_target_rect_with_limit(target, max_summaries)?;
        self.tap_optional_rect_at_pointer(rect, pointer_id, x_bp, y_bp)
    }

    /// Run an action plan, then wait for any supported object/text target
    /// selected by `target`.
    pub fn run_actions_and_wait_for_target_rect(
        &mut self,
        actions: &[AgentAction],
        target: AgentTargetSelector,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.wait_for_target_rect(target)
    }

    /// Run an action plan, then inspect at most `max_summaries` frame summaries
    /// for any supported object/text target selected by `target`.
    pub fn run_actions_and_wait_for_target_rect_with_limit(
        &mut self,
        actions: &[AgentAction],
        target: AgentTargetSelector,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.run_actions(actions)?;
        self.wait_for_target_rect_with_limit(target, max_summaries)
    }

    /// Run an action plan, then wait for the indexed object detection.
    pub fn run_actions_and_wait_for_object_rect(
        &mut self,
        actions: &[AgentAction],
        index: usize,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.wait_for_object_rect(index)
    }

    /// Run an action plan, then wait for the highest-confidence object target.
    pub fn run_actions_and_wait_for_best_object_rect(
        &mut self,
        actions: &[AgentAction],
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.wait_for_best_object_rect()
    }

    /// Run an action plan, then wait for the highest-confidence target for
    /// `class_id`.
    pub fn run_actions_and_wait_for_best_object_class_rect(
        &mut self,
        actions: &[AgentAction],
        class_id: u8,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.wait_for_best_object_class_rect(class_id)
    }

    /// Run an action plan, then wait for the highest-confidence object target
    /// matching `selector`.
    pub fn run_actions_and_wait_for_object_selector_rect(
        &mut self,
        actions: &[AgentAction],
        selector: AgentObjectSelector,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.wait_for_object_selector_rect(selector)
    }

    /// Run an action plan, then wait for the indexed text region.
    pub fn run_actions_and_wait_for_text_region_rect(
        &mut self,
        actions: &[AgentAction],
        index: usize,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.wait_for_text_region_rect(index)
    }

    /// Run an action plan, then wait for the largest text region.
    pub fn run_actions_and_wait_for_largest_text_region_rect(
        &mut self,
        actions: &[AgentAction],
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.wait_for_largest_text_region_rect()
    }

    /// Run an action plan, then inspect at most `max_summaries` frame summaries
    /// for the indexed object detection.
    pub fn run_actions_and_wait_for_object_rect_with_limit(
        &mut self,
        actions: &[AgentAction],
        index: usize,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.run_actions(actions)?;
        self.wait_for_object_rect_with_limit(index, max_summaries)
    }

    /// Run an action plan, then inspect at most `max_summaries` frame summaries
    /// for the highest-confidence object target.
    pub fn run_actions_and_wait_for_best_object_rect_with_limit(
        &mut self,
        actions: &[AgentAction],
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.run_actions(actions)?;
        self.wait_for_best_object_rect_with_limit(max_summaries)
    }

    /// Run an action plan, then inspect at most `max_summaries` frame summaries
    /// for the highest-confidence target for `class_id`.
    pub fn run_actions_and_wait_for_best_object_class_rect_with_limit(
        &mut self,
        actions: &[AgentAction],
        class_id: u8,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.run_actions(actions)?;
        self.wait_for_best_object_class_rect_with_limit(class_id, max_summaries)
    }

    /// Run an action plan, then inspect at most `max_summaries` frame summaries
    /// for the highest-confidence object target matching `selector`.
    pub fn run_actions_and_wait_for_object_selector_rect_with_limit(
        &mut self,
        actions: &[AgentAction],
        selector: AgentObjectSelector,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.run_actions(actions)?;
        self.wait_for_object_selector_rect_with_limit(selector, max_summaries)
    }

    /// Run an action plan, then inspect at most `max_summaries` frame summaries
    /// for the indexed text region.
    pub fn run_actions_and_wait_for_text_region_rect_with_limit(
        &mut self,
        actions: &[AgentAction],
        index: usize,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.run_actions(actions)?;
        self.wait_for_text_region_rect_with_limit(index, max_summaries)
    }

    /// Run an action plan, then inspect at most `max_summaries` frame summaries
    /// for the largest text region.
    pub fn run_actions_and_wait_for_largest_text_region_rect_with_limit(
        &mut self,
        actions: &[AgentAction],
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.run_actions(actions)?;
        self.wait_for_largest_text_region_rect_with_limit(max_summaries)
    }

    /// Run an action plan, then wait for any supported object/text target
    /// selected by `target` and tap its center.
    pub fn run_actions_and_tap_next_target(
        &mut self,
        actions: &[AgentAction],
        target: AgentTargetSelector,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_target(target)
    }

    /// Run an action plan, then wait for any supported object/text target
    /// selected by `target` and tap its center with a typed scrcpy pointer id.
    pub fn run_actions_and_tap_next_target_pointer(
        &mut self,
        actions: &[AgentAction],
        target: AgentTargetSelector,
        pointer_id: TouchPointerId,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_target_pointer(target, pointer_id)
    }

    /// Run an action plan, then wait for any supported object/text target
    /// selected by `target` and tap a relative point inside it.
    pub fn run_actions_and_tap_next_target_at(
        &mut self,
        actions: &[AgentAction],
        target: AgentTargetSelector,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_target_at(target, x_bp, y_bp)
    }

    /// Run an action plan, then wait for any supported object/text target
    /// selected by `target` and tap a relative point inside it with a typed
    /// scrcpy pointer id.
    pub fn run_actions_and_tap_next_target_at_pointer(
        &mut self,
        actions: &[AgentAction],
        target: AgentTargetSelector,
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_target_at_pointer(target, pointer_id, x_bp, y_bp)
    }

    /// Run an action plan, then wait for any supported object/text target
    /// selected by `target` within `max_summaries` frame summaries and tap its
    /// center if found.
    pub fn run_actions_and_tap_next_target_with_limit(
        &mut self,
        actions: &[AgentAction],
        target: AgentTargetSelector,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.run_actions(actions)?;
        self.tap_next_target_with_limit(target, max_summaries)
    }

    /// Run an action plan, then wait for any supported object/text target
    /// selected by `target` within `max_summaries` frame summaries and tap its
    /// center with a typed scrcpy pointer id if found.
    pub fn run_actions_and_tap_next_target_pointer_with_limit(
        &mut self,
        actions: &[AgentAction],
        target: AgentTargetSelector,
        pointer_id: TouchPointerId,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.run_actions(actions)?;
        self.tap_next_target_pointer_with_limit(target, pointer_id, max_summaries)
    }

    /// Run an action plan, then wait for any supported object/text target
    /// selected by `target` within `max_summaries` frame summaries and tap a
    /// relative point inside it if found.
    pub fn run_actions_and_tap_next_target_at_with_limit(
        &mut self,
        actions: &[AgentAction],
        target: AgentTargetSelector,
        x_bp: u16,
        y_bp: u16,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.run_actions(actions)?;
        self.tap_next_target_at_with_limit(target, x_bp, y_bp, max_summaries)
    }

    /// Run an action plan, then wait for any supported object/text target
    /// selected by `target` within `max_summaries` frame summaries and tap a
    /// relative point inside it with a typed scrcpy pointer id if found.
    pub fn run_actions_and_tap_next_target_at_pointer_with_limit(
        &mut self,
        actions: &[AgentAction],
        target: AgentTargetSelector,
        pointer_id: TouchPointerId,
        anchor_bp: (u16, u16),
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.run_actions(actions)?;
        self.tap_next_target_at_pointer_with_limit(
            target,
            pointer_id,
            anchor_bp.0,
            anchor_bp.1,
            max_summaries,
        )
    }

    /// Wait for the indexed object within `max_summaries` frame summaries, tap
    /// its center if found, and return the selected rectangle.
    pub fn tap_next_object_with_limit(
        &mut self,
        index: usize,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        let rect = self.wait_for_object_rect_with_limit(index, max_summaries)?;
        self.tap_optional_rect(rect)
    }

    /// Wait for the indexed object within `max_summaries` frame summaries, tap
    /// its center with a typed scrcpy pointer id if found, and return the
    /// selected rectangle.
    pub fn tap_next_object_pointer_with_limit(
        &mut self,
        index: usize,
        pointer_id: TouchPointerId,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        let rect = self.wait_for_object_rect_with_limit(index, max_summaries)?;
        self.tap_optional_rect_pointer(rect, pointer_id)
    }

    /// Wait for the indexed object within `max_summaries` frame summaries, tap a
    /// relative point inside it if found, and return the selected rectangle.
    pub fn tap_next_object_at_with_limit(
        &mut self,
        index: usize,
        x_bp: u16,
        y_bp: u16,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        let rect = self.wait_for_object_rect_with_limit(index, max_summaries)?;
        self.tap_optional_rect_at(rect, x_bp, y_bp)
    }

    /// Wait for the indexed object within `max_summaries` frame summaries, tap a
    /// relative point inside it with a typed scrcpy pointer id if found, and
    /// return the selected rectangle.
    pub fn tap_next_object_at_pointer_with_limit(
        &mut self,
        index: usize,
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        let rect = self.wait_for_object_rect_with_limit(index, max_summaries)?;
        self.tap_optional_rect_at_pointer(rect, pointer_id, x_bp, y_bp)
    }

    /// Wait for the best object within `max_summaries` frame summaries, tap its
    /// center if found, and return the selected rectangle.
    pub fn tap_next_best_object_with_limit(
        &mut self,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        let rect = self.wait_for_best_object_rect_with_limit(max_summaries)?;
        self.tap_optional_rect(rect)
    }

    /// Wait for the best object within `max_summaries` frame summaries, tap its
    /// center with a typed scrcpy pointer id if found, and return the selected
    /// rectangle.
    pub fn tap_next_best_object_pointer_with_limit(
        &mut self,
        pointer_id: TouchPointerId,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        let rect = self.wait_for_best_object_rect_with_limit(max_summaries)?;
        self.tap_optional_rect_pointer(rect, pointer_id)
    }

    /// Wait for the best object within `max_summaries` frame summaries, tap a
    /// relative point inside it if found, and return the selected rectangle.
    pub fn tap_next_best_object_at_with_limit(
        &mut self,
        x_bp: u16,
        y_bp: u16,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        let rect = self.wait_for_best_object_rect_with_limit(max_summaries)?;
        self.tap_optional_rect_at(rect, x_bp, y_bp)
    }

    /// Wait for the best object within `max_summaries` frame summaries, tap a
    /// relative point inside it with a typed scrcpy pointer id if found, and
    /// return the selected rectangle.
    pub fn tap_next_best_object_at_pointer_with_limit(
        &mut self,
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        let rect = self.wait_for_best_object_rect_with_limit(max_summaries)?;
        self.tap_optional_rect_at_pointer(rect, pointer_id, x_bp, y_bp)
    }

    /// Wait for the best object of `class_id` within `max_summaries` frame
    /// summaries, tap its center if found, and return the selected rectangle.
    pub fn tap_next_object_class_with_limit(
        &mut self,
        class_id: u8,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        let rect = self.wait_for_best_object_class_rect_with_limit(class_id, max_summaries)?;
        self.tap_optional_rect(rect)
    }

    /// Wait for the best object of `class_id` within `max_summaries` frame
    /// summaries, tap its center with a typed scrcpy pointer id if found, and
    /// return the selected rectangle.
    pub fn tap_next_object_class_pointer_with_limit(
        &mut self,
        class_id: u8,
        pointer_id: TouchPointerId,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        let rect = self.wait_for_best_object_class_rect_with_limit(class_id, max_summaries)?;
        self.tap_optional_rect_pointer(rect, pointer_id)
    }

    /// Wait for the best object of `class_id` within `max_summaries` frame
    /// summaries, tap a relative point inside it if found, and return the
    /// selected rectangle.
    pub fn tap_next_object_class_at_with_limit(
        &mut self,
        class_id: u8,
        x_bp: u16,
        y_bp: u16,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        let rect = self.wait_for_best_object_class_rect_with_limit(class_id, max_summaries)?;
        self.tap_optional_rect_at(rect, x_bp, y_bp)
    }

    /// Wait for the best object of `class_id` within `max_summaries` frame
    /// summaries, tap a relative point inside it with a typed scrcpy pointer id
    /// if found, and return the selected rectangle.
    pub fn tap_next_object_class_at_pointer_with_limit(
        &mut self,
        class_id: u8,
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        let rect = self.wait_for_best_object_class_rect_with_limit(class_id, max_summaries)?;
        self.tap_optional_rect_at_pointer(rect, pointer_id, x_bp, y_bp)
    }

    /// Wait for an object matching `selector` within `max_summaries` frame
    /// summaries, tap its center if found, and return the selected rectangle.
    pub fn tap_next_object_selector_with_limit(
        &mut self,
        selector: AgentObjectSelector,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        let rect = self.wait_for_object_selector_rect_with_limit(selector, max_summaries)?;
        self.tap_optional_rect(rect)
    }

    /// Wait for an object matching `selector` within `max_summaries` frame
    /// summaries, tap its center with a typed scrcpy pointer id if found, and
    /// return the selected rectangle.
    pub fn tap_next_object_selector_pointer_with_limit(
        &mut self,
        selector: AgentObjectSelector,
        pointer_id: TouchPointerId,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        let rect = self.wait_for_object_selector_rect_with_limit(selector, max_summaries)?;
        self.tap_optional_rect_pointer(rect, pointer_id)
    }

    /// Wait for an object matching `selector` within `max_summaries` frame
    /// summaries, tap a relative point inside it if found, and return the
    /// selected rectangle.
    pub fn tap_next_object_selector_at_with_limit(
        &mut self,
        selector: AgentObjectSelector,
        x_bp: u16,
        y_bp: u16,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        let rect = self.wait_for_object_selector_rect_with_limit(selector, max_summaries)?;
        self.tap_optional_rect_at(rect, x_bp, y_bp)
    }

    /// Wait for an object matching `selector` within `max_summaries` frame
    /// summaries, tap a relative point inside it with a typed scrcpy pointer id
    /// if found, and return the selected rectangle.
    pub fn tap_next_object_selector_at_pointer_with_limit(
        &mut self,
        selector: AgentObjectSelector,
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        let rect = self.wait_for_object_selector_rect_with_limit(selector, max_summaries)?;
        self.tap_optional_rect_at_pointer(rect, pointer_id, x_bp, y_bp)
    }

    /// Wait for the indexed text region within `max_summaries` frame summaries,
    /// tap its center if found, and return the selected rectangle.
    pub fn tap_next_text_region_with_limit(
        &mut self,
        index: usize,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        let rect = self.wait_for_text_region_rect_with_limit(index, max_summaries)?;
        self.tap_optional_rect(rect)
    }

    /// Wait for the indexed text region within `max_summaries` frame summaries,
    /// tap its center with a typed scrcpy pointer id if found, and return the
    /// selected rectangle.
    pub fn tap_next_text_region_pointer_with_limit(
        &mut self,
        index: usize,
        pointer_id: TouchPointerId,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        let rect = self.wait_for_text_region_rect_with_limit(index, max_summaries)?;
        self.tap_optional_rect_pointer(rect, pointer_id)
    }

    /// Wait for the indexed text region within `max_summaries` frame summaries,
    /// tap a relative point inside it if found, and return the selected
    /// rectangle.
    pub fn tap_next_text_region_at_with_limit(
        &mut self,
        index: usize,
        x_bp: u16,
        y_bp: u16,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        let rect = self.wait_for_text_region_rect_with_limit(index, max_summaries)?;
        self.tap_optional_rect_at(rect, x_bp, y_bp)
    }

    /// Wait for the indexed text region within `max_summaries` frame summaries,
    /// tap a relative point inside it with a typed scrcpy pointer id if found,
    /// and return the selected rectangle.
    pub fn tap_next_text_region_at_pointer_with_limit(
        &mut self,
        index: usize,
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        let rect = self.wait_for_text_region_rect_with_limit(index, max_summaries)?;
        self.tap_optional_rect_at_pointer(rect, pointer_id, x_bp, y_bp)
    }

    /// Wait for the largest text region within `max_summaries` frame summaries,
    /// tap its center if found, and return the selected rectangle.
    pub fn tap_next_largest_text_region_with_limit(
        &mut self,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        let rect = self.wait_for_largest_text_region_rect_with_limit(max_summaries)?;
        self.tap_optional_rect(rect)
    }

    /// Wait for the largest text region within `max_summaries` frame summaries,
    /// tap its center with a typed scrcpy pointer id if found, and return the
    /// selected rectangle.
    pub fn tap_next_largest_text_region_pointer_with_limit(
        &mut self,
        pointer_id: TouchPointerId,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        let rect = self.wait_for_largest_text_region_rect_with_limit(max_summaries)?;
        self.tap_optional_rect_pointer(rect, pointer_id)
    }

    /// Wait for the largest text region within `max_summaries` frame summaries,
    /// tap a relative point inside it if found, and return the selected
    /// rectangle.
    pub fn tap_next_largest_text_region_at_with_limit(
        &mut self,
        x_bp: u16,
        y_bp: u16,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        let rect = self.wait_for_largest_text_region_rect_with_limit(max_summaries)?;
        self.tap_optional_rect_at(rect, x_bp, y_bp)
    }

    /// Wait for the largest text region within `max_summaries` frame summaries,
    /// tap a relative point inside it with a typed scrcpy pointer id if found,
    /// and return the selected rectangle.
    pub fn tap_next_largest_text_region_at_pointer_with_limit(
        &mut self,
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        let rect = self.wait_for_largest_text_region_rect_with_limit(max_summaries)?;
        self.tap_optional_rect_at_pointer(rect, pointer_id, x_bp, y_bp)
    }

    /// Run an action plan, then wait for the indexed object within
    /// `max_summaries` frame summaries and tap its center if found.
    pub fn run_actions_and_tap_next_object_with_limit(
        &mut self,
        actions: &[AgentAction],
        index: usize,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.run_actions(actions)?;
        self.tap_next_object_with_limit(index, max_summaries)
    }

    /// Run an action plan, then wait for the indexed object within
    /// `max_summaries` frame summaries and tap its center with a typed scrcpy
    /// pointer id if found.
    pub fn run_actions_and_tap_next_object_pointer_with_limit(
        &mut self,
        actions: &[AgentAction],
        index: usize,
        pointer_id: TouchPointerId,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.run_actions(actions)?;
        self.tap_next_object_pointer_with_limit(index, pointer_id, max_summaries)
    }

    /// Run an action plan, then wait for the indexed object within
    /// `max_summaries` frame summaries and tap a relative point inside it if
    /// found.
    pub fn run_actions_and_tap_next_object_at_with_limit(
        &mut self,
        actions: &[AgentAction],
        index: usize,
        x_bp: u16,
        y_bp: u16,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.run_actions(actions)?;
        self.tap_next_object_at_with_limit(index, x_bp, y_bp, max_summaries)
    }

    /// Run an action plan, then wait for the indexed object within
    /// `max_summaries` frame summaries and tap a relative point inside it with a
    /// typed scrcpy pointer id if found.
    pub fn run_actions_and_tap_next_object_at_pointer_with_limit(
        &mut self,
        actions: &[AgentAction],
        index: usize,
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.run_actions(actions)?;
        self.tap_next_object_at_pointer_with_limit(index, pointer_id, x_bp, y_bp, max_summaries)
    }

    /// Run an action plan, then wait for the best object within `max_summaries`
    /// frame summaries and tap its center if found.
    pub fn run_actions_and_tap_next_best_object_with_limit(
        &mut self,
        actions: &[AgentAction],
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.run_actions(actions)?;
        self.tap_next_best_object_with_limit(max_summaries)
    }

    /// Run an action plan, then wait for the best object within `max_summaries`
    /// frame summaries and tap its center with a typed scrcpy pointer id if
    /// found.
    pub fn run_actions_and_tap_next_best_object_pointer_with_limit(
        &mut self,
        actions: &[AgentAction],
        pointer_id: TouchPointerId,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.run_actions(actions)?;
        self.tap_next_best_object_pointer_with_limit(pointer_id, max_summaries)
    }

    /// Run an action plan, then wait for the best object within `max_summaries`
    /// frame summaries and tap a relative point inside it if found.
    pub fn run_actions_and_tap_next_best_object_at_with_limit(
        &mut self,
        actions: &[AgentAction],
        x_bp: u16,
        y_bp: u16,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.run_actions(actions)?;
        self.tap_next_best_object_at_with_limit(x_bp, y_bp, max_summaries)
    }

    /// Run an action plan, then wait for the best object within `max_summaries`
    /// frame summaries and tap a relative point inside it with a typed scrcpy
    /// pointer id if found.
    pub fn run_actions_and_tap_next_best_object_at_pointer_with_limit(
        &mut self,
        actions: &[AgentAction],
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.run_actions(actions)?;
        self.tap_next_best_object_at_pointer_with_limit(pointer_id, x_bp, y_bp, max_summaries)
    }

    /// Run an action plan, then wait for the best object of `class_id` within
    /// `max_summaries` frame summaries and tap its center if found.
    pub fn run_actions_and_tap_next_object_class_with_limit(
        &mut self,
        actions: &[AgentAction],
        class_id: u8,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.run_actions(actions)?;
        self.tap_next_object_class_with_limit(class_id, max_summaries)
    }

    /// Run an action plan, then wait for the best object of `class_id` within
    /// `max_summaries` frame summaries and tap its center with a typed scrcpy
    /// pointer id if found.
    pub fn run_actions_and_tap_next_object_class_pointer_with_limit(
        &mut self,
        actions: &[AgentAction],
        class_id: u8,
        pointer_id: TouchPointerId,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.run_actions(actions)?;
        self.tap_next_object_class_pointer_with_limit(class_id, pointer_id, max_summaries)
    }

    /// Run an action plan, then wait for the best object of `class_id` within
    /// `max_summaries` frame summaries and tap a relative point inside it if
    /// found.
    pub fn run_actions_and_tap_next_object_class_at_with_limit(
        &mut self,
        actions: &[AgentAction],
        class_id: u8,
        x_bp: u16,
        y_bp: u16,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.run_actions(actions)?;
        self.tap_next_object_class_at_with_limit(class_id, x_bp, y_bp, max_summaries)
    }

    /// Run an action plan, then wait for the best object of `class_id` within
    /// `max_summaries` frame summaries and tap a relative point inside it with a
    /// typed scrcpy pointer id if found.
    pub fn run_actions_and_tap_next_object_class_at_pointer_with_limit(
        &mut self,
        actions: &[AgentAction],
        class_id: u8,
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.run_actions(actions)?;
        self.tap_next_object_class_at_pointer_with_limit(
            class_id,
            pointer_id,
            x_bp,
            y_bp,
            max_summaries,
        )
    }

    /// Run an action plan, then wait for an object matching `selector` within
    /// `max_summaries` frame summaries and tap its center if found.
    pub fn run_actions_and_tap_next_object_selector_with_limit(
        &mut self,
        actions: &[AgentAction],
        selector: AgentObjectSelector,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.run_actions(actions)?;
        self.tap_next_object_selector_with_limit(selector, max_summaries)
    }

    /// Run an action plan, then wait for an object matching `selector` within
    /// `max_summaries` frame summaries and tap its center with a typed scrcpy
    /// pointer id if found.
    pub fn run_actions_and_tap_next_object_selector_pointer_with_limit(
        &mut self,
        actions: &[AgentAction],
        selector: AgentObjectSelector,
        pointer_id: TouchPointerId,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.run_actions(actions)?;
        self.tap_next_object_selector_pointer_with_limit(selector, pointer_id, max_summaries)
    }

    /// Run an action plan, then wait for an object matching `selector` within
    /// `max_summaries` frame summaries and tap a relative point inside it if
    /// found.
    pub fn run_actions_and_tap_next_object_selector_at_with_limit(
        &mut self,
        actions: &[AgentAction],
        selector: AgentObjectSelector,
        x_bp: u16,
        y_bp: u16,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.run_actions(actions)?;
        self.tap_next_object_selector_at_with_limit(selector, x_bp, y_bp, max_summaries)
    }

    /// Run an action plan, then wait for an object matching `selector` within
    /// `max_summaries` frame summaries and tap a relative point inside it with
    /// a typed scrcpy pointer id if found.
    pub fn run_actions_and_tap_next_object_selector_at_pointer_with_limit(
        &mut self,
        actions: &[AgentAction],
        selector: AgentObjectSelector,
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.run_actions(actions)?;
        self.tap_next_object_selector_at_pointer_with_limit(
            selector,
            pointer_id,
            x_bp,
            y_bp,
            max_summaries,
        )
    }

    /// Run an action plan, then wait for the indexed text region within
    /// `max_summaries` frame summaries and tap its center if found.
    pub fn run_actions_and_tap_next_text_region_with_limit(
        &mut self,
        actions: &[AgentAction],
        index: usize,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.run_actions(actions)?;
        self.tap_next_text_region_with_limit(index, max_summaries)
    }

    /// Run an action plan, then wait for the indexed text region within
    /// `max_summaries` frame summaries and tap its center with a typed scrcpy
    /// pointer id if found.
    pub fn run_actions_and_tap_next_text_region_pointer_with_limit(
        &mut self,
        actions: &[AgentAction],
        index: usize,
        pointer_id: TouchPointerId,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.run_actions(actions)?;
        self.tap_next_text_region_pointer_with_limit(index, pointer_id, max_summaries)
    }

    /// Run an action plan, then wait for the indexed text region within
    /// `max_summaries` frame summaries and tap a relative point inside it if
    /// found.
    pub fn run_actions_and_tap_next_text_region_at_with_limit(
        &mut self,
        actions: &[AgentAction],
        index: usize,
        x_bp: u16,
        y_bp: u16,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.run_actions(actions)?;
        self.tap_next_text_region_at_with_limit(index, x_bp, y_bp, max_summaries)
    }

    /// Run an action plan, then wait for the indexed text region within
    /// `max_summaries` frame summaries and tap a relative point inside it with a
    /// typed scrcpy pointer id if found.
    pub fn run_actions_and_tap_next_text_region_at_pointer_with_limit(
        &mut self,
        actions: &[AgentAction],
        index: usize,
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.run_actions(actions)?;
        self.tap_next_text_region_at_pointer_with_limit(
            index,
            pointer_id,
            x_bp,
            y_bp,
            max_summaries,
        )
    }

    /// Run an action plan, then wait for the largest text region within
    /// `max_summaries` frame summaries and tap its center if found.
    pub fn run_actions_and_tap_next_largest_text_region_with_limit(
        &mut self,
        actions: &[AgentAction],
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.run_actions(actions)?;
        self.tap_next_largest_text_region_with_limit(max_summaries)
    }

    /// Run an action plan, then wait for the largest text region within
    /// `max_summaries` frame summaries and tap its center with a typed scrcpy
    /// pointer id if found.
    pub fn run_actions_and_tap_next_largest_text_region_pointer_with_limit(
        &mut self,
        actions: &[AgentAction],
        pointer_id: TouchPointerId,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.run_actions(actions)?;
        self.tap_next_largest_text_region_pointer_with_limit(pointer_id, max_summaries)
    }

    /// Run an action plan, then wait for the largest text region within
    /// `max_summaries` frame summaries and tap a relative point inside it if
    /// found.
    pub fn run_actions_and_tap_next_largest_text_region_at_with_limit(
        &mut self,
        actions: &[AgentAction],
        x_bp: u16,
        y_bp: u16,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.run_actions(actions)?;
        self.tap_next_largest_text_region_at_with_limit(x_bp, y_bp, max_summaries)
    }

    /// Run an action plan, then wait for the largest text region within
    /// `max_summaries` frame summaries and tap a relative point inside it with
    /// a typed scrcpy pointer id if found.
    pub fn run_actions_and_tap_next_largest_text_region_at_pointer_with_limit(
        &mut self,
        actions: &[AgentAction],
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
        max_summaries: usize,
    ) -> Result<Option<AgentRect>> {
        self.run_actions(actions)?;
        self.tap_next_largest_text_region_at_pointer_with_limit(
            pointer_id,
            x_bp,
            y_bp,
            max_summaries,
        )
    }

    /// Run an action plan, then wait for the indexed object and tap its center.
    pub fn run_actions_and_tap_next_object(
        &mut self,
        actions: &[AgentAction],
        index: usize,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_object(index)
    }

    /// Run an action plan, then wait for the indexed object and tap its center
    /// with a typed scrcpy pointer id.
    pub fn run_actions_and_tap_next_object_pointer(
        &mut self,
        actions: &[AgentAction],
        index: usize,
        pointer_id: TouchPointerId,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_object_pointer(index, pointer_id)
    }

    /// Run an action plan, then wait for the indexed object and tap a relative
    /// point inside it.
    pub fn run_actions_and_tap_next_object_at(
        &mut self,
        actions: &[AgentAction],
        index: usize,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_object_at(index, x_bp, y_bp)
    }

    /// Run an action plan, then wait for the indexed object and tap a relative
    /// point inside it with a typed scrcpy pointer id.
    pub fn run_actions_and_tap_next_object_at_pointer(
        &mut self,
        actions: &[AgentAction],
        index: usize,
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_object_at_pointer(index, pointer_id, x_bp, y_bp)
    }

    /// Run an action plan, then wait for the next best object and tap its
    /// center.
    pub fn run_actions_and_tap_next_best_object(
        &mut self,
        actions: &[AgentAction],
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_best_object()
    }

    /// Run an action plan, then wait for the next best object and tap its
    /// center with a typed scrcpy pointer id.
    pub fn run_actions_and_tap_next_best_object_pointer(
        &mut self,
        actions: &[AgentAction],
        pointer_id: TouchPointerId,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_best_object_pointer(pointer_id)
    }

    /// Run an action plan, then wait for the next best object and tap a
    /// relative point inside it.
    pub fn run_actions_and_tap_next_best_object_at(
        &mut self,
        actions: &[AgentAction],
        x_bp: u16,
        y_bp: u16,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_best_object_at(x_bp, y_bp)
    }

    /// Run an action plan, then wait for the next best object and tap a
    /// relative point inside it with a typed scrcpy pointer id.
    pub fn run_actions_and_tap_next_best_object_at_pointer(
        &mut self,
        actions: &[AgentAction],
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_best_object_at_pointer(pointer_id, x_bp, y_bp)
    }

    /// Run an action plan, then wait for the next best object of `class_id` and
    /// tap its center.
    pub fn run_actions_and_tap_next_object_class(
        &mut self,
        actions: &[AgentAction],
        class_id: u8,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_object_class(class_id)
    }

    /// Run an action plan, then wait for the next best object of `class_id` and
    /// tap its center with a typed scrcpy pointer id.
    pub fn run_actions_and_tap_next_object_class_pointer(
        &mut self,
        actions: &[AgentAction],
        class_id: u8,
        pointer_id: TouchPointerId,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_object_class_pointer(class_id, pointer_id)
    }

    /// Run an action plan, then wait for the next best object of `class_id` and
    /// tap a relative point inside it.
    pub fn run_actions_and_tap_next_object_class_at(
        &mut self,
        actions: &[AgentAction],
        class_id: u8,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_object_class_at(class_id, x_bp, y_bp)
    }

    /// Run an action plan, then wait for the next best object of `class_id` and
    /// tap a relative point inside it with a typed scrcpy pointer id.
    pub fn run_actions_and_tap_next_object_class_at_pointer(
        &mut self,
        actions: &[AgentAction],
        class_id: u8,
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_object_class_at_pointer(class_id, pointer_id, x_bp, y_bp)
    }

    /// Run an action plan, then wait for the next object matching `selector`
    /// and tap its center.
    pub fn run_actions_and_tap_next_object_selector(
        &mut self,
        actions: &[AgentAction],
        selector: AgentObjectSelector,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_object_selector(selector)
    }

    /// Run an action plan, then wait for the next object matching `selector`
    /// and tap its center with a typed scrcpy pointer id.
    pub fn run_actions_and_tap_next_object_selector_pointer(
        &mut self,
        actions: &[AgentAction],
        selector: AgentObjectSelector,
        pointer_id: TouchPointerId,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_object_selector_pointer(selector, pointer_id)
    }

    /// Run an action plan, then wait for the next object matching `selector`
    /// and tap a relative point inside it.
    pub fn run_actions_and_tap_next_object_selector_at(
        &mut self,
        actions: &[AgentAction],
        selector: AgentObjectSelector,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_object_selector_at(selector, x_bp, y_bp)
    }

    /// Run an action plan, then wait for the next object matching `selector`
    /// and tap a relative point inside it with a typed scrcpy pointer id.
    pub fn run_actions_and_tap_next_object_selector_at_pointer(
        &mut self,
        actions: &[AgentAction],
        selector: AgentObjectSelector,
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_object_selector_at_pointer(selector, pointer_id, x_bp, y_bp)
    }

    /// Run an action plan, then wait for the indexed text region and tap its
    /// center.
    pub fn run_actions_and_tap_next_text_region(
        &mut self,
        actions: &[AgentAction],
        index: usize,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_text_region(index)
    }

    /// Run an action plan, then wait for the indexed text region and tap its
    /// center with a typed scrcpy pointer id.
    pub fn run_actions_and_tap_next_text_region_pointer(
        &mut self,
        actions: &[AgentAction],
        index: usize,
        pointer_id: TouchPointerId,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_text_region_pointer(index, pointer_id)
    }

    /// Run an action plan, then wait for the indexed text region and tap a
    /// relative point inside it.
    pub fn run_actions_and_tap_next_text_region_at(
        &mut self,
        actions: &[AgentAction],
        index: usize,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_text_region_at(index, x_bp, y_bp)
    }

    /// Run an action plan, then wait for the indexed text region and tap a
    /// relative point inside it with a typed scrcpy pointer id.
    pub fn run_actions_and_tap_next_text_region_at_pointer(
        &mut self,
        actions: &[AgentAction],
        index: usize,
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_text_region_at_pointer(index, pointer_id, x_bp, y_bp)
    }

    /// Run an action plan, then wait for the next largest text region and tap
    /// its center.
    pub fn run_actions_and_tap_next_largest_text_region(
        &mut self,
        actions: &[AgentAction],
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_largest_text_region()
    }

    /// Run an action plan, then wait for the next largest text region and tap
    /// its center with a typed scrcpy pointer id.
    pub fn run_actions_and_tap_next_largest_text_region_pointer(
        &mut self,
        actions: &[AgentAction],
        pointer_id: TouchPointerId,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_largest_text_region_pointer(pointer_id)
    }

    /// Run an action plan, then wait for the next largest text region and tap a
    /// relative point inside it.
    pub fn run_actions_and_tap_next_largest_text_region_at(
        &mut self,
        actions: &[AgentAction],
        x_bp: u16,
        y_bp: u16,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_largest_text_region_at(x_bp, y_bp)
    }

    /// Run an action plan, then wait for the next largest text region and tap a
    /// relative point inside it with a typed scrcpy pointer id.
    pub fn run_actions_and_tap_next_largest_text_region_at_pointer(
        &mut self,
        actions: &[AgentAction],
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<AgentRect> {
        self.run_actions(actions)?;
        self.tap_next_largest_text_region_at_pointer(pointer_id, x_bp, y_bp)
    }

    /// Wait for the next object detection at `index`, tap its center, and
    /// return the selected rectangle.
    pub fn tap_next_object(&mut self, index: usize) -> Result<AgentRect> {
        let rect = self.wait_for_object_rect(index)?;
        self.tap_rect(rect)?;
        Ok(rect)
    }

    /// Wait for the next object detection at `index`, tap its center with a
    /// typed scrcpy pointer id, and return the selected rectangle.
    pub fn tap_next_object_pointer(
        &mut self,
        index: usize,
        pointer_id: TouchPointerId,
    ) -> Result<AgentRect> {
        let rect = self.wait_for_object_rect(index)?;
        self.tap_rect_pointer(pointer_id, rect)?;
        Ok(rect)
    }

    /// Wait for the next object detection at `index`, tap a relative point
    /// inside it, and return the selected rectangle.
    pub fn tap_next_object_at(&mut self, index: usize, x_bp: u16, y_bp: u16) -> Result<AgentRect> {
        let rect = self.wait_for_object_rect(index)?;
        self.tap_rect_at(rect, x_bp, y_bp)?;
        Ok(rect)
    }

    /// Wait for the next object detection at `index`, tap a relative point
    /// inside it with a typed scrcpy pointer id, and return the selected
    /// rectangle.
    pub fn tap_next_object_at_pointer(
        &mut self,
        index: usize,
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<AgentRect> {
        let rect = self.wait_for_object_rect(index)?;
        self.tap_rect_at_pointer(pointer_id, rect, x_bp, y_bp)?;
        Ok(rect)
    }

    /// Wait for the next best object target, tap its center, and return it.
    pub fn tap_next_best_object(&mut self) -> Result<AgentRect> {
        let rect = self.wait_for_best_object_rect()?;
        self.tap_rect(rect)?;
        Ok(rect)
    }

    /// Wait for the next best object target, tap its center with a typed scrcpy
    /// pointer id, and return it.
    pub fn tap_next_best_object_pointer(
        &mut self,
        pointer_id: TouchPointerId,
    ) -> Result<AgentRect> {
        let rect = self.wait_for_best_object_rect()?;
        self.tap_rect_pointer(pointer_id, rect)?;
        Ok(rect)
    }

    /// Wait for the next best object target, tap a relative point inside it,
    /// and return it.
    pub fn tap_next_best_object_at(&mut self, x_bp: u16, y_bp: u16) -> Result<AgentRect> {
        let rect = self.wait_for_best_object_rect()?;
        self.tap_rect_at(rect, x_bp, y_bp)?;
        Ok(rect)
    }

    /// Wait for the next best object target, tap a relative point inside it
    /// with a typed scrcpy pointer id, and return it.
    pub fn tap_next_best_object_at_pointer(
        &mut self,
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<AgentRect> {
        let rect = self.wait_for_best_object_rect()?;
        self.tap_rect_at_pointer(pointer_id, rect, x_bp, y_bp)?;
        Ok(rect)
    }

    /// Wait for the next best target of `class_id`, tap its center, and return
    /// it.
    pub fn tap_next_object_class(&mut self, class_id: u8) -> Result<AgentRect> {
        let rect = self.wait_for_object_selector_rect(AgentObjectSelector::class_id(class_id))?;
        self.tap_rect(rect)?;
        Ok(rect)
    }

    /// Wait for the next best target of `class_id`, tap its center with a typed
    /// scrcpy pointer id, and return it.
    pub fn tap_next_object_class_pointer(
        &mut self,
        class_id: u8,
        pointer_id: TouchPointerId,
    ) -> Result<AgentRect> {
        let rect = self.wait_for_object_selector_rect(AgentObjectSelector::class_id(class_id))?;
        self.tap_rect_pointer(pointer_id, rect)?;
        Ok(rect)
    }

    /// Wait for the next best target of `class_id`, tap a relative point inside
    /// it, and return it.
    pub fn tap_next_object_class_at(
        &mut self,
        class_id: u8,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<AgentRect> {
        let rect = self.wait_for_object_selector_rect(AgentObjectSelector::class_id(class_id))?;
        self.tap_rect_at(rect, x_bp, y_bp)?;
        Ok(rect)
    }

    /// Wait for the next best target of `class_id`, tap a relative point inside
    /// it with a typed scrcpy pointer id, and return it.
    pub fn tap_next_object_class_at_pointer(
        &mut self,
        class_id: u8,
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<AgentRect> {
        let rect = self.wait_for_object_selector_rect(AgentObjectSelector::class_id(class_id))?;
        self.tap_rect_at_pointer(pointer_id, rect, x_bp, y_bp)?;
        Ok(rect)
    }

    /// Wait for the next object matching `selector`, tap its center, and return
    /// it.
    pub fn tap_next_object_selector(&mut self, selector: AgentObjectSelector) -> Result<AgentRect> {
        let rect = self.wait_for_object_selector_rect(selector)?;
        self.tap_rect(rect)?;
        Ok(rect)
    }

    /// Wait for the next object matching `selector`, tap its center with a typed
    /// scrcpy pointer id, and return it.
    pub fn tap_next_object_selector_pointer(
        &mut self,
        selector: AgentObjectSelector,
        pointer_id: TouchPointerId,
    ) -> Result<AgentRect> {
        let rect = self.wait_for_object_selector_rect(selector)?;
        self.tap_rect_pointer(pointer_id, rect)?;
        Ok(rect)
    }

    /// Wait for the next object matching `selector`, tap a relative point inside
    /// it, and return it.
    pub fn tap_next_object_selector_at(
        &mut self,
        selector: AgentObjectSelector,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<AgentRect> {
        let rect = self.wait_for_object_selector_rect(selector)?;
        self.tap_rect_at(rect, x_bp, y_bp)?;
        Ok(rect)
    }

    /// Wait for the next object matching `selector`, tap a relative point inside
    /// it with a typed scrcpy pointer id, and return it.
    pub fn tap_next_object_selector_at_pointer(
        &mut self,
        selector: AgentObjectSelector,
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<AgentRect> {
        let rect = self.wait_for_object_selector_rect(selector)?;
        self.tap_rect_at_pointer(pointer_id, rect, x_bp, y_bp)?;
        Ok(rect)
    }

    /// Wait for the next text region at `index`, tap its center, and return it.
    pub fn tap_next_text_region(&mut self, index: usize) -> Result<AgentRect> {
        let rect = self.wait_for_text_region_rect(index)?;
        self.tap_rect(rect)?;
        Ok(rect)
    }

    /// Wait for the next text region at `index`, tap its center with a typed
    /// scrcpy pointer id, and return it.
    pub fn tap_next_text_region_pointer(
        &mut self,
        index: usize,
        pointer_id: TouchPointerId,
    ) -> Result<AgentRect> {
        let rect = self.wait_for_text_region_rect(index)?;
        self.tap_rect_pointer(pointer_id, rect)?;
        Ok(rect)
    }

    /// Wait for the next text region at `index`, tap a relative point inside it,
    /// and return it.
    pub fn tap_next_text_region_at(
        &mut self,
        index: usize,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<AgentRect> {
        let rect = self.wait_for_text_region_rect(index)?;
        self.tap_rect_at(rect, x_bp, y_bp)?;
        Ok(rect)
    }

    /// Wait for the next text region at `index`, tap a relative point inside it
    /// with a typed scrcpy pointer id, and return it.
    pub fn tap_next_text_region_at_pointer(
        &mut self,
        index: usize,
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<AgentRect> {
        let rect = self.wait_for_text_region_rect(index)?;
        self.tap_rect_at_pointer(pointer_id, rect, x_bp, y_bp)?;
        Ok(rect)
    }

    /// Wait for the next largest text region, tap its center, and return it.
    pub fn tap_next_largest_text_region(&mut self) -> Result<AgentRect> {
        let rect = self.wait_for_largest_text_region_rect()?;
        self.tap_rect(rect)?;
        Ok(rect)
    }

    /// Wait for the next largest text region, tap its center with a typed
    /// scrcpy pointer id, and return it.
    pub fn tap_next_largest_text_region_pointer(
        &mut self,
        pointer_id: TouchPointerId,
    ) -> Result<AgentRect> {
        let rect = self.wait_for_largest_text_region_rect()?;
        self.tap_rect_pointer(pointer_id, rect)?;
        Ok(rect)
    }

    /// Wait for the next largest text region, tap a relative point inside it,
    /// and return it.
    pub fn tap_next_largest_text_region_at(&mut self, x_bp: u16, y_bp: u16) -> Result<AgentRect> {
        let rect = self.wait_for_largest_text_region_rect()?;
        self.tap_rect_at(rect, x_bp, y_bp)?;
        Ok(rect)
    }

    /// Wait for the next largest text region, tap a relative point inside it
    /// with a typed scrcpy pointer id, and return it.
    pub fn tap_next_largest_text_region_at_pointer(
        &mut self,
        pointer_id: TouchPointerId,
        x_bp: u16,
        y_bp: u16,
    ) -> Result<AgentRect> {
        let rect = self.wait_for_largest_text_region_rect()?;
        self.tap_rect_at_pointer(pointer_id, rect, x_bp, y_bp)?;
        Ok(rect)
    }

    /// Read events until the next AI stats envelope is observed.
    ///
    /// Native scrcpy messages and unknown extension envelopes are skipped.
    pub fn wait_for_ai_stats(&mut self) -> Result<AiStats> {
        loop {
            if let DeviceEvent::AiStats(stats) = self
                .recv_device_event()
                .map_err(|e| io_to_wait_error(e, "ai stats"))?
            {
                return Ok(stats);
            }
        }
    }

    /// Set the device clipboard and wait for the matching ACK_CLIPBOARD.
    pub fn set_clipboard_and_wait_ack(
        &mut self,
        text: impl Into<String>,
        paste: bool,
    ) -> Result<u64> {
        let sequence = self.next_clipboard_sequence();
        self.set_clipboard_sequenced(sequence, text, paste)?;
        self.flush()?;
        self.wait_for_clipboard_ack(sequence)?;
        Ok(sequence)
    }

    /// Run an action plan, set the device clipboard, and wait for the matching
    /// ACK_CLIPBOARD.
    ///
    /// The action plan and SET_CLIPBOARD command share one checked dispatcher
    /// barrier before the ACK wait.
    pub fn run_actions_and_set_clipboard_and_wait_ack(
        &mut self,
        actions: &[AgentAction],
        text: impl Into<String>,
        paste: bool,
    ) -> Result<u64> {
        let sequence = self.next_clipboard_sequence();
        self.queue_actions(actions)?;
        self.set_clipboard_sequenced(sequence, text, paste)?;
        self.flush()?;
        self.wait_for_clipboard_ack(sequence)?;
        Ok(sequence)
    }

    /// Flush pending coalesced writes at a deterministic boundary.
    pub fn flush(&self) -> Result<()> {
        self.client.flush_wait().map(|_| ())
    }

    /// Access the underlying receiver for advanced read patterns.
    pub fn receiver_mut(&mut self) -> io::Result<&mut DeviceMessageReceiver<R>> {
        self.receiver
            .as_mut()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "agent receiver closed"))
    }

    /// Close the control dispatcher and recover both underlying streams.
    pub fn close(mut self) -> Result<AgentControlClosed<T, R>> {
        self.client.close();
        let dispatcher = self
            .dispatcher
            .take()
            .ok_or(Error::DispatcherDown("agent dispatcher already joined"))?;
        let transport = dispatcher.join()?;
        let reader = self
            .receiver
            .take()
            .ok_or(Error::DispatcherDown("agent receiver already taken"))?
            .into_inner();
        Ok(AgentControlClosed { transport, reader })
    }

    /// Close only the command/write side.
    ///
    /// Use this after [`Self::detach_latest_frame_summary_receiver`] has moved
    /// the reader into a background pump, or when the caller intentionally does
    /// not need to recover the reader through this agent.
    pub fn close_transport(mut self) -> Result<T> {
        self.client.close();
        let dispatcher = self
            .dispatcher
            .take()
            .ok_or(Error::DispatcherDown("agent dispatcher already joined"))?;
        dispatcher.join()
    }

    /// Checked close: report any queued command error while still recovering
    /// the underlying transport and reader.
    pub fn close_checked(mut self) -> Result<AgentControlCloseReport<T, R>> {
        let command_result = self.client.close_wait();
        let dispatcher = self
            .dispatcher
            .take()
            .ok_or(Error::DispatcherDown("agent dispatcher already joined"))?;
        let transport = dispatcher.join()?;
        let reader = self
            .receiver
            .take()
            .ok_or(Error::DispatcherDown("agent receiver already taken"))?
            .into_inner();
        Ok(AgentControlCloseReport {
            closed: AgentControlClosed { transport, reader },
            command_result,
        })
    }

    /// Checked variant of [`Self::close_transport`].
    pub fn close_transport_checked(mut self) -> Result<AgentControlCommandCloseReport<T>> {
        let command_result = self.client.close_wait();
        let dispatcher = self
            .dispatcher
            .take()
            .ok_or(Error::DispatcherDown("agent dispatcher already joined"))?;
        let transport = dispatcher.join()?;
        Ok(AgentControlCommandCloseReport {
            transport,
            command_result,
        })
    }

    pub(super) fn next_clipboard_sequence(&mut self) -> u64 {
        let sequence = self.next_clipboard_sequence;
        self.next_clipboard_sequence = self.next_clipboard_sequence.wrapping_add(1).max(1);
        sequence
    }

    fn point_to_pixels(&self, point: AgentPoint) -> (i32, i32) {
        let (width, height) = self.screen_size();
        point.to_pixels(width, height)
    }

    fn queue_action(&self, action: &AgentAction) -> Result<()> {
        match action {
            AgentAction::TypeText(text) => self.client.type_text(text.clone()),
            AgentAction::TypeTextStrict(text) => self.client.type_text_strict(text.clone()),
            AgentAction::Key {
                scancode,
                pressed,
                mods,
            } => self.client.send(HidCommand::Key {
                scancode: *scancode,
                pressed: *pressed,
                mods: *mods,
            }),
            AgentAction::KeyTap { scancode, mods } => self.client.tap_key(*scancode, *mods),
            AgentAction::KeyboardChord { chord } => self.client.key_chord(*chord),
            AgentAction::KeyBatch { len, frames } => {
                self.client.send_key_batch_fixed(*len, *frames)
            }
            AgentAction::MouseMotion { dx, dy, buttons } => {
                self.client.mouse_motion(*dx, *dy, *buttons)
            }
            AgentAction::MouseButtons { buttons } => self.client.mouse_buttons(*buttons),
            AgentAction::MouseScroll { hscroll, vscroll } => {
                self.client.mouse_scroll(*hscroll as f32, *vscroll as f32)
            }
            AgentAction::MouseBatch { len, frames } => {
                self.client.send_mouse_batch_fixed(*len, *frames)
            }
            AgentAction::InjectKeycode {
                action,
                keycode,
                repeat,
                metastate,
            } => self
                .client
                .inject_keycode(*action, *keycode, *repeat, *metastate),
            AgentAction::AndroidKeyTap { keycode, metastate } => {
                self.client.tap_android_keycode(*keycode, *metastate)
            }
            AgentAction::AndroidKeyBatch { len, frames } => {
                self.client.send_android_key_batch_fixed(*len, *frames)
            }
            AgentAction::BackOrScreenOn { action } => self
                .client
                .back_or_screen_on(AndroidKeyAction::new(*action)),
            AgentAction::PressHome => self.client.press_home(),
            AgentAction::PressBack => self.client.press_back(),
            AgentAction::OpenRecents => self.client.open_recents(),
            AgentAction::VolumeUp => self.client.volume_up(),
            AgentAction::VolumeDown => self.client.volume_down(),
            AgentAction::VolumeMute => self.client.volume_mute(),
            AgentAction::Tap { x, y } => self.queue_tap(*x, *y),
            AgentAction::TapPointer { pointer_id, x, y } => {
                self.queue_tap_pointer(TouchPointerId::new(*pointer_id), *x, *y)
            }
            AgentAction::TapPoint { point } => {
                let (x, y) = self.point_to_pixels(*point);
                self.queue_tap(x, y)
            }
            AgentAction::TapPointPointer { pointer_id, point } => {
                let (x, y) = self.point_to_pixels(*point);
                self.queue_tap_pointer(TouchPointerId::new(*pointer_id), x, y)
            }
            AgentAction::TapRect { rect } => {
                let (x, y) = self.point_to_pixels(rect.center());
                self.queue_tap(x, y)
            }
            AgentAction::TapRectAt { rect, x_bp, y_bp } => {
                let (x, y) = self.point_to_pixels(rect.try_point_at_basis_points(*x_bp, *y_bp)?);
                self.queue_tap(x, y)
            }
            AgentAction::TapRectPointer { pointer_id, rect } => {
                let (x, y) = self.point_to_pixels(rect.center());
                self.queue_tap_pointer(TouchPointerId::new(*pointer_id), x, y)
            }
            AgentAction::TapRectAtPointer {
                pointer_id,
                rect,
                x_bp,
                y_bp,
            } => {
                let (x, y) = self.point_to_pixels(rect.try_point_at_basis_points(*x_bp, *y_bp)?);
                self.queue_tap_pointer(TouchPointerId::new(*pointer_id), x, y)
            }
            AgentAction::DoubleTap { x, y } => self.queue_double_tap(*x, *y),
            AgentAction::DoubleTapPointer { pointer_id, x, y } => {
                self.queue_double_tap_pointer(TouchPointerId::new(*pointer_id), *x, *y)
            }
            AgentAction::DoubleTapPoint { point } => {
                let (x, y) = self.point_to_pixels(*point);
                self.queue_double_tap(x, y)
            }
            AgentAction::DoubleTapPointPointer { pointer_id, point } => {
                let (x, y) = self.point_to_pixels(*point);
                self.queue_double_tap_pointer(TouchPointerId::new(*pointer_id), x, y)
            }
            AgentAction::DoubleTapRect { rect } => {
                let (x, y) = self.point_to_pixels(rect.center());
                self.queue_double_tap(x, y)
            }
            AgentAction::DoubleTapRectAt { rect, x_bp, y_bp } => {
                let (x, y) = self.point_to_pixels(rect.try_point_at_basis_points(*x_bp, *y_bp)?);
                self.queue_double_tap(x, y)
            }
            AgentAction::DoubleTapRectPointer { pointer_id, rect } => {
                let (x, y) = self.point_to_pixels(rect.center());
                self.queue_double_tap_pointer(TouchPointerId::new(*pointer_id), x, y)
            }
            AgentAction::DoubleTapRectAtPointer {
                pointer_id,
                rect,
                x_bp,
                y_bp,
            } => {
                let (x, y) = self.point_to_pixels(rect.try_point_at_basis_points(*x_bp, *y_bp)?);
                self.queue_double_tap_pointer(TouchPointerId::new(*pointer_id), x, y)
            }
            AgentAction::LongPress { x, y, duration } => self.queue_long_press(*x, *y, *duration),
            AgentAction::LongPressPointer {
                pointer_id,
                x,
                y,
                duration,
            } => self.queue_long_press_pointer(TouchPointerId::new(*pointer_id), *x, *y, *duration),
            AgentAction::LongPressPoint { point, duration } => {
                let (x, y) = self.point_to_pixels(*point);
                self.queue_long_press(x, y, *duration)
            }
            AgentAction::LongPressPointPointer {
                pointer_id,
                point,
                duration,
            } => {
                let (x, y) = self.point_to_pixels(*point);
                self.queue_long_press_pointer(TouchPointerId::new(*pointer_id), x, y, *duration)
            }
            AgentAction::LongPressRect { rect, duration } => {
                let (x, y) = self.point_to_pixels(rect.center());
                self.queue_long_press(x, y, *duration)
            }
            AgentAction::LongPressRectAt {
                rect,
                x_bp,
                y_bp,
                duration,
            } => {
                let (x, y) = self.point_to_pixels(rect.try_point_at_basis_points(*x_bp, *y_bp)?);
                self.queue_long_press(x, y, *duration)
            }
            AgentAction::LongPressRectPointer {
                pointer_id,
                rect,
                duration,
            } => {
                let (x, y) = self.point_to_pixels(rect.center());
                self.queue_long_press_pointer(TouchPointerId::new(*pointer_id), x, y, *duration)
            }
            AgentAction::LongPressRectAtPointer {
                pointer_id,
                rect,
                x_bp,
                y_bp,
                duration,
            } => {
                let (x, y) = self.point_to_pixels(rect.try_point_at_basis_points(*x_bp, *y_bp)?);
                self.queue_long_press_pointer(TouchPointerId::new(*pointer_id), x, y, *duration)
            }
            AgentAction::Swipe { from, to, steps } => self.queue_swipe(*from, *to, *steps),
            AgentAction::SwipePointer {
                pointer_id,
                from,
                to,
                steps,
            } => self.queue_swipe_pointer(TouchPointerId::new(*pointer_id), *from, *to, *steps),
            AgentAction::SwipePoints { from, to, steps } => self.queue_swipe(
                self.point_to_pixels(*from),
                self.point_to_pixels(*to),
                *steps,
            ),
            AgentAction::SwipePointsPointer {
                pointer_id,
                from,
                to,
                steps,
            } => self.queue_swipe_pointer(
                TouchPointerId::new(*pointer_id),
                self.point_to_pixels(*from),
                self.point_to_pixels(*to),
                *steps,
            ),
            AgentAction::SwipeRect {
                rect,
                from_x_bp,
                from_y_bp,
                to_x_bp,
                to_y_bp,
                steps,
            } => self.queue_swipe(
                self.point_to_pixels(rect.try_point_at_basis_points(*from_x_bp, *from_y_bp)?),
                self.point_to_pixels(rect.try_point_at_basis_points(*to_x_bp, *to_y_bp)?),
                *steps,
            ),
            AgentAction::SwipeRectPointer {
                pointer_id,
                rect,
                from_x_bp,
                from_y_bp,
                to_x_bp,
                to_y_bp,
                steps,
            } => self.queue_swipe_pointer(
                TouchPointerId::new(*pointer_id),
                self.point_to_pixels(rect.try_point_at_basis_points(*from_x_bp, *from_y_bp)?),
                self.point_to_pixels(rect.try_point_at_basis_points(*to_x_bp, *to_y_bp)?),
                *steps,
            ),
            AgentAction::Pinch {
                first_from,
                first_to,
                second_from,
                second_to,
                steps,
            } => self.queue_pinch(*first_from, *first_to, *second_from, *second_to, *steps),
            AgentAction::PinchPoints {
                first_from,
                first_to,
                second_from,
                second_to,
                steps,
            } => self.queue_pinch(
                self.point_to_pixels(*first_from),
                self.point_to_pixels(*first_to),
                self.point_to_pixels(*second_from),
                self.point_to_pixels(*second_to),
                *steps,
            ),
            AgentAction::Scroll {
                x,
                y,
                hscroll,
                vscroll,
                buttons,
            } => self.client.send(HidCommand::InjectScroll {
                x: *x,
                y: *y,
                hscroll: *hscroll as f32,
                vscroll: *vscroll as f32,
                buttons: *buttons,
            }),
            AgentAction::ScrollPoint {
                point,
                hscroll,
                vscroll,
                buttons,
            } => {
                let (x, y) = self.point_to_pixels(*point);
                self.client.send(HidCommand::InjectScroll {
                    x,
                    y,
                    hscroll: *hscroll as f32,
                    vscroll: *vscroll as f32,
                    buttons: *buttons,
                })
            }
            AgentAction::ScrollRect {
                rect,
                hscroll,
                vscroll,
                buttons,
            } => {
                let (x, y) = self.point_to_pixels(rect.center());
                self.client.send(HidCommand::InjectScroll {
                    x,
                    y,
                    hscroll: *hscroll as f32,
                    vscroll: *vscroll as f32,
                    buttons: *buttons,
                })
            }
            AgentAction::ScrollRectAt {
                rect,
                x_bp,
                y_bp,
                hscroll,
                vscroll,
                buttons,
            } => {
                let (x, y) = self.point_to_pixels(rect.try_point_at_basis_points(*x_bp, *y_bp)?);
                self.client.send(HidCommand::InjectScroll {
                    x,
                    y,
                    hscroll: *hscroll as f32,
                    vscroll: *vscroll as f32,
                    buttons: *buttons,
                })
            }
            AgentAction::ScrollBatch { len, frames } => {
                let mut batch = self.client.scroll_frame_batcher();
                Self::queue_agent_scroll_frames_into(&mut batch, *len, frames)?;
                batch.flush()
            }
            AgentAction::CancelTouch { pointer_id } => self
                .client
                .send(HidCommand::MultitouchCancel { id: *pointer_id }),
            AgentAction::TouchFrames { len, frames } => {
                let mut batch = self.client.touch_frame_batcher();
                Self::queue_agent_touch_frames_into(&mut batch, *len, frames)?;
                batch.flush()
            }
            AgentAction::ThreeFingerScreenshot => self.queue_three_finger_screenshot(),
            AgentAction::SetScreenSize { width, height } => self.set_screen_size(*width, *height),
            AgentAction::LaunchApp(name) => self.client.launch_app(name.clone()),
            AgentAction::SetScreenPower { on } => self.client.set_screen_power(*on),
            AgentAction::ShowNotifications => self.client.show_notifications(),
            AgentAction::ShowQuickSettings => self.client.show_quick_settings(),
            AgentAction::CollapsePanels => self.client.collapse_panels(),
            AgentAction::RotateDevice => self.client.rotate_device(),
            AgentAction::ResizeDisplay { width, height } => {
                self.client.resize_display(*width, *height)
            }
            AgentAction::SetTorch { on } => self.client.set_torch(*on),
            AgentAction::CameraZoomIn => self.client.camera_zoom_in(),
            AgentAction::CameraZoomOut => self.client.camera_zoom_out(),
            AgentAction::OpenHardKeyboardSettings => self.client.open_hard_keyboard_settings(),
            AgentAction::ResetVideo => self.client.reset_video(),
            AgentAction::AiConfig {
                flags,
                sample_interval_ms,
                feature_dim,
            } => self
                .client
                .configure_ai(*flags, *sample_interval_ms, *feature_dim),
            AgentAction::AiQuery { since_timestamp_ms } => {
                self.client.query_ai(*since_timestamp_ms)
            }
            AgentAction::AiPause => self.client.pause_ai(),
            AgentAction::SetClipboard { text, paste } => {
                self.client.set_clipboard(text.clone(), *paste)
            }
            AgentAction::SetClipboardSequenced {
                sequence,
                text,
                paste,
            } => self
                .client
                .set_clipboard_sequenced(*sequence, text.clone(), *paste),
            AgentAction::RequestClipboard { copy_key } => self.client.request_clipboard(*copy_key),
            AgentAction::GamepadButton { button, pressed } => {
                self.client.send_button(*button, *pressed)
            }
            AgentAction::GamepadButtons { buttons } => self.client.send_buttons(*buttons),
            AgentAction::GamepadFrame { frame } => self.client.send_frame(*frame),
            AgentAction::GamepadFrameUnchecked { frame } => {
                self.client.send_frame_unchecked(*frame)
            }
            AgentAction::GamepadFrameBatch { len, frames } => {
                self.client.send_frame_batch_fixed(*len, *frames)
            }
            AgentAction::GamepadFrameBatchUnchecked { len, frames } => {
                self.client.send_frame_batch_fixed_unchecked(*len, *frames)
            }
            AgentAction::GamepadPackedFrame { frame } => self.client.send_frame_packed(*frame),
            AgentAction::GamepadPackedFrameBatch { len, frames } => {
                self.client.send_frame_packed_batch_fixed(*len, *frames)
            }
            AgentAction::Wait(duration) => {
                self.flush()?;
                std::thread::sleep(*duration);
                Ok(())
            }
            AgentAction::Flush => self.flush(),
        }
    }

    fn queue_planned_action(
        &self,
        action: &AgentAction,
        batches: PlanBatchers<'_, '_>,
    ) -> Result<()> {
        let (touch_batch, key_batch, android_key_batch, mouse_batch, scroll_batch, gamepad_batch) =
            batches;
        if !matches!(
            action,
            AgentAction::Scroll { .. }
                | AgentAction::ScrollPoint { .. }
                | AgentAction::ScrollRect { .. }
                | AgentAction::ScrollRectAt { .. }
                | AgentAction::ScrollBatch { .. }
        ) {
            scroll_batch.flush()?;
        }
        match action {
            AgentAction::Tap { x, y } => {
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                Self::queue_tap_into(touch_batch, *x, *y)
            }
            AgentAction::TapPointer { pointer_id, x, y } => {
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                Self::queue_tap_pointer_into(touch_batch, TouchPointerId::new(*pointer_id), *x, *y)
            }
            AgentAction::TapPoint { point } => {
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                let (x, y) = self.point_to_pixels(*point);
                Self::queue_tap_into(touch_batch, x, y)
            }
            AgentAction::TapPointPointer { pointer_id, point } => {
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                let (x, y) = self.point_to_pixels(*point);
                Self::queue_tap_pointer_into(touch_batch, TouchPointerId::new(*pointer_id), x, y)
            }
            AgentAction::TapRect { rect } => {
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                let (x, y) = self.point_to_pixels(rect.center());
                Self::queue_tap_into(touch_batch, x, y)
            }
            AgentAction::TapRectAt { rect, x_bp, y_bp } => {
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                let (x, y) = self.point_to_pixels(rect.try_point_at_basis_points(*x_bp, *y_bp)?);
                Self::queue_tap_into(touch_batch, x, y)
            }
            AgentAction::TapRectPointer { pointer_id, rect } => {
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                let (x, y) = self.point_to_pixels(rect.center());
                Self::queue_tap_pointer_into(touch_batch, TouchPointerId::new(*pointer_id), x, y)
            }
            AgentAction::TapRectAtPointer {
                pointer_id,
                rect,
                x_bp,
                y_bp,
            } => {
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                let (x, y) = self.point_to_pixels(rect.try_point_at_basis_points(*x_bp, *y_bp)?);
                Self::queue_tap_pointer_into(touch_batch, TouchPointerId::new(*pointer_id), x, y)
            }
            AgentAction::DoubleTap { x, y } => {
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                Self::queue_double_tap_into(touch_batch, *x, *y)
            }
            AgentAction::DoubleTapPointer { pointer_id, x, y } => {
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                Self::queue_double_tap_pointer_into(
                    touch_batch,
                    TouchPointerId::new(*pointer_id),
                    *x,
                    *y,
                )
            }
            AgentAction::DoubleTapPoint { point } => {
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                let (x, y) = self.point_to_pixels(*point);
                Self::queue_double_tap_into(touch_batch, x, y)
            }
            AgentAction::DoubleTapPointPointer { pointer_id, point } => {
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                let (x, y) = self.point_to_pixels(*point);
                Self::queue_double_tap_pointer_into(
                    touch_batch,
                    TouchPointerId::new(*pointer_id),
                    x,
                    y,
                )
            }
            AgentAction::DoubleTapRect { rect } => {
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                let (x, y) = self.point_to_pixels(rect.center());
                Self::queue_double_tap_into(touch_batch, x, y)
            }
            AgentAction::DoubleTapRectAt { rect, x_bp, y_bp } => {
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                let (x, y) = self.point_to_pixels(rect.try_point_at_basis_points(*x_bp, *y_bp)?);
                Self::queue_double_tap_into(touch_batch, x, y)
            }
            AgentAction::DoubleTapRectPointer { pointer_id, rect } => {
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                let (x, y) = self.point_to_pixels(rect.center());
                Self::queue_double_tap_pointer_into(
                    touch_batch,
                    TouchPointerId::new(*pointer_id),
                    x,
                    y,
                )
            }
            AgentAction::DoubleTapRectAtPointer {
                pointer_id,
                rect,
                x_bp,
                y_bp,
            } => {
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                let (x, y) = self.point_to_pixels(rect.try_point_at_basis_points(*x_bp, *y_bp)?);
                Self::queue_double_tap_pointer_into(
                    touch_batch,
                    TouchPointerId::new(*pointer_id),
                    x,
                    y,
                )
            }
            AgentAction::Swipe { from, to, steps } => {
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                Self::queue_swipe_into(touch_batch, *from, *to, *steps)
            }
            AgentAction::SwipePointer {
                pointer_id,
                from,
                to,
                steps,
            } => {
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                Self::queue_swipe_pointer_into(
                    touch_batch,
                    TouchPointerId::new(*pointer_id),
                    *from,
                    *to,
                    *steps,
                )
            }
            AgentAction::SwipePoints { from, to, steps } => {
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                Self::queue_swipe_into(
                    touch_batch,
                    self.point_to_pixels(*from),
                    self.point_to_pixels(*to),
                    *steps,
                )
            }
            AgentAction::SwipePointsPointer {
                pointer_id,
                from,
                to,
                steps,
            } => {
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                Self::queue_swipe_pointer_into(
                    touch_batch,
                    TouchPointerId::new(*pointer_id),
                    self.point_to_pixels(*from),
                    self.point_to_pixels(*to),
                    *steps,
                )
            }
            AgentAction::SwipeRect {
                rect,
                from_x_bp,
                from_y_bp,
                to_x_bp,
                to_y_bp,
                steps,
            } => {
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                Self::queue_swipe_into(
                    touch_batch,
                    self.point_to_pixels(rect.try_point_at_basis_points(*from_x_bp, *from_y_bp)?),
                    self.point_to_pixels(rect.try_point_at_basis_points(*to_x_bp, *to_y_bp)?),
                    *steps,
                )
            }
            AgentAction::SwipeRectPointer {
                pointer_id,
                rect,
                from_x_bp,
                from_y_bp,
                to_x_bp,
                to_y_bp,
                steps,
            } => {
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                Self::queue_swipe_pointer_into(
                    touch_batch,
                    TouchPointerId::new(*pointer_id),
                    self.point_to_pixels(rect.try_point_at_basis_points(*from_x_bp, *from_y_bp)?),
                    self.point_to_pixels(rect.try_point_at_basis_points(*to_x_bp, *to_y_bp)?),
                    *steps,
                )
            }
            AgentAction::Pinch {
                first_from,
                first_to,
                second_from,
                second_to,
                steps,
            } => {
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                Self::queue_pinch_into(
                    touch_batch,
                    *first_from,
                    *first_to,
                    *second_from,
                    *second_to,
                    *steps,
                )
            }
            AgentAction::PinchPoints {
                first_from,
                first_to,
                second_from,
                second_to,
                steps,
            } => {
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                Self::queue_pinch_into(
                    touch_batch,
                    self.point_to_pixels(*first_from),
                    self.point_to_pixels(*first_to),
                    self.point_to_pixels(*second_from),
                    self.point_to_pixels(*second_to),
                    *steps,
                )
            }
            AgentAction::CancelTouch { pointer_id } => {
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                touch_batch.cancel(*pointer_id)
            }
            AgentAction::TouchFrames { len, frames } => {
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                Self::queue_agent_touch_frames_into(touch_batch, *len, frames)
            }
            AgentAction::ThreeFingerScreenshot => {
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                self.queue_three_finger_screenshot_into(touch_batch)
            }
            AgentAction::Key {
                scancode,
                pressed,
                mods,
            } => {
                touch_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                key_batch.key(*scancode, *pressed, *mods)
            }
            AgentAction::KeyTap { scancode, mods } => {
                touch_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                key_batch.tap_key(*scancode, *mods)
            }
            AgentAction::KeyboardChord { chord } => {
                touch_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                key_batch.chord(*chord)
            }
            AgentAction::KeyBatch { len, frames } => {
                touch_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                Self::queue_agent_key_frames_into(key_batch, *len, frames)
            }
            AgentAction::InjectKeycode {
                action,
                keycode,
                repeat,
                metastate,
            } => {
                touch_batch.flush()?;
                key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                android_key_batch.keycode(*action, *keycode, *repeat, *metastate)
            }
            AgentAction::AndroidKeyTap { keycode, metastate } => {
                touch_batch.flush()?;
                key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                android_key_batch.tap_keycode(*keycode, *metastate)
            }
            AgentAction::AndroidKeyBatch { len, frames } => {
                touch_batch.flush()?;
                key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                Self::queue_agent_android_key_frames_into(android_key_batch, *len, frames)
            }
            AgentAction::PressHome => {
                touch_batch.flush()?;
                key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                android_key_batch.key_event(AndroidKeyAction::DOWN, AndroidKeycode::HOME, 0, 0)
            }
            AgentAction::PressBack => {
                touch_batch.flush()?;
                key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                android_key_batch.key_event(AndroidKeyAction::DOWN, AndroidKeycode::BACK, 0, 0)
            }
            AgentAction::OpenRecents => {
                touch_batch.flush()?;
                key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                android_key_batch.key_event(
                    AndroidKeyAction::DOWN,
                    AndroidKeycode::APP_SWITCH,
                    0,
                    0,
                )
            }
            AgentAction::VolumeUp => {
                touch_batch.flush()?;
                key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                android_key_batch.key_event(AndroidKeyAction::DOWN, AndroidKeycode::VOLUME_UP, 0, 0)
            }
            AgentAction::VolumeDown => {
                touch_batch.flush()?;
                key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                android_key_batch.key_event(
                    AndroidKeyAction::DOWN,
                    AndroidKeycode::VOLUME_DOWN,
                    0,
                    0,
                )
            }
            AgentAction::VolumeMute => {
                touch_batch.flush()?;
                key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                android_key_batch.key_event(
                    AndroidKeyAction::DOWN,
                    AndroidKeycode::VOLUME_MUTE,
                    0,
                    0,
                )
            }
            AgentAction::MouseMotion { dx, dy, buttons } => {
                touch_batch.flush()?;
                key_batch.flush()?;
                android_key_batch.flush()?;
                gamepad_batch.flush()?;
                mouse_batch.motion(*dx, *dy, *buttons)
            }
            AgentAction::MouseButtons { buttons } => {
                touch_batch.flush()?;
                key_batch.flush()?;
                android_key_batch.flush()?;
                gamepad_batch.flush()?;
                mouse_batch.buttons(*buttons)
            }
            AgentAction::MouseBatch { len, frames } => {
                touch_batch.flush()?;
                key_batch.flush()?;
                android_key_batch.flush()?;
                gamepad_batch.flush()?;
                Self::queue_agent_mouse_frames_into(mouse_batch, *len, frames)
            }
            AgentAction::Scroll {
                x,
                y,
                hscroll,
                vscroll,
                buttons,
            } => {
                touch_batch.flush()?;
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                scroll_batch.scroll_with_buttons(*x, *y, *hscroll as f32, *vscroll as f32, *buttons)
            }
            AgentAction::ScrollPoint {
                point,
                hscroll,
                vscroll,
                buttons,
            } => {
                touch_batch.flush()?;
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                let (x, y) = self.point_to_pixels(*point);
                scroll_batch.scroll_with_buttons(x, y, *hscroll as f32, *vscroll as f32, *buttons)
            }
            AgentAction::ScrollRect {
                rect,
                hscroll,
                vscroll,
                buttons,
            } => {
                touch_batch.flush()?;
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                let (x, y) = self.point_to_pixels(rect.center());
                scroll_batch.scroll_with_buttons(x, y, *hscroll as f32, *vscroll as f32, *buttons)
            }
            AgentAction::ScrollRectAt {
                rect,
                x_bp,
                y_bp,
                hscroll,
                vscroll,
                buttons,
            } => {
                touch_batch.flush()?;
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                let (x, y) = self.point_to_pixels(rect.try_point_at_basis_points(*x_bp, *y_bp)?);
                scroll_batch.scroll_with_buttons(x, y, *hscroll as f32, *vscroll as f32, *buttons)
            }
            AgentAction::ScrollBatch { len, frames } => {
                touch_batch.flush()?;
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                Self::queue_agent_scroll_frames_into(scroll_batch, *len, frames)
            }
            AgentAction::GamepadFrame { frame } => {
                touch_batch.flush()?;
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.push_dedupe(*frame)
            }
            AgentAction::GamepadFrameUnchecked { frame } => {
                touch_batch.flush()?;
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.push_unchecked(*frame)
            }
            AgentAction::GamepadFrameBatch { len, frames } => {
                touch_batch.flush()?;
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.push_dedupe_slice(*len, frames)
            }
            AgentAction::GamepadFrameBatchUnchecked { len, frames } => {
                touch_batch.flush()?;
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.push_unchecked_slice(*len, frames)
            }
            AgentAction::GamepadPackedFrame { frame } => {
                touch_batch.flush()?;
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.push_packed(*frame)
            }
            AgentAction::GamepadPackedFrameBatch { len, frames } => {
                touch_batch.flush()?;
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.push_packed_slice(*len, frames)
            }
            AgentAction::LongPress { x, y, duration } => {
                touch_batch.flush()?;
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                self.queue_long_press(*x, *y, *duration)
            }
            AgentAction::LongPressPointer {
                pointer_id,
                x,
                y,
                duration,
            } => {
                touch_batch.flush()?;
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                self.queue_long_press_pointer(TouchPointerId::new(*pointer_id), *x, *y, *duration)
            }
            AgentAction::LongPressPoint { point, duration } => {
                touch_batch.flush()?;
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                let (x, y) = self.point_to_pixels(*point);
                self.queue_long_press(x, y, *duration)
            }
            AgentAction::LongPressPointPointer {
                pointer_id,
                point,
                duration,
            } => {
                touch_batch.flush()?;
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                let (x, y) = self.point_to_pixels(*point);
                self.queue_long_press_pointer(TouchPointerId::new(*pointer_id), x, y, *duration)
            }
            AgentAction::LongPressRect { rect, duration } => {
                touch_batch.flush()?;
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                let (x, y) = self.point_to_pixels(rect.center());
                self.queue_long_press(x, y, *duration)
            }
            AgentAction::LongPressRectAt {
                rect,
                x_bp,
                y_bp,
                duration,
            } => {
                touch_batch.flush()?;
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                let (x, y) = self.point_to_pixels(rect.try_point_at_basis_points(*x_bp, *y_bp)?);
                self.queue_long_press(x, y, *duration)
            }
            AgentAction::LongPressRectPointer {
                pointer_id,
                rect,
                duration,
            } => {
                touch_batch.flush()?;
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                let (x, y) = self.point_to_pixels(rect.center());
                self.queue_long_press_pointer(TouchPointerId::new(*pointer_id), x, y, *duration)
            }
            AgentAction::LongPressRectAtPointer {
                pointer_id,
                rect,
                x_bp,
                y_bp,
                duration,
            } => {
                touch_batch.flush()?;
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                let (x, y) = self.point_to_pixels(rect.try_point_at_basis_points(*x_bp, *y_bp)?);
                self.queue_long_press_pointer(TouchPointerId::new(*pointer_id), x, y, *duration)
            }
            AgentAction::Wait(duration) => {
                touch_batch.flush()?;
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                self.flush()?;
                std::thread::sleep(*duration);
                Ok(())
            }
            AgentAction::Flush => {
                touch_batch.flush()?;
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                self.flush()
            }
            _ => {
                touch_batch.flush()?;
                key_batch.flush()?;
                android_key_batch.flush()?;
                mouse_batch.flush()?;
                gamepad_batch.flush()?;
                self.queue_action(action)
            }
        }
    }

    fn try_queue_action(&self, action: &AgentAction) -> Result<()> {
        match action {
            AgentAction::TypeText(text) => self.client.try_send(HidCommand::TypeText(text.clone())),
            AgentAction::TypeTextStrict(text) => self
                .client
                .try_send(HidCommand::TypeTextStrict(text.clone())),
            AgentAction::Key {
                scancode,
                pressed,
                mods,
            } => self.client.try_send(HidCommand::Key {
                scancode: *scancode,
                pressed: *pressed,
                mods: *mods,
            }),
            AgentAction::KeyTap { scancode, mods } => self.client.try_tap_key(*scancode, *mods),
            AgentAction::KeyboardChord { chord } => self.client.try_key_chord(*chord),
            AgentAction::KeyBatch { len, frames } => {
                self.client.try_send_key_batch_fixed(*len, *frames)
            }
            AgentAction::MouseMotion { dx, dy, buttons } => {
                self.client.try_mouse_motion(*dx, *dy, *buttons)
            }
            AgentAction::MouseButtons { buttons } => self.client.try_mouse_buttons(*buttons),
            AgentAction::MouseScroll { hscroll, vscroll } => self
                .client
                .try_mouse_scroll(*hscroll as f32, *vscroll as f32),
            AgentAction::MouseBatch { len, frames } => {
                self.client.try_send_mouse_batch_fixed(*len, *frames)
            }
            AgentAction::InjectKeycode {
                action,
                keycode,
                repeat,
                metastate,
            } => self
                .client
                .try_inject_keycode(*action, *keycode, *repeat, *metastate),
            AgentAction::AndroidKeyTap { keycode, metastate } => {
                self.client.try_tap_android_keycode(*keycode, *metastate)
            }
            AgentAction::AndroidKeyBatch { len, frames } => {
                self.client.try_send_android_key_batch_fixed(*len, *frames)
            }
            AgentAction::BackOrScreenOn { action } => self
                .client
                .try_back_or_screen_on(AndroidKeyAction::new(*action)),
            AgentAction::PressHome => self.client.try_press_android_key(AndroidKeycode::HOME),
            AgentAction::PressBack => self.client.try_press_android_key(AndroidKeycode::BACK),
            AgentAction::OpenRecents => self
                .client
                .try_press_android_key(AndroidKeycode::APP_SWITCH),
            AgentAction::VolumeUp => self.client.try_press_android_key(AndroidKeycode::VOLUME_UP),
            AgentAction::VolumeDown => self
                .client
                .try_press_android_key(AndroidKeycode::VOLUME_DOWN),
            AgentAction::VolumeMute => self
                .client
                .try_press_android_key(AndroidKeycode::VOLUME_MUTE),
            AgentAction::Tap { x, y } => self.try_queue_tap(*x, *y),
            AgentAction::TapPointer { pointer_id, x, y } => {
                self.try_queue_tap_pointer(TouchPointerId::new(*pointer_id), *x, *y)
            }
            AgentAction::TapPoint { point } => {
                let (x, y) = self.point_to_pixels(*point);
                self.try_queue_tap(x, y)
            }
            AgentAction::TapPointPointer { pointer_id, point } => {
                let (x, y) = self.point_to_pixels(*point);
                self.try_queue_tap_pointer(TouchPointerId::new(*pointer_id), x, y)
            }
            AgentAction::TapRect { rect } => {
                let (x, y) = self.point_to_pixels(rect.center());
                self.try_queue_tap(x, y)
            }
            AgentAction::TapRectAt { rect, x_bp, y_bp } => {
                let (x, y) = self.point_to_pixels(rect.try_point_at_basis_points(*x_bp, *y_bp)?);
                self.try_queue_tap(x, y)
            }
            AgentAction::TapRectPointer { pointer_id, rect } => {
                let (x, y) = self.point_to_pixels(rect.center());
                self.try_queue_tap_pointer(TouchPointerId::new(*pointer_id), x, y)
            }
            AgentAction::TapRectAtPointer {
                pointer_id,
                rect,
                x_bp,
                y_bp,
            } => {
                let (x, y) = self.point_to_pixels(rect.try_point_at_basis_points(*x_bp, *y_bp)?);
                self.try_queue_tap_pointer(TouchPointerId::new(*pointer_id), x, y)
            }
            AgentAction::DoubleTap { x, y } => self.try_queue_double_tap(*x, *y),
            AgentAction::DoubleTapPointer { pointer_id, x, y } => {
                self.try_queue_double_tap_pointer(TouchPointerId::new(*pointer_id), *x, *y)
            }
            AgentAction::DoubleTapPoint { point } => {
                let (x, y) = self.point_to_pixels(*point);
                self.try_queue_double_tap(x, y)
            }
            AgentAction::DoubleTapPointPointer { pointer_id, point } => {
                let (x, y) = self.point_to_pixels(*point);
                self.try_queue_double_tap_pointer(TouchPointerId::new(*pointer_id), x, y)
            }
            AgentAction::DoubleTapRect { rect } => {
                let (x, y) = self.point_to_pixels(rect.center());
                self.try_queue_double_tap(x, y)
            }
            AgentAction::DoubleTapRectAt { rect, x_bp, y_bp } => {
                let (x, y) = self.point_to_pixels(rect.try_point_at_basis_points(*x_bp, *y_bp)?);
                self.try_queue_double_tap(x, y)
            }
            AgentAction::DoubleTapRectPointer { pointer_id, rect } => {
                let (x, y) = self.point_to_pixels(rect.center());
                self.try_queue_double_tap_pointer(TouchPointerId::new(*pointer_id), x, y)
            }
            AgentAction::DoubleTapRectAtPointer {
                pointer_id,
                rect,
                x_bp,
                y_bp,
            } => {
                let (x, y) = self.point_to_pixels(rect.try_point_at_basis_points(*x_bp, *y_bp)?);
                self.try_queue_double_tap_pointer(TouchPointerId::new(*pointer_id), x, y)
            }
            AgentAction::Swipe { from, to, steps } => self.try_queue_swipe(*from, *to, *steps),
            AgentAction::SwipePointer {
                pointer_id,
                from,
                to,
                steps,
            } => self.try_queue_swipe_pointer(TouchPointerId::new(*pointer_id), *from, *to, *steps),
            AgentAction::SwipePoints { from, to, steps } => self.try_queue_swipe(
                self.point_to_pixels(*from),
                self.point_to_pixels(*to),
                *steps,
            ),
            AgentAction::SwipePointsPointer {
                pointer_id,
                from,
                to,
                steps,
            } => self.try_queue_swipe_pointer(
                TouchPointerId::new(*pointer_id),
                self.point_to_pixels(*from),
                self.point_to_pixels(*to),
                *steps,
            ),
            AgentAction::SwipeRect {
                rect,
                from_x_bp,
                from_y_bp,
                to_x_bp,
                to_y_bp,
                steps,
            } => self.try_queue_swipe(
                self.point_to_pixels(rect.try_point_at_basis_points(*from_x_bp, *from_y_bp)?),
                self.point_to_pixels(rect.try_point_at_basis_points(*to_x_bp, *to_y_bp)?),
                *steps,
            ),
            AgentAction::SwipeRectPointer {
                pointer_id,
                rect,
                from_x_bp,
                from_y_bp,
                to_x_bp,
                to_y_bp,
                steps,
            } => self.try_queue_swipe_pointer(
                TouchPointerId::new(*pointer_id),
                self.point_to_pixels(rect.try_point_at_basis_points(*from_x_bp, *from_y_bp)?),
                self.point_to_pixels(rect.try_point_at_basis_points(*to_x_bp, *to_y_bp)?),
                *steps,
            ),
            AgentAction::Pinch {
                first_from,
                first_to,
                second_from,
                second_to,
                steps,
            } => self.try_queue_pinch(*first_from, *first_to, *second_from, *second_to, *steps),
            AgentAction::PinchPoints {
                first_from,
                first_to,
                second_from,
                second_to,
                steps,
            } => self.try_queue_pinch(
                self.point_to_pixels(*first_from),
                self.point_to_pixels(*first_to),
                self.point_to_pixels(*second_from),
                self.point_to_pixels(*second_to),
                *steps,
            ),
            AgentAction::Scroll {
                x,
                y,
                hscroll,
                vscroll,
                buttons,
            } => self.client.try_scroll_with_buttons(
                *x,
                *y,
                *hscroll as f32,
                *vscroll as f32,
                *buttons,
            ),
            AgentAction::ScrollPoint {
                point,
                hscroll,
                vscroll,
                buttons,
            } => {
                let (x, y) = self.point_to_pixels(*point);
                self.client.try_scroll_with_buttons(
                    x,
                    y,
                    *hscroll as f32,
                    *vscroll as f32,
                    *buttons,
                )
            }
            AgentAction::ScrollRect {
                rect,
                hscroll,
                vscroll,
                buttons,
            } => {
                let (x, y) = self.point_to_pixels(rect.center());
                self.client.try_scroll_with_buttons(
                    x,
                    y,
                    *hscroll as f32,
                    *vscroll as f32,
                    *buttons,
                )
            }
            AgentAction::ScrollRectAt {
                rect,
                x_bp,
                y_bp,
                hscroll,
                vscroll,
                buttons,
            } => {
                let (x, y) = self.point_to_pixels(rect.try_point_at_basis_points(*x_bp, *y_bp)?);
                self.client.try_scroll_with_buttons(
                    x,
                    y,
                    *hscroll as f32,
                    *vscroll as f32,
                    *buttons,
                )
            }
            AgentAction::ScrollBatch { len, frames } => {
                let mut batch = self.client.scroll_frame_batcher();
                Self::try_queue_agent_scroll_frames_into(&mut batch, *len, frames)?;
                batch.try_flush()
            }
            AgentAction::CancelTouch { pointer_id } => self.client.try_cancel_touch(*pointer_id),
            AgentAction::TouchFrames { len, frames } => {
                let mut batch = self.client.touch_frame_batcher();
                Self::try_queue_agent_touch_frames_into(&mut batch, *len, frames)?;
                batch.try_flush()
            }
            AgentAction::ThreeFingerScreenshot => self.try_queue_three_finger_screenshot(),
            AgentAction::SetScreenSize { width, height } => {
                self.client.try_set_screen_size(*width, *height)?;
                self.screen_width.store(*width, Ordering::Relaxed);
                self.screen_height.store(*height, Ordering::Relaxed);
                Ok(())
            }
            AgentAction::LaunchApp(name) => self
                .client
                .try_send(HidCommand::LaunchApp { name: name.clone() }),
            AgentAction::SetScreenPower { on } => {
                self.client.try_send(HidCommand::SetScreenPower { on: *on })
            }
            AgentAction::ShowNotifications => self.client.try_send(HidCommand::ShowNotifications),
            AgentAction::ShowQuickSettings => self.client.try_send(HidCommand::ShowQuickSettings),
            AgentAction::CollapsePanels => self.client.try_send(HidCommand::CollapsePanels),
            AgentAction::RotateDevice => self.client.try_send(HidCommand::RotateDevice),
            AgentAction::ResizeDisplay { width, height } => {
                self.client.try_send(HidCommand::ResizeDisplay {
                    width: *width,
                    height: *height,
                })
            }
            AgentAction::SetTorch { on } => self.client.try_send(HidCommand::SetTorch { on: *on }),
            AgentAction::CameraZoomIn => self.client.try_send(HidCommand::CameraZoomIn),
            AgentAction::CameraZoomOut => self.client.try_send(HidCommand::CameraZoomOut),
            AgentAction::OpenHardKeyboardSettings => {
                self.client.try_send(HidCommand::OpenHardKeyboardSettings)
            }
            AgentAction::ResetVideo => self.client.try_send(HidCommand::ResetVideo),
            AgentAction::AiConfig {
                flags,
                sample_interval_ms,
                feature_dim,
            } => self
                .client
                .try_configure_ai(*flags, *sample_interval_ms, *feature_dim),
            AgentAction::AiQuery { since_timestamp_ms } => {
                self.client.try_query_ai(*since_timestamp_ms)
            }
            AgentAction::AiPause => self.client.try_pause_ai(),
            AgentAction::SetClipboard { text, paste } => {
                self.client.try_send(HidCommand::SetClipboard {
                    text: text.clone(),
                    paste: *paste,
                })
            }
            AgentAction::SetClipboardSequenced {
                sequence,
                text,
                paste,
            } => self.client.try_send(HidCommand::SetClipboardSequenced {
                sequence: *sequence,
                text: text.clone(),
                paste: *paste,
            }),
            AgentAction::RequestClipboard { copy_key } => {
                self.client.try_request_clipboard(*copy_key)
            }
            AgentAction::GamepadButton { button, pressed } => {
                self.client.try_send_button(*button, *pressed)
            }
            AgentAction::GamepadButtons { buttons } => self.client.try_send_buttons(*buttons),
            AgentAction::GamepadFrame { frame } => self.client.try_send_frame(*frame),
            AgentAction::GamepadFrameUnchecked { frame } => {
                self.client.try_send_frame_unchecked(*frame)
            }
            AgentAction::GamepadFrameBatch { len, frames } => {
                self.client.try_send_frame_batch_fixed(*len, *frames)
            }
            AgentAction::GamepadFrameBatchUnchecked { len, frames } => self
                .client
                .try_send_frame_batch_fixed_unchecked(*len, *frames),
            AgentAction::GamepadPackedFrame { frame } => self.client.try_send_frame_packed(*frame),
            AgentAction::GamepadPackedFrameBatch { len, frames } => {
                self.client.try_send_frame_packed_batch_fixed(*len, *frames)
            }
            AgentAction::Wait(_)
            | AgentAction::LongPress { .. }
            | AgentAction::LongPressPointer { .. }
            | AgentAction::LongPressPoint { .. }
            | AgentAction::LongPressPointPointer { .. }
            | AgentAction::LongPressRect { .. }
            | AgentAction::LongPressRectAt { .. }
            | AgentAction::LongPressRectPointer { .. }
            | AgentAction::LongPressRectAtPointer { .. } => {
                Err(Error::SessionLifecycle(TIMED_ACTION_REQUIRES_BLOCKING))
            }
            AgentAction::Flush => self.client.try_flush(),
        }
    }

    fn try_queue_planned_action(
        &self,
        action: &AgentAction,
        batches: PlanBatchers<'_, '_>,
    ) -> Result<()> {
        let (touch_batch, key_batch, android_key_batch, mouse_batch, scroll_batch, gamepad_batch) =
            batches;
        if !matches!(
            action,
            AgentAction::Scroll { .. }
                | AgentAction::ScrollPoint { .. }
                | AgentAction::ScrollRect { .. }
                | AgentAction::ScrollRectAt { .. }
                | AgentAction::ScrollBatch { .. }
        ) {
            scroll_batch.try_flush()?;
        }
        match action {
            AgentAction::Tap { x, y } => {
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                Self::try_queue_tap_into(touch_batch, *x, *y)
            }
            AgentAction::TapPointer { pointer_id, x, y } => {
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                Self::try_queue_tap_pointer_into(
                    touch_batch,
                    TouchPointerId::new(*pointer_id),
                    *x,
                    *y,
                )
            }
            AgentAction::TapPoint { point } => {
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                let (x, y) = self.point_to_pixels(*point);
                Self::try_queue_tap_into(touch_batch, x, y)
            }
            AgentAction::TapPointPointer { pointer_id, point } => {
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                let (x, y) = self.point_to_pixels(*point);
                Self::try_queue_tap_pointer_into(
                    touch_batch,
                    TouchPointerId::new(*pointer_id),
                    x,
                    y,
                )
            }
            AgentAction::TapRect { rect } => {
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                let (x, y) = self.point_to_pixels(rect.center());
                Self::try_queue_tap_into(touch_batch, x, y)
            }
            AgentAction::TapRectAt { rect, x_bp, y_bp } => {
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                let (x, y) = self.point_to_pixels(rect.try_point_at_basis_points(*x_bp, *y_bp)?);
                Self::try_queue_tap_into(touch_batch, x, y)
            }
            AgentAction::TapRectPointer { pointer_id, rect } => {
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                let (x, y) = self.point_to_pixels(rect.center());
                Self::try_queue_tap_pointer_into(
                    touch_batch,
                    TouchPointerId::new(*pointer_id),
                    x,
                    y,
                )
            }
            AgentAction::TapRectAtPointer {
                pointer_id,
                rect,
                x_bp,
                y_bp,
            } => {
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                let (x, y) = self.point_to_pixels(rect.try_point_at_basis_points(*x_bp, *y_bp)?);
                Self::try_queue_tap_pointer_into(
                    touch_batch,
                    TouchPointerId::new(*pointer_id),
                    x,
                    y,
                )
            }
            AgentAction::DoubleTap { x, y } => {
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                Self::try_queue_double_tap_into(touch_batch, *x, *y)
            }
            AgentAction::DoubleTapPointer { pointer_id, x, y } => {
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                Self::try_queue_double_tap_pointer_into(
                    touch_batch,
                    TouchPointerId::new(*pointer_id),
                    *x,
                    *y,
                )
            }
            AgentAction::DoubleTapPoint { point } => {
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                let (x, y) = self.point_to_pixels(*point);
                Self::try_queue_double_tap_into(touch_batch, x, y)
            }
            AgentAction::DoubleTapPointPointer { pointer_id, point } => {
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                let (x, y) = self.point_to_pixels(*point);
                Self::try_queue_double_tap_pointer_into(
                    touch_batch,
                    TouchPointerId::new(*pointer_id),
                    x,
                    y,
                )
            }
            AgentAction::DoubleTapRect { rect } => {
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                let (x, y) = self.point_to_pixels(rect.center());
                Self::try_queue_double_tap_into(touch_batch, x, y)
            }
            AgentAction::DoubleTapRectAt { rect, x_bp, y_bp } => {
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                let (x, y) = self.point_to_pixels(rect.try_point_at_basis_points(*x_bp, *y_bp)?);
                Self::try_queue_double_tap_into(touch_batch, x, y)
            }
            AgentAction::DoubleTapRectPointer { pointer_id, rect } => {
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                let (x, y) = self.point_to_pixels(rect.center());
                Self::try_queue_double_tap_pointer_into(
                    touch_batch,
                    TouchPointerId::new(*pointer_id),
                    x,
                    y,
                )
            }
            AgentAction::DoubleTapRectAtPointer {
                pointer_id,
                rect,
                x_bp,
                y_bp,
            } => {
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                let (x, y) = self.point_to_pixels(rect.try_point_at_basis_points(*x_bp, *y_bp)?);
                Self::try_queue_double_tap_pointer_into(
                    touch_batch,
                    TouchPointerId::new(*pointer_id),
                    x,
                    y,
                )
            }
            AgentAction::Swipe { from, to, steps } => {
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                Self::try_queue_swipe_into(touch_batch, *from, *to, *steps)
            }
            AgentAction::SwipePointer {
                pointer_id,
                from,
                to,
                steps,
            } => {
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                Self::try_queue_swipe_pointer_into(
                    touch_batch,
                    TouchPointerId::new(*pointer_id),
                    *from,
                    *to,
                    *steps,
                )
            }
            AgentAction::SwipePoints { from, to, steps } => {
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                Self::try_queue_swipe_into(
                    touch_batch,
                    self.point_to_pixels(*from),
                    self.point_to_pixels(*to),
                    *steps,
                )
            }
            AgentAction::SwipePointsPointer {
                pointer_id,
                from,
                to,
                steps,
            } => {
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                Self::try_queue_swipe_pointer_into(
                    touch_batch,
                    TouchPointerId::new(*pointer_id),
                    self.point_to_pixels(*from),
                    self.point_to_pixels(*to),
                    *steps,
                )
            }
            AgentAction::SwipeRect {
                rect,
                from_x_bp,
                from_y_bp,
                to_x_bp,
                to_y_bp,
                steps,
            } => {
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                Self::try_queue_swipe_into(
                    touch_batch,
                    self.point_to_pixels(rect.try_point_at_basis_points(*from_x_bp, *from_y_bp)?),
                    self.point_to_pixels(rect.try_point_at_basis_points(*to_x_bp, *to_y_bp)?),
                    *steps,
                )
            }
            AgentAction::SwipeRectPointer {
                pointer_id,
                rect,
                from_x_bp,
                from_y_bp,
                to_x_bp,
                to_y_bp,
                steps,
            } => {
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                Self::try_queue_swipe_pointer_into(
                    touch_batch,
                    TouchPointerId::new(*pointer_id),
                    self.point_to_pixels(rect.try_point_at_basis_points(*from_x_bp, *from_y_bp)?),
                    self.point_to_pixels(rect.try_point_at_basis_points(*to_x_bp, *to_y_bp)?),
                    *steps,
                )
            }
            AgentAction::Pinch {
                first_from,
                first_to,
                second_from,
                second_to,
                steps,
            } => {
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                Self::try_queue_pinch_into(
                    touch_batch,
                    *first_from,
                    *first_to,
                    *second_from,
                    *second_to,
                    *steps,
                )
            }
            AgentAction::PinchPoints {
                first_from,
                first_to,
                second_from,
                second_to,
                steps,
            } => {
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                Self::try_queue_pinch_into(
                    touch_batch,
                    self.point_to_pixels(*first_from),
                    self.point_to_pixels(*first_to),
                    self.point_to_pixels(*second_from),
                    self.point_to_pixels(*second_to),
                    *steps,
                )
            }
            AgentAction::CancelTouch { pointer_id } => {
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                touch_batch.try_cancel(*pointer_id)
            }
            AgentAction::TouchFrames { len, frames } => {
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                Self::try_queue_agent_touch_frames_into(touch_batch, *len, frames)
            }
            AgentAction::ThreeFingerScreenshot => {
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                self.try_queue_three_finger_screenshot_into(touch_batch)
            }
            AgentAction::Key {
                scancode,
                pressed,
                mods,
            } => {
                touch_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                key_batch.try_key(*scancode, *pressed, *mods)
            }
            AgentAction::KeyTap { scancode, mods } => {
                touch_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                key_batch.try_tap_key(*scancode, *mods)
            }
            AgentAction::KeyboardChord { chord } => {
                touch_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                key_batch.try_chord(*chord)
            }
            AgentAction::KeyBatch { len, frames } => {
                touch_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                Self::try_queue_agent_key_frames_into(key_batch, *len, frames)
            }
            AgentAction::InjectKeycode {
                action,
                keycode,
                repeat,
                metastate,
            } => {
                touch_batch.try_flush()?;
                key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                android_key_batch.try_keycode(*action, *keycode, *repeat, *metastate)
            }
            AgentAction::AndroidKeyTap { keycode, metastate } => {
                touch_batch.try_flush()?;
                key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                android_key_batch.try_tap_keycode(*keycode, *metastate)
            }
            AgentAction::AndroidKeyBatch { len, frames } => {
                touch_batch.try_flush()?;
                key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                Self::try_queue_agent_android_key_frames_into(android_key_batch, *len, frames)
            }
            AgentAction::PressHome => {
                touch_batch.try_flush()?;
                key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                android_key_batch.try_key_event(AndroidKeyAction::DOWN, AndroidKeycode::HOME, 0, 0)
            }
            AgentAction::PressBack => {
                touch_batch.try_flush()?;
                key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                android_key_batch.try_key_event(AndroidKeyAction::DOWN, AndroidKeycode::BACK, 0, 0)
            }
            AgentAction::OpenRecents => {
                touch_batch.try_flush()?;
                key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                android_key_batch.try_key_event(
                    AndroidKeyAction::DOWN,
                    AndroidKeycode::APP_SWITCH,
                    0,
                    0,
                )
            }
            AgentAction::VolumeUp => {
                touch_batch.try_flush()?;
                key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                android_key_batch.try_key_event(
                    AndroidKeyAction::DOWN,
                    AndroidKeycode::VOLUME_UP,
                    0,
                    0,
                )
            }
            AgentAction::VolumeDown => {
                touch_batch.try_flush()?;
                key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                android_key_batch.try_key_event(
                    AndroidKeyAction::DOWN,
                    AndroidKeycode::VOLUME_DOWN,
                    0,
                    0,
                )
            }
            AgentAction::VolumeMute => {
                touch_batch.try_flush()?;
                key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                android_key_batch.try_key_event(
                    AndroidKeyAction::DOWN,
                    AndroidKeycode::VOLUME_MUTE,
                    0,
                    0,
                )
            }
            AgentAction::MouseMotion { dx, dy, buttons } => {
                touch_batch.try_flush()?;
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                mouse_batch.try_motion(*dx, *dy, *buttons)
            }
            AgentAction::MouseButtons { buttons } => {
                touch_batch.try_flush()?;
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                mouse_batch.try_buttons(*buttons)
            }
            AgentAction::MouseBatch { len, frames } => {
                touch_batch.try_flush()?;
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                Self::try_queue_agent_mouse_frames_into(mouse_batch, *len, frames)
            }
            AgentAction::Scroll {
                x,
                y,
                hscroll,
                vscroll,
                buttons,
            } => {
                touch_batch.try_flush()?;
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                scroll_batch.try_scroll_with_buttons(
                    *x,
                    *y,
                    *hscroll as f32,
                    *vscroll as f32,
                    *buttons,
                )
            }
            AgentAction::ScrollPoint {
                point,
                hscroll,
                vscroll,
                buttons,
            } => {
                touch_batch.try_flush()?;
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                let (x, y) = self.point_to_pixels(*point);
                scroll_batch.try_scroll_with_buttons(
                    x,
                    y,
                    *hscroll as f32,
                    *vscroll as f32,
                    *buttons,
                )
            }
            AgentAction::ScrollRect {
                rect,
                hscroll,
                vscroll,
                buttons,
            } => {
                touch_batch.try_flush()?;
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                let (x, y) = self.point_to_pixels(rect.center());
                scroll_batch.try_scroll_with_buttons(
                    x,
                    y,
                    *hscroll as f32,
                    *vscroll as f32,
                    *buttons,
                )
            }
            AgentAction::ScrollRectAt {
                rect,
                x_bp,
                y_bp,
                hscroll,
                vscroll,
                buttons,
            } => {
                touch_batch.try_flush()?;
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                let (x, y) = self.point_to_pixels(rect.try_point_at_basis_points(*x_bp, *y_bp)?);
                scroll_batch.try_scroll_with_buttons(
                    x,
                    y,
                    *hscroll as f32,
                    *vscroll as f32,
                    *buttons,
                )
            }
            AgentAction::ScrollBatch { len, frames } => {
                touch_batch.try_flush()?;
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                Self::try_queue_agent_scroll_frames_into(scroll_batch, *len, frames)
            }
            AgentAction::GamepadFrame { frame } => {
                touch_batch.try_flush()?;
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_push_dedupe(*frame)
            }
            AgentAction::GamepadFrameUnchecked { frame } => {
                touch_batch.try_flush()?;
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_push_unchecked(*frame)
            }
            AgentAction::GamepadFrameBatch { len, frames } => {
                touch_batch.try_flush()?;
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_push_dedupe_slice(*len, frames)
            }
            AgentAction::GamepadFrameBatchUnchecked { len, frames } => {
                touch_batch.try_flush()?;
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_push_unchecked_slice(*len, frames)
            }
            AgentAction::GamepadPackedFrame { frame } => {
                touch_batch.try_flush()?;
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_push_packed(*frame)
            }
            AgentAction::GamepadPackedFrameBatch { len, frames } => {
                touch_batch.try_flush()?;
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_push_packed_slice(*len, frames)
            }
            AgentAction::Wait(_)
            | AgentAction::LongPress { .. }
            | AgentAction::LongPressPointer { .. }
            | AgentAction::LongPressPoint { .. }
            | AgentAction::LongPressPointPointer { .. }
            | AgentAction::LongPressRect { .. }
            | AgentAction::LongPressRectAt { .. }
            | AgentAction::LongPressRectPointer { .. }
            | AgentAction::LongPressRectAtPointer { .. } => {
                touch_batch.try_flush()?;
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                Err(Error::SessionLifecycle(TIMED_ACTION_REQUIRES_BLOCKING))
            }
            AgentAction::Flush => {
                touch_batch.try_flush()?;
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                self.client.try_flush()
            }
            _ => {
                touch_batch.try_flush()?;
                key_batch.try_flush()?;
                android_key_batch.try_flush()?;
                mouse_batch.try_flush()?;
                gamepad_batch.try_flush()?;
                self.try_queue_action(action)
            }
        }
    }

    fn queue_tap(&self, x: i32, y: i32) -> Result<()> {
        self.queue_tap_pointer(TouchPointerId::finger(0), x, y)
    }

    fn queue_tap_pointer(&self, pointer_id: TouchPointerId, x: i32, y: i32) -> Result<()> {
        let mut batch = self.client.touch_frame_batcher();
        Self::queue_tap_pointer_into(&mut batch, pointer_id, x, y)?;
        batch.flush()
    }

    fn queue_double_tap(&self, x: i32, y: i32) -> Result<()> {
        self.queue_double_tap_pointer(TouchPointerId::finger(0), x, y)
    }

    fn queue_double_tap_pointer(&self, pointer_id: TouchPointerId, x: i32, y: i32) -> Result<()> {
        let mut batch = self.client.touch_frame_batcher();
        Self::queue_double_tap_pointer_into(&mut batch, pointer_id, x, y)?;
        batch.flush()
    }

    fn queue_long_press(&self, x: i32, y: i32, duration: Duration) -> Result<()> {
        self.queue_long_press_pointer(TouchPointerId::finger(0), x, y, duration)
    }

    fn queue_long_press_pointer(
        &self,
        pointer_id: TouchPointerId,
        x: i32,
        y: i32,
        duration: Duration,
    ) -> Result<()> {
        {
            let mut batch = self.client.touch_frame_batcher();
            batch.down_pointer(pointer_id, x, y, 1.0)?;
            batch.flush()?;
        }
        self.flush()?;
        std::thread::sleep(duration);
        let mut batch = self.client.touch_frame_batcher();
        batch.up_pointer(pointer_id, x, y)?;
        batch.flush()
    }

    fn queue_swipe(&self, from: (i32, i32), to: (i32, i32), steps: usize) -> Result<()> {
        self.queue_swipe_pointer(TouchPointerId::finger(0), from, to, steps)
    }

    fn queue_swipe_pointer(
        &self,
        pointer_id: TouchPointerId,
        from: (i32, i32),
        to: (i32, i32),
        steps: usize,
    ) -> Result<()> {
        let mut batch = self.client.touch_frame_batcher();
        Self::queue_swipe_pointer_into(&mut batch, pointer_id, from, to, steps)?;
        batch.flush()
    }

    fn queue_pinch(
        &self,
        first_from: (i32, i32),
        first_to: (i32, i32),
        second_from: (i32, i32),
        second_to: (i32, i32),
        steps: usize,
    ) -> Result<()> {
        let mut batch = self.client.touch_frame_batcher();
        Self::queue_pinch_into(
            &mut batch,
            first_from,
            first_to,
            second_from,
            second_to,
            steps,
        )?;
        batch.flush()
    }

    fn queue_three_finger_screenshot(&self) -> Result<()> {
        let mut batch = self.client.touch_frame_batcher();
        self.queue_three_finger_screenshot_into(&mut batch)?;
        batch.flush()
    }

    fn try_queue_tap(&self, x: i32, y: i32) -> Result<()> {
        self.try_queue_tap_pointer(TouchPointerId::finger(0), x, y)
    }

    fn try_queue_tap_pointer(&self, pointer_id: TouchPointerId, x: i32, y: i32) -> Result<()> {
        let mut batch = self.client.touch_frame_batcher();
        Self::try_queue_tap_pointer_into(&mut batch, pointer_id, x, y)?;
        batch.try_flush()
    }

    fn try_queue_double_tap(&self, x: i32, y: i32) -> Result<()> {
        self.try_queue_double_tap_pointer(TouchPointerId::finger(0), x, y)
    }

    fn try_queue_double_tap_pointer(
        &self,
        pointer_id: TouchPointerId,
        x: i32,
        y: i32,
    ) -> Result<()> {
        let mut batch = self.client.touch_frame_batcher();
        Self::try_queue_double_tap_pointer_into(&mut batch, pointer_id, x, y)?;
        batch.try_flush()
    }

    fn try_queue_swipe(&self, from: (i32, i32), to: (i32, i32), steps: usize) -> Result<()> {
        self.try_queue_swipe_pointer(TouchPointerId::finger(0), from, to, steps)
    }

    fn try_queue_swipe_pointer(
        &self,
        pointer_id: TouchPointerId,
        from: (i32, i32),
        to: (i32, i32),
        steps: usize,
    ) -> Result<()> {
        let mut batch = self.client.touch_frame_batcher();
        Self::try_queue_swipe_pointer_into(&mut batch, pointer_id, from, to, steps)?;
        batch.try_flush()
    }

    fn try_queue_pinch(
        &self,
        first_from: (i32, i32),
        first_to: (i32, i32),
        second_from: (i32, i32),
        second_to: (i32, i32),
        steps: usize,
    ) -> Result<()> {
        let mut batch = self.client.touch_frame_batcher();
        Self::try_queue_pinch_into(
            &mut batch,
            first_from,
            first_to,
            second_from,
            second_to,
            steps,
        )?;
        batch.try_flush()
    }

    fn try_queue_three_finger_screenshot(&self) -> Result<()> {
        let mut batch = self.client.touch_frame_batcher();
        self.try_queue_three_finger_screenshot_into(&mut batch)?;
        batch.try_flush()
    }

    fn queue_agent_touch_frames_into(
        batch: &mut TouchFrameBatcher<'_>,
        len: usize,
        frames: &[AgentTouchFrame; TOUCH_BATCH_FRAMES],
    ) -> Result<()> {
        if len > TOUCH_BATCH_FRAMES {
            return Err(Error::SessionLifecycle("touch frame batch length overflow"));
        }
        let mut converted = [TouchFrame::EMPTY; TOUCH_BATCH_FRAMES];
        for (dst, src) in converted.iter_mut().zip(frames.iter()).take(len) {
            *dst = src.into_touch_frame();
        }
        batch.push_many_slice(&converted[..len])
    }

    fn try_queue_agent_touch_frames_into(
        batch: &mut TouchFrameBatcher<'_>,
        len: usize,
        frames: &[AgentTouchFrame; TOUCH_BATCH_FRAMES],
    ) -> Result<()> {
        if len > TOUCH_BATCH_FRAMES {
            return Err(Error::SessionLifecycle("touch frame batch length overflow"));
        }
        let mut converted = [TouchFrame::EMPTY; TOUCH_BATCH_FRAMES];
        for (dst, src) in converted.iter_mut().zip(frames.iter()).take(len) {
            *dst = src.into_touch_frame();
        }
        batch.try_push_many_slice(&converted[..len])
    }

    fn queue_agent_key_frames_into(
        batch: &mut KeyboardFrameBatcher<'_>,
        len: usize,
        frames: &[KeyboardFrame; KEYBOARD_BATCH_FRAMES],
    ) -> Result<()> {
        if len > KEYBOARD_BATCH_FRAMES {
            return Err(Error::SessionLifecycle("keyboard batch length overflow"));
        }
        batch.push_many_slice(&frames[..len])
    }

    fn try_queue_agent_key_frames_into(
        batch: &mut KeyboardFrameBatcher<'_>,
        len: usize,
        frames: &[KeyboardFrame; KEYBOARD_BATCH_FRAMES],
    ) -> Result<()> {
        if len > KEYBOARD_BATCH_FRAMES {
            return Err(Error::SessionLifecycle("keyboard batch length overflow"));
        }
        batch.try_push_many_slice(&frames[..len])
    }

    fn queue_agent_android_key_frames_into(
        batch: &mut AndroidKeyFrameBatcher<'_>,
        len: usize,
        frames: &[AndroidKeyFrame; ANDROID_KEY_BATCH_FRAMES],
    ) -> Result<()> {
        if len > ANDROID_KEY_BATCH_FRAMES {
            return Err(Error::SessionLifecycle("android key batch length overflow"));
        }
        batch.push_many_slice(&frames[..len])
    }

    fn try_queue_agent_android_key_frames_into(
        batch: &mut AndroidKeyFrameBatcher<'_>,
        len: usize,
        frames: &[AndroidKeyFrame; ANDROID_KEY_BATCH_FRAMES],
    ) -> Result<()> {
        if len > ANDROID_KEY_BATCH_FRAMES {
            return Err(Error::SessionLifecycle("android key batch length overflow"));
        }
        batch.try_push_many_slice(&frames[..len])
    }

    fn queue_agent_mouse_frames_into(
        batch: &mut MouseFrameBatcher<'_>,
        len: usize,
        frames: &[MouseFrame; MOUSE_BATCH_FRAMES],
    ) -> Result<()> {
        if len > MOUSE_BATCH_FRAMES {
            return Err(Error::SessionLifecycle("mouse batch length overflow"));
        }
        batch.push_many_slice(&frames[..len])
    }

    fn try_queue_agent_mouse_frames_into(
        batch: &mut MouseFrameBatcher<'_>,
        len: usize,
        frames: &[MouseFrame; MOUSE_BATCH_FRAMES],
    ) -> Result<()> {
        if len > MOUSE_BATCH_FRAMES {
            return Err(Error::SessionLifecycle("mouse batch length overflow"));
        }
        batch.try_push_many_slice(&frames[..len])
    }

    fn queue_agent_scroll_frames_into(
        batch: &mut ScrollFrameBatcher<'_>,
        len: usize,
        frames: &[AgentScrollFrame; SCROLL_BATCH_FRAMES],
    ) -> Result<()> {
        if len > SCROLL_BATCH_FRAMES {
            return Err(Error::SessionLifecycle("scroll batch length overflow"));
        }
        let mut converted = [ScrollFrame::EMPTY; SCROLL_BATCH_FRAMES];
        for (dst, src) in converted.iter_mut().zip(frames.iter()).take(len) {
            *dst = src.into_scroll_frame();
        }
        batch.push_many_slice(&converted[..len])
    }

    fn try_queue_agent_scroll_frames_into(
        batch: &mut ScrollFrameBatcher<'_>,
        len: usize,
        frames: &[AgentScrollFrame; SCROLL_BATCH_FRAMES],
    ) -> Result<()> {
        if len > SCROLL_BATCH_FRAMES {
            return Err(Error::SessionLifecycle("scroll batch length overflow"));
        }
        let mut converted = [ScrollFrame::EMPTY; SCROLL_BATCH_FRAMES];
        for (dst, src) in converted.iter_mut().zip(frames.iter()).take(len) {
            *dst = src.into_scroll_frame();
        }
        batch.try_push_many_slice(&converted[..len])
    }

    fn queue_tap_into(batch: &mut TouchFrameBatcher<'_>, x: i32, y: i32) -> Result<()> {
        Self::queue_tap_pointer_into(batch, TouchPointerId::finger(0), x, y)
    }

    fn queue_tap_pointer_into(
        batch: &mut TouchFrameBatcher<'_>,
        pointer_id: TouchPointerId,
        x: i32,
        y: i32,
    ) -> Result<()> {
        batch.down_pointer(pointer_id, x, y, 1.0)?;
        batch.up_pointer(pointer_id, x, y)
    }

    fn try_queue_tap_into(batch: &mut TouchFrameBatcher<'_>, x: i32, y: i32) -> Result<()> {
        Self::try_queue_tap_pointer_into(batch, TouchPointerId::finger(0), x, y)
    }

    fn try_queue_tap_pointer_into(
        batch: &mut TouchFrameBatcher<'_>,
        pointer_id: TouchPointerId,
        x: i32,
        y: i32,
    ) -> Result<()> {
        batch.try_down_pointer(pointer_id, x, y, 1.0)?;
        batch.try_up_pointer(pointer_id, x, y)
    }

    fn queue_double_tap_into(batch: &mut TouchFrameBatcher<'_>, x: i32, y: i32) -> Result<()> {
        Self::queue_double_tap_pointer_into(batch, TouchPointerId::finger(0), x, y)
    }

    fn queue_double_tap_pointer_into(
        batch: &mut TouchFrameBatcher<'_>,
        pointer_id: TouchPointerId,
        x: i32,
        y: i32,
    ) -> Result<()> {
        batch.down_pointer(pointer_id, x, y, 1.0)?;
        batch.up_pointer(pointer_id, x, y)?;
        batch.down_pointer(pointer_id, x, y, 1.0)?;
        batch.up_pointer(pointer_id, x, y)
    }

    fn try_queue_double_tap_into(batch: &mut TouchFrameBatcher<'_>, x: i32, y: i32) -> Result<()> {
        Self::try_queue_double_tap_pointer_into(batch, TouchPointerId::finger(0), x, y)
    }

    fn try_queue_double_tap_pointer_into(
        batch: &mut TouchFrameBatcher<'_>,
        pointer_id: TouchPointerId,
        x: i32,
        y: i32,
    ) -> Result<()> {
        batch.try_down_pointer(pointer_id, x, y, 1.0)?;
        batch.try_up_pointer(pointer_id, x, y)?;
        batch.try_down_pointer(pointer_id, x, y, 1.0)?;
        batch.try_up_pointer(pointer_id, x, y)
    }

    fn queue_swipe_into(
        batch: &mut TouchFrameBatcher<'_>,
        from: (i32, i32),
        to: (i32, i32),
        steps: usize,
    ) -> Result<()> {
        Self::queue_swipe_pointer_into(batch, TouchPointerId::finger(0), from, to, steps)
    }

    fn queue_swipe_pointer_into(
        batch: &mut TouchFrameBatcher<'_>,
        pointer_id: TouchPointerId,
        from: (i32, i32),
        to: (i32, i32),
        steps: usize,
    ) -> Result<()> {
        let steps = steps.max(1);
        batch.down_pointer(pointer_id, from.0, from.1, 1.0)?;
        for i in 1..=steps {
            let t = i as f32 / steps as f32;
            let x = lerp_i32(from.0, to.0, t);
            let y = lerp_i32(from.1, to.1, t);
            batch.move_pointer_to(pointer_id, x, y, 1.0)?;
        }
        batch.up_pointer(pointer_id, to.0, to.1)
    }

    fn try_queue_swipe_into(
        batch: &mut TouchFrameBatcher<'_>,
        from: (i32, i32),
        to: (i32, i32),
        steps: usize,
    ) -> Result<()> {
        Self::try_queue_swipe_pointer_into(batch, TouchPointerId::finger(0), from, to, steps)
    }

    fn try_queue_swipe_pointer_into(
        batch: &mut TouchFrameBatcher<'_>,
        pointer_id: TouchPointerId,
        from: (i32, i32),
        to: (i32, i32),
        steps: usize,
    ) -> Result<()> {
        let steps = steps.max(1);
        batch.try_down_pointer(pointer_id, from.0, from.1, 1.0)?;
        for i in 1..=steps {
            let t = i as f32 / steps as f32;
            let x = lerp_i32(from.0, to.0, t);
            let y = lerp_i32(from.1, to.1, t);
            batch.try_move_pointer_to(pointer_id, x, y, 1.0)?;
        }
        batch.try_up_pointer(pointer_id, to.0, to.1)
    }

    fn queue_pinch_into(
        batch: &mut TouchFrameBatcher<'_>,
        first_from: (i32, i32),
        first_to: (i32, i32),
        second_from: (i32, i32),
        second_to: (i32, i32),
        steps: usize,
    ) -> Result<()> {
        let steps = steps.max(1);
        batch.down(0, first_from.0, first_from.1, 1.0)?;
        batch.down(1, second_from.0, second_from.1, 1.0)?;
        for i in 1..=steps {
            let t = i as f32 / steps as f32;
            batch.move_to(
                0,
                lerp_i32(first_from.0, first_to.0, t),
                lerp_i32(first_from.1, first_to.1, t),
                1.0,
            )?;
            batch.move_to(
                1,
                lerp_i32(second_from.0, second_to.0, t),
                lerp_i32(second_from.1, second_to.1, t),
                1.0,
            )?;
        }
        batch.up(0, first_to.0, first_to.1)?;
        batch.up(1, second_to.0, second_to.1)
    }

    fn try_queue_pinch_into(
        batch: &mut TouchFrameBatcher<'_>,
        first_from: (i32, i32),
        first_to: (i32, i32),
        second_from: (i32, i32),
        second_to: (i32, i32),
        steps: usize,
    ) -> Result<()> {
        let steps = steps.max(1);
        batch.try_down(0, first_from.0, first_from.1, 1.0)?;
        batch.try_down(1, second_from.0, second_from.1, 1.0)?;
        for i in 1..=steps {
            let t = i as f32 / steps as f32;
            batch.try_move_to(
                0,
                lerp_i32(first_from.0, first_to.0, t),
                lerp_i32(first_from.1, first_to.1, t),
                1.0,
            )?;
            batch.try_move_to(
                1,
                lerp_i32(second_from.0, second_to.0, t),
                lerp_i32(second_from.1, second_to.1, t),
                1.0,
            )?;
        }
        batch.try_up(0, first_to.0, first_to.1)?;
        batch.try_up(1, second_to.0, second_to.1)
    }

    fn queue_three_finger_screenshot_into(&self, batch: &mut TouchFrameBatcher<'_>) -> Result<()> {
        let (screen_w, screen_h) = self.screen_size();
        let w = screen_w as i32;
        let h = screen_h as i32;
        for id in 0u64..3 {
            batch.down(id, w / 4 * (id as i32 + 1), h / 4, 1.0)?;
        }
        for step in 1..=10 {
            for id in 0u64..3 {
                batch.move_to(
                    id,
                    w / 4 * (id as i32 + 1),
                    h / 4 + (h / 2 * step / 10),
                    1.0,
                )?;
            }
        }
        for id in 0u64..3 {
            batch.up(id, w / 4 * (id as i32 + 1), h * 3 / 4)?;
        }
        Ok(())
    }

    fn try_queue_three_finger_screenshot_into(
        &self,
        batch: &mut TouchFrameBatcher<'_>,
    ) -> Result<()> {
        let (screen_w, screen_h) = self.screen_size();
        let w = screen_w as i32;
        let h = screen_h as i32;
        for id in 0u64..3 {
            batch.try_down(id, w / 4 * (id as i32 + 1), h / 4, 1.0)?;
        }
        for step in 1..=10 {
            for id in 0u64..3 {
                batch.try_move_to(
                    id,
                    w / 4 * (id as i32 + 1),
                    h / 4 + (h / 2 * step / 10),
                    1.0,
                )?;
            }
        }
        for id in 0u64..3 {
            batch.try_up(id, w / 4 * (id as i32 + 1), h * 3 / 4)?;
        }
        Ok(())
    }
}
