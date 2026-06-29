# android-hid-connect

A pure-Rust port of [scrcpy][scrcpy]'s UHID control surface — drive a
connected Android device by emitting USB HID reports over scrcpy-server's
control socket.

[scrcpy]: https://github.com/Genymobile/scrcpy

## Documentation

| 文档 | 用途 |
| ---- | ---- |
| [`README.md`](README.md) | 协议概览 + 高级 API + 入门(本文件)|
| [`AGENTS.md`](AGENTS.md) | 协作约定(目录规则 + 允许/禁止 + AI agent meta-rule)|
| [`ACCEPTANCE.md`](ACCEPTANCE.md) | AC 验收点 + 真机回归记录 + 历史 bug |
| [`CHANGELOG.md`](CHANGELOG.md) | 变更日志(release-please 自动读)|
| [`docs/INDEX.md`](docs/INDEX.md) | 全部专题文档导航 |
| [`docs/architecture.md`](docs/architecture.md) | 模块分层 + 线程模型 + 纯度边界 |
| [`docs/wire-format.md`](docs/wire-format.md) | 22 control_msg + 3 HID report + 3 device_msg 字节速查 |
| [`docs/scrcpy-protocol-compatibility.md`](docs/scrcpy-protocol-compatibility.md) | scrcpy v2.7 byte-exact 契约 + 跟踪流程 |
| [`docs/ai-agent-integration.md`](docs/ai-agent-integration.md) | LLM / agent runtime 集成指南 |
| [`docs/development.md`](docs/development.md) | 本地开发循环 + 真机 E2E + CI 矩阵 |
| [`docs/comparison-with-handsets.md`](docs/comparison-with-handsets.md) | 与 `handsets` 仓库的多维度对比 |

第一次接触本 crate:先读本文件 → 然后 [`docs/ai-agent-integration.md`](docs/ai-agent-integration.md) 或 [`docs/architecture.md`](docs/architecture.md)。

## What this crate does

scrcpy (and any compatible `scrcpy-server` running on an Android
device) accepts control messages over a TCP socket. The three most
interesting messages for synthesising real input are
`UHID_CREATE` / `UHID_INPUT` / `UHID_DESTROY`: they make the device
believe a USB keyboard, mouse or gamepad was just plugged in, and
then feed it HID reports that the Android input subsystem delivers to
the focused app as if they came from real hardware.

This crate gives you:

| Module                  | What it provides                                                  |
| ----------------------- | ----------------------------------------------------------------- |
| `hid::KeyboardHid`      | 8-byte HID keyboard reports, scancode tracking, phantom state.    |
| `hid::MouseHid`         | 5-byte HID mouse reports, scroll residual accumulator.            |
| `hid::GamepadHid`       | 15-byte HID gamepad reports, up to 8 concurrent slots, dpad hat.  |
| `control::ControlMessage` | All 22 scrcpy control messages plus AI extension tags, byte-exact serialization. |
| `control::AiConfig` / `control::AiQuery` | Typed AI summary pipeline control messages and flags. |
| `types::AndroidKeyAction` | Typed Android `KeyEvent.ACTION_*` values for key down/up events. |
| `types::AndroidKeycode` | Typed Android `KeyEvent.KEYCODE_*` values for non-UHID key injection. |
| `types::TouchAction` | Typed Android `MotionEvent.ACTION_*` values for touch frames. |
| `types::TouchPointerId` | Typed scrcpy touch pointer ids, including mouse/generic/virtual finger constants. |
| `types::ClipboardCopyKey` | Typed scrcpy clipboard request selector: none, copy, or cut. |
| `client::TouchFrameBatcher` | Fixed-stack touch batching for custom agent gesture paths.     |
| `client::KeyboardFrameBatcher` | Fixed-stack UHID keyboard edge batching for macros.        |
| `client::KeyboardChordFrame` | Fixed-stack shortcut/chord expansion for UHID keyboard plans. |
| `client::AndroidKeyFrameBatcher` | Fixed-stack Android KeyEvent batching for framework keys. |
| `client::MouseFrame` | Fixed-stack relative UHID mouse frame batching.                 |
| `client::MouseFrameBatcher` | Caller-side fixed-stack mouse batching for pointer loops.    |
| `client::ScrollFrame` | Fixed-stack Android absolute scroll event batching.          |
| `client::ScrollFrameBatcher` | Caller-side fixed-stack absolute scroll batching.      |
| `client::GamepadFrameBatcher` | Fixed-stack/vector gamepad frame batching for high-rate loops. |
| `client::KEYBOARD_BATCH_FRAMES` | Public 32-frame fixed-buffer keyboard batch size.         |
| `client::KEYBOARD_CHORD_KEYS` | Public 6-key USB HID chord key cap.                       |
| `client::ANDROID_KEY_BATCH_FRAMES` | Public 32-frame fixed-buffer Android key batch size.  |
| `client::MOUSE_BATCH_FRAMES` | Public 32-frame fixed-buffer mouse batch size.               |
| `client::SCROLL_BATCH_FRAMES` | Public 32-frame fixed-buffer scroll batch size.         |
| `client::GAMEPAD_BATCH_FRAMES` | Public 32-frame fixed-buffer gamepad batch size.             |
| `device::DeviceMessage` | scrcpy server→host clipboard, ACK, and UHID output parsing.       |
| `device::DeviceEvent`   | Unified native scrcpy + AI extension event parser.                |
| `device::LatestFrameSummaryReceiver` | Newest-only AI frame cache for low-latency perception loops. |
| `async_device`          | Optional Tokio async parser, ordered receiver, and latest-frame receiver. |
| `agent::AgentControlSession` | One object combining cloned command producers and device replies. |
| `agent::AgentPoint` | Eq-safe normalized screenshot point for screen-size independent plans. |
| `agent::AgentRect` | Eq-safe normalized vision/object rectangle target and frame-summary selectors. |
| `agent::AgentObjectSelector` | Class/confidence filter for deterministic object target selection. |
| `agent::AgentTargetSelector` | Unified object/text target selector for AI planner APIs. |
| `agent::AgentTouchFrame` | Eq-safe integer-pressure touch samples for custom agent plans.    |
| `agent::AgentScrollFrame` | Eq-safe integer-delta absolute scroll samples for custom plans. |
| `agent::AgentAction`    | Typed action plans with one checked dispatcher boundary.          |
| `agent::AgentPlanSummary` | Transport-free plan validity, prefix, and dispatch-pressure summary. |
| `agent::AgentPlanBoundedPrefix` | Longest safe non-blocking action prefix for a command bound. |
| `transport`             | `MockTransport` for tests, `open_tcp("127.0.0.1", 27183)` helper. |

All of the HID descriptors and report builders are byte-for-byte
identical to the C versions in `scrcpy/app/src/hid/` and
`scrcpy/app/src/uhid/`, and the wire format is byte-for-byte
identical to `sc_control_msg_serialize` in
`scrcpy/app/src/control_msg.c`.

## Setup

1. Push scrcpy-server to the device:

   ```bash
   adb push scrcpy-server /data/local/tmp/scrcpy-server
   ```

2. Start the server in UHID mode (no video, no audio, no control — just
   a listening socket):

   ```bash
   adb shell CLASSPATH=/data/local/tmp/scrcpy-server \
       app_process / com.genymobile.scrcpy.Server 2.7 \
       --no-control --no-video --no-audio --no-clipboard-autosync
   ```

3. Forward the server's local-abstract socket to a local TCP port:

   ```bash
   adb forward tcp:27183 localabstract:scrcpy
   ```

4. Use the library from Rust:

   ```rust,no_run
   use android_hid_connect::{KeyboardHid, HidDevice, Modifiers};
   use android_hid_connect::transport::{open_tcp, send_one};

   let mut sock = open_tcp("127.0.0.1", 27183).unwrap();
   let mut kbd = KeyboardHid::new();
   send_one(&mut sock, &kbd.open_message(None).unwrap()).unwrap();
   send_one(&mut sock, &kbd.key_event(0x04, true, Modifiers::LSHIFT).unwrap()).unwrap(); // Shift+A
   send_one(&mut sock, &kbd.key_event(0x04, false, Modifiers::empty()).unwrap()).unwrap();
   send_one(&mut sock, &kbd.close_message().unwrap()).unwrap();
   ```

