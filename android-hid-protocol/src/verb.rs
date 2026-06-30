//! Every wire verb the daemon dispatches.
//!
//! The full list mirrors `handsets/docs/wire.md` and is the canonical
//! vocabulary of the `android-hid-protocol` crate. Adding a new verb here
//! is a wire-protocol change and must be paired with a daemon-side handler
//! plus a round-trip test in this module.

use std::str::FromStr;

use thiserror::Error;

/// Every verb dispatched by the daemon, in the same groups used by
/// `handsets/docs/wire.md`. Unknown verbs are rejected with
/// [`ErrorCode::UnknownCmd`] on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[allow(non_camel_case_types)]
pub enum Verb {
    // ---- Lifecycle ----
    Ping,
    Info,
    Quit,

    // ---- Inspect (a11y) ----
    Dump,
    DumpActive,

    // ---- Capture ----
    Screenshot,
    Stream,
    StreamH264,
    StreamH265,
    StreamTileJpeg,
    StreamTileH265,
    Keyframe,

    // ---- Input ----
    Tap,
    Swipe,
    SwipeDir,
    Down,
    Move,
    Up,
    Scroll,
    Key,
    Text,

    // ---- Waits ----
    WaitForIdle,
    WaitForText,
    WaitForActivity,

    // ---- Composites ----
    TapAndDump,
    TapAndSettle,

    // ---- v2 atomic (UHID + a11y) ----
    /// Resolve selector on a11y tree, take center, tap, return dump.
    SelectAndTap,
    /// Project AI detection box onto a11y tree, tap the matched node.
    AiAnchorTap,
    /// Type text then wait_for_text in one call.
    TypeAndWait,
    /// Retry node_click until a11y tree contains the target text.
    ClickUntilText,
    /// dump_active + H.265 keyframe in one response.
    DumpAndFrame,
    /// Force H.265 keyframe and wait until it's been emitted.
    KeyframeAndWait,
    /// Pull H.265 VPS/SPS/PPS once.
    HevcParamSets,

    // ---- State ----
    State,
    StateWatch,

    // ---- Node actions ----
    NodeClick,
    NodeLongClick,
    NodeSetText,
    NodeScroll,
    NodeFocus,
    Submit,

    // ---- Packages ----
    PmList,
    PmPath,
    PmUninstall,
    PmGrant,
    PmRevoke,
    Deeplinks,
    Install,
    InstallMulti,

    // ---- User-data providers ----
    Sms,
    Calls,
    Contacts,
    Calendar,

    // ---- Clipboard ----
    ClipGet,
    ClipSet,
    ClipWatch,

    // ---- Activities ----
    AmStart,
    AmForceStop,
    AmKill,
    AmBroadcast,

    // ---- Files ----
    Pull,
    Push,

    // ---- Props / settings / system ----
    Getprop,
    Setprop,
    SettingsGet,
    SettingsPut,
    WmInfo,
    WmRotation,

    // ---- Diagnostics ----
    Dumpsys,
    Logcat,
    Shell,
    Monitor,

    // ---- Misc ----
    Paste,
}

/// Failure mode when parsing a verb name.
#[derive(Debug, Error, PartialEq, Eq)]
#[error("unknown verb: {0}")]
pub struct VerbParseError(pub String);

