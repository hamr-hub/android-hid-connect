//! `bridge` — the typed `Action` to byte-exact UHID execution path.
//!
//! The kernel's typed `Action` surface terminates here:
//! 12 typed writes plus 6 Phase 4/5 typed reads per v3 §3.6.3.
//! The single function `ActionInjector::apply` turns an
//! `Action` into Android input by delegating to the byte-exact
//! UHID drivers in `android-hid-connect`. No daemon, no shell, no
//! `adb exec-out`. The kernel-internal intent to UHID hot path
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
//!   for `Tap` / `Swipe` / `Key` / `GamepadFrame` / `InjectRaw`
//!   is O(1) system calls (UHID_INPUT × N where N is the swipe step
//!   count).
//! - **Stability**: every error path returns a structured
//!   [`BridgeOutcome`] with `landed: false` and a
//!   `code: reason` string — the executor never sees a panic.
//! - **AI compatibility**: every [`Action`] variant either maps
//!   cleanly (`Tap` / `Swipe` / `TypeText` / `Key` /
//!   `GamepadFrame` / `Launch` / `SetClipboard` / `InjectRaw`) or returns a typed
//!   `not_implemented` error (`TapSelector` / `Wait` /
//!   `GetUiRepr` / `DumpObservation` / `LocalizeText` /
//!   `DetectElement` / `Ground` / `AskVisual`).
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

use android_hid_connect::error::TransportWrite;
use android_hid_connect::multitouch::{ACTION_DOWN, ACTION_UP};
use android_hid_connect::session::{HidSession, OpenRequest};
use android_hid_connect::types::{AndroidKeyAction, AndroidKeycode, Modifiers, Scancode};

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
    /// Typed error code iff `landed == false`. Lets the executor
    /// branch retry logic without string-parsing `error` — a
    /// `Timeout` / `KernelError` is retryable; a `PermissionDenied`
    /// / `NotImplemented` is not.
    pub code: Option<AtomicErrorCode>,
}

impl BridgeOutcome {
    fn ok(id: ActionId, elapsed: Duration) -> Self {
        Self {
            id,
            landed: true,
            elapsed,
            error: None,
            code: None,
        }
    }

    fn rejected(id: ActionId, code: AtomicErrorCode, reason: impl Into<String>) -> Self {
        Self {
            id,
            landed: false,
            elapsed: Duration::ZERO,
            error: Some(format!("{}: {}", code.as_str(), reason.into())),
            code: Some(code),
        }
    }

    /// `true` iff the caller may retry the action as-is. `Timeout`
    /// and `KernelError` are transient (device / network hiccup);
    /// `PermissionDenied` / `NotImplemented` / `AppNotForeground`
    /// need a different action first, so they are not retryable.
    #[must_use]
    pub fn retryable(&self) -> bool {
        matches!(
            self.code,
            Some(AtomicErrorCode::Timeout | AtomicErrorCode::KernelError | AtomicErrorCode::SelectorMiss)
        )
    }
}

/// Classify an `android_hid_connect::Error` into the AI-facing
/// [`AtomicErrorCode`]. The bridge used to collapse every session
/// failure to `KernelError`; this gives the executor's retry logic
/// a precise typed signal (payload-too-large is a caller bug, a
/// closed session is not retryable, etc.).
fn classify_session_error(err: &android_hid_connect::Error) -> AtomicErrorCode {
    use android_hid_connect::Error as E;
    match err {
        // Caller handed us bytes the wire can't carry, or a scancode
        // / pointer id out of range — a programming error in the
        // action, not a transient device fault. Not retryable.
        E::ControlMessageTooLarge { .. }
        | E::NameTooLong { .. }
        | E::ReportDescTooLong { .. }
        | E::ScancodeOutOfRange(_)
        | E::PointerIdOutOfRange(..) => AtomicErrorCode::PermissionDenied,
        // The requested device/slot was never opened, or the session
        // is closed / mid-lifecycle. The action references a
        // capability the running session doesn't expose.
        E::UnknownGamepad(_)
        | E::NoGamepadSlot
        | E::SessionLifecycle(_)
        | E::PointerNotActive(_)
        | E::PointerAlreadyDown(_)
        | E::BufferFullCritical
        | E::DispatcherDown(_) => AtomicErrorCode::NotImplemented,
        // A wait elapsed before the device settled — genuinely
        // transient, retry may succeed.
        E::AgentTimeout(_) => AtomicErrorCode::Timeout,
        // Transport I/O — retryable (reconnect / re-forward).
        E::Transport(_) => AtomicErrorCode::KernelError,
    }
}