## Examples

```bash
cargo run --example type_keys       # type "Hello, world!" into the focused app
cargo run --example gamepad_demo    # open a virtual gamepad and tilt the stick
```

## High-level `HidSession` (for AI / agent control)

The `session` module wraps the lifecycle of one or more UHID devices
behind a single object that is **panic-safe** — when the session drops
(either explicitly via `close()` or implicitly via `Drop`) it sends
`UHID_DESTROY` for every device it opened, even mid-unwind.

```rust,no_run
use android_hid_connect::session::{HidSession, OpenRequest};
use android_hid_connect::transport::open_tcp;

let sock = open_tcp("127.0.0.1", 27183).unwrap();
let mut s = HidSession::open(sock, OpenRequest::all()).unwrap();
s.set_screen_size(1080, 2400);

s.type_text("hello").unwrap();              // 4 UHID_INPUTs (h down/up + ...)
s.tap((540, 1200)).unwrap();                // 2 INJECT_TOUCH_EVENTs
s.swipe((100, 500), (900, 500),
        std::time::Duration::from_millis(300), 10).unwrap();
s.set_stick(android_hid_connect::GamepadAxis::LeftX, -0.7).unwrap();
s.set_button(android_hid_connect::GamepadButton::South, true).unwrap();
s.configure_ai(
    android_hid_connect::AI_FLAG_KEYFRAMES | android_hid_connect::AI_FLAG_OBJECTS,
    16,
    0,
).unwrap();                               // AI_CONFIG for AI-enabled servers

s.close().unwrap();   // explicit; DESTROY for kbd + mouse + gamepad
let _sock = s.into_inner();
```

For real-time game control, disable input coalescing when you need
every packet to be written immediately (no input batching):

```rust,no_run
use android_hid_connect::session::{GamepadFrameRaw, HidSession, OpenRequest};
use android_hid_connect::transport::open_tcp;

let sock = open_tcp("127.0.0.1", 27183).unwrap();
let mut low_latency_session = HidSession::open(
    sock,
    OpenRequest::gamepad_only().with_coalesce(false),
).unwrap();
let mut i: i16 = 0; // your normalized [-32767, 32767] axis value
let _ = low_latency_session.set_stick_raw(android_hid_connect::GamepadAxis::LeftX, i);
let _ = low_latency_session.set_buttons(
    android_hid_connect::GamepadButton::South as u32
        | android_hid_connect::GamepadButton::DpadUp as u32,
); // one full button frame

// If your input loop produces a full gamepad frame every tick, send one combined call:
let _ = low_latency_session.set_frame_raw(
    android_hid_connect::GamepadButton::South as u32
        | android_hid_connect::GamepadButton::RightShoulder as u32,
    -3000,
    1200,
    900,
    -700,
    16384,
    2000,
);

// If you already have a packed frame ring, send it as one dispatch
// command to cut dispatcher overhead:
let frames = [
    GamepadFrameRaw::new(0, 0, 0, 0, 0, 0, 0),
    GamepadFrameRaw::new(1, -1000, 0, 500, 0, 0, 0),
];
let _ = low_latency_session.set_frame_raw_batch(&frames);

// If both X/Y axes arrive together (e.g. from SDL/dualsense loop), this also helps:
let _ = low_latency_session.set_left_stick_raw(-3000, 1200);      // left stick pair
let _ = low_latency_session.set_sticks_raw(1000, 200, -800, -1000, 0, 16384); // full frame

// If your control loop emits raw 15-byte gamepad payloads already, send them
// in one command:
let raw_frame = [0u8; 15];
let _ = low_latency_session.set_frame_raw_packed(&raw_frame)?;
let packed = android_hid_connect::session::GamepadFrameRaw::new(
    android_hid_connect::GamepadButton::South as u32,
    -3000,
    1200,
    900,
    -700,
    16384,
    2000,
).pack();
let _ = low_latency_session.set_frame_raw_unchecked(
    android_hid_connect::GamepadButton::South as u32,
    -3000,
    1200,
    900,
    -700,
    16384,
    2000,
)?; // same fields, no diffing path

let _ = low_latency_session.set_frame_raw_packed(&packed)?;
	// If your dispatcher receives one frame sample per tick, keep it on the
	// fastest path with one unchecked frame dispatch:
	// client.send_frame_unchecked(GamepadFrameRaw::new(0, 0, 0, 0, 0, 0, 0)).unwrap();
	// For single-axis/button updates, skip full-frame packing:
	// client.send_stick_raw(GamepadAxis::LeftX, 1000).unwrap();
	// client.send_left_stick_raw(1000, -1000).unwrap();
	// client.send_button(GamepadButton::South, true).unwrap();

	// In tight loops use non-blocking shortcuts and optionally flush at
	// phase boundaries:
	// client.try_send_stick_raw(GamepadAxis::LeftX, 1100).unwrap_or(());
	// client.try_send_sticks_raw(200, 100, -200, -100, 0, 16384).unwrap_or(());
	// client.try_flush().unwrap_or(());

	// Through HidClient, keep the same idea while staying lock-free for producer
	// threads:
	// client.send_frame_packed_batch(vec![raw_frame]).unwrap();
	```

`HidSession<T>` is `Send` whenever `T: TransportWrite + Send`, so it
can be moved into a tokio task or threaded LLM loop. The Drop impl
calls `try_close_all` inside `catch_unwind`, so a panic in user code
never leaves a half-open UHID device on the device side.

## Agent control session