impl Verb {
    /// Wire spelling of this verb.
    pub const fn as_str(self) -> &'static str {
        match self {
            // Lifecycle
            Self::Ping => "ping",
            Self::Info => "info",
            Self::Quit => "quit",
            // Inspect
            Self::Dump => "dump",
            Self::DumpActive => "dump_active",
            // Capture
            Self::Screenshot => "screenshot",
            Self::Stream => "stream",
            Self::StreamH264 => "stream_h264",
            Self::StreamH265 => "stream_h265",
            Self::StreamTileJpeg => "stream_tilejpeg",
            Self::StreamTileH265 => "stream_tile_h265",
            Self::Keyframe => "keyframe",
            // Input
            Self::Tap => "tap",
            Self::Swipe => "swipe",
            Self::SwipeDir => "swipe_dir",
            Self::Down => "down",
            Self::Move => "move",
            Self::Up => "up",
            Self::Scroll => "scroll",
            Self::Key => "key",
            Self::Text => "text",
            // Waits
            Self::WaitForIdle => "wait_for_idle",
            Self::WaitForText => "wait_for_text",
            Self::WaitForActivity => "wait_for_activity",
            // Composites
            Self::TapAndDump => "tap_and_dump",
            Self::TapAndSettle => "tap_and_settle",
            // v2 atomic
            Self::SelectAndTap => "select_and_tap",
            Self::AiAnchorTap => "ai_anchor_tap",
            Self::TypeAndWait => "type_and_wait",
            Self::ClickUntilText => "click_until_text",
            Self::DumpAndFrame => "dump_and_frame",
            Self::KeyframeAndWait => "keyframe_and_wait",
            Self::HevcParamSets => "hevc_param_sets",
            // State
            Self::State => "state",
            Self::StateWatch => "state_watch",
            // Node actions
            Self::NodeClick => "node_click",
            Self::NodeLongClick => "node_long_click",
            Self::NodeSetText => "node_set_text",
            Self::NodeScroll => "node_scroll",
            Self::NodeFocus => "node_focus",
            Self::Submit => "submit",
            // Packages
            Self::PmList => "pm_list",
            Self::PmPath => "pm_path",
            Self::PmUninstall => "pm_uninstall",
            Self::PmGrant => "pm_grant",
            Self::PmRevoke => "pm_revoke",
            Self::Deeplinks => "deeplinks",
            Self::Install => "install",
            Self::InstallMulti => "install_multi",
            // User-data providers
            Self::Sms => "sms",
            Self::Calls => "calls",
            Self::Contacts => "contacts",
            Self::Calendar => "calendar",
            // Clipboard
            Self::ClipGet => "clip_get",
            Self::ClipSet => "clip_set",
            Self::ClipWatch => "clip_watch",
            // Activities
            Self::AmStart => "am_start",
            Self::AmForceStop => "am_force_stop",
            Self::AmKill => "am_kill",
            Self::AmBroadcast => "am_broadcast",
            // Files
            Self::Pull => "pull",
            Self::Push => "push",
            // Props / settings / system
            Self::Getprop => "getprop",
            Self::Setprop => "setprop",
            Self::SettingsGet => "settings_get",
            Self::SettingsPut => "settings_put",
            Self::WmInfo => "wm_info",
            Self::WmRotation => "wm_rotation",
            // Diagnostics
            Self::Dumpsys => "dumpsys",
            Self::Logcat => "logcat",
            Self::Shell => "shell",
            Self::Monitor => "monitor",
            // Misc
            Self::Paste => "paste",
        }
    }

    /// Parse a wire verb name into a typed [`Verb`].
    pub fn parse(s: &str) -> Result<Self, VerbParseError> {
        Ok(match s {
            "ping" => Self::Ping,
            "info" => Self::Info,
            "quit" => Self::Quit,
            "dump" => Self::Dump,
            "dump_active" => Self::DumpActive,
            "screenshot" => Self::Screenshot,
            "stream" => Self::Stream,
            "stream_h264" => Self::StreamH264,
            "stream_h265" => Self::StreamH265,
            "stream_tilejpeg" => Self::StreamTileJpeg,
            "stream_tile_h265" => Self::StreamTileH265,
            "keyframe" => Self::Keyframe,
            "tap" => Self::Tap,
            "swipe" => Self::Swipe,
            "swipe_dir" => Self::SwipeDir,
            "down" => Self::Down,
            "move" => Self::Move,
            "up" => Self::Up,
            "scroll" => Self::Scroll,
            "key" => Self::Key,
            "text" => Self::Text,
            "wait_for_idle" => Self::WaitForIdle,
            "wait_for_text" => Self::WaitForText,
            "wait_for_activity" => Self::WaitForActivity,
            "tap_and_dump" => Self::TapAndDump,
            "tap_and_settle" => Self::TapAndSettle,
            "select_and_tap" => Self::SelectAndTap,
            "ai_anchor_tap" => Self::AiAnchorTap,
            "type_and_wait" => Self::TypeAndWait,
            "click_until_text" => Self::ClickUntilText,
            "dump_and_frame" => Self::DumpAndFrame,
            "keyframe_and_wait" => Self::KeyframeAndWait,
            "hevc_param_sets" => Self::HevcParamSets,
            "state" => Self::State,
            "state_watch" => Self::StateWatch,
            "node_click" => Self::NodeClick,
            "node_long_click" => Self::NodeLongClick,
            "node_set_text" => Self::NodeSetText,
            "node_scroll" => Self::NodeScroll,
            "node_focus" => Self::NodeFocus,
            "submit" => Self::Submit,
            "pm_list" => Self::PmList,
            "pm_path" => Self::PmPath,
            "pm_uninstall" => Self::PmUninstall,
            "pm_grant" => Self::PmGrant,
            "pm_revoke" => Self::PmRevoke,
            "deeplinks" => Self::Deeplinks,
            "install" => Self::Install,
            "install_multi" => Self::InstallMulti,
            "sms" => Self::Sms,
            "calls" => Self::Calls,
            "contacts" => Self::Contacts,
            "calendar" => Self::Calendar,
            "clip_get" => Self::ClipGet,
            "clip_set" => Self::ClipSet,
            "clip_watch" => Self::ClipWatch,
            "am_start" => Self::AmStart,
            "am_force_stop" => Self::AmForceStop,
            "am_kill" => Self::AmKill,
            "am_broadcast" => Self::AmBroadcast,
            "pull" => Self::Pull,
            "push" => Self::Push,
            "getprop" => Self::Getprop,
            "setprop" => Self::Setprop,
            "settings_get" => Self::SettingsGet,
            "settings_put" => Self::SettingsPut,
            "wm_info" => Self::WmInfo,
            "wm_rotation" => Self::WmRotation,
            "dumpsys" => Self::Dumpsys,
            "logcat" => Self::Logcat,
            "shell" => Self::Shell,
            "monitor" => Self::Monitor,
            "paste" => Self::Paste,
            other => return Err(VerbParseError(other.to_owned())),
        })
    }

    /// True for verbs that write multiple frames instead of a single
    /// response.
    ///
    /// Streaming verbs end the multi-frame reply with a single
    /// [`Frame::empty_marker()`] terminator (see [`crate::frame`]), so
    /// callers must loop with [`crate::Frame::decode`] until they see
    /// the terminator rather than assuming one frame == one response.
    ///
    /// Includes capture streams, file `pull`, diagnostics, state
    /// watchers, clipboard watcher, and `shell`.
    pub const fn is_streaming(self) -> bool {
        matches!(
            self,
            Self::Stream
                | Self::StreamH264
                | Self::StreamH265
                | Self::StreamTileJpeg
                | Self::StreamTileH265
                | Self::Pull
                | Self::Dumpsys
                | Self::Logcat
                | Self::Monitor
                | Self::StateWatch
                | Self::ClipWatch
                | Self::Shell
        )
    }

    /// Inverse of [`Self::is_streaming`] — true for verbs that
    /// produce exactly one response frame per request.
    ///
    /// This is the default; the constant is provided so callers can
    /// write `if verb.is_unary() { call(...) } else { stream(...) }`
    /// without hard-coding the list twice.
    pub const fn is_unary(self) -> bool {
        !self.is_streaming()
    }
}

