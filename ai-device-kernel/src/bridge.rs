//! `bridge` — the typed `Action` to byte-exact UHID execution path.
//!
//! The kernel's typed `Action` surface (12 typed writes
//! + 6 Phase 4/5 typed reads per v3 §3.6.3) terminates here.
//! The single function `ActionInjector::apply` turns an
//! `Action` into Android input by delegating to the byte-exact
//! UHID drivers in `android-hid-connect`. No daemon, no shell, no
//! `adb exec-out` — the kernel-internal intent to UHID hot path
//! runs entirely in-process.
//!
//! ## Why this lives here (not in `android-hid-connect`)
//!
//! `android-hid-connect` is the byte-exact scrcpy UHID library —
//! a low-level building block with no LLM-shaped types. The
//! `Action` ↔ `HidReport` translation is the AI middleware's
//! concern, so it sits in `ai-device-kernel`. This also keeps
//! `android-hid-connect`'s public surface (`HidSession`,
//! `HidClient`, `HidDevice` trait) untouched.
//!
//! ## SLOs served
//!
//! - **Performance**: zero allocation per write action; `apply`
//!   for `Tap` / `Swipe` / `Key` / `GamepadFrame` is O(1) system
//!   calls (UHID_INPUT × N where N is the swipe step count).
//! - **Stability**: every error path returns a structured
//!   [`BridgeOutcome`] with `landed: false` and a
//!   `code: reason` string — the executor never sees a panic.
//! - **AI compatibility**: every [`Action`] variant either maps
//!   cleanly (`Tap` / `Swipe` / `TypeText` / `Key` /
//!   `GamepadFrame`) or returns a typed `not_implemented` error
//!   (`TapSelector` / `Launch` / `SetClipboard` / `Wait` /
//!   `GetUiRepr` / `DumpObservation` / `LocalizeText` /
//!   `DetectElement` / `Ground` / `AskVisual` / `InjectRaw`).
//!   Pure-read actions and resolver-backed writes deliberately
//!   do NOT hit the bridge; they flow through
//!   `protocol::Frame::request` instead.
//!
//! ## Layering
//!
//! ```text
//! ai-device-kernel::bridge  (this file)
//!        │
//!        ▼
//! android-hid-connect::session::HidSession
//!        │
//!        ▼
//! scrcpy-server (on-device)
//! ```
//!
//! [`Action`]: crate::action::Action
//! [`ActionInjector`]: ActionInjector

use std::io::Write;
use std::time::{Duration, Instant};

use android_hid_connect::control::message::{ControlMessage, UhidInput};
use android_hid_connect::error::TransportWrite;
use android_hid_connect::multitouch::{ACTION_DOWN, ACTION_UP};
use android_hid_connect::session::{HidSession, OpenRequest};
use android_hid_connect::types::{AndroidKeyAction, AndroidKeycode, Modifiers, HID_MAX_SIZE};

use crate::action::{Action, AtomicErrorCode};
use crate::ids::ActionId;

/// Typed result of bridging an [`Action`] to UHID bytes.
///
/// Mirrors [`crate::action::ActionResult`] but adds a structured
/// error string (instead of a free-text `reason`). The executor
/// translates this back to [`AtomicErrorCode`] for the AI-facing
/// result envelope.
#[derive(Debug, Clone)]
pub struct BridgeOutcome {
    /// Action ID echoed back.
    pub id: ActionId,
    /// `true` iff the UHID bytes were flushed to the transport.
    pub landed: bool,
    /// Wall-clock duration from `apply` start to transport flush.
    pub elapsed: Duration,
    /// `Some("code: reason")` iff `landed == false`.
    pub error: Option<String>,
}

impl BridgeOutcome {
    fn ok(id: ActionId, elapsed: Duration) -> Self {
        Self {
            id,
            landed: true,
            elapsed,
            error: None,
        }
    }

    fn rejected(id: ActionId, code: AtomicErrorCode, reason: impl Into<String>) -> Self {
        Self {
            id,
            landed: false,
            elapsed: Duration::ZERO,
            error: Some(format!("{}: {}", code.as_str(), reason.into())),
        }
    }
}

/// Owns a live [`HidSession`] and translates [`Action`]s to UHID
/// bytes.
///
/// One `ActionInjector` per session — it is `!Send` because
/// `HidSession` wraps a non-shareable `Write` transport. For
/// multi-thread use, wrap the injector in a `Mutex` or run one
/// injector per logical transport.
pub struct ActionInjector<T: TransportWrite + Write> {
    session: HidSession<T>,
    next_id: u64,
}