`AgentControlSession` is the highest-level std-only facade for agent
runtimes. It consumes the scrcpy dummy/meta prefix, opens the requested
UHID devices, exposes a cloneable `HidClient` for low-latency command
producers, and keeps one byte-aligned reader for device replies.
It also provides common intent helpers (`tap`, `swipe`, `cancel_touch`,
`type_text`, `type_text_strict`, typed UHID scancode key/tap helpers,
`double_tap`, `long_press`, `pinch`, `pinch_points`, `three_finger_screenshot`, `scroll`, normalized `AgentPoint` / `AgentRect` tap/swipe/scroll helpers, `press_back`,
`press_home`, `open_recents`, typed Android key taps, `back_or_screen_on`, volume keys, panels,
display/torch/camera controls, `launch_app`, clipboard readback and ACK waits)
so agents do not need to assemble low-level command enums for routine actions. On
`TcpStream` readers, timeout variants temporarily set the socket read
timeout and return `Error::AgentTimeout` on `TimedOut` / `WouldBlock`.
Touch helpers batch DOWN/MOVE/UP/CANCEL frames into a fixed stack buffer
before dispatch, reducing channel sends without heap allocation on routine
taps and swipes. Use `try_tap`, `try_tap_pointer`, `try_tap_point`,
`try_tap_rect`, `try_tap_rect_at_pointer`, `try_double_tap`, or
`try_double_tap_rect_at_pointer` when a direct tap/double-tap should also
respect non-blocking dispatcher back-pressure and still end with a checked
barrier. Use `try_scroll`, `try_scroll_point`,
`try_scroll_rect_at_with_buttons`, and related variants for the same checked
non-blocking behavior on direct absolute scrolls. Use `try_mouse_motion`,
`try_mouse_button_state`, and `try_mouse_scroll` for checked non-blocking UHID
mouse reports in relative pointer-control loops. Use `try_key`,
`try_tap_scancode`, `try_scancode_chord`, `try_inject_android_key_event`,
`try_tap_android_key`, `try_press_home`, and related `try_press_*` /
`try_volume_*` helpers for checked non-blocking keyboard and Android key
dispatch. Use direct gamepad `send_button`, `send_stick_raw`,
`send_frame_unchecked`, `send_frame_packed`, and matching `try_send_*` helpers
for checked non-blocking UHID gamepad updates without dropping to a cloned
client. Use `try_set_screen_power`, `try_resize_display`, `try_set_torch`,
`try_configure_ai`, `try_query_ai`, `try_launch_app`, and
`try_request_clipboard_key` for checked non-blocking non-input control and AI
extension commands. Agent waits and gesture boundaries use
`HidClient::flush_wait`
when they must wait until the dispatcher has processed prior commands;
that barrier also returns the first dispatcher-side command error observed
since the previous acknowledged flush. Use `AgentControlSession::close_checked`
when an agent needs to recover the transport/reader and still inspect queued
command errors at shutdown. Use
`AgentControlSession::detach_latest_frame_summary_receiver` when the agent
wants newest-only AI frame perception: it explicitly moves the single
byte-aligned reader into a latest-frame pump, leaving the agent command path
usable through `run_actions` / `clone_client` and making
`close_transport_checked` the appropriate shutdown path for the write side.
`run_actions_and_wait_for_next_latest_frame` and its predicate/seq/timestamp
variants combine a checked action barrier with the detached latest-frame cache,
capturing the cache version after dispatch so they wait for a post-barrier
snapshot instead of returning a pre-action cached frame. Their `*_timeout`
variants bound the observation wait and return `Error::AgentTimeout("latest frame summary")`
when no matching post-barrier frame arrives within the control-loop budget. If
the enqueue side must never block on dispatcher back-pressure, use the matching
`try_run_actions_and_wait_for_next_latest_frame` family; it uses
`try_run_actions` for non-blocking action enqueue plus a checked barrier before
waiting on the same newest-only latest-frame cache. Use
`try_run_actions_and_wait_for_next_latest_target_rect` or
`try_run_actions_and_tap_next_latest_target_at_pointer_timeout` when the same
non-blocking action path should continue directly into generic target selection
or anchored typed-pointer target taps. If
the agent observes before choosing actions, use
`LatestFrameSummaryReceiver::observe` with the
`run_actions_and_wait_for_next_latest_frame_after_observation` family to accept
any cached/new frame observed since that one-read boundary after the checked
action barrier. Boundary and raw-version variants remain available for callers
that store only `LatestFrameSummaryBoundary` or the raw counter directly. The
generic `AgentTargetSelector` helpers also expose
`run_actions_and_wait_for_next_latest_target_rect_after_observation` and
`run_actions_and_tap_next_latest_target_after_observation_timeout` for immediate
selection/tap on that explicit observation boundary.
`run_actions_and_tap_next_latest_target_at_pointer_timeout` composes the same
post-barrier latest-frame wait with immediate `AgentTargetSelector` selection
and tap dispatch, including center/anchor/typed-pointer variants for indexed
objects, best objects, class-filtered objects, indexed text, and largest text.
When a planner wants a zero-wait decision from the frame it just observed,
`latest_observation_target_rect` and `tap_latest_observation_target_at_pointer`
select or tap directly from `LatestFrameSummaryObservation`, returning `None`
without dispatch if the observation has no snapshot or matching target.
For zero-wait target dispatch from an already cached snapshot, use
`tap_latest_target_at_pointer`, `tap_latest_object_selector_at_pointer`, or
`tap_latest_largest_text_region_at` to reuse the same object/text selection and
anchored tap behavior as the ordered `tap_next_*` helpers without waiting for
another frame.

The intended low-latency loop is to observe once, plan against that stable
snapshot/boundary, dispatch with one checked barrier, then wait only for frames
newer than the observation if the plan needs post-action feedback:

```rust,no_run
use std::time::Duration;

use android_hid_connect::{
    AgentAction, AgentControlSession, AgentTargetSelector, OpenRequest, TouchPointerId,
};

let (_prefix, mut agent) =
    AgentControlSession::connect_tcp("127.0.0.1", 27183, OpenRequest::all()).unwrap();
agent.set_screen_size(1080, 2400).unwrap();

let (latest, _latest_pump) = agent.detach_latest_frame_summary_receiver().unwrap();
let observation = latest.observe();

// Reuse the already-observed frame when it has a suitable target.
let tapped = agent
    .tap_latest_observation_target_at_pointer(
        &observation,
        AgentTargetSelector::best_object(),
        TouchPointerId::VIRTUAL_FINGER,
        5_000,
        5_000,
    )
    .unwrap();

if tapped.is_none() {
    let next = agent
        .run_actions_and_wait_for_next_latest_frame_after_observation_timeout(
            &[AgentAction::query_ai(0)],
            &latest,
            &observation,
            Duration::from_millis(120),
        )
        .unwrap();
    println!("{}", next.summary.describe());
}
```

