//! Agent-friendly high-level control/session facade.
//!
//! This module combines the write side (`HidClient` + dispatcher thread) with
//! the read side (`DeviceMessageReceiver`) so an AI agent can keep cheap cloned
//! command producers while retaining a single, byte-aligned device-message
//! reader.

use std::io::Read;
use std::sync::atomic::AtomicU16;

use crate::client::{HidClient, HidDispatcher};
use crate::device::DeviceMessageReceiver;
use crate::error::{Result, TransportWrite};

mod action;
mod estimator;
mod geometry;
mod session;
mod session_tcp;
mod types;

pub use action::{
    AgentAction, AgentPlanBoundedPrefix, AgentPlanBoundedPrefixStop, AgentPlanSummary,
};
pub use estimator::PlanCommandEstimator;
pub use types::{
    AgentObjectSelector, AgentPoint, AgentRect, AgentScrollFrame, AgentTargetSelector,
    AgentTouchFrame,
};

/// Default bound for the agent command channel.
pub const DEFAULT_AGENT_COMMAND_BOUND: usize = crate::client::DEFAULT_CHANNEL_BOUND;
/// Default touch metadata width, matching `HidSession`.
pub const DEFAULT_AGENT_SCREEN_WIDTH: u16 = 1080;
/// Default touch metadata height, matching `HidSession`.
pub const DEFAULT_AGENT_SCREEN_HEIGHT: u16 = 1920;

// The following constants are used as error-message fragments in agent control
// plan bounds checks. They follow SCREAMING_SNAKE_CASE to mirror the
// scrcpy-side identifiers they embed; suppress the `non_snake_case` lint for
// this module so the `use` imports in submodules do not warn.
#[allow(non_snake_case)]
const TIMED_ACTION_REQUIRES_BLOCKING: &str = "timed action requires queue_actions or run_actions";
const STRICT_TEXT_UNSUPPORTED: &str = "unsupported char in type_text_strict";
const LAUNCH_APP_NAME_TOO_LONG: &str = "launch app name too long";
const TRY_RUN_EXCEEDS_COMMAND_BOUND: &str = "try_run_actions exceeds command bound";
const TRY_TAP_EXCEEDS_COMMAND_BOUND: &str = "try_tap exceeds command bound";
const TRY_DOUBLE_TAP_EXCEEDS_COMMAND_BOUND: &str = "try_double_tap exceeds command bound";
const TRY_SCROLL_EXCEEDS_COMMAND_BOUND: &str = "try_scroll exceeds command bound";
const TRY_KEY_EXCEEDS_COMMAND_BOUND: &str = "try_key exceeds command bound";
const TRY_ANDROID_KEY_EXCEEDS_COMMAND_BOUND: &str = "try_android_key exceeds command bound";
const TRY_MOUSE_EXCEEDS_COMMAND_BOUND: &str = "try_mouse exceeds command bound";
const TRY_GAMEPAD_EXCEEDS_COMMAND_BOUND: &str = "try_gamepad exceeds command bound";
const TRY_CONTROL_EXCEEDS_COMMAND_BOUND: &str = "try_control exceeds command bound";
const TRY_AI_EXCEEDS_COMMAND_BOUND: &str = "try_ai exceeds command bound";
const TRY_CLIPBOARD_EXCEEDS_COMMAND_BOUND: &str = "try_clipboard exceeds command bound";

/// One owned agent control session.
pub struct AgentControlSession<T: TransportWrite + Send + 'static, R: Read> {
    client: HidClient,
    dispatcher: Option<HidDispatcher<T>>,
    receiver: Option<DeviceMessageReceiver<R>>,
    command_bound: usize,
    next_clipboard_sequence: u64,
    screen_width: AtomicU16,
    screen_height: AtomicU16,
}

impl<T: TransportWrite + Send + 'static, R: Read> std::fmt::Debug for AgentControlSession<T, R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentControlSession")
            .field("client", &self.client)
            .finish_non_exhaustive()
    }
}

/// Resources recovered after closing an [`AgentControlSession`].
#[derive(Debug)]
pub struct AgentControlClosed<T, R> {
    pub transport: T,
    pub reader: R,
}

/// Resources recovered from a checked close plus the dispatcher command result.
#[derive(Debug)]
pub struct AgentControlCloseReport<T, R> {
    pub closed: AgentControlClosed<T, R>,
    pub command_result: Result<()>,
}

/// Result of closing an agent after its reader has been detached.
#[derive(Debug)]
pub struct AgentControlCommandCloseReport<T> {
    pub transport: T,
    pub command_result: Result<()>,
}

impl<T, R> AgentControlCloseReport<T, R> {
    /// Convert into the recovered resources, returning the dispatcher command
    /// error if one was observed before shutdown.
    pub fn into_result(self) -> Result<AgentControlClosed<T, R>> {
        self.command_result.map(|()| self.closed)
    }
}

impl<T> AgentControlCommandCloseReport<T> {
    /// Convert into the recovered write transport, returning the dispatcher
    /// command error if one was observed before shutdown.
    pub fn into_result(self) -> Result<T> {
        self.command_result.map(|()| self.transport)
    }
}

#[cfg(test)]
mod tests;
