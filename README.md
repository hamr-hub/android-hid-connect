# android-hid-connect

A pure-Rust port of [scrcpy][scrcpy]'s UHID control surface — drive a
connected Android device by emitting USB HID reports over scrcpy-server's
control socket.

[scrcpy]: https://github.com/Genymobile/scrcpy

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
| `control::ControlMessage` | All 22 scrcpy control message types, byte-exact serialization.  |
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
- Use `client.try_flush()`/`client.flush()` when you want to force a drain
  of coalesced UHID_INPUT messages at deterministic loop boundaries.
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

## Crate design notes

* `HidDevice` trait is implemented by each driver and exposes
  `open_message` / `close_message`. Real-time state changes go
  through `key_event`, `motion_message`, `axis_event` etc. and
  produce a `ControlMessage::UhidInput` directly.
* `is_critical()` on `ControlMessage` matches scrcpy's
  `sc_control_msg_is_droppable`: only `UHID_CREATE` and `UHID_DESTROY`
  cannot be dropped if the underlying buffer is full.
* The library depends only on `thiserror` and `std::io::Write`; you
  can wrap any `TcpStream` / `Vec<u8>` / `Cursor<…>` / mock buffer
  that implements `Write` and pass it to `send_one` / `send_batch`.

## Testing

```bash
cargo test
```

The test suite includes 45 unit tests (HID descriptors, scancode
validation, button / hat / scroll logic, control-message byte layout)
and 7 integration tests that drive a real local TCP socket and
verify the on-wire format byte-for-byte.

## License

MIT OR Apache-2.0, same as scrcpy.
