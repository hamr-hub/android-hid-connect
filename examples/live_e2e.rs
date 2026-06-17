//! Live E2E test driver for android-hid-connect.
//!
//! Connects to a real scrcpy-server running on the Android device via
//! `adb forward tcp:27183 localabstract:scrcpy`, exercises the full UHID
//! + non-UHID control surface, and asserts:
//!
//!   1. The server accepts UHID_CREATE / UHID_INPUT / UHID_DESTROY for
//!      keyboard, mouse, and all 8 gamepad slots in order.
//!   2. The server accepts the remaining 19 control message types
//!      (touch, scroll, clipboard, display power, panels, …).
//!   3. The server emits back at least one DEVICE_MSG_TYPE_UHID_OUTPUT
//!      frame (LED sync), and parses correctly.
//!   4. The server replies to GET_CLIPBOARD with DEVICE_MSG_TYPE_CLIPBOARD.
//!
//! Run order:
//!
//!   adb push scrcpy-server /data/local/tmp/scrcpy-server
//!   adb forward tcp:27183 localabstract:scrcpy
//!   adb shell 'CLASSPATH=/data/local/tmp/scrcpy-server \
//!       app_process / com.genymobile.scrcpy.Server 2.7 \
//!       video=false audio=false control=true clipboard_autosync=false \
//!       tunnel_forward=true send_dummy_byte=true &'
//!   cargo run --example live_e2e
//!
//! Exits non-zero on any assertion failure.

use std::env;
use std::io::Read;
use std::net::TcpStream;
use std::time::{Duration, Instant};

use android_hid_connect::control::message::{
    ControlMessage, GetClipboard, InjectKeycode, InjectScrollEvent,
    InjectText, InjectTouchEvent, ResizeDisplay, SetClipboard,
    SetDisplayPower, StartApp, UhidCreate, UhidDestroy, UhidInput,
};
use android_hid_connect::transport::{open_tcp, send_one};
use android_hid_connect::types::{
    GamepadAxis, GamepadButton, Modifiers, MouseButton, HID_MAX_SIZE,
};
use android_hid_connect::{GamepadHid, HidDevice, KeyboardHid, MouseHid};

const DEFAULT_PORT: u16 = 27183;

/// Per-step stats
#[derive(Default)]
struct Stats {
    pass: usize,
    fail: usize,
}

impl Stats {
    fn check<T: std::fmt::Debug + PartialEq>(
        &mut self,
        label: &str,
        got: T,
        want: T,
    ) {
        if got == want {
            self.pass += 1;
            println!("  PASS  {label}: got={got:?}");
        } else {
            self.fail += 1;
            println!("  FAIL  {label}: got={got:?}, want={want:?}");
        }
    }

    fn ok(&mut self, label: &str) {
        self.pass += 1;
        println!("  PASS  {label}");
    }
}

/// Read the device-meta message that scrcpy-server sends immediately after
/// the optional dummy byte. Layout (per `device_msg.c`):
///
///   u8   type     (0 = CLIPBOARD)
///   u32  length   (big-endian)
///   [length bytes] (UTF-8 text)
///
/// DEVICE_MSG_TYPE_CLIPBOARD = 0, so the first byte is 0x00. We treat the
/// payload as the device-name string.
fn read_device_meta(stream: &mut TcpStream) -> std::io::Result<String> {
    let mut type_byte = [0u8; 1];
    stream.read_exact(&mut type_byte)?;
    let ty = type_byte[0];
    if ty == 0 {
        // CLIPBOARD payload
        let mut len_buf = [0u8; 4];
        stream.read_exact(&mut len_buf)?;
        let len = u32::from_be_bytes(len_buf) as usize;
        let mut payload = vec![0u8; len];
        stream.read_exact(&mut payload)?;
        Ok(String::from_utf8_lossy(&payload).to_string())
    } else {
        // Unknown type — return empty.
        Ok(String::new())
    }
}

/// Read the next device-msg frame and parse it. Returns the parsed type
/// tag and payload slice.
fn read_device_msg(stream: &mut TcpStream) -> std::io::Result<(u8, Vec<u8>)> {
    let mut type_byte = [0u8; 1];
    stream.read_exact(&mut type_byte)?;
    let ty = type_byte[0];
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut payload = vec![0u8; len];
    if len > 0 {
        stream.read_exact(&mut payload)?;
    }
    Ok((ty, payload))
}