For LLM/tool runtimes that already produce a list of intended steps,
`AgentAction` provides a typed plan format. `queue_actions(&[…])`
enqueues the plan without waiting, while `run_actions(&[…])` adds one
checked `flush_wait` boundary after the final action. `try_run_actions(&[…])`
uses non-blocking action sends plus a non-blocking checked barrier enqueue, so
high-contention schedulers can fail fast on a full command queue while still
getting dispatcher-side error reporting when the barrier is accepted. It also
preflights the plan plus barrier against the session's configured command bound
and rejects known oversized plans before partial dispatch. Touch-heavy,
low-level keyboard, Android framework key, relative mouse, absolute scroll,
and full-frame gamepad plans share fixed-stack batchers across consecutive
compatible actions and flush before cross-device boundaries, so tap/swipe,
key-macro, pointer, scroll, or gamepad-frame plans do not devolve into one
channel send per sample and do not reorder later control actions ahead of
buffered input.
`try_queue_actions(&[…])` uses
non-blocking dispatcher sends for high-contention schedulers; it returns
back-pressure errors instead of blocking, and rejects timing-dependent
`Wait` / `LongPress` actions because those require a blocking timing
barrier. Use `AgentAction::can_try_queue`,
`AgentAction::first_non_try_queueable`, or
`AgentAction::try_queueable_prefix_len` to preflight or split plans before
calling `try_queue_actions`. Use `AgentAction::first_blocking_timing` or
`AgentAction::blocking_timing_prefix_len` when a mixed-plan scheduler only
needs the handoff point for the blocking suffix. Known timing incompatibilities
are rejected before any part of the plan is dispatched.
`try_queue_actions_prefix` intentionally queues only the leading non-blocking
segment and returns the number of actions accepted, which lets a scheduler
route the remaining blocking suffix through `run_actions`; malformed metadata
before the first blocking barrier is rejected before that prefix is dispatched.
`try_run_actions_prefix` uses the same suffix handoff boundary, but dispatches
the accepted prefix through `try_run_actions` so schedulers get a checked
dispatcher barrier, command-bound preflight, and command-error reporting without
inspecting malformed metadata in the blocking suffix.
Use `AgentAction::structural_error`,
`AgentAction::validate_structure`, `AgentAction::first_structural_error`, or
`AgentAction::validate_plan_structure` to reject malformed fixed-buffer
lengths, fixed keyboard chords, unsupported strict-text characters, and
oversized app-launch names or rect-relative basis-point anchors before
`queue_actions` dispatches any earlier plan action. For the full non-blocking path,
`AgentAction::first_try_queue_error` and `AgentAction::validate_try_queue_plan`
report the first malformed action or blocking timing barrier in plan order
before `try_queue_actions` dispatches any earlier action.
When a scheduler needs all of that information in one pass, use
`AgentAction::plan_summary(&actions)` or `AgentPlanSummary::analyze(&actions)`.
The summary reports structural and try-queue rejection indexes, blocking prefix
lengths, and estimated dispatcher command pressure for `queue_actions`,
`run_actions`, full `try_queue_actions`, checked `try_run_actions`, and
unchecked or checked prefix dispatch through `try_queue_actions_prefix` /
`try_run_actions_prefix`.
Use `try_queue_dispatch_fits_bound` for unchecked non-blocking enqueue,
`try_run_dispatch_fits_bound` when a final checked barrier is required, or
`try_queue_prefix_dispatch_fits_bound` / `try_run_prefix_dispatch_fits_bound`
when routing only a leading slice to a bounded non-blocking command queue; the
checked-prefix helper reserves the final barrier even when the accepted prefix
is empty. These helpers also account for static preflight errors, so a zero
command estimate for a malformed plan is not mistaken for a safe no-op. Use
`AgentAction::bounded_try_queue_prefix(&actions, bound)` when
the whole plan or blocking prefix does not fit and the scheduler needs the
longest leading slice that is still valid for `try_queue_actions`. Use
`AgentAction::bounded_try_run_prefix(&actions, bound)` when that slice must
also reserve one dispatcher command for a final checked barrier. Use
`AgentControlSession::bounded_try_queue_prefix_with_session_bound(&actions)` to
analyze that split against the session's actual bounded queue without
dispatching, or
`AgentControlSession::bounded_try_run_prefix_with_session_bound(&actions)` for
the checked-barrier variant. Use
`AgentControlSession::try_queue_actions_bounded_prefix(&actions, bound)` to
compute and dispatch that slice in one call; it queues prefixes stopped by
command budget or blocking timing, but rejects malformed metadata anywhere in
the supplied plan before dispatching any accepted prefix. Use
`try_run_actions_bounded_prefix(&actions, bound)` to dispatch the accepted
prefix and then enqueue a non-blocking checked barrier. Use
`AgentControlSession::command_bound()` and
`try_queue_actions_bounded_prefix_with_session_bound(&actions)` or
`try_run_actions_bounded_prefix_with_session_bound(&actions)` when the
scheduler should use the actual dispatcher queue capacity configured by
`from_parts_with_bound`. The returned `AgentPlanBoundedPrefix` exposes
`accepted_range`, `remaining_range`, `estimated_checked_dispatch_commands`, and
checked `split_slice` helpers so schedulers can advance action queues or
parallel metadata arrays without allocation or hand-written index math.
`AgentPlanBoundedPrefixStop` also has `is_command_bound`, `is_blocking_timing`,
`is_try_queue_error`, `index`, `error`, and `required_dispatch_commands`
helpers so loops can decide whether to retry with a larger budget, hand off to
a blocking path, or reject the plan without repeating enum matching.
Android framework key injection, DOWN/UP taps, and fixed-stack
Android key event batches can use `AndroidKeycode` and `AndroidKeyAction` constants, and low-level touch injection can use
`TouchAction` and `TouchPointerId` constants, avoiding raw
action/keycode/touch-action/pointer-id magic numbers in agent plans and tools.
`TouchPointerId` exposes scrcpy-compatible mouse, generic-finger, and
virtual-finger ids for callers that need those special pointer semantics, and
`AgentAction` has pointer-aware tap/double-tap/swipe/long-press constructors so
planned gestures keep those ids through fixed-stack batching.
Low-level UHID keyboard macros can use
`KeyboardFrameBatcher` or fixed `KEYBOARD_BATCH_FRAMES` actions to enqueue
scancode edge sequences with one dispatcher command. Common UHID keyboard
shortcuts can use `KeyboardChordFrame` or `AgentAction::ctrl_scancode` /
`try_scancode_chord`, which expand into ordered down/up edge batches while
preserving modifier state until the final release. Android framework key
macros can use `AndroidKeyFrameBatcher` or fixed
`ANDROID_KEY_BATCH_FRAMES` actions for the same channel-pressure reduction.
Android absolute scroll plans can use `AgentScrollFrame` and fixed
`SCROLL_BATCH_FRAMES` actions for adjacent `INJECT_SCROLL_EVENT` samples.
High-rate gamepad plans can use fixed
`GAMEPAD_BATCH_FRAMES` raw or packed frame actions, and consecutive raw,
unchecked, or packed full-frame gamepad actions are folded into
plan-scoped batches when their wire semantics match. An agent can enqueue
up to 32 gamepad samples through one planned dispatcher action without a
`Vec` allocation. Relative UHID mouse helpers cover motion, button state,
scroll, and `MouseFrameBatcher` fixed-stack batching for pointer-control loops.
Vision-driven agents can use `AgentPoint` for normalized screenshot coordinates
and `AgentRect` for object/text detection boxes, converting them at dispatch
time using the session's tracked screen size so the same plan can run against
different device resolutions. Rect helpers default to the center point, or use
`AgentRect::try_point_at_basis_points`, `tap_rect_at`, `double_tap_rect_at`,
`long_press_rect_at`, `swipe_rect`, and `scroll_rect_at` to target relative
anchors inside a detected UI region without hand-written coordinate math.
`AgentRect` can also select indexed or best object detections and largest text
regions directly from an AI `FrameSummary`.
Use `AgentObjectSelector` when object targets need class and minimum-confidence
constraints while keeping the same deterministic confidence/area tie-break.
Use `AgentTargetSelector` when a planner needs one typed value for indexed
objects, best object, class-filtered object, indexed text, or largest text.
`AgentControlSession` builds on that with `wait_for_best_object_rect`,
`wait_for_best_object_class_rect`, `wait_for_object_selector_rect`,
unified selector waits such as `wait_for_target_rect` and
`run_actions_and_wait_for_target_rect_with_limit`, plus TCP-bounded
`wait_for_target_rect_timeout`,
`wait_for_largest_text_region_rect`, frame synchronization helpers such as
`wait_for_scene_change`, `wait_for_motion`, `wait_for_stable_frames`,
frame-budgeted `*_with_limit` variants such as
`wait_for_scene_change_with_limit`,
`wait_for_object_selector_rect_with_limit`,
`run_actions_and_wait_for_stable_frames_with_limit`, and
`run_actions_and_wait_for_object_selector_rect_with_limit`,
fresh-frame gates such as `wait_for_frame_summary_after_seq`,
`wait_for_frame_summary_after_timestamp`,
`run_actions_and_wait_for_frame_summary_after_seq`, and
`run_actions_and_wait_for_frame_summary_after_timestamp`,
detached latest-frame target taps such as
`run_actions_and_wait_for_next_latest_target_rect_timeout` and
`run_actions_and_tap_next_latest_target_at_pointer_timeout`,
optional bounded taps for indexed/best/class/selector/text target families
such as `tap_next_object_at_pointer_with_limit`,
`tap_next_object_class_pointer_with_limit`,
`tap_next_text_region_at_pointer_with_limit`, and
`run_actions_and_tap_next_largest_text_region_at_with_limit`,
unified selector taps such as `tap_next_target_at_pointer_with_limit` and
`run_actions_and_tap_next_target_at_pointer_with_limit`, plus TCP-bounded
`tap_next_target_at_pointer_timeout` and
`run_actions_and_tap_next_target_at_pointer_timeout`,
`run_actions_and_wait_for_scene_change`,
`run_actions_and_wait_for_motion`,
`run_actions_and_wait_for_stable_frames`,
`run_actions_and_wait_for_object_selector_rect`,
`run_actions_and_wait_for_largest_text_region_rect`,
`run_actions_and_tap_next_best_object_at`,
`run_actions_and_tap_next_object_class_at`,
`run_actions_and_tap_next_text_region_pointer`,
`run_actions_and_tap_next_object_selector_at_pointer`,
`run_actions_and_tap_next_largest_text_region_at`, and
`tap_next_*` / `tap_next_*_at` / `tap_next_*_pointer` helpers that block on the
mixed `DeviceEvent` stream and dispatch center, anchored, or typed-pointer taps
through the normal touch batch path. The `run_actions_and_tap_next_*` family
covers the same indexed/best/class/selector/text target families as
`tap_next_*`, with one checked action-plan barrier before the target wait.
TCP-backed sessions also expose matching `*_timeout` variants for bounded
vision waits and taps, including unified `AgentTargetSelector` waits/taps,
fresh-frame waits, anchored typed-pointer target taps, and action-plus-target
waits. Use `*_with_limit` when the agent needs to bound
the number of AI frame summaries inspected; bounded tap variants return
`Ok(None)` without dispatching a target tap when the budget is exhausted. Use
`*_timeout` when it needs a wall-clock read bound on `TcpStream`.
AI-enabled servers can be controlled through `configure_ai`, `query_ai`, and
`pause_ai` on `HidSession`, `HidClient`, or `AgentControlSession`; the same
operations are available as `AgentAction::configure_ai`,
`AgentAction::query_ai`, and `AgentAction::pause_ai` for mixed action plans.
Use `AgentControlSession::query_ai_and_wait_stats`,
`run_actions_and_query_ai_and_wait_stats`, or their TCP `*_timeout` variants
when an agent needs one ordered query/read workflow. Clipboard workflows have
the same ordered helpers:
`run_actions_and_get_clipboard_and_wait_key` queues an action plan, appends
`GET_CLIPBOARD`, and uses one checked dispatcher boundary before reading;
`run_actions_and_set_clipboard_and_wait_ack` does the same for
`SET_CLIPBOARD` plus matching ACK.
Two-pointer pinch/spread gestures can use raw pixel endpoints or normalized
`AgentPoint` endpoints and still share the plan-scoped touch batcher. Custom touch paths can use `AgentTouchFrame`, which stores wire-format
integer pressure and batches with adjacent tap/swipe/cancel actions in the
same plan.