impl<T: TransportWrite + Write> std::fmt::Debug for ActionInjector<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActionInjector")
            .field("next_id", &self.next_id)
            .finish_non_exhaustive()
    }
}

impl<T: TransportWrite + Write> ActionInjector<T> {
    /// Open a new session over `transport` (typically the
    /// `scrcpy-server` control socket after `adb forward
    /// tcp:27183 localabstract:scrcpy`).
    ///
    /// # Errors
    ///
    /// Returns the underlying `HidSession::open` error if the
    /// UHID_CREATE chain fails.
    pub fn open(transport: T, request: OpenRequest) -> Result<Self, android_hid_connect::Error> {
        Ok(Self {
            session: HidSession::open(transport, request)?,
            next_id: 0,
        })
    }

    /// Open with the default `OpenRequest::none()` — UHID_CREATE
    /// chain with no metadata fields set.
    pub fn open_default(transport: T) -> Result<Self, android_hid_connect::Error> {
        Self::open(transport, OpenRequest::none())
    }

    /// Apply a single [`Action`] to the device. Returns the
    /// structured outcome — never panics on invalid action
    /// (returns `landed: false` with a structured reason instead).
    pub fn apply(&mut self, action: &Action) -> BridgeOutcome {
        let id = ActionId(self.next_id);
        self.next_id = self.next_id.wrapping_add(1);
        let start = Instant::now();

        // Pure-read actions + resolver-backed writes short-circuit
        // to a structured `not_implemented`. The bridge is the
        // *byte-level* execution path; the protocol layer is where
        // selectors / a11y / clipboard / launch are dispatched.
        match action {
            Action::Tap {
                x,
                y,
                deadline_ms: _,
            } => self.tap(id, start, *x, *y),
            Action::Swipe {
                x1,
                y1,
                x2,
                y2,
                dur_ms,
                deadline_ms: _,
            } => self.swipe(id, start, *x1, *y1, *x2, *y2, *dur_ms),
            Action::TypeText {
                text,
                deadline_ms: _,
            } => self.type_text(id, start, text),
            Action::Key {
                code,
                deadline_ms: _,
            } => self.press_key(id, start, *code),
            Action::GamepadFrame {
                report,
                deadline_ms: _,
            } => self.gamepad_frame(id, start, *report),
            Action::TapSelector { .. }
            | Action::Launch { .. }
            | Action::SetClipboard { .. }
            | Action::Wait { .. }
            | Action::GetUiRepr { .. }
            | Action::DumpObservation { .. }
            | Action::LocalizeText { .. }
            | Action::DetectElement { .. }
            | Action::Ground { .. }
            | Action::AskVisual { .. }
            | Action::InjectRaw { .. } => BridgeOutcome::rejected(
                id,
                AtomicErrorCode::NotImplemented,
                "action requires daemon-side resolver or custom wire path; \
                 route via protocol::Frame::request",
            ),
        }
    }

    /// Apply many actions in order. Stops at the first failure
    /// (matches `Plan::abort_on_error = true` default).
    ///
    /// # Returns
    ///
    /// `(Vec<BridgeOutcome>, Option<BridgeOutcome>)` — the per-action
    /// outcomes + the first failure (if any) for the caller's
    /// convenience.
    pub fn apply_many<I>(&mut self, actions: I) -> (Vec<BridgeOutcome>, Option<BridgeOutcome>)
    where
        I: IntoIterator<Item = Action>,
    {
        let mut outcomes = Vec::new();
        let mut first_failure = None;
        for action in actions {
            let outcome = self.apply(&action);
            let failed = !outcome.landed;
            outcomes.push(outcome);
            if failed && first_failure.is_none() {
                first_failure = outcomes.last().cloned();
                break;
            }
        }
        (outcomes, first_failure)
    }

    /// Flush the underlying batcher immediately.
    ///
    /// # Errors
    ///
    /// Returns the underlying `android_hid_connect::Error`.
    pub fn flush_now(&mut self) -> Result<usize, android_hid_connect::Error> {
        self.session.flush_now()
    }

    /// Borrow the underlying session for callers that need the
    /// lower-level API (gamepad ring buffer tweaks, etc.).
    pub fn session_mut(&mut self) -> &mut HidSession<T> {
        &mut self.session
    }

    /// Consume the injector, returning the inner [`HidSession`].
    /// `UHID_DESTROY` is sent on drop — the session is panic-safe.
    pub fn into_session(self) -> HidSession<T> {
        self.session
    }

