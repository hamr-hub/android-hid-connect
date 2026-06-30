//! Scenario-aware connection mode resolution.
//!
//! Different agent workloads want different transport topologies:
//!
//! | Scenario       | Backend                | Why                                    |
//! |----------------|------------------------|----------------------------------------|
//! | Gaming240Hz    | scrcpy direct realtime | bypass mpsc; 240Hz HID gamepad        |
//! | BulkText       | scrcpy direct coalesced| 1ms bucket + 32-frame batcher         |
//! | UiAutomation   | daemon TCP             | warm socket p50 1.34ms; no scrcpy     |
//! | VisionLoop     | dual socket            | atomic tap-and-dump in 1 RTT          |
//! | MultiDevice    | fanout                 | typed N-of-N session                  |
//! | Background     | daemon stream          | dumpsys/logcat/monitor long-lived     |
//! | AdbOnly        | adb subprocess         | fallback when neither daemon nor scrcpy|
//!
//! Choosing the topology up front (per session) is cheaper than
//! per-verb routing because it lets the connection layer skip the
//! un-needed hops. See `docs/agent-v2-design.md` §2 for the full
//! rationale.

use std::net::SocketAddr;

/// The workload the agent will run.
///
/// `Scenario` is a **session-level** decision, not a per-verb one. Once
/// the session is open the transport topology is fixed; per-verb
/// routing within a session still happens in
/// [`UnifiedBackend::choose`](crate::backend::unified::UnifiedBackend::choose).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Scenario {
    /// 60-240Hz gamepad control. Bypasses the mpsc dispatcher and
    /// writes HID reports directly to the scrcpy socket. No a11y
    /// support on this path — game loops never need it.
    Gaming240Hz,
    /// LLM bulk text injection (1k+ chars). Goes through the scrcpy
    /// UHID path with the 1ms coalescing window + 32-frame batcher
    /// to amortize per-character syscalls.
    BulkText,
    /// General tap / swipe / key / clipboard. Routed over the
    /// on-device daemon (warm socket) for sub-2ms p50 latency. No
    /// scrcy dependency.
    UiAutomation,
    /// See-then-act loop (LLM agent with vision + a11y). Holds both
    /// a scrcpy socket (for input) and a daemon socket (for
    /// screenshots + a11y dumps) so atomic ops like
    /// `select_and_tap` can be served in a single round-trip.
    VisionLoop,
    /// Multi-device fanout — typed session that owns N
    /// [`UnifiedBackend`](crate::backend::unified::UnifiedBackend)
    /// instances. The verb-translation layer round-robins or
    /// parallel-dispatches.
    MultiDevice,
    /// Background diagnostics (dumpsys / logcat / monitor). Long-lived
    /// daemon stream; the agent does not hold the writer end after the
    /// verb is sent.
    Background,
    /// Last-resort fallback: no daemon installed, no scrcpy-server
    /// pushed. Falls through to spawning `adb shell` per verb. Slow
    /// but always works on any device with `adb` access.
    AdbOnly,
}

impl Scenario {
    /// Short human label suitable for log lines and metrics tags.
    #[inline]
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Gaming240Hz => "gaming-240hz",
            Self::BulkText => "bulk-text",
            Self::UiAutomation => "ui-automation",
            Self::VisionLoop => "vision-loop",
            Self::MultiDevice => "multi-device",
            Self::Background => "background",
            Self::AdbOnly => "adb-only",
        }
    }

    /// True for scenarios that need a scrcpy UHID socket.
    #[inline]
    #[must_use]
    pub const fn uses_scrcpy(self) -> bool {
        matches!(self, Self::Gaming240Hz | Self::BulkText | Self::VisionLoop)
    }

    /// True for scenarios that need a daemon TCP socket.
    #[inline]
    #[must_use]
    pub const fn uses_daemon(self) -> bool {
        matches!(
            self,
            Self::UiAutomation | Self::VisionLoop | Self::MultiDevice | Self::Background
        )
    }

    /// True if this scenario allows the realtime direct-write
    /// fast-path (skip mpsc coalescing).
    #[inline]
    #[must_use]
    pub const fn allows_realtime(self) -> bool {
        matches!(self, Self::Gaming240Hz)
    }
}