For custom gesture paths, use `TouchFrameBatcher` directly; it buffers
up to `TOUCH_BATCH_FRAMES` touch samples on the caller thread and sends
them through one dispatcher command per flush.

```rust,no_run
use android_hid_connect::{
    AgentAction, AgentControlSession, AgentObjectSelector, AgentPlanBoundedPrefixStop,
    AgentPlanSummary, AgentPoint, AgentRect, AgentScrollFrame, AgentTargetSelector,
    AgentTouchFrame, AndroidKeyAction, AndroidKeycode, AndroidKeyFrame, ClipboardCopyKey,
    GamepadFrameRaw, KeyboardChordFrame, KeyboardFrame, Modifiers, MouseButton, MouseFrame,
    OpenRequest, Scancode, TouchPointerId,
};

let (prefix, mut agent) = AgentControlSession::connect_tcp(
    "127.0.0.1",
    27183,
    OpenRequest::all().with_coalesce(false),
).unwrap();
println!("connected to {}", prefix.device_name);
agent.set_screen_size(1080, 2400).unwrap();

let client = agent.clone_client();
client
    .send_frame_unchecked(GamepadFrameRaw::new(1, 0, 0, 0, 0, 0, 0))
    .unwrap();

agent.tap(540, 1200).unwrap();
agent.tap_pointer(TouchPointerId::VIRTUAL_FINGER, 540, 1200).unwrap();
agent.tap_point(AgentPoint::CENTER).unwrap();
let text_box = AgentRect::try_from_pixels(120, 500, 841, 121, 1080, 2400).unwrap();
agent.tap_rect(text_box).unwrap();
agent.tap_rect_at(text_box, 1_000, 5_000).unwrap(); // left-side anchor in the box
agent.swipe_rect(text_box, (0, 5_000), (10_000, 5_000), 4).unwrap();
agent
    .pinch_points(
        AgentPoint::try_from_basis_points(4_000, 5_000).unwrap(),
        AgentPoint::try_from_basis_points(3_000, 5_000).unwrap(),
        AgentPoint::try_from_basis_points(6_000, 5_000).unwrap(),
        AgentPoint::try_from_basis_points(7_000, 5_000).unwrap(),
        6,
    )
    .unwrap();
agent.cancel_touch(0).unwrap();
agent
    .mouse_motion_buttons(12, -4, &[MouseButton::Left])
    .unwrap();
agent.mouse_scroll(0, -1).unwrap();
agent.double_tap(540, 1200).unwrap();
agent.scroll(540, 1200, 0.0, -16.0).unwrap();
agent.type_text("hello from agent").unwrap();
agent.type_text_strict("ASCII-only strict text").unwrap();
agent.tap_scancode(Scancode::A, Modifiers::LSHIFT).unwrap(); // Shift+A via UHID
agent.tap_android_key(AndroidKeycode::BACK).unwrap(); // Android KeyEvent DOWN+UP
agent.press_back().unwrap();
agent.back_or_screen_on(AndroidKeyAction::UP).unwrap();
agent
    .inject_android_key_event(AndroidKeyAction::UP, AndroidKeycode::BACK, 0, 0)
    .unwrap();
agent
    .configure_ai(android_hid_connect::AI_FLAG_OBJECTS | android_hid_connect::AI_FLAG_TEXT, 16, 0)
    .unwrap();
agent.query_ai(0).unwrap();
agent.show_notifications().unwrap();
agent.run_actions(&[
    AgentAction::tap(540, 1200),
    AgentAction::tap_pointer(TouchPointerId::VIRTUAL_FINGER, 540, 1200),
    AgentAction::tap_point(AgentPoint::CENTER),
    AgentAction::swipe_points_pointer(
        TouchPointerId::VIRTUAL_FINGER,
        AgentPoint::try_from_basis_points(1_000, 8_000).unwrap(),
        AgentPoint::try_from_basis_points(9_000, 8_000).unwrap(),
        4,
    ),
    AgentAction::tap_rect(text_box),
    AgentAction::tap_rect_at(text_box, 1_000, 5_000),
    AgentAction::swipe_rect(text_box, (0, 5_000), (10_000, 5_000), 4),
    AgentAction::swipe_points(
        AgentPoint::try_from_basis_points(1_000, 8_000).unwrap(),
        AgentPoint::try_from_basis_points(9_000, 8_000).unwrap(),
        4,
    ),
    AgentAction::pinch_points(
        AgentPoint::try_from_basis_points(4_000, 5_000).unwrap(),
        AgentPoint::try_from_basis_points(3_000, 5_000).unwrap(),
        AgentPoint::try_from_basis_points(6_000, 5_000).unwrap(),
        AgentPoint::try_from_basis_points(7_000, 5_000).unwrap(),
        6,
    ),
    AgentAction::swipe((160, 1800), (920, 1800), 4),
    AgentAction::cancel_touch(0),
    AgentAction::try_touch_frames(&[
        AgentTouchFrame::down_pointer(TouchPointerId::VIRTUAL_FINGER, 500, 900, u16::MAX),
        AgentTouchFrame::move_pointer_to(TouchPointerId::VIRTUAL_FINGER, 560, 960, 32768),
        AgentTouchFrame::up_pointer(TouchPointerId::VIRTUAL_FINGER, 560, 960),
    ]).unwrap(),
    AgentAction::scroll(540, 1200, 0, -16),
    AgentAction::scroll_point(AgentPoint::CENTER, 0, -16),
    AgentAction::scroll_rect(text_box, 0, -16),
    AgentAction::scroll_rect_at(text_box, 1_000, 5_000, 0, -16),
    AgentAction::try_scroll_batch(&[
        AgentScrollFrame::scroll(540, 1200, 0, -16),
        AgentScrollFrame::new(540, 1200, 0, -8, 0),
    ]).unwrap(),
    AgentAction::type_text("hello from a typed plan"),
    AgentAction::type_text_strict("strict ASCII plan text"),
    AgentAction::tap_scancode(Scancode::A, Modifiers::LSHIFT),
    AgentAction::ctrl_scancode(Scancode::C),
    AgentAction::try_scancode_chord(&[Scancode::K, Scancode::C], Modifiers::LCTRL).unwrap(),
    AgentAction::keyboard_chord_fixed(KeyboardChordFrame::scancode(Scancode::V, Modifiers::LCTRL)),
    AgentAction::try_key_batch(&[
        KeyboardFrame::scancode_down(Scancode::LeftCtrl, Modifiers::LCTRL),
        KeyboardFrame::scancode_down(Scancode::C, Modifiers::LCTRL),
        KeyboardFrame::scancode(Scancode::C, false, Modifiers::LCTRL),
        KeyboardFrame::scancode_up(Scancode::LeftCtrl),
    ]).unwrap(),
    AgentAction::mouse_motion_buttons(12, -4, &[MouseButton::Left]),
    AgentAction::try_mouse_batch(&[
        MouseFrame::motion_buttons(4, 0, &[MouseButton::Left]),
        MouseFrame::motion(0, 6, 0),
    ]).unwrap(),
    AgentAction::tap_android_key(AndroidKeycode::BACK),
    AgentAction::try_android_key_batch(&[
        AndroidKeyFrame::down(AndroidKeycode::ENTER, 0),
        AndroidKeyFrame::up(AndroidKeycode::ENTER, 0),
    ]).unwrap(),
    AgentAction::try_gamepad_frame_batch_unchecked(&[
        GamepadFrameRaw::new(1, 0, 0, 0, 0, 0, 0),
        GamepadFrameRaw::new(1, 1000, -1000, 0, 0, 0, 0),
    ]).unwrap(),
    AgentAction::configure_ai(android_hid_connect::AI_FLAG_OBJECTS, 16, 0),
    AgentAction::query_ai(0),
    AgentAction::pause_ai(),
    AgentAction::back_or_screen_on(AndroidKeyAction::DOWN),
    AgentAction::SetScreenPower { on: true },
]).unwrap();
let quick_plan = [AgentAction::tap(10, 20), AgentAction::Flush];
let quick_summary = AgentPlanSummary::analyze(&quick_plan);
assert_eq!(quick_summary, AgentAction::plan_summary(&quick_plan));
assert!(quick_summary.try_queue_prefix_dispatch_fits_bound(2));
assert!(quick_summary.try_run_prefix_dispatch_fits_bound(3));
let quick_prefix = AgentAction::bounded_try_queue_prefix(&quick_plan, 2);
assert_eq!(quick_prefix.stop, AgentPlanBoundedPrefixStop::EndOfPlan);
println!(
    "quick plan queue commands ~= {}",
    quick_summary.estimated_queue_dispatch_commands
);
agent.run_actions(&quick_plan).unwrap();
let ack = agent
    .set_clipboard_and_wait_ack("copied by agent", false)
    .unwrap();
println!("clipboard ack sequence={ack}");
let current = agent
    .get_clipboard_and_wait_key(ClipboardCopyKey::COPY)
    .unwrap();
println!("device clipboard={current:?}");
let maybe_current = agent.get_clipboard_and_wait_key_timeout(
    ClipboardCopyKey::COPY,
    std::time::Duration::from_millis(500),
);
println!("bounded clipboard read={maybe_current:?}");
let copied_after_action = agent
    .run_actions_and_get_clipboard_and_wait_key(
        &[AgentAction::tap(10, 20)],
        ClipboardCopyKey::COPY,
    )
    .unwrap();
println!("action clipboard={copied_after_action:?}");
let ack_after_action = agent
    .run_actions_and_set_clipboard_and_wait_ack(
        &[AgentAction::tap(10, 20)],
        "queued clipboard",
        false,
    )
    .unwrap();
println!("action clipboard ack sequence={ack_after_action}");
let stable = agent
    .run_actions_and_wait_for_stable_frames(
        &[AgentAction::tap_rect(text_box)],
        2,
    )
    .unwrap();
println!("stable after action at frame#{}", stable.frame_seq);
let ai_stats = agent
    .query_ai_and_wait_stats_timeout(0, std::time::Duration::from_millis(500))
    .unwrap();
println!("ai stats fps={:.1}", ai_stats.current_fps);
let action_ai_stats = agent
    .run_actions_and_query_ai_and_wait_stats_timeout(
        &[AgentAction::tap(10, 20)],
        0,
        std::time::Duration::from_millis(500),
    )
    .unwrap();
println!("action ai stats fps={:.1}", action_ai_stats.current_fps);
let object_rect = agent.tap_next_best_object().unwrap();
println!("tapped detected object at {:?}", object_rect.center());
let button_rect = agent
    .tap_next_object_selector(AgentObjectSelector::class_min_confidence(7, 220))
    .unwrap();
println!("tapped confident class 7 object at {:?}", button_rect.center());
let checkbox_rect = agent
    .tap_next_object_selector_at(AgentObjectSelector::class_min_confidence(9, 220), 1_000, 5_000)
    .unwrap();
println!("tapped left anchor in class 9 object at {:?}", checkbox_rect);
let stylus_rect = agent
    .tap_next_best_object_at_pointer(TouchPointerId::VIRTUAL_FINGER, 5_000, 5_000)
    .unwrap();
println!("tapped typed-pointer target at {:?}", stylus_rect.center());
let acted_target = agent
    .run_actions_and_tap_next_object_selector_at_pointer(
        &[AgentAction::tap(540, 1200)],
        AgentObjectSelector::class_min_confidence(7, 220),
        TouchPointerId::VIRTUAL_FINGER,
        5_000,
        5_000,
    )
    .unwrap();
println!("acted, observed, and tapped target at {:?}", acted_target.center());
let acted_text = agent
    .run_actions_and_tap_next_text_region_pointer(
        &[AgentAction::tap(540, 1200)],
        0,
        TouchPointerId::VIRTUAL_FINGER,
    )
    .unwrap();
println!("acted, observed, and tapped text at {:?}", acted_text.center());
let generic_target = agent
    .run_actions_and_tap_next_target_at_pointer_with_limit(
        &[AgentAction::tap(540, 1200)],
        AgentTargetSelector::object_class_min_confidence(7, 220),
        TouchPointerId::VIRTUAL_FINGER,
        (5_000, 5_000),
        4,
    )
    .unwrap();
println!("bounded generic target tap={generic_target:?}");

let report = agent.close_checked().unwrap();
if let Err(err) = report.command_result {
    eprintln!("queued command failed before close: {err}");
}
let closed = report.closed;
let _stream = closed.transport;
```

