//! Capability registry — see v3 §3.5.
//!
//! The legacy `android-hid-daemon` exposes 70+ wire verbs. The AI
//! Device Kernel **internalises** those verbs as typed
//! [`Capability`] implementations; the agent only ever sees
//! the 12 typed [`Action`](crate::action::Action) variants. This
//! crate doesn't ship the actual capability implementations
//! (those live on the device), but it provides the registration
//! surface plus a typed mapping from `Action` → `Capability` name
//! list (already declared on `Action::capabilities()`).
//!
//! ## Why this layer exists
//!
//! - The agent doesn't see 70+ verb names; it sees 12 typed actions.
//! - The daemon can swap, version, or remove a capability behind
//!   the typed surface without breaking agents.
//! - Capability profiles (e.g. `phantom`, `read-only`) can
//!   refuse certain capabilities without exposing the agent to
//!   the underlying verb names.
//! - Each capability can be unit-tested in isolation — see the
//!   `MockCapability` and `RecordingCapability` test fixtures
//!   below.

use std::collections::HashMap;

use thiserror::Error;

/// Stable name of one capability. Lives in a typed `&'static str`
/// (no string allocation) so the hot path (240 Hz gamepad) doesn't
/// pay for `String` hashing.
pub type CapabilityName = &'static str;

/// Capability requirement list, as returned by
/// [`Action::capabilities`](crate::action::Action::capabilities).
///
/// Tiny owned `Vec`; for hot-path callers, the recommended fast
/// path is to call `kind_label()` + a hand-written dispatcher and
/// only use `capabilities()` at planning time.
pub type CapabilityRequirements = Vec<CapabilityName>;

/// Errors raised by the capability registry.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum CapabilityError {
    /// Caller asked for a capability the registry doesn't contain.
    #[error("unknown capability `{name}`")]
    Unknown {
        /// Missing capability name (debug-only).
        name: &'static str,
    },
    /// Capability exists but refuses to execute under the current
    /// profile (e.g. `phantom` profile refuses `Launch`).
    #[error("capability `{name}` refused: {reason}")]
    Refused {
        /// Refused capability name.
        name: &'static str,
        /// Human-readable refusal reason (for log lines; not for
        /// wire protocol — see [`ActionResult::error`] for that).
        reason: String,
    },
}

/// Opaque execution context for a capability. Concrete shape is
/// supplied by the daemon (it owns I/O); this crate only exposes
/// the trait. The device-side crate (currently `android-hid-daemon`)
/// provides the executable `CapabilityContext`.
pub trait CapabilityContext {
    /// Server-measured elapsed-time accessor (for grounded timing).
    fn elapsed_ms(&self) -> u32;
}

/// Outcome of a capability execution. Single-purpose: an
/// "executed or refused" answer + a reason. Per-capability rich
/// output is encoded in the [`ActionResult`](crate::action::ActionResult)
/// at the typed boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapabilityOutput {
    /// Capability ran successfully.
    Executed,
    /// Capability refused under the current profile.
    Refused {
        /// Refusal reason (log-only).
        reason: String,
    },
}

/// Capability trait — implementors know how to talk to a single
/// underlying subsystem (UHID, a11y, clipboard, …).
///
/// Implementors are intended to live on the device side (the
/// daemon crate wraps them). This crate defines the trait so the
/// typed [`Action`] layer has a stable contract to dispatch on.
pub trait Capability: Send + Sync {
    /// Stable name used by [`Action::capabilities`](crate::action::Action::capabilities).
    fn name(&self) -> &'static str;

    /// Decide whether this capability is willing to run under
    /// the supplied `profile`. `profile == "default"` is
    /// always allowed.
    fn allowed_in_profile(&self, profile: &str) -> bool;

    /// Execute the capability against the given context.
    ///
    /// `err_if_refused` is the convenience "short-circuit" path:
    /// passing `Some(reason)` is equivalent to returning
    /// `CapabilityOutput::Refused { reason }` and is friendlier
    /// when the caller already has the reason computed.
    fn execute(
        &self,
        ctx: &dyn CapabilityContext,
        err_if_refused: Option<String>,
    ) -> Result<CapabilityOutput, CapabilityError>;
}

/// Registry holding one or more capabilities by name. Lookup is
/// O(1) average.
///
/// Manual `Debug` impl that only prints the names so we don't
/// require `Capability: Debug` on the trait object (avoids
/// trait-object Debug dispatch interactions in some downstream
/// debug-mode builds).
pub struct CapabilityRegistry {
    caps: HashMap<CapabilityName, Box<dyn Capability>>,
}

impl std::fmt::Debug for CapabilityRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CapabilityRegistry")
            .field("count", &self.caps.len())
            .field("names", &self.caps.keys().copied().collect::<Vec<_>>())
            .finish()
    }
}