/// Connection topology the session will use.
///
/// `ConnectionMode` is **derived** from a [`Scenario`] and a
/// [`ConnectionHints`]. Constructing a session picks one
/// `ConnectionMode` and never switches at runtime; if the user
/// needs a different topology they should open a second session.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ConnectionMode {
    /// Direct scrcpy UHID socket. `realtime = true` means
    /// [`crate::session::HidSession::gamepad_only_realtime`] mode
    /// (bypass coalescing); `realtime = false` keeps the 1ms
    /// coalescing window.
    ScrcpyDirect {
        /// Skip the 1ms coalescing window and write HID reports
        /// immediately. Only safe for sustained, predictable
        /// workloads (gamepad at 60+ Hz).
        realtime: bool,
    },
    /// Single daemon TCP socket. Used for unary + streaming verbs
    /// that the daemon covers (a11y, screenshots, providers, etc.).
    Daemon {
        /// Resolved socket address (typically `127.0.0.1:<forwarded port>`).
        addr: SocketAddr,
    },
    /// Both sockets held concurrently so atomic ops can combine
    /// scrcpy input + daemon read in one round-trip.
    DualSocket {
        /// scrcpy control socket (HID writes).
        scrcpy: SocketAddr,
        /// daemon control socket (a11y / screenshot / H.265).
        daemon: SocketAddr,
    },
    /// Multi-device fanout. The session owns N parallel
    /// connections; verb dispatch can be round-robin or pinned.
    Fanout {
        /// One socket address per device. Order is stable for
        /// pinned dispatch.
        addrs: Vec<SocketAddr>,
    },
    /// `adb shell` subprocess fallback. Slow but ubiquitous.
    AdbShell,
}

impl ConnectionMode {
    /// Derive the connection topology from a scenario + hints.
    ///
    /// Does not actually open a socket — see
    /// [`ConnectionHints::build_session`] for the real constructor.
    #[must_use]
    pub fn for_scenario(scenario: Scenario, hints: &ConnectionHints) -> Self {
        match scenario {
            Scenario::Gaming240Hz => Self::ScrcpyDirect { realtime: true },
            Scenario::BulkText => Self::ScrcpyDirect { realtime: false },
            Scenario::UiAutomation => Self::Daemon {
                addr: hints.daemon_addr,
            },
            Scenario::VisionLoop => Self::DualSocket {
                scrcpy: hints.scrcpy_addr,
                daemon: hints.daemon_addr,
            },
            Scenario::MultiDevice => Self::Fanout {
                addrs: hints.fanout_addrs.clone(),
            },
            Scenario::Background => Self::Daemon {
                addr: hints.daemon_addr,
            },
            Scenario::AdbOnly => Self::AdbShell,
        }
    }

    /// Short human label for log + metrics.
    #[inline]
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::ScrcpyDirect { realtime: true } => "scrcpy-direct-realtime",
            Self::ScrcpyDirect { realtime: false } => "scrcpy-direct-coalesced",
            Self::Daemon { .. } => "daemon",
            Self::DualSocket { .. } => "dual-socket",
            Self::Fanout { .. } => "fanout",
            Self::AdbShell => "adb-shell",
        }
    }

    /// True if this topology needs a scrcpy UHID socket.
    #[inline]
    #[must_use]
    pub const fn has_scrcpy(&self) -> bool {
        matches!(self, Self::ScrcpyDirect { .. } | Self::DualSocket { .. })
    }

    /// True if this topology needs a daemon TCP socket.
    #[inline]
    #[must_use]
    pub const fn has_daemon(&self) -> bool {
        matches!(
            self,
            Self::Daemon { .. } | Self::DualSocket { .. } | Self::Fanout { .. }
        )
    }
}