impl FromStr for Verb {
    type Err = VerbParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// All variants. Used by the round-trip test so we don't drift
    /// the test list when a new variant is added.
    const ALL: &[Verb] = &[
        Verb::Ping,
        Verb::Info,
        Verb::Quit,
        Verb::Dump,
        Verb::DumpActive,
        Verb::Screenshot,
        Verb::Stream,
        Verb::StreamH264,
        Verb::StreamTileJpeg,
        Verb::Keyframe,
        Verb::Tap,
        Verb::Swipe,
        Verb::SwipeDir,
        Verb::Down,
        Verb::Move,
        Verb::Up,
        Verb::Scroll,
        Verb::Key,
        Verb::Text,
        Verb::WaitForIdle,
        Verb::WaitForText,
        Verb::WaitForActivity,
        Verb::TapAndDump,
        Verb::TapAndSettle,
        Verb::State,
        Verb::StateWatch,
        Verb::NodeClick,
        Verb::NodeLongClick,
        Verb::NodeSetText,
        Verb::NodeScroll,
        Verb::NodeFocus,
        Verb::Submit,
        Verb::PmList,
        Verb::PmPath,
        Verb::PmUninstall,
        Verb::PmGrant,
        Verb::PmRevoke,
        Verb::Deeplinks,
        Verb::Install,
        Verb::InstallMulti,
        Verb::Sms,
        Verb::Calls,
        Verb::Contacts,
        Verb::Calendar,
        Verb::ClipGet,
        Verb::ClipSet,
        Verb::ClipWatch,
        Verb::AmStart,
        Verb::AmForceStop,
        Verb::AmKill,
        Verb::AmBroadcast,
        Verb::Pull,
        Verb::Push,
        Verb::Getprop,
        Verb::Setprop,
        Verb::SettingsGet,
        Verb::SettingsPut,
        Verb::WmInfo,
        Verb::WmRotation,
        Verb::Dumpsys,
        Verb::Logcat,
        Verb::Shell,
        Verb::Monitor,
        Verb::Paste,
    ];

