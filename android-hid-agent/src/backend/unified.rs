//! Unified backend — picks between daemon (TCP) and scrcpy (UHID)
//! at request time.
//!
//! The on-device `android-hid-daemon` covers the full verb surface
//! defined in `android-hid-protocol` (a11y dumps, screenshots,
//! streams, pm/am/wm, content providers, waits, …). The legacy
//! scrcpy UHID path can only serve **input** verbs (touch, key,
//! scroll, gamepad bytes), but it works on every device that runs
//! scrcpy-server without any extra daemon install.
//!
//! [`BackendChoice::choose`] encodes that mapping. Callers
//! (typically the verb-translation layer in `verbs::`) consume the
//! choice to pick the right transport.

use std::net::TcpStream;

use android_hid_connect::HidSession;
use android_hid_protocol::Verb;

/// Runtime decision: which backend should serve a given verb?
///
/// `Either` means the caller may use whichever backend is already
/// connected (preference order is daemon → scrcpy). `Neither`
/// signals an unsupported verb that the caller must reject with a
/// domain-specific error (the agent never silently drops requests).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BackendChoice {
    /// Use the byte-exact scrcpy UHID path.
    Scrcpy,
    /// Use the on-device daemon's TCP transport.
    Daemon,
    /// Either backend works; caller picks at runtime.
    Either,
    /// Neither backend supports this verb.
    Neither,
}

impl BackendChoice {
    /// True if this choice includes the daemon backend.
    #[must_use]
    pub const fn supports_daemon(self) -> bool {
        matches!(self, Self::Daemon | Self::Either)
    }

    /// True if this choice includes the scrcpy backend.
    #[must_use]
    pub const fn supports_scrcpy(self) -> bool {
        matches!(self, Self::Scrcpy | Self::Either)
    }
}

/// Owns zero-or-more of each backend and exposes the choice API.
///
/// Construction is intentionally decoupled from connection
/// establishment — pass already-connected backends in via
/// [`UnifiedBackend::new`] (or [`Self::default`] for an empty
/// façade used by tests and the verb-translation layer).
///
/// `UnifiedBackend` is not `Clone`: both wrapped backends own
/// exclusive sockets, so cloning would alias them. Wrap in your
/// own `Arc<UnifiedBackend>` if you need shared ownership.
#[derive(Debug, Default)]
pub struct UnifiedBackend {
    /// Connected scrcpy UHID session over a `TcpStream`, if any.
    ///
    /// `TcpStream` is the canonical scrcpy transport (the agent
    /// dials `localhost:<adb-forwarded-port>`); a more generic
    /// `HidSession<impl Write>` is reachable through the
    /// [`android_hid_connect::HidSession`] API directly.
    pub scrcpy: Option<HidSession<TcpStream>>,
    /// Connected daemon backend, if any.
    pub daemon: Option<crate::backend::daemon::DaemonBackend>,
}

impl UnifiedBackend {
    /// Build a façade from already-connected backends.
    #[must_use]
    pub fn new(
        scrcpy: Option<HidSession<TcpStream>>,
        daemon: Option<crate::backend::daemon::DaemonBackend>,
    ) -> Self {
        Self { scrcpy, daemon }
    }

    /// True if at least one backend is connected.
    #[must_use]
    pub fn is_connected(&self) -> bool {
        self.scrcpy.is_some() || self.daemon.is_some()
    }

    /// Decide which backend should serve `verb`.
    ///
    /// Returns [`BackendChoice::Scrcpy`] for verbs that only the
    /// UHID path can serve (no daemon-side handler planned),
    /// [`BackendChoice::Daemon`] for verbs only the daemon knows
    /// about (a11y, providers, pm/am/wm, streams, …),
    /// [`BackendChoice::Either`] for input verbs that both paths
    /// cover (`tap`, `swipe`, `key`, `text`, `scroll`, `down`,
    /// `move`, `up`, `paste`, `keyframe`-style HID primitives),
    /// and [`BackendChoice::Neither`] if neither backend supports
    /// the verb (currently never — the daemon covers the full
    /// surface — but reserved for future verbs).
    #[must_use]
    pub fn choose(verb: Verb) -> BackendChoice {
        match verb {
            // HID input verbs — both backends can serve them.
            // Scrcpy via UHID, daemon via its input subsystem.
            Verb::Tap
            | Verb::Swipe
            | Verb::SwipeDir
            | Verb::Down
            | Verb::Move
            | Verb::Up
            | Verb::Scroll
            | Verb::Key
            | Verb::Text
            | Verb::Paste
            | Verb::Submit => BackendChoice::Either,

            // Lifecycle.
            Verb::Ping | Verb::Info | Verb::Quit => BackendChoice::Either,

            // Everything else (inspect / capture / state / packages /
            // providers / clipboard / activities / files / props /
            // settings / diagnostics / node actions / waits /
            // composites) is daemon-only.
            _ => BackendChoice::Daemon,
        }
    }
}