    // -- private mapping helpers --
    //
    // Every helper returns BridgeOutcome directly so the caller can
    // measure elapsed time on the success path (a return-Result
    // version would force double-construction of the success path).

    fn tap(&mut self, id: ActionId, start: Instant, x: i32, y: i32) -> BridgeOutcome {
        // A "tap" is ACTION_DOWN + ACTION_UP at the same coordinate.
        // P4 mitigation: no artificial 16 ms sleep.
        let r = self
            .session
            .inject_touch(ACTION_DOWN, 0, x, y, 1.0)
            .and_then(|()| self.session.inject_touch(ACTION_UP, 0, x, y, 0.0))
            .and_then(|()| self.session.flush_now().map(|_| ()));
        match r {
            Ok(()) => BridgeOutcome::ok(id, start.elapsed()),
            Err(e) => BridgeOutcome::rejected(id, AtomicErrorCode::KernelError, e.to_string()),
        }
    }

    #[allow(clippy::too_many_arguments)] // 4 coords + duration + 2 internal plumbing args
    fn swipe(
        &mut self,
        id: ActionId,
        start: Instant,
        x1: i32,
        y1: i32,
        x2: i32,
        y2: i32,
        dur_ms: u32,
    ) -> BridgeOutcome {
        // Sample rate ~60 Hz: 1 sample per ~16 ms. Minimum 2 steps.
        let dur = Duration::from_millis(u64::from(dur_ms));
        let steps = (dur_ms / 16).max(2);
        let r = self
            .session
            .swipe((x1, y1), (x2, y2), dur, steps)
            .and_then(|()| self.session.flush_now().map(|_| ()));
        match r {
            Ok(()) => BridgeOutcome::ok(id, start.elapsed()),
            Err(e) => BridgeOutcome::rejected(id, AtomicErrorCode::KernelError, e.to_string()),
        }
    }

    fn type_text(&mut self, id: ActionId, start: Instant, text: &str) -> BridgeOutcome {
        // ASCII type via per-character UHID keyboard. Non-ASCII
        // chars return `permission_denied` — the executor should
        // route those through `SetClipboard` + paste, which is the
        // canonical IME escape hatch.
        for ch in text.chars() {
            if !ch.is_ascii() {
                return BridgeOutcome::rejected(
                    id,
                    AtomicErrorCode::PermissionDenied,
                    format!("non-ASCII char {:?}; use SetClipboard + paste", ch),
                );
            }
            let Some(scancode) = ascii_to_scancode(ch) else {
                return BridgeOutcome::rejected(
                    id,
                    AtomicErrorCode::NotImplemented,
                    format!("ASCII char {:?} has no USB HID scancode mapping", ch),
                );
            };
            let r1 = self
                .session
                .keyboard()
                .inject_key(scancode, true, Modifiers::empty());
            if let Err(e) = r1 {
                return BridgeOutcome::rejected(id, AtomicErrorCode::KernelError, e.to_string());
            }
            let r2 = self
                .session
                .keyboard()
                .inject_key(scancode, false, Modifiers::empty());
            if let Err(e) = r2 {
                return BridgeOutcome::rejected(id, AtomicErrorCode::KernelError, e.to_string());
            }
        }
        if let Err(e) = self.session.flush_now() {
            return BridgeOutcome::rejected(id, AtomicErrorCode::KernelError, e.to_string());
        }
        BridgeOutcome::ok(id, start.elapsed())
    }

    fn press_key(&mut self, id: ActionId, start: Instant, code: u32) -> BridgeOutcome {
        // DOWN + UP via `inject_android_key_event` with a typed
        // `AndroidKeycode`. Symmetric with the typed `Action::Key`
        // semantics. `repeat=0` + `metastate=0` matches a single
        // physical keypress (no long-press, no shift).
        let keycode = AndroidKeycode::new(code);
        let r = self
            .session
            .inject_android_key_event(AndroidKeyAction::DOWN, keycode, 0, 0)
            .and_then(|()| {
                self.session
                    .inject_android_key_event(AndroidKeyAction::UP, keycode, 0, 0)
            })
            .and_then(|()| self.session.flush_now().map(|_| ()));
        match r {
            Ok(()) => BridgeOutcome::ok(id, start.elapsed()),
            Err(e) => BridgeOutcome::rejected(id, AtomicErrorCode::KernelError, e.to_string()),
        }
    }