    #[test]
    fn round_trip_every_variant() {
        assert!(ALL.len() >= 60, "have {} variants", ALL.len());
        for &v in ALL {
            let s = v.as_str();
            assert_eq!(Verb::parse(s), Ok(v), "round-trip failed for {v:?}");
        }
    }

    #[test]
    fn unknown_verb_is_error() {
        assert_eq!(
            Verb::parse("not_a_verb"),
            Err(VerbParseError("not_a_verb".to_owned()))
        );
        assert_eq!(
            Verb::parse(""),
            Err(VerbParseError(String::new()))
        );
    }

    #[test]
    fn fromstr_trait_works() {
        use std::str::FromStr;
        assert_eq!(Verb::from_str("ping"), Ok(Verb::Ping));
        assert_eq!(Verb::from_str("paste"), Ok(Verb::Paste));
    }

    #[test]
    fn is_streaming_matches_documented_set() {
        // Capture streams + push-pull + diagnostics + state/clipboard watchers + shell
        for v in [
            Verb::Stream,
            Verb::StreamH264,
            Verb::StreamTileJpeg,
            Verb::Pull,
            Verb::Dumpsys,
            Verb::Logcat,
            Verb::Monitor,
            Verb::StateWatch,
            Verb::ClipWatch,
            Verb::Shell,
        ] {
            assert!(v.is_streaming(), "{v:?} should be streaming");
            assert!(!v.is_unary(), "{v:?} should not be unary");
        }
    }

    #[test]
    fn is_unary_matches_documented_set() {
        // Spot-check a handful of unary verbs.
        for v in [
            Verb::Ping,
            Verb::Info,
            Verb::Quit,
            Verb::Tap,
            Verb::Swipe,
            Verb::Text,
            Verb::Dump,
            Verb::Screenshot,
            Verb::Paste,
        ] {
            assert!(v.is_unary(), "{v:?} should be unary");
            assert!(!v.is_streaming(), "{v:?} should not be streaming");
        }
    }

    #[test]
    fn streaming_set_is_disjoint_from_unary_set() {
        for &v in ALL {
            assert_ne!(
                v.is_streaming(),
                v.is_unary(),
                "{v:?} should be exactly one of streaming/unary"
            );
        }
    }
}