`HidClient::request_clipboard_key(ClipboardCopyKey::COPY)` is
request-only by design: it writes scrcpy `GET_CLIPBOARD`, then the
payload arrives later as `DeviceMessage::Clipboard` on the device-message
reader. Use `AgentControlSession::get_clipboard_and_wait_key` when an
agent needs the one-call request/read workflow.

## Benchmarks

```bash
cargo bench --bench uhid_throughput
```

Benchmark cases:

| Bench | What it measures |
|-------|------------------|
| `keyboard inject_key (no I/O)` | cost of one key down/up event (no transport) |
| `uhid_input serialize`         | cost of serializing one `UHID_INPUT` message |
| `send_one into MockTransport`  | end-to-end serialize + write to a `Vec<u8>` |
| `gamepad frame pack` | cost of packing one `GamepadFrameRaw` into 15-byte payload |
| `session set_frame_raw_unchecked single` | one single-frame raw unchecked write |
| `session set_frame_raw_unchecked single (direct)` | one single-frame raw unchecked write with coalescing off |
| `session set_frame_raw_packed_batch 512` | fast-path packed frame throughput (no state diff) |
| `session set_frame_raw_batch_deduped 512` | full-state batch path with `GamepadHid` dedupe |
| `session set_frame_raw_batch_unchecked 512` | full-state batch path with no dedupe |
| `session set_frame_raw_packed_batch 512 (direct)` | packed batch path with coalescing off |
| `session set_frame_raw_batch_unchecked 512 (direct)` | full-state batch path with direct writer (coalesce off) |
| `session set_frame_raw_batch_unchecked 512 (coalesce steady-state)` | reused coalescing session steady-state batch |
| `session set_frame_raw_batch_unchecked 512 (direct steady-state)` | reused direct session steady-state batch |
| `client send_frame_unchecked one frame (steady-state)` | session path vs client single-frame dispatch overhead |
| `client gamepad frame batcher unchecked 32` | fixed-buffer `GamepadFrameBatcher` dispatch overhead |
| `client gamepad frame packed batch fixed 32` | packed-frame fixed-stack batch dispatch without `Vec` allocation |
| `client gamepad frame packed batcher 32` | packed-frame fixed-stack batcher hot-path behavior |