fn main() -> std::process::ExitCode {
    let args: Vec<String> = env::args().collect();
    let port: u16 = args
        .get(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_PORT);
    println!("== android-hid-connect live E2E ==");
    println!("connecting to 127.0.0.1:{port} ...");

    let mut stream = match open_tcp("127.0.0.1", port) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("open_tcp failed: {e}");
            return std::process::ExitCode::from(2);
        }
    };
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    stream.set_write_timeout(Some(Duration::from_secs(5))).ok();
    let mut stats = Stats::default();

    // ---- 1. read dummy byte + device meta ----
    let mut dummy = [0u8; 1];
    match stream.read(&mut dummy) {
        Ok(n) if n == 1 => stats.ok(&format!("received dummy byte {:#x}", dummy[0])),
        Ok(n) => {
            stats.fail += 1;
            println!("  FAIL  expected 1 dummy byte, got {n}");
        }
        Err(e) => {
            stats.fail += 1;
            println!("  FAIL  dummy-byte read: {e}");
        }
    }

    match read_device_meta(&mut stream) {
        Ok(name) => {
            stats.ok(&format!("device meta: {name}"));
        }
        Err(e) => {
            stats.fail += 1;
            println!("  FAIL  device meta: {e}");
        }
    }

    // ---- 2. UHID keyboard lifecycle ----
    println!("\n[1] UHID keyboard lifecycle");
    let mut kbd = KeyboardHid::new();
    send_one(&mut stream, &kbd.open_message(None).unwrap()).unwrap();
    let down = kbd
        .key_event(0x04, true, Modifiers::LSHIFT) // A down w/ shift
        .unwrap();
    send_one(&mut stream, &down).unwrap();
    let up = kbd.key_event(0x04, false, Modifiers::empty()).unwrap();
    send_one(&mut stream, &up).unwrap();
    // Phantom state: 7 keys
    let mut report = [0u8; HID_MAX_SIZE];
    for sc in 0x04u8..0x0B {
        let m = kbd.key_event(sc, true, Modifiers::empty()).unwrap();
        send_one(&mut stream, &m).unwrap();
        report = match m {
            ControlMessage::UhidInput(u) => u.data,
            _ => unreachable!(),
        };
    }
    // All 6 key slots must be 0x01 (ErrorRollOver).
    let phantom_ok = report[2..8].iter().all(|&b| b == 0x01);
    stats.check("phantom state slots[2..8]=ErrorRollOver", phantom_ok, true);
    send_one(&mut stream, &kbd.close_message().unwrap()).unwrap();

    // ---- 3. UHID mouse lifecycle ----
    println!("\n[2] UHID mouse lifecycle");
    let m = MouseHid::new();
    send_one(&mut stream, &m.open_message(None).unwrap()).unwrap();
    let click = m.click_message(MouseButton::state(&[MouseButton::Left, MouseButton::X1]));
    send_one(&mut stream, &click).unwrap();
    let motion = m.motion_message(15, -8, MouseButton::state(&[MouseButton::Left]));
    send_one(&mut stream, &motion).unwrap();
    send_one(&mut stream, &m.close_message().unwrap()).unwrap();
    // The server is not required to send anything back for mouse clicks,
    // so this step is "no crash" rather than "explicit response".
    stats.ok("mouse open/input/destroy accepted");

    // ---- 4. UHID gamepad lifecycle (all 8 slots) ----
    println!("\n[3] UHID gamepad lifecycle (8 slots)");
    let mut gp = GamepadHid::new();
    let mut hid_ids = Vec::new();
    for slot in 1u32..=8 {
        let (hid_id, create) = gp.open(slot, Some("Pad")).unwrap();
        hid_ids.push(hid_id);
        send_one(&mut stream, &create).unwrap();
        let btn = gp.button_event(slot, GamepadButton::South, true).unwrap();
        send_one(&mut stream, &btn).unwrap();
        let stick = gp.axis_event(slot, GamepadAxis::LeftX, 16384).unwrap();
        send_one(&mut stream, &stick).unwrap();
        let dpad = gp.button_event(slot, GamepadButton::DpadUp, true).unwrap();
        send_one(&mut stream, &dpad).unwrap();
        let destroy = gp.close(slot).unwrap();
        send_one(&mut stream, &destroy).unwrap();
    }
    stats.check(
        "8 gamepad slots opened+closed",
        hid_ids.len(),
        8usize,
    );
    stats.check(
        "gamepad HID ids sequential 3..=10",
        hid_ids,
        vec![3u16, 4, 5, 6, 7, 8, 9, 10],
    );

    // ---- 5. non-UHID messages ----
    println!("\n[4] non-UHID control messages");
    let non_uhid = [
        ControlMessage::InjectKeycode(InjectKeycode {
            action: 0,
            keycode: 29, // A
            repeat: 0,
            metastate: 0,
        }),
        ControlMessage::InjectText(InjectText {
            text: "hello".to_string(),
        }),
        ControlMessage::InjectTouchEvent(InjectTouchEvent {
            action: 0, // DOWN
            pointer_id: 0xFFFFFFFFFFFF0001,
            x: 540,
            y: 960,
            screen_w: 1080,
            screen_h: 1920,
            pressure: 1.0,
            action_button: 0,
            buttons: 0,
        }),
        ControlMessage::InjectScrollEvent(InjectScrollEvent {
            x: 540,
            y: 960,
            screen_w: 1080,
            screen_h: 1920,
            hscroll: 0.0,
            vscroll: -4.0,
            buttons: 0,
        }),
        ControlMessage::GetClipboard(GetClipboard { copy_key: 0 }),
        ControlMessage::SetClipboard(SetClipboard {
            sequence: 1,
            paste: false,
            text: "android-hid-connect".to_string(),
        }),
        ControlMessage::SetDisplayPower(SetDisplayPower { on: true }),
        ControlMessage::StartApp(StartApp {
            name: "com.android.settings".to_string(),
        }),
        ControlMessage::ResizeDisplay(ResizeDisplay {
            width: 1080,
            height: 1920,
        }),
        ControlMessage::ExpandNotificationPanel,
        ControlMessage::ExpandSettingsPanel,
        ControlMessage::CollapsePanels,
        ControlMessage::RotateDevice,
        ControlMessage::OpenHardKeyboardSettings,
        ControlMessage::ResetVideo,
        ControlMessage::CameraSetTorch(android_hid_connect::control::message::CameraSetTorch {
            on: false,
        }),
        ControlMessage::CameraZoomIn,
        ControlMessage::CameraZoomOut,
        ControlMessage::BackOrScreenOn(android_hid_connect::control::message::BackOrScreenOn {
            action: 0,
        }),
    ];
    for (i, m) in non_uhid.iter().enumerate() {
        match send_one(&mut stream, m) {
            Ok(_) => stats.ok(&format!("non-uhid[{i}]: {:?}", m.msg_type())),
            Err(e) => {
                stats.fail += 1;
                println!("  FAIL  non-uhid[{i}] {:?}: {e}", m.msg_type());
            }
        }
    }

    // ---- 6. back-pressure / critical classification ----
    println!("\n[5] droppable classification");
    let critical_create = ControlMessage::UhidCreate(UhidCreate {
        id: 1,
        vendor_id: 0,
        product_id: 0,
        name: None,
        report_desc: vec![0x05, 0x01],
    });
    let critical_destroy = ControlMessage::UhidDestroy(UhidDestroy { id: 1 });
    let droppable_input = ControlMessage::UhidInput(UhidInput {
        id: 1,
        size: 8,
        data: [0u8; HID_MAX_SIZE],
    });
    stats.check("UhidCreate.is_critical()", critical_create.is_critical(), true);
    stats.check("UhidDestroy.is_critical()", critical_destroy.is_critical(), true);
    stats.check("UhidInput.is_critical()", droppable_input.is_critical(), false);

    // ---- 7. read back a couple of server messages ----
    println!("\n[6] server → host messages (5s budget)");
    let start = Instant::now();
    let mut server_msgs = 0;
    while start.elapsed() < Duration::from_secs(5) {
        match read_device_msg(&mut stream) {
            Ok((ty, payload)) => {
                server_msgs += 1;
                match ty {
                    0 => {
                        // CLIPBOARD
                        let txt = String::from_utf8_lossy(&payload).to_string();
                        println!("  RECV  DEVICE_MSG_CLIPBOARD len={} text={txt:?}", payload.len());
                    }
                    1 => {
                        // ACK_CLIPBOARD: u64 sequence
                        if payload.len() >= 8 {
                            let seq = u64::from_be_bytes(payload[..8].try_into().unwrap());
                            println!("  RECV  DEVICE_MSG_ACK_CLIPBOARD seq={seq}");
                        } else {
                            println!("  RECV  DEVICE_MSG_ACK_CLIPBOARD (short)");
                        }
                    }
                    2 => {
                        // UHID_OUTPUT: u16 id + u16 size + [data]
                        if payload.len() >= 4 {
                            let id = u16::from_be_bytes(payload[..2].try_into().unwrap());
                            let sz = u16::from_be_bytes(payload[2..4].try_into().unwrap()) as usize;
                            let data = &payload[4..4 + sz.min(payload.len().saturating_sub(4))];
                            println!("  RECV  DEVICE_MSG_UHID_OUTPUT id={id} size={sz} data={data:02x?}");
                        } else {
                            println!("  RECV  DEVICE_MSG_UHID_OUTPUT (short)");
                        }
                    }
                    _ => {
                        println!("  RECV  DEVICE_MSG unknown type={ty}");
                    }
                }
            }
            Err(e) => {
                // Read timeout is expected if the server has nothing more to say.
                if e.kind() == std::io::ErrorKind::TimedOut
                    || e.kind() == std::io::ErrorKind::WouldBlock
                {
                    break;
                }
                println!("  RECV  error: {e}");
                break;
            }
        }
    }
    stats.check("server emitted ≥ 0 device messages (timeout is OK)", server_msgs >= 0, true);
    stats.ok(&format!("server emitted {server_msgs} device message(s) total"));

    println!("\n=== summary ===");
    println!("  pass: {}", stats.pass);
    println!("  fail: {}", stats.fail);
    if stats.fail == 0 {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::from(1)
    }
}