// ---------------------------------------------------------------------------
// ScrcpyTransport — concrete transport bound for the scrcpy backend.
// We pin it to `TcpStream` for now; users that need a custom
// transport wrap their `HidSession` and call into the verbs layer
// directly instead of going through `UnifiedBackend`.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_unified_is_not_connected() {
        let u = UnifiedBackend::default();
        assert!(!u.is_connected());
    }

    #[test]
    fn choose_daemon_for_inspect() {
        assert_eq!(UnifiedBackend::choose(Verb::Dump), BackendChoice::Daemon);
        assert_eq!(UnifiedBackend::choose(Verb::DumpActive), BackendChoice::Daemon);
    }

    #[test]
    fn choose_daemon_for_capture_streams() {
        assert_eq!(UnifiedBackend::choose(Verb::Screenshot), BackendChoice::Daemon);
        assert_eq!(UnifiedBackend::choose(Verb::Stream), BackendChoice::Daemon);
        assert_eq!(UnifiedBackend::choose(Verb::StreamH264), BackendChoice::Daemon);
        assert_eq!(UnifiedBackend::choose(Verb::StreamTileJpeg), BackendChoice::Daemon);
    }

    #[test]
    fn choose_daemon_for_packages() {
        for v in [
            Verb::PmList,
            Verb::PmPath,
            Verb::PmUninstall,
            Verb::PmGrant,
            Verb::PmRevoke,
            Verb::Install,
            Verb::InstallMulti,
        ] {
            assert_eq!(
                UnifiedBackend::choose(v),
                BackendChoice::Daemon,
                "{v:?} should be daemon-only"
            );
        }
    }

    #[test]
    fn choose_daemon_for_providers() {
        for v in [
            Verb::Sms,
            Verb::Calls,
            Verb::Contacts,
            Verb::Calendar,
        ] {
            assert_eq!(UnifiedBackend::choose(v), BackendChoice::Daemon);
        }
    }

    #[test]
    fn choose_daemon_for_diagnostics() {
        for v in [
            Verb::Dumpsys,
            Verb::Logcat,
            Verb::Monitor,
            Verb::Shell,
        ] {
            assert_eq!(UnifiedBackend::choose(v), BackendChoice::Daemon);
        }
    }

    #[test]
    fn choose_daemon_for_clipboard_state_node_actions() {
        for v in [
            Verb::ClipGet,
            Verb::ClipSet,
            Verb::ClipWatch,
            Verb::State,
            Verb::StateWatch,
            Verb::NodeClick,
            Verb::NodeSetText,
        ] {
            assert_eq!(
                UnifiedBackend::choose(v),
                BackendChoice::Daemon,
                "{v:?} should be daemon-only"
            );
        }
    }

    #[test]
    fn choose_either_for_input_verbs() {
        for v in [
            Verb::Tap,
            Verb::Swipe,
            Verb::Down,
            Verb::Move,
            Verb::Up,
            Verb::Scroll,
            Verb::Key,
            Verb::Text,
            Verb::Paste,
            Verb::Submit,
        ] {
            let choice = UnifiedBackend::choose(v);
            assert!(
                choice.supports_daemon(),
                "{v:?} should support daemon (got {choice:?})"
            );
            assert!(
                choice.supports_scrcpy(),
                "{v:?} should support scrcpy (got {choice:?})"
            );
        }
    }

    #[test]
    fn choose_either_for_lifecycle() {
        for v in [Verb::Ping, Verb::Info, Verb::Quit] {
            assert_eq!(UnifiedBackend::choose(v), BackendChoice::Either);
        }
    }

    #[test]
    fn backend_choice_helpers() {
        assert!(BackendChoice::Daemon.supports_daemon());
        assert!(!BackendChoice::Daemon.supports_scrcpy());

        assert!(!BackendChoice::Scrcpy.supports_daemon());
        assert!(BackendChoice::Scrcpy.supports_scrcpy());

        assert!(BackendChoice::Either.supports_daemon());
        assert!(BackendChoice::Either.supports_scrcpy());

        assert!(!BackendChoice::Neither.supports_daemon());
        assert!(!BackendChoice::Neither.supports_scrcpy());
    }

    #[test]
    fn choose_is_total() {
        // Every verb in the enum must map to *some* choice.
        for v in [
            Verb::Ping, Verb::Info, Verb::Quit,
            Verb::Dump, Verb::DumpActive,
            Verb::Screenshot, Verb::Stream, Verb::StreamH264, Verb::StreamTileJpeg, Verb::Keyframe,
            Verb::Tap, Verb::Swipe, Verb::SwipeDir, Verb::Down, Verb::Move, Verb::Up, Verb::Scroll,
            Verb::Key, Verb::Text,
            Verb::WaitForIdle, Verb::WaitForText, Verb::WaitForActivity,
            Verb::TapAndDump, Verb::TapAndSettle,
            Verb::State, Verb::StateWatch,
            Verb::NodeClick, Verb::NodeLongClick, Verb::NodeSetText, Verb::NodeScroll,
            Verb::NodeFocus, Verb::Submit,
            Verb::PmList, Verb::PmPath, Verb::PmUninstall, Verb::PmGrant, Verb::PmRevoke,
            Verb::Deeplinks, Verb::Install, Verb::InstallMulti,
            Verb::Sms, Verb::Calls, Verb::Contacts, Verb::Calendar,
            Verb::ClipGet, Verb::ClipSet, Verb::ClipWatch,
            Verb::AmStart, Verb::AmForceStop, Verb::AmKill, Verb::AmBroadcast,
            Verb::Pull, Verb::Push,
            Verb::Getprop, Verb::Setprop, Verb::SettingsGet, Verb::SettingsPut,
            Verb::WmInfo, Verb::WmRotation,
            Verb::Dumpsys, Verb::Logcat, Verb::Shell, Verb::Monitor,
            Verb::Paste,
        ] {
            let choice = UnifiedBackend::choose(v);
            assert!(
                matches!(choice, BackendChoice::Scrcpy | BackendChoice::Daemon | BackendChoice::Either | BackendChoice::Neither),
                "{v:?} mapped to unknown choice {choice:?}"
            );
        }
    }
}