/// Per-environment connection hints supplied by the caller.
///
/// `ConnectionHints` is just a bag of addresses — the session
/// opener validates them and bails out early if a required
/// address is missing. Defaults target the standard scrcpy
/// (`27183`) and handsets-style daemon (`9008`) ports.
#[derive(Debug, Clone)]
pub struct ConnectionHints {
    /// scrcpy control socket (default `127.0.0.1:27183`).
    pub scrcpy_addr: SocketAddr,
    /// daemon TCP socket (default `127.0.0.1:9008`).
    pub daemon_addr: SocketAddr,
    /// For [`Scenario::MultiDevice`] — one entry per device.
    pub fanout_addrs: Vec<SocketAddr>,
    /// If `true`, opening a session that needs a missing backend
    /// returns [`SessionError::BackendUnavailable`] instead of
    /// silently substituting a fallback. Default `true`.
    pub strict: bool,
}

impl Default for ConnectionHints {
    fn default() -> Self {
        Self {
            scrcpy_addr: "127.0.0.1:27183"
                .parse()
                .expect("static scrcpy port is a valid SocketAddr"),
            daemon_addr: "127.0.0.1:9008"
                .parse()
                .expect("static daemon port is a valid SocketAddr"),
            fanout_addrs: Vec::new(),
            strict: true,
        }
    }
}

impl ConnectionHints {
    /// Build a hint bag with all scrcpy + daemon addresses
    /// pointing at loopback defaults.
    #[must_use]
    pub fn loopback() -> Self {
        Self::default()
    }

    /// Override the scrcpy socket address.
    #[must_use]
    pub fn with_scrcpy(mut self, addr: SocketAddr) -> Self {
        self.scrcpy_addr = addr;
        self
    }

    /// Override the daemon socket address.
    #[must_use]
    pub fn with_daemon(mut self, addr: SocketAddr) -> Self {
        self.daemon_addr = addr;
        self
    }

    /// Register a device for [`Scenario::MultiDevice`].
    pub fn push_fanout(&mut self, addr: SocketAddr) {
        self.fanout_addrs.push(addr);
    }

    /// Toggle strict mode (default `true`).
    #[must_use]
    pub fn with_strict(mut self, strict: bool) -> Self {
        self.strict = strict;
        self
    }
}

/// Session-open error. Returned by the future
/// `ConnectionHints::build_session` (Phase A landing).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionError {
    /// The scenario requires a scrcpy socket but `scrcpy_addr`
    /// could not be reached. Surfaces in `strict` mode only.
    ScrcpyUnavailable(SocketAddr),
    /// The scenario requires a daemon socket but `daemon_addr`
    /// could not be reached. Surfaces in `strict` mode only.
    DaemonUnavailable(SocketAddr),
    /// The scenario requires at least one fanout device but
    /// `fanout_addrs` is empty.
    FanoutEmpty,
    /// The hand-rolled wire greeting didn't match `PROTO/1\n` —
    /// either the daemon is the wrong version, or the listener
    /// is something else on the same port.
    HandshakeMismatch {
        /// The address we tried to dial.
        addr: SocketAddr,
        /// The bytes that came back (truncated to 8).
        actual: [u8; 8],
    },
    /// Underlying I/O error from the OS (connection refused,
    /// network unreachable, …).
    Io(String),
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ScrcpyUnavailable(addr) => {
                write!(f, "scrcpy socket {addr} unreachable (strict mode)")
            }
            Self::DaemonUnavailable(addr) => {
                write!(f, "daemon socket {addr} unreachable (strict mode)")
            }
            Self::FanoutEmpty => write!(f, "multi-device scenario needs ≥1 fanout addr"),
            Self::HandshakeMismatch { addr, actual } => write!(
                f,
                "daemon handshake mismatch on {addr}: expected PROTO/1\\n, got {}",
                String::from_utf8_lossy(actual)
            ),
            Self::Io(s) => write!(f, "session I/O error: {s}"),
        }
    }
}

