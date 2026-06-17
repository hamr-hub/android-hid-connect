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