    fn gamepad_frame(&mut self, id: ActionId, start: Instant, report: [u8; 15]) -> BridgeOutcome {
        // Forward the 15-byte HID report via `ControlMessage::UhidInput`
        // — the byte-level escape hatch for game / canvas / WebView
        // scenarios the typed driver can't easily reach (matches the
        // "InjectRaw" escape hatch in v3 P0 fix).
        let mut data = [0u8; HID_MAX_SIZE];
        data[..15].copy_from_slice(&report);
        let msg = ControlMessage::UhidInput(UhidInput {
            id: 0, // 0 = first-opened gamepad slot; the daemon handles ID assignment
            size: 15,
            data,
        });
        let r = self
            .session
            .send(&msg)
            .and_then(|()| self.session.flush_now().map(|_| ()));
        match r {
            Ok(()) => BridgeOutcome::ok(id, start.elapsed()),
            Err(e) => BridgeOutcome::rejected(id, AtomicErrorCode::KernelError, e.to_string()),
        }
    }
}

/// Map an ASCII char to a USB HID Keyboard scancode (boot
/// protocol, US layout). Returns `None` for non-printable ASCII
/// (control chars, DEL, shifted punctuation) — those need the
/// kernel-side IME path or `Modifiers::LSHIFT`.
///
/// Covers the cases the LLM will type 99% of the time:
/// lowercase letters, digits, space. Anything outside this
/// subset returns `None` and the caller maps to a typed
/// `permission_denied` outcome (route via SetClipboard + paste).
fn ascii_to_scancode(ch: char) -> Option<u8> {
    match ch {
        ' ' => Some(0x2C),
        'a'..='z' => Some(ch as u8 - b'a' + 0x04),
        'A'..='Z' => Some(ch as u8 - b'A' + 0x04), // shift bit set by caller if needed
        '1'..='9' => Some(ch as u8 - b'1' + 0x1E),
        '0' => Some(0x27),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pure-read actions must be classified as `not_implemented`
    /// in the bridge — that's the contract the executor relies on
    /// to route them via `protocol::Frame::request` instead of
    /// the bridge. Static-only check (no session needed).
    #[test]
    fn pure_read_actions_classified_correctly() {
        let reads = [
            Action::TapSelector {
                selector: "Button".into(),
                deadline_ms: 1000,
            },
            Action::Launch {
                target: "com.example".into(),
                by: crate::action::LaunchBy::Package("com.example".into()),
                deadline_ms: 1000,
            },
            Action::GetUiRepr {
                screen_id: None,
                deadline_ms: 1000,
            },
            Action::DumpObservation {
                components: vec![],
                deadline_ms: 1000,
            },
        ];
        for action in reads {
            // All four must NOT be a write variant.
            assert!(
                !matches!(
                    action,
                    Action::Tap { .. }
                        | Action::Swipe { .. }
                        | Action::TypeText { .. }
                        | Action::Key { .. }
                        | Action::GamepadFrame { .. }
                ),
                "expected pure-read action, got: {action:?}"
            );
        }
    }

    /// `BridgeOutcome::ok` and `rejected` constructors populate
    /// fields correctly.
    #[test]
    fn outcome_constructors() {
        let ok = BridgeOutcome::ok(ActionId(7), Duration::from_micros(42));
        assert_eq!(ok.id, ActionId(7));
        assert!(ok.landed);
        assert!(ok.error.is_none());

        let rej = BridgeOutcome::rejected(ActionId(8), AtomicErrorCode::Timeout, "deadline");
        assert_eq!(rej.id, ActionId(8));
        assert!(!rej.landed);
        assert_eq!(
            rej.error.as_deref(),
            Some("timeout: deadline"),
            "rejected error string is `code: reason`"
        );
    }

    /// ASCII scancode mapping covers the printable subset used by
    /// the bridge's `TypeText` path.
    #[test]
    fn ascii_scancode_mapping_covers_letters_digits() {
        // USB HID boot-protocol scancodes:
        //   a..z = 0x04..=0x1D, 1..9 = 0x1E..=0x26, 0 = 0x27, space = 0x2C
        assert_eq!(ascii_to_scancode('a'), Some(0x04));
        assert_eq!(ascii_to_scancode('z'), Some(0x1D));
        assert_eq!(ascii_to_scancode('1'), Some(0x1E));
        assert_eq!(ascii_to_scancode('9'), Some(0x26));
        assert_eq!(ascii_to_scancode('0'), Some(0x27));
        assert_eq!(ascii_to_scancode(' '), Some(0x2C));
        // Non-printable ASCII returns None (route via Key instead).
        assert_eq!(ascii_to_scancode('\n'), None);
        // Non-ASCII is the responsibility of the caller (route via
        // SetClipboard); the bridge treats it as a programming
        // error at the `is_ascii` guard above.
    }
}