impl Default for CapabilityRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl CapabilityRegistry {
    /// Build an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            caps: HashMap::new(),
        }
    }

    /// Register a capability under its declared name. Panics if
    /// the name is empty or if a different capability was already
    /// registered under that name — registration should be a
    /// one-shot during daemon startup.
    pub fn register(&mut self, cap: Box<dyn Capability>) -> &mut Self {
        let name = cap.name();
        assert!(!name.is_empty(), "capability name must be non-empty");
        if self.caps.insert(name, cap).is_some() {
            panic!("duplicate capability name: {name}");
        }
        self
    }

    /// Lookup a capability by name. `None` if not registered.
    #[must_use]
    pub fn get(&self, name: CapabilityName) -> Option<&dyn Capability> {
        self.caps.get(name).map(|b| b.as_ref())
    }

    /// Number of registered capabilities.
    #[must_use]
    pub fn len(&self) -> usize {
        self.caps.len()
    }

    /// True if the registry holds zero capabilities.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.caps.is_empty()
    }

    /// Iterate (name, capability) pairs.
    pub fn iter(&self) -> impl Iterator<Item = (&'static str, &dyn Capability)> {
        self.caps.iter().map(|(k, v)| (*k, v.as_ref()))
    }
}

/// All `Action::capabilities()` names referenced by the typed
/// surface — keeps the daemon's registration list and the
/// `Action` enum in sync (`cargo test` catches drift).
pub const ALL_CAPABILITY_NAMES: &[CapabilityName] = &[
    "input.motion_event",
    "input.key_event",
    "uhid.inject",
    "a11y.resolve",
    "a11y.observe",
    "frame.observe",
    "shell.ime",
    "pm.start_activity",
    "clipboard.set",
    "predicate.wait",
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::Action;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Test fixture — a capability that always executes and
    /// counts its invocations.
    #[derive(Debug)]
    struct RecordingCapability {
        name: &'static str,
        count: AtomicUsize,
    }

    impl RecordingCapability {
        fn new(name: &'static str) -> Self {
            Self {
                name,
                count: AtomicUsize::new(0),
            }
        }
        fn executed(&self) -> usize {
            self.count.load(Ordering::Relaxed)
        }
    }

    impl Capability for RecordingCapability {
        fn name(&self) -> &'static str {
            self.name
        }
        fn allowed_in_profile(&self, profile: &str) -> bool {
            profile == "default" || profile == "test"
        }
        fn execute(
            &self,
            _ctx: &dyn CapabilityContext,
            err_if_refused: Option<String>,
        ) -> Result<CapabilityOutput, CapabilityError> {
            if let Some(reason) = err_if_refused {
                return Err(CapabilityError::Refused {
                    name: self.name,
                    reason,
                });
            }
            self.count.fetch_add(1, Ordering::Relaxed);
            Ok(CapabilityOutput::Executed)
        }
    }

    /// Capability that always refuses (even in `default` profile).
    #[derive(Debug)]
    struct RefusingCapability;

    impl Capability for RefusingCapability {
        fn name(&self) -> &'static str {
            "refusing.test"
        }
        fn allowed_in_profile(&self, _profile: &str) -> bool {
            false
        }
        fn execute(
            &self,
            _ctx: &dyn CapabilityContext,
            _: Option<String>,
        ) -> Result<CapabilityOutput, CapabilityError> {
            Err(CapabilityError::Refused {
                name: self.name(),
                reason: "test-capability-refuses".into(),
            })
        }
    }

    /// Mock context for tests — provides a fixed elapsed_ms.
    struct TestContext {
        elapsed_ms: u32,
    }
    impl CapabilityContext for TestContext {
        fn elapsed_ms(&self) -> u32 {
            self.elapsed_ms
        }
    }

    #[test]
    fn registry_register_and_lookup() {
        let mut reg = CapabilityRegistry::new();
        assert!(reg.is_empty());
        let cap = RecordingCapability::new("input.test");
        reg.register(Box::new(cap));
        assert_eq!(reg.len(), 1);
        let c = reg.get("input.test").expect("registered");
        assert_eq!(c.name(), "input.test");
    }

    #[test]
    #[should_panic(expected = "duplicate capability name")]
    fn registry_rejects_duplicate_names() {
        let mut reg = CapabilityRegistry::new();
        reg.register(Box::new(RecordingCapability::new("dup")));
        reg.register(Box::new(RecordingCapability::new("dup")));
    }

    #[test]
    #[should_panic(expected = "non-empty")]
    fn registry_rejects_empty_name() {
        let mut reg = CapabilityRegistry::new();
        reg.register(Box::new(RecordingCapability::new("")));
    }

    #[test]
    fn lookup_unknown_returns_none() {
        let reg = CapabilityRegistry::new();
        assert!(reg.get("never.registered").is_none());
    }

    #[test]
    fn capability_execute_increments_count() {
        let cap = RecordingCapability::new("input.test");
        let ctx = TestContext { elapsed_ms: 5 };
        for _ in 0..3 {
            cap.execute(&ctx, None)
                .expect("execution should succeed");
        }
        assert_eq!(cap.executed(), 3);
    }

    #[test]
    fn capability_works_with_dyn_dispatch() {
        let mut reg = CapabilityRegistry::new();
        reg.register(Box::new(RecordingCapability::new("a")));
        reg.register(Box::new(RecordingCapability::new("b")));
        let ctx = TestContext { elapsed_ms: 1 };
        let a = reg.get("a").expect("a");
        let b = reg.get("b").expect("b");
        assert_eq!(a.execute(&ctx, None).unwrap(), CapabilityOutput::Executed);
        assert_eq!(b.execute(&ctx, None).unwrap(), CapabilityOutput::Executed);
    }

    #[test]
    fn refusing_capability_always_errors() {
        let mut reg = CapabilityRegistry::new();
        reg.register(Box::new(RefusingCapability));
        let cap = reg.get("refusing.test").expect("registered");
        assert!(!cap.allowed_in_profile("default"));
        let ctx = TestContext { elapsed_ms: 0 };
        let err = cap.execute(&ctx, None).unwrap_err();
        assert!(matches!(err, CapabilityError::Refused { .. }));
    }

    #[test]
    fn err_if_refused_short_circuit() {
        let cap = RecordingCapability::new("a");
        let ctx = TestContext { elapsed_ms: 0 };
        let err = cap
            .execute(&ctx, Some("policy says no".into()))
            .unwrap_err();
        match err {
            CapabilityError::Refused { name, reason } => {
                assert_eq!(name, "a");
                assert_eq!(reason, "policy says no");
            }
            _ => panic!("expected Refused"),
        }
    }

    #[test]
    fn action_capabilities_drift_check() {
        // The `ALL_CAPABILITY_NAMES` const must contain every name
        // the typed `Action::capabilities()` returns, across all
        // 12 variants. If a future action references a new
        // capability name, this test catches it.
        //
        // We sweep every variant and union the names.
        fn collect_names(action: &Action, out: &mut Vec<CapabilityName>) {
            for name in action.capabilities() {
                if !out.contains(&name) {
                    out.push(name);
                }
            }
        }
        let all_actions = [
            Action::Tap {
                x: 0,
                y: 0,
                deadline_ms: 0,
            },
            Action::TapSelector {
                selector: "x".into(),
                deadline_ms: 0,
            },
            Action::TypeText {
                text: "x".into(),
                deadline_ms: 0,
            },
            Action::Key {
                code: 0,
                deadline_ms: 0,
            },
            Action::Swipe {
                x1: 0,
                y1: 0,
                x2: 0,
                y2: 0,
                dur_ms: 0,
                deadline_ms: 0,
            },
            Action::GamepadFrame {
                report: [0; 15],
                deadline_ms: 0,
            },
            Action::Launch {
                target: "x".into(),
                by: crate::action::LaunchBy::Package("x".into()),
                deadline_ms: 0,
            },
            Action::SetClipboard {
                text: "x".into(),
                paste: false,
                deadline_ms: 0,
            },
            Action::Wait {
                predicate: crate::predicate::Predicate::Activity {
                    component: "x".into(),
                    timeout_ms: 0,
                },
                deadline_ms: 0,
            },
            Action::GetUiRepr {
                screen_id: None,
                deadline_ms: 0,
            },
            Action::DumpObservation {
                components: vec![crate::action::ObservationComponent::A11y],
                deadline_ms: 0,
            },
            Action::InjectRaw {
                bytes: vec![],
                deadline_ms: 0,
            },
        ];
        let mut seen = Vec::new();
        for action in &all_actions {
            collect_names(action, &mut seen);
        }
        for name in &seen {
            assert!(
                ALL_CAPABILITY_NAMES.contains(name),
                "Action references unregistered capability `{name}`; add it \
                 to ALL_CAPABILITY_NAMES (drift guard)."
            );
        }
        for name in ALL_CAPABILITY_NAMES {
            assert!(
                seen.contains(name),
                "ALL_CAPABILITY_NAMES lists `{name}` but no Action references it; \
                 drop the dead entry."
            );
        }
    }

    #[test]
    fn registry_iter_drains_in_insertion_order() {
        // HashMap doesn't promise order; what we actually want is
        // "every entry is drained exactly once". Test that.
        let mut reg = CapabilityRegistry::new();
        reg.register(Box::new(RecordingCapability::new("a")));
        reg.register(Box::new(RecordingCapability::new("b")));
        reg.register(Box::new(RecordingCapability::new("c")));
        let drained: std::collections::HashSet<&'static str> =
            reg.iter().map(|(k, _)| k).collect();
        let expected: std::collections::HashSet<&'static str> =
            ["a", "b", "c"].iter().copied().collect();
        assert_eq!(drained, expected);
    }
}