/// Reject an outcome from a live session error, mapping it through
/// [`classify_session_error`] so the code is precise instead of a
/// blanket `KernelError`.
fn reject_session(id: ActionId, err: &android_hid_connect::Error) -> BridgeOutcome {
    BridgeOutcome::rejected(id, classify_session_error(err), err.to_string())
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
    /// When `> 0`, per-action helpers stage their UHID messages
    /// without flushing — `apply_many` sets this so an N-action
    /// chunk lands in a single terminal flush (N syscalls → 1).
    batch_depth: u32,
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
            batch_depth: 0,
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
            Action::Launch {
                target,
                by: _,
                deadline_ms: _,
            } => self.launch(id, start, target),
            Action::SetClipboard {
                text,
                paste,
                deadline_ms: _,
            } => self.set_clipboard(id, start, text, *paste),
            Action::InjectRaw {
                bytes,
                deadline_ms: _,
            } => self.inject_raw(id, start, bytes),
            Action::TapSelector { .. }
            | Action::Wait { .. }
            | Action::GetUiRepr { .. }
            | Action::DumpObservation { .. }
            | Action::LocalizeText { .. }
            | Action::DetectElement { .. }
            | Action::Ground { .. }
            | Action::AskVisual { .. } => BridgeOutcome::rejected(
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
    /// P2: the whole chunk is staged into the coalescing batcher and
    /// lands in a **single** terminal flush — an N-action chunk costs
    /// 1 syscall instead of N. If any action fails, the successfully
    /// staged prefix is still flushed so the device state matches the
    /// reported outcomes.
    ///
    /// # Returns
    ///
    /// `(Vec<BridgeOutcome>, Option<usize>)` — the per-action outcomes
    /// plus the **index** of the first failure (if any). Returning an
    /// index instead of a cloned [`BridgeOutcome`] avoids cloning the
    /// `String`-carrying error on the failure path; the caller reads
    /// `outcomes[idx]`. Mirrors [`crate::plan::PlanResult::first_failure`].
    pub fn apply_many<I>(&mut self, actions: I) -> (Vec<BridgeOutcome>, Option<usize>)
    where
        I: IntoIterator<Item = Action>,
    {
        let mut outcomes = Vec::new();
        let mut first_failure = None;
        self.batch_depth = self.batch_depth.saturating_add(1);
        for action in actions {
            let outcome = self.apply(&action);
            let failed = !outcome.landed;
            outcomes.push(outcome);
            if failed {
                first_failure = Some(outcomes.len() - 1);
                break;
            }
        }
        self.batch_depth = self.batch_depth.saturating_sub(1);
        // Terminal flush lands everything staged during the batch.
        // Even on the abort path, the staged prefix must reach the
        // device so its state is consistent with the outcomes we
        // return. A flush failure only downgrades an otherwise-ok
        // run — it never masks a real per-action failure.
        if self.batch_depth == 0 {
            if let Err(e) = self.session.flush_now() {
                if first_failure.is_none() {
                    let idx = outcomes.len();
                    outcomes.push(reject_session(ActionId(u64::MAX), &e));
                    first_failure = Some(idx);
                }
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

    /// Flush the batcher unless we're inside an `apply_many` batch,
    /// in which case the terminal flush (one per chunk) lands
    /// everything. P2: N-action chunk = 1 flush instead of N.
    #[inline]
    fn maybe_flush(&mut self) -> Result<(), android_hid_connect::Error> {
        if self.batch_depth == 0 {
            self.session.flush_now().map(|_| ())
        } else {
            Ok(())
        }
    }

    fn tap(&mut self, id: ActionId, start: Instant, x: i32, y: i32) -> BridgeOutcome {
        // A "tap" is ACTION_DOWN + ACTION_UP at the same coordinate.
        // P4 mitigation: no artificial 16 ms sleep.
        let r = self
            .session
            .inject_touch(ACTION_DOWN, 0, x, y, 1.0)
            .and_then(|()| self.session.inject_touch(ACTION_UP, 0, x, y, 0.0))
            .and_then(|()| self.maybe_flush());
        match r {
            Ok(()) => BridgeOutcome::ok(id, start.elapsed()),
            Err(e) => reject_session(id, &e),
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
        // Step density: max of a time-based rate (~60 Hz, 1 sample per
        // 16 ms) and a distance-based floor (1 sample per ~40 px).
        //
        // C5: the time-only rate under-samples fast, long swipes — a
        // 600 px fling in 32 ms would emit just 2 steps (300 px jumps),
        // too coarse for Android's velocity tracker to classify as a
        // fling. The distance floor guarantees enough intermediate
        // points for reliable fling recognition regardless of duration,
        // while a hard cap keeps a huge slow drag from flooding the
        // wire. Endpoints are always exact (byte-identical), so short
        // swipes are unaffected.
        const PX_PER_STEP: u32 = 40;
        const MAX_STEPS: u32 = 256;
        let dur = Duration::from_millis(u64::from(dur_ms));
        let dx = (x2 - x1).unsigned_abs();
        let dy = (y2 - y1).unsigned_abs();
        let distance = dx.max(dy);
        let time_steps = dur_ms / 16;
        let dist_steps = distance / PX_PER_STEP;
        let steps = time_steps.max(dist_steps).clamp(2, MAX_STEPS);
        let r = self
            .session
            .swipe((x1, y1), (x2, y2), dur, steps)
            .and_then(|()| self.maybe_flush());
        match r {
            Ok(()) => BridgeOutcome::ok(id, start.elapsed()),
            Err(e) => reject_session(id, &e),
        }
    }

    fn type_text(&mut self, id: ActionId, start: Instant, text: &str) -> BridgeOutcome {
        // ASCII type via per-character UHID keyboard. Delegates the
        // char → (scancode, modifiers) mapping to the canonical
        // `Scancode::try_from_char`, which is full US-QWERTY: it sets
        // `LSHIFT` for uppercase letters and shifted symbols, and
        // covers newline / tab / backspace / the symbol row. Chars
        // outside that set (non-ASCII, control chars) return
        // `permission_denied` — the executor should route those
        // through `SetClipboard` + paste, the canonical IME escape
        // hatch.
        let mut mods = Modifiers::empty();
        for ch in text.chars() {
            let Some(scancode) = Scancode::try_from_char(ch, &mut mods) else {
                return BridgeOutcome::rejected(
                    id,
                    AtomicErrorCode::PermissionDenied,
                    format!("char {:?} has no US-QWERTY scancode; use SetClipboard + paste", ch),
                );
            };
            let byte = scancode.to_u8();
            if let Err(e) = self.session.key(byte, true, mods) {
                return reject_session(id, &e);
            }
            if let Err(e) = self.session.key(byte, false, mods) {
                return reject_session(id, &e);
            }
        }
        if let Err(e) = self.maybe_flush() {
            return reject_session(id, &e);
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
            .and_then(|()| self.maybe_flush());
        match r {
            Ok(()) => BridgeOutcome::ok(id, start.elapsed()),
            Err(e) => reject_session(id, &e),
        }
    }

    fn gamepad_frame(&mut self, id: ActionId, start: Instant, report: [u8; 15]) -> BridgeOutcome {
        let r = self
            .session
            .inject_raw_gamepad_report(&report)
            .and_then(|()| self.maybe_flush());
        match r {
            Ok(()) => BridgeOutcome::ok(id, start.elapsed()),
            Err(e) => reject_session(id, &e),
        }
    }

    fn launch(&mut self, id: ActionId, start: Instant, target: &str) -> BridgeOutcome {
        let r = self
            .session
            .launch_app(target)
            .and_then(|()| self.maybe_flush());
        match r {
            Ok(()) => BridgeOutcome::ok(id, start.elapsed()),
            Err(e) => reject_session(id, &e),
        }
    }

    fn set_clipboard(
        &mut self,
        id: ActionId,
        start: Instant,
        text: &str,
        paste: bool,
    ) -> BridgeOutcome {
        let r = self
            .session
            .set_clipboard(text, paste)
            .and_then(|()| self.maybe_flush());
        match r {
            Ok(()) => BridgeOutcome::ok(id, start.elapsed()),
            Err(e) => reject_session(id, &e),
        }
    }

    fn inject_raw(&mut self, id: ActionId, start: Instant, bytes: &[u8]) -> BridgeOutcome {
        let r = self
            .session
            .inject_raw_gamepad_report(bytes)
            .and_then(|()| self.maybe_flush());
        match r {
            Ok(()) => BridgeOutcome::ok(id, start.elapsed()),
            Err(e) => reject_session(id, &e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TAG_UHID_CREATE: u8 = 12;
    const TAG_UHID_INPUT: u8 = 13;

    fn split_messages(bytes: &[u8]) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            let tag = bytes[i];
            let Some(len) = message_len(&bytes[i..], tag) else {
                break;
            };
            out.push(bytes[i..i + len].to_vec());
            i += len;
        }
        out
    }

    fn message_len(bytes: &[u8], tag: u8) -> Option<usize> {
        match tag {
            TAG_UHID_CREATE => {
                let name_len = *bytes.get(7)? as usize;
                let desc_len_idx = 8 + name_len;
                let desc_len =
                    u16::from_be_bytes([*bytes.get(desc_len_idx)?, *bytes.get(desc_len_idx + 1)?])
                        as usize;
                Some(10 + name_len + desc_len)
            }
            TAG_UHID_INPUT => {
                let size = u16::from_be_bytes([*bytes.get(3)?, *bytes.get(4)?]) as usize;
                Some(5 + size)
            }
            14 => Some(3),
            _ => None,
        }
    }

    fn uhid_inputs(bytes: &[u8]) -> Vec<Vec<u8>> {
        split_messages(bytes)
            .into_iter()
            .filter(|msg| msg[0] == TAG_UHID_INPUT)
            .collect()
    }

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

    #[test]
    fn inject_raw_emits_single_gamepad_uhid_input() {
        let mut injector = ActionInjector::open(
            android_hid_connect::transport::MockTransport::new(),
            OpenRequest::gamepad_only_realtime(),
        )
        .expect("open gamepad session");

        let payload = vec![0xAA, 0xBB, 0xCC, 0xDD];
        let outcome = injector.apply(&Action::InjectRaw {
            bytes: payload.clone(),
            deadline_ms: 10,
        });

        assert!(outcome.landed, "raw inject failed: {:?}", outcome.error);
        injector.flush_now().expect("flush");
        injector.session_mut().close().expect("close");
        let bytes = injector.into_session().into_inner().into_bytes();
        let inputs = uhid_inputs(&bytes);
        assert_eq!(inputs.len(), 1);
        assert_eq!(u16::from_be_bytes([inputs[0][1], inputs[0][2]]), 3);
        assert_eq!(u16::from_be_bytes([inputs[0][3], inputs[0][4]]), 4);
        assert_eq!(&inputs[0][5..], payload.as_slice());
    }

    #[test]
    fn inject_raw_rejects_empty_payload() {
        let mut injector = ActionInjector::open(
            android_hid_connect::transport::MockTransport::new(),
            OpenRequest::gamepad_only_realtime(),
        )
        .expect("open gamepad session");

        let outcome = injector.apply(&Action::InjectRaw {
            bytes: vec![],
            deadline_ms: 10,
        });

        assert!(!outcome.landed);
        assert!(
            outcome
                .error
                .as_deref()
                .is_some_and(|err| err.contains("raw gamepad report is empty")),
            "unexpected error: {:?}",
            outcome.error
        );
    }

    #[test]
    fn inject_raw_requires_open_gamepad() {
        let mut injector = ActionInjector::open(
            android_hid_connect::transport::MockTransport::new(),
            OpenRequest::none(),
        )
        .expect("open session");

        let outcome = injector.apply(&Action::InjectRaw {
            bytes: vec![1, 2, 3],
            deadline_ms: 10,
        });

        assert!(!outcome.landed);
        assert!(
            outcome
                .error
                .as_deref()
                .is_some_and(|err| err.contains("gamepad not open")),
            "unexpected error: {:?}",
            outcome.error
        );
        // C6: a missing device is a capability mismatch, not a
        // transient kernel fault — precise code + not retryable.
        assert_eq!(outcome.code, Some(AtomicErrorCode::NotImplemented));
        assert!(!outcome.retryable(), "not_implemented must not be retryable");
    }

    /// C6: a payload the wire cannot carry is classified as a
    /// caller error (`permission_denied`), never a blanket
    /// `kernel_error`, and is not retryable.
    #[test]
    fn oversized_raw_payload_is_permission_denied() {
        let mut injector = ActionInjector::open(
            android_hid_connect::transport::MockTransport::new(),
            OpenRequest::gamepad_only_realtime(),
        )
        .expect("open gamepad session");

        // HID_MAX_SIZE is 4096; anything larger cannot be framed.
        let outcome = injector.apply(&Action::InjectRaw {
            bytes: vec![0u8; 5000],
            deadline_ms: 10,
        });

        assert!(!outcome.landed);
        assert_eq!(outcome.code, Some(AtomicErrorCode::PermissionDenied));
        assert!(!outcome.retryable());
    }

    /// `TypeText` now delegates to the canonical `Scancode::try_from_char`,
    /// so uppercase letters carry the LSHIFT modifier byte and the full
    /// symbol / newline / tab set is typeable byte-exactly.
    #[test]
    fn type_text_uppercase_sets_shift_modifier() {
        // Keyboard UHID_INPUT payload layout (after the 5-byte
        // tag+id+size header): [mod_byte, reserved, sc0, sc1, ...].
        // So msg[5] = modifier byte, msg[7] = first scancode.
        const HID_LSHIFT: u8 = 0x02; // USB HID left-shift modifier bit.
        const SC_A: u8 = 0x04; // 'a'/'A' share scancode 0x04.

        let mut injector = ActionInjector::open(
            android_hid_connect::transport::MockTransport::new(),
            OpenRequest {
                kbd: true,
                mouse: false,
                gamepad: false,
                coalesce: false,
                coalesce_window: std::time::Duration::ZERO,
                coalesce_hard_limit: 0,
            },
        )
        .expect("open keyboard session");

        let outcome = injector.apply(&Action::TypeText {
            text: "aA".into(),
            deadline_ms: 100,
        });
        assert!(outcome.landed, "type failed: {:?}", outcome.error);
        injector.flush_now().expect("flush");
        injector.session_mut().close().expect("close");
        let bytes = injector.into_session().into_inner().into_bytes();
        let inputs = uhid_inputs(&bytes);
        // 'a' → down+up, 'A' → down+up = 4 keyboard reports.
        assert_eq!(inputs.len(), 4, "expected 4 key reports");

        // Lowercase 'a' down: no shift, scancode 0x04.
        assert_eq!(inputs[0][5], 0x00, "'a' must not carry a modifier");
        assert_eq!(inputs[0][7], SC_A);
        // Uppercase 'A' down (report index 2): LSHIFT set, same scancode.
        assert_eq!(
            inputs[2][5], HID_LSHIFT,
            "'A' must set the LSHIFT modifier byte (was typing lowercase)"
        );
        assert_eq!(inputs[2][7], SC_A);
    }

    /// Newline / tab / shifted symbols are typeable — the old subset
    /// mapping rejected these as `not_implemented`.
    #[test]
    fn type_text_covers_newline_tab_and_symbols() {
        let mut injector = ActionInjector::open(
            android_hid_connect::transport::MockTransport::new(),
            OpenRequest {
                kbd: true,
                mouse: false,
                gamepad: false,
                coalesce: false,
                coalesce_window: std::time::Duration::ZERO,
                coalesce_hard_limit: 0,
            },
        )
        .expect("open keyboard session");

        // "\n\t!@" — newline, tab, and two shifted symbols.
        let outcome = injector.apply(&Action::TypeText {
            text: "\n\t!@".into(),
            deadline_ms: 100,
        });
        assert!(
            outcome.landed,
            "newline/tab/symbols must type, got: {:?}",
            outcome.error
        );
    }

    /// Non-ASCII still routes to the SetClipboard+paste escape hatch.
    #[test]
    fn type_text_rejects_non_ascii() {
        let mut injector = ActionInjector::open(
            android_hid_connect::transport::MockTransport::new(),
            OpenRequest {
                kbd: true,
                mouse: false,
                gamepad: false,
                coalesce: false,
                coalesce_window: std::time::Duration::ZERO,
                coalesce_hard_limit: 0,
            },
        )
        .expect("open keyboard session");

        let outcome = injector.apply(&Action::TypeText {
            text: "café".into(),
            deadline_ms: 100,
        });
        assert!(!outcome.landed);
        assert_eq!(
            outcome
                .error
                .as_deref()
                .map(|e| e.starts_with("permission_denied")),
            Some(true),
            "non-ASCII must be permission_denied: {:?}",
            outcome.error
        );
    }

    /// P1: a coalesced injector emits byte-identical output to a
    /// direct injector for the same swipe, but collapses the swipe's
    /// N touch messages into a single transport flush (one syscall).
    #[test]
    fn coalesced_swipe_is_byte_identical_but_one_flush() {
        fn run_swipe(coalesce: bool) -> (Vec<u8>, u64) {
            let req = OpenRequest {
                kbd: false,
                mouse: false,
                gamepad: false,
                coalesce,
                coalesce_window: if coalesce {
                    std::time::Duration::from_millis(1)
                } else {
                    std::time::Duration::ZERO
                },
                coalesce_hard_limit: if coalesce { 4096 } else { 0 },
            };
            let mut injector =
                ActionInjector::open(android_hid_connect::transport::MockTransport::new(), req)
                    .expect("open session");
            // Clear the UHID_CREATE flushes so we measure only the swipe.
            let base_flushes = injector.session_mut().flushes();
            let outcome = injector.apply(&Action::Swipe {
                x1: 100,
                y1: 200,
                x2: 400,
                y2: 800,
                dur_ms: 48, // steps = 48/16 = 3 → DOWN + 3 MOVE + UP = 5 msgs
                deadline_ms: 1000,
            });
            assert!(outcome.landed, "swipe failed: {:?}", outcome.error);
            let swipe_flushes = injector.session_mut().flushes() - base_flushes;
            injector.session_mut().close().expect("close");
            let bytes = injector.into_session().into_inner().into_bytes();
            (bytes, swipe_flushes)
        }

        let (direct_bytes, direct_flushes) = run_swipe(false);
        let (coalesced_bytes, coalesced_flushes) = run_swipe(true);

        // Byte-exactness is non-negotiable (AGENTS.md constraint #1).
        assert_eq!(
            direct_bytes, coalesced_bytes,
            "coalescing must not change the wire bytes"
        );
        // Swipe emits INJECT_TOUCH_EVENT messages (DOWN + 3 MOVE + UP
        // for dur_ms=48 → steps=3), not UHID_INPUT — so we assert on
        // flush counts, which is the syscall signal P1 targets.
        // Direct mode: one flush per pushed message + the helper's
        // trailing flush_now(). Coalesced mode: the whole swipe
        // batches, then a single flush_now() lands it.
        assert!(
            coalesced_flushes < direct_flushes,
            "coalesced ({coalesced_flushes}) must be fewer than direct ({direct_flushes})"
        );
        assert_eq!(coalesced_flushes, 1, "swipe should land in a single flush");
        assert!(
            direct_flushes >= 5,
            "direct swipe should flush per-message (got {direct_flushes})"
        );
    }

    /// P2 + P4: apply_many stages a whole chunk into one terminal
    /// flush, and reports the first failure as an index (no clone).
    #[test]
    fn apply_many_single_flush_and_index_failure() {
        let mut injector = ActionInjector::open(
            android_hid_connect::transport::MockTransport::new(),
            OpenRequest {
                kbd: true,
                mouse: false,
                gamepad: false,
                coalesce: true,
                coalesce_window: std::time::Duration::from_millis(1),
                coalesce_hard_limit: 4096,
            },
        )
        .expect("open keyboard session");

        let base = injector.session_mut().flushes();
        // Three taps land, then a type of non-ASCII fails at index 3.
        let (outcomes, first_failure) = injector.apply_many([
            Action::Tap { x: 1, y: 1, deadline_ms: 100 },
            Action::Tap { x: 2, y: 2, deadline_ms: 100 },
            Action::Tap { x: 3, y: 3, deadline_ms: 100 },
            Action::TypeText { text: "é".into(), deadline_ms: 100 },
            // Never reached — aborts at the first failure.
            Action::Tap { x: 9, y: 9, deadline_ms: 100 },
        ]);
        let chunk_flushes = injector.session_mut().flushes() - base;

        // P4: first_failure is the index, and outcomes[idx] is the reject.
        assert_eq!(first_failure, Some(3), "type of 'é' fails at index 3");
        assert_eq!(outcomes.len(), 4, "aborts after the failure; last tap skipped");
        assert!(!outcomes[3].landed);
        assert_eq!(outcomes[3].code, Some(AtomicErrorCode::PermissionDenied));
        // First three taps all landed.
        assert!(outcomes[..3].iter().all(|o| o.landed));

        // P2: the entire chunk cost a single terminal flush, not one
        // per action. (3 taps × 2 touch msgs would be ≥6 flushes in
        // per-action mode.)
        assert_eq!(chunk_flushes, 1, "apply_many must land in one flush");
    }

    /// P2: an all-success chunk also lands in exactly one flush and
    /// reports no failure.
    #[test]
    fn apply_many_all_success_one_flush() {
        let mut injector = ActionInjector::open(
            android_hid_connect::transport::MockTransport::new(),
            OpenRequest {
                kbd: true,
                mouse: false,
                gamepad: false,
                coalesce: true,
                coalesce_window: std::time::Duration::from_millis(1),
                coalesce_hard_limit: 4096,
            },
        )
        .expect("open keyboard session");

        let base = injector.session_mut().flushes();
        let (outcomes, first_failure) = injector.apply_many([
            Action::Tap { x: 1, y: 1, deadline_ms: 100 },
            Action::TypeText { text: "hi".into(), deadline_ms: 100 },
            Action::Tap { x: 5, y: 5, deadline_ms: 100 },
        ]);
        let chunk_flushes = injector.session_mut().flushes() - base;

        assert_eq!(first_failure, None);
        assert!(outcomes.iter().all(|o| o.landed));
        assert_eq!(chunk_flushes, 1, "all-success chunk = one flush");
    }

    /// C5: a fast, long swipe must be densely sampled by distance even
    /// when its short duration would otherwise yield only 2 time-based
    /// steps — enough intermediate points for on-device fling
    /// recognition.
    #[test]
    fn fast_long_swipe_is_densely_sampled() {
        // INJECT_TOUCH_EVENT tag + a zeroed 8-byte pointer id prefix.
        const TAG_TOUCH: u8 = 2;
        fn count_touches(bytes: &[u8]) -> usize {
            let mut n = 0;
            let mut i = 0;
            // payload = action(1)+ptr(8)+x(4)+y(4)+w(2)+h(2)+p(2)+ab(4)+b(4) = 31
            while i + 1 + 31 <= bytes.len() {
                if bytes[i] == TAG_TOUCH
                    && matches!(bytes[i + 1], 0..=2)
                    && bytes[i + 2..i + 10].iter().all(|b| *b == 0)
                {
                    n += 1;
                    i += 1 + 31;
                } else {
                    i += 1;
                }
            }
            n
        }

        let mut injector = ActionInjector::open(
            android_hid_connect::transport::MockTransport::new(),
            OpenRequest {
                kbd: false,
                mouse: false,
                gamepad: false,
                coalesce: false,
                coalesce_window: std::time::Duration::ZERO,
                coalesce_hard_limit: 0,
            },
        )
        .expect("open session");

        // 800 px horizontal fling in 16 ms: time_steps = 1 → clamped
        // to 2, but dist_steps = 800/40 = 20 wins. Expect DOWN + ~20
        // MOVE + final MOVE + UP ≈ 22+ touch events.
        let outcome = injector.apply(&Action::Swipe {
            x1: 100,
            y1: 900,
            x2: 900,
            y2: 900,
            dur_ms: 16,
            deadline_ms: 1000,
        });
        assert!(outcome.landed, "swipe failed: {:?}", outcome.error);
        injector.session_mut().close().expect("close");
        let bytes = injector.into_session().into_inner().into_bytes();
        let touches = count_touches(&bytes);
        assert!(
            touches >= 20,
            "fast long swipe must be densely sampled (got {touches} touch events)"
        );
    }
}