## Real-time tuning checklist

For a 60/120/240Hz gamepad loop, these settings are usually the
best starting point:

- Prefer `set_frame_raw_packed_batch` when you can emit packed
  15-byte reports, or `set_frame_raw_batch_unchecked` when you only
  have normalized fields.
- If you already keep packed frames in a fixed `[[u8; 15]; 32]`
  ring, use `send_frame_packed_batch_fixed` to keep the command hot
  path allocation-free.
- If your loop already emits fixed packed-batch groups, use
  `PackedGamepadFrameBatcher` to aggregate locally and avoid one
  command allocation per sample.
- In high-contention producer loops, use `try_*` APIs on batchers:
  `try_push`, `try_push_many`, `try_flush` and `try_send_*`.
  They return `SessionLifecycle` when the bounded channel is full
  instead of blocking; keep the loop running and decide whether to
  skip or retry samples based on your control tolerance.
- For per-axis/button high-rate updates, prefer the new one-shot
  `send_*` helpers (`send_stick_raw`, `send_left_stick_raw`,
  `send_sticks_raw`, `send_button`, `send_buttons`) and their
  `try_*` counterparts to avoid full-frame packing on every tick.
- For relative pointer-control loops, use `client.mouse_motion`,
  `client.mouse_buttons`, `client.mouse_scroll`, or `MouseFrameBatcher`
  to aggregate pointer samples locally before one dispatcher send.
  If the planner already has a fixed ring of up to `MOUSE_BATCH_FRAMES`
  samples, use `client.send_mouse_batch_fixed` with `MouseFrame`.
- For high-rate absolute scroll paths, use `ScrollFrameBatcher` or
  `send_scroll_batch_fixed` with `ScrollFrame` to aggregate
  `INJECT_SCROLL_EVENT` samples while preserving screen-size metadata
  and event order.
- For high-sample touch paths, use `TouchFrameBatcher` instead of
  sending individual `MultitouchMove` commands; it keeps gesture
  planning allocation-free and reduces dispatcher channel pressure.
  Use `push_many_slice` when a planner already produced a contiguous
  touch path, and use `try_down`, `try_move_to`, `try_up`,
  `try_push_many_slice`, and `try_flush` when the gesture producer must
  never block on the dispatcher queue.
- When using `HidClient`/`AgentControlSession` for absolute touch,
  call `set_screen_size(width, height)` before gesture injection if the
  target display is not the default 1080x1920.
- Use `client.flush()`/`client.try_flush()` when you only need to enqueue a
  coalesced-write drain request. Use `client.flush_wait()` at deterministic
  phase boundaries where the caller must wait until the dispatcher has
  processed all earlier commands and surface any earlier command error. Use
  `client.try_flush_wait()` when the barrier enqueue itself must respect
  non-blocking back-pressure semantics.
- Use `client.close_wait()` or `AgentControlSession::close_checked()` at
  shutdown when command errors matter; plain `close()` remains a lightweight
  fire-and-forget shutdown request.
- When you emit one frame per tick and still use `HidClient`, use
  `GamepadFrameBatcher` to aggregate a few frames locally before the
  channel send. For batches up to `32`, the batcher now uses a stack
  buffer to avoid per-batch `Vec` allocation:

```rust,no_run
use android_hid_connect::client::{GamepadFrameBatcher, HidClient};
use android_hid_connect::session::GamepadFrameRaw;

fn consume_loop(client: &HidClient, frame_iter: impl Iterator<Item = GamepadFrameRaw>) {
    let mut batcher = GamepadFrameBatcher::unchecked(client, 8);
    for frame in frame_iter {
        // Batches locally; flush is automatic on Drop.
        batcher.push(frame).unwrap();
    }
}
```

- For batch sizes above `32`, `GamepadFrameBatcher` and
  `PackedGamepadFrameBatcher` use a vector-backed path. Flush moves that
  vector into the dispatcher command without cloning the frame payload;
  if the channel is full or disconnected, the batch is recovered into
  the batcher so callers can retry or drop explicitly.
- When sending full frames through `HidClient` one-by-one, use
  `client.send_frame_unchecked` to keep the dispatcher path on the
  no-dedupe branch.
- Session-side batch helpers (`set_frame_raw_batch`, `set_frame_raw_batch_unchecked`
  and `set_frame_raw_packed_batch`) also fall back to the single-frame
  path automatically when length is 1, so you can keep call sites uniform.
- Use `OpenRequest::gamepad_only_realtime()` (coalescing disabled) for
  absolute minimum per-frame latency.
- `OpenRequest::gamepad_only().with_coalesce_window(Duration::ZERO)` is now
  treated as direct mode, so it is equivalent from a latency perspective.
- All `*_batch` client APIs now auto-fallback to the single-frame command
  when the caller passes exactly one frame, so you can keep a single call
  shape and still stay on the shortest path.
- For AI/game loops that already have full frame structs, keep `set_frame_raw_batch_unchecked`
  on the hot path and pass frames as a single contiguous slice (`&[GamepadFrameRaw]`)
  to avoid temporary per-frame packing.
- Keep coalescing enabled when your loop can tolerate 1ms buckets, and
  tune with:

```rust
let low_jitter = OpenRequest::gamepad_only()
    .with_coalesce_window(std::time::Duration::from_millis(1))
    .with_coalesce_hard_limit(2048);
```

- Set `with_coalesce_hard_limit` to a smaller value to force more frequent
  flushes when control bandwidth is constrained.

- If you need producer threads, create the dispatcher with a larger bound
  to avoid back-pressure in bursts:

```rust
let (client, dispatcher) = session
    .into_client_with_bound(2048)
    .unwrap();
```

- If you observe `try_send_*` returning `SessionLifecycle`, reduce per-frame
  fanout first (skip non-essential sends), then lower batch size, then raise
  the bound by 2–4×.

## Wire format

The control socket is a plain TCP stream. Every message starts with a
1-byte type tag, followed by a type-specific payload. The three UHID
messages are:

```text
UHID_CREATE  (type = 12)
   u8  type = 12
   u16 id                  (big-endian, 1 = keyboard, 2 = mouse, 3..=10 = gamepads)
   u16 vendor_id           (big-endian)
   u16 product_id          (big-endian)
   u8  name_len            (1 byte length prefix; max 127)
   [name bytes]
   u16 report_desc_size    (big-endian)
   [report_desc bytes]

UHID_INPUT   (type = 13)
   u8  type = 13
   u16 id                  (big-endian)
   u16 size                (big-endian, ≤ 15)
   [data bytes]

UHID_DESTROY (type = 14)
   u8  type = 14
   u16 id                  (big-endian)
```

Reports must follow the report descriptors shipped with each driver:

* **Keyboard** — 8 bytes: modifier bitmap, reserved, 6× scancode
  (USB HID Usage IDs 0x04..=0x65, or 0xE0..=0xE7 for modifiers).
  When more than 6 non-modifier keys are pressed, the slot list is
  filled with the USB HID `ErrorRollOver` (0x01) "phantom state".

* **Mouse** — 5 bytes: button bitmap (5 buttons, 3 padding), signed
  relative X/Y motion, signed vertical wheel, signed horizontal AC
  Pan. Each motion byte is clamped to `[-127, 127]`.

* **Gamepad** — 15 bytes: 4× 16-bit stick axes (LE, rescaled from
  i16 to u16), 2× 16-bit triggers, 16-bit button bitmap, 4-bit hat
  switch. The dpad state is encoded in the upper 16 bits of the
  internal button bitmap and transformed into a hat switch value
  0..=8 (0 = centred).

The full list of supported control messages (clipboard, touch, display
power, …) is in `src/control/message.rs::ControlMessage`.