impl std::error::Error for SessionError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scenario_as_str_is_stable() {
        // The label is part of the metrics surface; lock it in.
        assert_eq!(Scenario::Gaming240Hz.as_str(), "gaming-240hz");
        assert_eq!(Scenario::BulkText.as_str(), "bulk-text");
        assert_eq!(Scenario::UiAutomation.as_str(), "ui-automation");
        assert_eq!(Scenario::VisionLoop.as_str(), "vision-loop");
        assert_eq!(Scenario::MultiDevice.as_str(), "multi-device");
        assert_eq!(Scenario::Background.as_str(), "background");
        assert_eq!(Scenario::AdbOnly.as_str(), "adb-only");
    }

    #[test]
    fn gaming_uses_scrcpy_only() {
        assert!(Scenario::Gaming240Hz.uses_scrcpy());
        assert!(!Scenario::Gaming240Hz.uses_daemon());
        assert!(Scenario::Gaming240Hz.allows_realtime());
    }

    #[test]
    fn ui_automation_uses_daemon_only() {
        assert!(!Scenario::UiAutomation.uses_scrcpy());
        assert!(Scenario::UiAutomation.uses_daemon());
        assert!(!Scenario::UiAutomation.allows_realtime());
    }

    #[test]
    fn vision_loop_uses_both() {
        assert!(Scenario::VisionLoop.uses_scrcpy());
        assert!(Scenario::VisionLoop.uses_daemon());
        assert!(!Scenario::VisionLoop.allows_realtime());
    }

    #[test]
    fn background_uses_daemon_only() {
        assert!(!Scenario::Background.uses_scrcpy());
        assert!(Scenario::Background.uses_daemon());
    }

    #[test]
    fn adb_only_uses_neither() {
        assert!(!Scenario::AdbOnly.uses_scrcpy());
        assert!(!Scenario::AdbOnly.uses_daemon());
    }

    #[test]
    fn multi_device_uses_daemon_only() {
        assert!(!Scenario::MultiDevice.uses_scrcpy());
        assert!(Scenario::MultiDevice.uses_daemon());
    }

    #[test]
    fn bulk_text_uses_scrcpy_only_no_realtime() {
        assert!(Scenario::BulkText.uses_scrcpy());
        assert!(!Scenario::BulkText.uses_daemon());
        assert!(!Scenario::BulkText.allows_realtime());
    }

    #[test]
    fn for_scenario_gaming_picks_scrcpy_direct_realtime() {
        let hints = ConnectionHints::default();
        let mode = ConnectionMode::for_scenario(Scenario::Gaming240Hz, &hints);
        assert_eq!(mode, ConnectionMode::ScrcpyDirect { realtime: true });
        assert!(mode.has_scrcpy());
        assert!(!mode.has_daemon());
    }

    #[test]
    fn for_scenario_bulk_text_picks_scrcpy_direct_coalesced() {
        let hints = ConnectionHints::default();
        let mode = ConnectionMode::for_scenario(Scenario::BulkText, &hints);
        assert_eq!(mode, ConnectionMode::ScrcpyDirect { realtime: false });
    }

    #[test]
    fn for_scenario_ui_automation_picks_daemon() {
        let hints = ConnectionHints::default();
        let mode = ConnectionMode::for_scenario(Scenario::UiAutomation, &hints);
        assert_eq!(
            mode,
            ConnectionMode::Daemon {
                addr: hints.daemon_addr
            }
        );
        assert!(mode.has_daemon());
        assert!(!mode.has_scrcpy());
    }

    #[test]
    fn for_scenario_vision_loop_picks_dual() {
        let hints = ConnectionHints::default();
        let mode = ConnectionMode::for_scenario(Scenario::VisionLoop, &hints);
        assert_eq!(
            mode,
            ConnectionMode::DualSocket {
                scrcpy: hints.scrcpy_addr,
                daemon: hints.daemon_addr,
            }
        );
        assert!(mode.has_scrcpy());
        assert!(mode.has_daemon());
    }

    #[test]
    fn for_scenario_multi_device_picks_fanout() {
        let mut hints = ConnectionHints::default();
        hints.push_fanout("10.0.0.1:9008".parse().unwrap());
        hints.push_fanout("10.0.0.2:9008".parse().unwrap());
        let mode = ConnectionMode::for_scenario(Scenario::MultiDevice, &hints);
        assert_eq!(mode, ConnectionMode::Fanout { addrs: hints.fanout_addrs.clone() });
    }

    #[test]
    fn for_scenario_background_picks_daemon() {
        let hints = ConnectionHints::default();
        let mode = ConnectionMode::for_scenario(Scenario::Background, &hints);
        assert!(matches!(mode, ConnectionMode::Daemon { .. }));
    }

    #[test]
    fn for_scenario_adb_only_picks_adb() {
        let hints = ConnectionHints::default();
        let mode = ConnectionMode::for_scenario(Scenario::AdbOnly, &hints);
        assert_eq!(mode, ConnectionMode::AdbShell);
    }

    #[test]
    fn connection_mode_as_str_is_distinct() {
        // Every variant has a unique label.
        let labels = [
            ConnectionMode::ScrcpyDirect { realtime: true }.as_str(),
            ConnectionMode::ScrcpyDirect { realtime: false }.as_str(),
            ConnectionMode::Daemon {
                addr: "127.0.0.1:9008".parse().unwrap(),
            }
            .as_str(),
            ConnectionMode::DualSocket {
                scrcpy: "127.0.0.1:27183".parse().unwrap(),
                daemon: "127.0.0.1:9008".parse().unwrap(),
            }
            .as_str(),
            ConnectionMode::Fanout {
                addrs: vec!["127.0.0.1:9008".parse().unwrap()],
            }
            .as_str(),
            ConnectionMode::AdbShell.as_str(),
        ];
        let unique: std::collections::HashSet<_> = labels.iter().copied().collect();
        assert_eq!(unique.len(), labels.len(), "duplicate mode label: {labels:?}");
    }

    #[test]
    fn default_hints_match_well_known_ports() {
        let hints = ConnectionHints::default();
        assert_eq!(hints.scrcpy_addr.port(), 27183);
        assert_eq!(hints.daemon_addr.port(), 9008);
        assert!(hints.strict);
        assert!(hints.fanout_addrs.is_empty());
    }

    #[test]
    fn hint_builder_overrides() {
        let scrcpy: SocketAddr = "10.0.0.5:28000".parse().unwrap();
        let daemon: SocketAddr = "10.0.0.5:9009".parse().unwrap();
        let hints = ConnectionHints::default()
            .with_scrcpy(scrcpy)
            .with_daemon(daemon)
            .with_strict(false);
        assert_eq!(hints.scrcpy_addr, scrcpy);
        assert_eq!(hints.daemon_addr, daemon);
        assert!(!hints.strict);
    }

    #[test]
    fn session_error_display_includes_addr() {
        let addr: SocketAddr = "127.0.0.1:9008".parse().unwrap();
        let err = SessionError::DaemonUnavailable(addr);
        let s = err.to_string();
        assert!(s.contains("9008"));
        assert!(s.contains("daemon"));
    }

    #[test]
    fn handshake_mismatch_display_includes_bytes() {
        let addr: SocketAddr = "127.0.0.1:9008".parse().unwrap();
        let err = SessionError::HandshakeMismatch {
            addr,
            actual: *b"HELLO!!!",
        };
        let s = err.to_string();
        assert!(s.contains("HELLO"));
    }
}