Native scrcpy device-to-host messages are parsed by
`device::read_device_message` or `device::DeviceMessageReceiver`. This
reverse stream is type-specific rather than a generic envelope:

```text
DEVICE_MSG_CLIPBOARD       type(0) + text_len(4 BE) + UTF-8 text
DEVICE_MSG_ACK_CLIPBOARD   type(1) + sequence(8 BE)
DEVICE_MSG_UHID_OUTPUT     type(2) + id(2 BE) + size(2 BE) + data
```

`ACK_CLIPBOARD` and `UHID_OUTPUT` intentionally do not have a `u32`
payload length prefix; this matches scrcpy v2.7's
`DeviceMessageWriter` and `sc_device_msg_deserialize`.

AI-enabled streams can use `device::read_device_event` or
`DeviceMessageReceiver::read_next_event` to parse native scrcpy messages and
the AI extension envelopes (`FrameSummary`, `AiStats`, or unknown typed
envelopes) through one byte-aligned API. `AgentControlSession` exposes the same
mixed stream via `recv_device_event`, `wait_for_frame_summary`, and
`wait_for_ai_stats`; clipboard waits also skip unrelated AI events. Agent code
can use `wait_for_frame_summary_matching`, `wait_for_scene_change`,
`wait_for_motion`, `wait_for_stable_frames`,
`wait_for_frame_summary_after_seq`, `wait_for_frame_summary_after_timestamp`,
`run_actions_and_wait_for_frame_summary_after_seq`,
`run_actions_and_get_clipboard_and_wait_key`, and
`run_actions_and_set_clipboard_and_wait_ack` to synchronize after UI actions
without decoding video frames or hand-rolling dispatcher barriers.
Use `configure_ai`, `query_ai`, and `pause_ai` from `HidSession`, `HidClient`,
or `AgentControlSession` to control the AI summary pipeline; use
`AgentControlSession::query_ai_and_wait_stats` or
`run_actions_and_query_ai_and_wait_stats` to send `AI_QUERY`, flush the
dispatcher boundary, and consume the stats response in one call.

For Agent runtimes that need to send controls and consume device
responses concurrently, use the bounded background receivers. Native-only
consumers can use `spawn_default_device_message_receiver`; AI-enabled
consumers can use `spawn_default_device_event_receiver` for the unified
`DeviceEvent` stream. Perception loops that care more about the freshest
summary than ordered replay can use `spawn_latest_frame_summary_receiver`
instead; it continuously drains the mixed event stream, skips non-frame
events, and exposes a versioned `LatestFrameSummarySnapshot` plus copyable
`LatestFrameSummaryBoundary` tokens through `observe`, `boundary`, `snapshot`,
`snapshot_after_observation`, `snapshot_after_boundary`,
`snapshot_after_version`, `snapshot_matching`, `wait_next_after_observation`,
`wait_next_after_boundary`, `wait_next`, `wait_next_matching`, `wait_matching`,
`wait_after_frame_seq`, and `wait_after_timestamp`. `observe` returns
`LatestFrameSummaryObservation`, a single-read boundary plus optional snapshot
for observe-plan-act loops, with constructors (`from_parts`, `at_boundary`,
`at_version`, `from_snapshot`), a consumer (`into_snapshot`), and zero-clone
accessors for `boundary`, `boundary_version`, `snapshot`, `summary`, and
`accepts`; each blocking wait also has a `*_timeout` variant for bounded
control loops. For `AgentControlSession`, prefer
`detach_latest_frame_summary_receiver` so there is still exactly one consumer
of the byte stream; then use `run_actions_and_wait_for_next_latest_frame` when
an action should be followed by the next newest-only AI observation, or
`tap_latest_object_selector_at_pointer` / `tap_latest_largest_text_region_at`
when the current cached snapshot is already good enough to target immediately:

```rust,no_run
use android_hid_connect::{
    open_tcp, spawn_default_device_event_receiver, spawn_default_device_message_receiver,
    DeviceEvent, DeviceMessage,
};

let mut stream = open_tcp("127.0.0.1", 27183).unwrap();
let reader = stream.try_clone().unwrap();
let (device_rx, device_pump) = spawn_default_device_message_receiver(reader).unwrap();

// Keep `stream` for control writes. Consume `device_rx` on another task/thread.
if let Ok(Ok(DeviceMessage::AckClipboard { sequence })) = device_rx.recv() {
    println!("clipboard ack sequence={sequence}");
}

drop(device_rx);
let reader = device_pump.join().unwrap();

let (event_rx, event_pump) = spawn_default_device_event_receiver(reader).unwrap();
if let Ok(Ok(DeviceEvent::FrameSummary(summary))) = event_rx.recv() {
    println!("{}", summary.describe());
}
drop(event_rx);
let _reader = event_pump.join().unwrap();
```

Async runtimes can enable the optional Tokio adapter. Native-only consumers can
use `read_device_message_async` / `spawn_default_async_device_message_receiver`;
AI-enabled consumers can use `read_device_event_async` /
`spawn_default_async_device_event_receiver` for the same mixed `DeviceEvent`
stream as the std parser. Async perception loops can use
`spawn_async_latest_frame_summary_receiver`; it uses a Tokio `watch` channel so
slow consumers observe the newest `LatestFrameSummarySnapshot` instead of
building a stale frame backlog, with one-read `LatestFrameSummaryObservation`,
`LatestFrameSummaryBoundary` markers, predicate waits, and `*_timeout` waits for
bounded async control loops:

```toml
android-hid-connect = { version = "0.1", features = ["tokio"] }
```

```rust,no_run
use android_hid_connect::{
    read_device_event_async, read_device_message_async, spawn_default_async_device_event_receiver,
    spawn_default_async_device_message_receiver, DeviceEvent, DeviceMessage,
};

async fn consume<R>(mut reader: R) -> std::io::Result<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    let (mut rx, pump) = spawn_default_async_device_message_receiver(reader);
    if let Some(Ok(DeviceMessage::Clipboard(text))) = rx.recv().await {
        println!("clipboard={text}");
    }
    reader = pump.join().await?;
    let _ = read_device_message_async(&mut reader).await;

    let (mut event_rx, event_pump) = spawn_default_async_device_event_receiver(reader);
    if let Some(Ok(DeviceEvent::FrameSummary(summary))) = event_rx.recv().await {
        println!("{}", summary.describe());
    }
    let mut reader = event_pump.join().await?;
    let _ = read_device_event_async(&mut reader).await;
    Ok(())
}
```

## Crate design notes

* `HidDevice` trait is implemented by each driver and exposes
  `open_message` / `close_message`. Real-time state changes go
  through `key_event`, `motion_message`, `axis_event` etc. and
  produce a `ControlMessage::UhidInput` directly.
* `is_critical()` on `ControlMessage` matches scrcpy's
  `sc_control_msg_is_droppable`: only `UHID_CREATE` and `UHID_DESTROY`
  cannot be dropped if the underlying buffer is full.
* `ControlMessage::serialize_into` is transactional for caller-provided
  buffers: validation errors leave the original buffer contents intact,
  so high-throughput code can safely reuse scratch buffers.
* `DeviceMessageReceiver` and `spawn_device_message_receiver` keep the
  server→host side byte-aligned, so a clipboard ACK cannot desynchronise
  later UHID output reads.
* The default build depends only on `thiserror` and standard `Read` /
  `Write` traits; you can wrap any `TcpStream` / `Vec<u8>` /
  `Cursor<…>` / mock buffer and pass it to `send_one`, `send_batch`, or
  the device-message receivers. Enable the optional `tokio` feature for
  async device-message parsing.

## Testing

```bash
cargo test
cargo test --features tokio
```

The test suite includes unit tests for HID descriptors, scancode
validation, button / hat / scroll logic, control-message byte layout,
server→host device-message parsing, optional Tokio async receiver
coverage, and integration tests that drive a real local TCP socket and
verify the on-wire format byte-for-byte.

## License

MIT OR Apache-2.0, same as scrcpy.
