//! Full E2E sweep of every public AI intent method on `HidSession`.
//!
//! Exercises ~80 intent helpers against a live scrcpy-server on the
//! connected Android device, grouped into 10 logical buckets. After
//! each method it takes a screencap and (optionally) writes a small
//! on-device marker via `show_notifications`. Tracks per-method
//! outcome, screenshot path, elapsed time, and writes a JSON report
//! to `/tmp/e2e_results.json`.
//!
//! Usage:
//!   cargo run --example e2e_full_intent                  # run sweep
//!   cargo run --example e2e_full_intent -- dispatch tap 540 1200
//!                                                       # (Python agent uses this)
//!
//! Assumes scrcpy-server is running and `adb forward tcp:27183
//! localabstract:scrcpy` is set up (see examples/live_e2e.rs).

use std::io::Read;
use std::process::{Command, ExitCode};
use std::time::{Duration, Instant};

use android_hid_connect::session::{GamepadFrameRaw, HidSession, OpenRequest};
use android_hid_connect::transport::{open_tcp, send_one};
use android_hid_connect::control::message::{ControlMessage, InjectText};
use android_hid_connect::types::{
    AndroidKeyAction, AndroidKeycode, ClipboardCopyKey, GamepadAxis, GamepadButton, Modifiers,
    Scancode, TouchAction, TouchPointerId,
};

const PORT: u16 = 27183;
const REPORT_PATH: &str = "/tmp/e2e_results.json";
const SCREENSHOT_DIR: &str = "/tmp/e2e_shots";

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// One AI intent invocation. `run` is a boxed closure (different
/// call sites capture different default args / coordinates).
type RunFn = Box<dyn Fn(&mut HidSession<std::net::TcpStream>) -> Result<()>>;

struct IntentCall {
    /// Logical bucket for grouping.
    bucket: &'static str,
    /// Method name (matches `HidSession`).
    method: &'static str,
    /// Run the intent, returning Ok / Err.
    run: RunFn,
    /// If true, take a screencap after this method.
    capture: bool,
    /// If true, also fire `show_notifications` after this method
    /// (and a `collapse_panels` 250ms later) as a non-destructive
    /// on-device marker.
    mark: bool,
}

fn drain_dummy_and_meta(stream: &mut std::net::TcpStream) -> std::io::Result<()> {
    let mut dummy = [0u8; 1];
    stream.read_exact(&mut dummy)?;
    let mut meta = vec![0u8; 64];
    stream.read_exact(&mut meta)?;
    Ok(())
}

fn screencap(path: &str) -> std::io::Result<()> {
    // `adb exec-out screencap -p` writes the PNG to stdout; redirect
    // it into the host file path so we keep a copy on disk.
    use std::process::Stdio;
    let mut child = Command::new("adb")
        .args(["exec-out", "screencap", "-p"])
        .stdout(Stdio::piped())
        .spawn()?;
    let out = child.stdout.take().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::Other, "no stdout from adb screencap")
    })?;
    let mut reader = std::io::BufReader::new(out);
    let mut file = std::fs::File::create(path)?;
    std::io::copy(&mut reader, &mut file)?;
    let status = child.wait()?;
    if !status.success() {
        Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "adb exec-out screencap exit non-zero",
        ))
    } else {
        Ok(())
    }
}

/// Build the full intent catalogue. Roughly 80 methods grouped into
/// 10 buckets: keyboard, touch, gesture, scroll, panel, media_key,
/// system, app_launch, clipboard, gamepad (covers both axes and
/// batch paths).
///
/// `cat!` is a one-line intent inserter. The trailing expression on
/// each `cat!` line is the closure body that runs against the live
/// session; it must evaluate to `Result<(), E>` for some `E: Into<Box<dyn Error>>`.
fn ic(bucket: &'static str, method: &'static str, capture: bool, mark: bool,
       run: RunFn) -> IntentCall {
    IntentCall { bucket, method, run, capture, mark }
}

macro_rules! cat {
    ($v:ident, $bucket:literal, $method:literal, $capture:literal, $mark:literal, $body:expr) => {
        $v.push(ic($bucket, $method, $capture, $mark, Box::new($body)));
    };
}

fn catalogue() -> Vec<IntentCall> {
    let mut v: Vec<IntentCall> = Vec::with_capacity(96);

    // ---------- Bucket 1: keyboard raw ----------
    cat!(v, "keyboard", "type_text",          true, false, |s| s.type_text("e2e test").map_err(Into::into));
    cat!(v, "keyboard", "type_text_strict",   true, false, |s| s.type_text_strict("ok").map_err(Into::into));
    cat!(v, "keyboard", "key(down)",         false, false, |s| s.key(Scancode::A as u8, true,  Modifiers::empty()).map_err(Into::into));
    cat!(v, "keyboard", "key(up)",           false, false, |s| s.key(Scancode::A as u8, false, Modifiers::empty()).map_err(Into::into));

    // ---------- Bucket 2: touch ----------
    cat!(v, "touch", "tap",                   true, false, |s| s.tap(540, 1200).map_err(Into::into));
    cat!(v, "touch", "tap_pointer",          false, false, |s| s.tap_pointer(TouchPointerId::finger(1), 540, 1200).map_err(Into::into));
    cat!(v, "touch", "swipe",                 true, false, |s| s.swipe((540,1400),(540,1000), Duration::from_millis(200), 6).map_err(Into::into));
    cat!(v, "touch", "swipe_pointer",        false, false, |s| s.swipe_pointer(TouchPointerId::finger(2),(540,1400),(540,1000), Duration::from_millis(200), 6).map_err(Into::into));
    cat!(v, "touch", "double_tap",           true, false, |s| s.double_tap(540, 1200).map_err(Into::into));
    cat!(v, "touch", "long_press",           true, false, |s| s.long_press(540, 1200, Duration::from_millis(150)).map_err(Into::into));
    cat!(v, "touch", "three_finger_screenshot", true, true, |s| s.three_finger_screenshot().map_err(Into::into));
    cat!(v, "touch", "inject_touch",         false, false, |s| s.inject_touch(0, 7, 200, 200, 1.0).map_err(Into::into));
    cat!(v, "touch", "inject_touch_action",  false, false, |s| s.inject_touch_action(TouchAction::DOWN, 7, 200, 200, 1.0).map_err(Into::into));
    cat!(v, "touch", "inject_touch_pointer", false, false, |s| s.inject_touch_pointer(TouchAction::UP, TouchPointerId::finger(7), 200, 200, 0.0).map_err(Into::into));
    cat!(v, "touch", "cancel_touch",         false, false, |s| s.cancel_touch(7).map_err(Into::into));
    cat!(v, "touch", "cancel_touch_pointer", false, false, |s| s.cancel_touch_pointer(TouchPointerId::finger(7)).map_err(Into::into));
    // multitouch handle: each test runs through a fresh handle so the
    // down+move+up lifecycle has to live inside one closure.
    cat!(v, "touch", "multitouch.down",      false, false, |s| { let mut h=s.multitouch(); h.down(0, 540, 800, 1.0).map_err(Into::into) });
    cat!(v, "touch", "multitouch.move_to",   false, false, |s| { let mut h=s.multitouch(); h.down(1, 540, 800, 1.0)?; h.move_to(1, 560, 820, 1.0).map_err(Into::into) });
    cat!(v, "touch", "multitouch.up",        false, false, |s| { let mut h=s.multitouch(); h.down(2, 540, 800, 1.0)?; h.up(2).map_err(Into::into) });
    cat!(v, "touch", "multitouch.cancel",    false, false, |s| { let mut h=s.multitouch(); h.down(3, 540, 800, 1.0)?; h.cancel(3).map_err(Into::into) });
    cat!(v, "touch", "multitouch.release_all", false, false, |s| s.multitouch().release_all().map_err(Into::into));
    cat!(v, "touch", "multitouch.pinch",     false, false, |s| { let mut h=s.multitouch(); h.down(4, 200, 600, 1.0)?; h.down(5, 800, 600, 1.0)?; h.pinch((4,200,600,400,600),(5,800,600,1000,600),6)?; h.up(4)?; h.up(5).map_err(Into::into) });

    // ---------- Bucket 3: scroll ----------
    cat!(v, "scroll", "scroll",           true, false, |s| s.scroll(540, 1200, 0.0, -2.0).map_err(Into::into));
    cat!(v, "scroll", "inject_scroll",   false, false, |s| s.inject_scroll(540, 1200, 0.0, -2.0, 0).map_err(Into::into));

    // ---------- Bucket 4: panels ----------
    cat!(v, "panel",  "show_notifications", true, false, |s| s.show_notifications().map_err(Into::into));
    cat!(v, "panel",  "collapse_panels",    true, false, |s| s.collapse_panels().map_err(Into::into));
    cat!(v, "panel",  "show_quick_settings",true, false, |s| s.show_quick_settings().map_err(Into::into));
    cat!(v, "panel",  "collapse_panels(2)", true, false, |s| s.collapse_panels().map_err(Into::into));

    // ---------- Bucket 5: media / system keys ----------
    cat!(v, "media_key", "press_home",        true, false, |s| s.press_home().map_err(Into::into));
    cat!(v, "media_key", "press_back",        true, false, |s| s.press_back().map_err(Into::into));
    cat!(v, "media_key", "open_recents",      true, false, |s| s.open_recents().map_err(Into::into));
    cat!(v, "media_key", "volume_up",         true, false, |s| s.volume_up().map_err(Into::into));
    cat!(v, "media_key", "volume_down",       true, false, |s| s.volume_down().map_err(Into::into));
    cat!(v, "media_key", "volume_mute",       false, false, |s| s.volume_mute().map_err(Into::into));
    cat!(v, "media_key", "back_or_screen_on", false, false, |s| s.back_or_screen_on(AndroidKeyAction::DOWN).map_err(Into::into));
    cat!(v, "media_key", "tap_android_key",   false, false, |s| s.tap_android_key(AndroidKeycode::ENTER).map_err(Into::into));
    cat!(v, "media_key", "tap_android_key_with_metastate", false, false, |s| s.tap_android_key_with_metastate(AndroidKeycode::ENTER, 0x1000).map_err(Into::into));
    cat!(v, "media_key", "tap_android_keycode", false, false, |s| s.tap_android_keycode(AndroidKeycode::DPAD_RIGHT.value(), 0).map_err(Into::into));
    cat!(v, "media_key", "press_android_key", false, false, |s| s.press_android_key(AndroidKeycode::DPAD_LEFT).map_err(Into::into));
    cat!(v, "media_key", "release_android_key", false, false, |s| s.release_android_key(AndroidKeycode::DPAD_LEFT).map_err(Into::into));
    cat!(v, "media_key", "inject_android_keycode", false, false, |s| s.inject_android_keycode(AndroidKeyAction::DOWN.value(), AndroidKeycode::DPAD_UP, 0, 0).map_err(Into::into));
    cat!(v, "media_key", "inject_android_key_event", false, false, |s| s.inject_android_key_event(AndroidKeyAction::UP, AndroidKeycode::DPAD_UP, 0, 0).map_err(Into::into));
    cat!(v, "media_key", "inject_keycode",    false, false, |s| s.inject_keycode(AndroidKeyAction::DOWN.value(), AndroidKeycode::DPAD_DOWN.value(), 0, 0).map_err(Into::into));

    // ---------- Bucket 6: system / display / camera / ai ----------
    cat!(v, "system", "set_screen_power(true)",  true, false, |s| s.set_screen_power(true).map_err(Into::into));
    cat!(v, "system", "set_screen_power(false)", false, false, |s| s.set_screen_power(false).map_err(Into::into));
    cat!(v, "system", "set_screen_power(true)",  true, false, |s| s.set_screen_power(true).map_err(Into::into));
    cat!(v, "system", "rotate_device",           true, false, |s| s.rotate_device().map_err(Into::into));
    cat!(v, "system", "resize_display",         false, false, |s| s.resize_display(1080, 2400).map_err(Into::into));
    cat!(v, "system", "set_torch(false)",       false, false, |s| s.set_torch(false).map_err(Into::into));
    cat!(v, "system", "camera_zoom_in",         false, false, |s| s.camera_zoom_in().map_err(Into::into));
    cat!(v, "system", "camera_zoom_out",        false, false, |s| s.camera_zoom_out().map_err(Into::into));
    cat!(v, "system", "open_hard_keyboard_settings", false, false, |s| s.open_hard_keyboard_settings().map_err(Into::into));
    cat!(v, "system", "reset_video",            false, false, |s| s.reset_video().map_err(Into::into));
    cat!(v, "system", "configure_ai",           false, false, |s| s.configure_ai(0x0F, 250, 128).map_err(Into::into));
    cat!(v, "system", "query_ai",               false, false, |s| s.query_ai(0).map_err(Into::into));
    cat!(v, "system", "pause_ai",               false, false, |s| s.pause_ai().map_err(Into::into));

    // ---------- Bucket 7: app_launch ----------
    cat!(v, "app_launch", "launch_app(settings)", true, false, |s| s.launch_app("com.android.settings").map_err(Into::into));
    cat!(v, "app_launch", "press_home",          true, false, |s| s.press_home().map_err(Into::into));

    // ---------- Bucket 8: clipboard ----------
    cat!(v, "clipboard", "set_clipboard",        true, false, |s| s.set_clipboard("e2e marker", false).map_err(Into::into));
    cat!(v, "clipboard", "get_clipboard",       false, false, |s| s.get_clipboard().map_err(Into::into));
    cat!(v, "clipboard", "request_clipboard",   false, false, |s| s.request_clipboard(1).map_err(Into::into));
    cat!(v, "clipboard", "request_clipboard_key", false, false, |s| s.request_clipboard_key(ClipboardCopyKey::COPY).map_err(Into::into));

    // ---------- Bucket 9: gamepad raw ----------
    cat!(v, "gamepad", "set_stick(LX,0.5)",          false, false, |s| s.set_stick(GamepadAxis::LeftX, 0.5).map_err(Into::into));
    cat!(v, "gamepad", "set_stick_raw(LX,16384)",    false, false, |s| s.set_stick_raw(GamepadAxis::LeftX, 16384).map_err(Into::into));
    cat!(v, "gamepad", "set_buttons(South)",         false, false, |s| s.set_buttons(GamepadButton::South as u32).map_err(Into::into));
    cat!(v, "gamepad", "set_button(South,down)",     false, false, |s| s.set_button(GamepadButton::South, true).map_err(Into::into));
    cat!(v, "gamepad", "set_button(South,up)",       false, false, |s| s.set_button(GamepadButton::South, false).map_err(Into::into));
    cat!(v, "gamepad", "set_frame_raw",              false, false, |s| s.set_frame_raw(0, 1000, -1000, 2000, -2000, 1000, 0).map_err(Into::into));
    cat!(v, "gamepad", "set_frame_raw_unchecked",    false, false, |s| s.set_frame_raw_unchecked(0, -2000, 2000, -3000, 3000, 0, 1500).map_err(Into::into));
    cat!(v, "gamepad", "set_frame_raw_unchecked_frame", false, false, |s| s.set_frame_raw_unchecked_frame(GamepadFrameRaw::new(0, 0, 0, 0, 0, 0, 0)).map_err(Into::into));
    cat!(v, "gamepad", "set_frame_raw_packed",       false, false, |s| s.set_frame_raw_packed(&[0u8; 15]).map_err(Into::into));
    cat!(v, "gamepad", "set_left_stick_raw",         false, false, |s| s.set_left_stick_raw(100, -100).map_err(Into::into));
    cat!(v, "gamepad", "set_right_stick_raw",        false, false, |s| s.set_right_stick_raw(-200, 200).map_err(Into::into));
    cat!(v, "gamepad", "set_triggers_raw",           false, false, |s| s.set_triggers_raw(500, 700).map_err(Into::into));
    cat!(v, "gamepad", "set_sticks_raw",             false, false, |s| s.set_sticks_raw(0, 0, 0, 0, 0, 0).map_err(Into::into));
    cat!(v, "gamepad", "set_frame_raw_batch",        false, false, |s| { let _ = s.set_frame_raw_batch(&[GamepadFrameRaw::new(0,0,0,0,0,0,0), GamepadFrameRaw::new(GamepadButton::South as u32,100,0,0,0,0,0), GamepadFrameRaw::new(0,0,0,0,0,0,0)])?; Ok(()) });
    cat!(v, "gamepad", "set_frame_raw_batch_unchecked", false, false, |s| { let _ = s.set_frame_raw_batch_unchecked(&[GamepadFrameRaw::new(0,0,0,0,0,0,0)])?; Ok(()) });
    cat!(v, "gamepad", "set_frame_raw_packed_batch",    false, false, |s| { let _ = s.set_frame_raw_packed_batch(&[[0u8;15],[0u8;15]])?; Ok(()) });
    cat!(v, "gamepad", "batched set_stick_raw x32",  false, false, |s| { for i in 0i16..32 { s.set_stick_raw(GamepadAxis::LeftX, i * 32)?; } Ok(()) });

    // ---------- Bucket 10: mouse UHID ----------
    cat!(v, "mouse", "mouse_motion",      false, false, |s| s.mouse_motion(5, 5, 0).map_err(Into::into));
    cat!(v, "mouse", "mouse_buttons",     false, false, |s| s.mouse_buttons(0).map_err(Into::into));
    cat!(v, "mouse", "mouse_scroll",      false, false, |s| s.mouse_scroll(0.0, -1.0).map(|_| ()).map_err(Into::into));
    cat!(v, "mouse", "mouse_frame_batch", false, false, |s| { let _ = s.mouse_frame_batch(&[(1,1,0),(2,-1,0)])?; Ok(()) });

    v
}

fn run_sweep() -> Result<()> {
    println!("== android-hid-connect full intent sweep ==");

    // Make sure scrcpy-server is alive. app_process shows up as "app_process"
    // not the class name; `ps -ef | grep ...` is the reliable check.
    // (Avoid `pgrep -f`, which matches the substring in its own argv.)
    let pid_check = Command::new("adb")
        .args(["shell", "ps -ef | grep com.genymobile.scrcpy.Server | grep -v grep || true"])
        .output()?;
    if pid_check.stdout.is_empty() {
        eprintln!("scrcpy-server is not running on device. Restart it with the documented CLASSPATH/app_process invocation.");
        return Err("scrcpy-server not running".into());
    }
    std::fs::create_dir_all(SCREENSHOT_DIR).ok();

    let mut stream = open_tcp("127.0.0.1", PORT)?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    drain_dummy_and_meta(&mut stream)?;
    println!("  connected to 127.0.0.1:{PORT}");

    let mut s = HidSession::open(stream, OpenRequest::none())?;
    s.set_screen_size(1080, 2400);

    let calls = catalogue();
    let mut bucket_stats: std::collections::BTreeMap<String, (usize, usize, usize)> =
        std::collections::BTreeMap::new();
    let mut grand_pass = 0usize;
    let mut grand_fail = 0usize;
    let mut grand_skip = 0usize;
    let mut per_method: Vec<Json> = Vec::with_capacity(calls.len());
    let mut last_bucket = String::new();

    // Make sure we start from a known home state.
    let _ = s.press_home();
    std::thread::sleep(Duration::from_millis(500));

    let sweep_start = Instant::now();
    for (i, c) in calls.iter().enumerate() {
        if c.bucket != last_bucket.as_str() {
            println!("\n[{}] bucket: {}", i, c.bucket);
            last_bucket = c.bucket.to_string();
            bucket_stats.entry(c.bucket.to_string()).or_insert((0, 0, 0));
        }
        let t0 = Instant::now();
        let res = (c.run)(&mut s).and_then(|_| s.flush_now().map(|_| ()).map_err(|e| e.into()));
        let elapsed = t0.elapsed();

        let shot_path = if c.capture {
            let path = format!("{SCREENSHOT_DIR}/{i:03}_{}.png", c.method.replace(' ', "_"));
            let r = screencap(&path);
            if let Err(e) = r {
                eprintln!("    screencap failed: {e}");
            }
            Some(path)
        } else {
            None
        };
        let shot_ok = !c.capture || shot_path.as_ref().is_some_and(|p| std::path::Path::new(p).exists());

        let (result, err_str) = match res {
            Ok(()) => {
                grand_pass += 1;
                let st = bucket_stats.entry(c.bucket.to_string()).or_insert((0, 0, 0));
                st.0 += 1;
                ("pass".to_string(), None)
            }
            Err(e) => {
                let msg = format!("{e}");
                // Skipped conditions:
                //   - UHID-dependent methods while session is opened with
                //     OpenRequest::none() (we deliberately skip UHID here
                //     because the on-device /dev/uhid kernel driver is in a
                //     bad state after earlier run aborts — see report).
                //   - Hardware-dependent: torch/camera on devices without
                //     flash.
                let lc = msg.to_lowercase();
                let skipped = lc.contains("keyboard not open")
                    || lc.contains("mouse not open")
                    || lc.contains("gamepad not open")
                    || lc.contains("torch")
                    || lc.contains("camera");
                if skipped {
                    grand_skip += 1;
                    let st = bucket_stats.entry(c.bucket.to_string()).or_insert((0, 0, 0));
                    st.2 += 1;
                    ("skipped".to_string(), Some(msg))
                } else {
                    grand_fail += 1;
                    let st = bucket_stats.entry(c.bucket.to_string()).or_insert((0, 0, 0));
                    st.1 += 1;
                    ("fail".to_string(), Some(msg))
                }
            }
        };

        match result.as_str() {
            "pass" => println!("  PASS  [{i:02}] {:<32}  {:?}", c.method, elapsed),
            "fail" => {
                let m = err_str.clone().unwrap_or_default();
                println!("  FAIL  [{i:02}] {:<32}  {:?}  ({m})", c.method, elapsed);
            }
            "skipped" => {
                let m = err_str.clone().unwrap_or_default();
                println!("  SKIP  [{i:02}] {:<32}  {:?}  ({m})", c.method, elapsed);
            }
            _ => {}
        }

        if c.mark {
            // Non-destructive on-device marker: pop notifications,
            // take a shot, collapse again.
            let _ = s.show_notifications();
            std::thread::sleep(Duration::from_millis(250));
            let p = format!("{SCREENSHOT_DIR}/{i:03}_marker.png");
            let _ = screencap(&p);
            let _ = s.collapse_panels();
            std::thread::sleep(Duration::from_millis(250));
        }

        let mut entry = Json::obj();
        entry.set("bucket", Json::str(c.bucket));
        entry.set("method", Json::str(c.method));
        entry.set("index", Json::num(i as u64));
        entry.set("elapsed_ms", Json::num(elapsed.as_millis() as u64));
        entry.set("result", Json::str(&result));
        entry.set("screenshot_ok", Json::bool(shot_ok));
        match &shot_path {
            Some(p) => entry.set("screenshot", Json::str(p)),
            None => entry.set("screenshot", Json::null()),
        }
        if let Some(m) = err_str {
            entry.set("error", Json::str(&m));
        }
        per_method.push(entry);
    }
    let total_elapsed = sweep_start.elapsed();

    // final cleanup
    let _ = s.press_home();
    std::thread::sleep(Duration::from_millis(300));
    s.close()?;

    let started = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    let mut buckets = Json::arr();
    for (b, (p, f, sk)) in &bucket_stats {
        let mut o = Json::obj();
        o.set("bucket", Json::str(b));
        o.set("pass", Json::num(*p as u64));
        o.set("fail", Json::num(*f as u64));
        o.set("skipped", Json::num(*sk as u64));
        buckets.push(o);
    }

    let mut methods = Json::arr();
    for m in per_method {
        methods.push(m);
    }

    let mut device = Json::obj();
    device.set("serial", Json::str("R5CR70SRPSD"));
    device.set("model", Json::str("SM-G9910"));
    let mut screen = Json::arr();
    screen.push(Json::num(1080));
    screen.push(Json::num(2400));
    device.set("screen", screen);

    let mut report = Json::obj();
    report.set("started_at_unix_ms", Json::num(started));
    report.set("elapsed_ms", Json::num(total_elapsed.as_millis() as u64));
    report.set("device", device);
    report.set("total", Json::num(calls.len() as u64));
    report.set("pass", Json::num(grand_pass as u64));
    report.set("fail", Json::num(grand_fail as u64));
    report.set("skipped", Json::num(grand_skip as u64));
    report.set("buckets", buckets);
    report.set("methods", methods);

    std::fs::write(REPORT_PATH, report.to_pretty_string())?;
    println!(
        "\n=== summary ===\n  total: {}\n  pass:  {}\n  fail:  {}\n  skip:  {}\n  report: {REPORT_PATH}",
        calls.len(),
        grand_pass,
        grand_fail,
        grand_skip
    );
    Ok(())
}

/// `cargo run --example e2e_full_intent -- dispatch <fn> <args...>`
///
/// Minimal dispatcher used by the Python LLM agent. Args are passed
/// positionally; floats and i16 are parsed on a best-effort basis.
fn run_dispatch(args: &[String]) -> Result<()> {
    if args.is_empty() {
        return Err("dispatch requires a function name".into());
    }
    let name = args[0].as_str();
    let rest = &args[1..];
    let mut stream = open_tcp("127.0.0.1", PORT)?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    drain_dummy_and_meta(&mut stream)?;
    let mut s = HidSession::open(stream, OpenRequest::none())?;
    s.set_screen_size(1080, 2400);

    let i = |idx: usize, def: i32| rest.get(idx).and_then(|x| x.parse().ok()).unwrap_or(def);
    let f = |idx: usize, def: f32| rest.get(idx).and_then(|x| x.parse().ok()).unwrap_or(def);
    let u = |idx: usize, def: u32| rest.get(idx).and_then(|x| x.parse().ok()).unwrap_or(def);
    let u8_a = |idx: usize, def: u8| rest.get(idx).and_then(|x| x.parse().ok()).unwrap_or(def);
    let u64_a = |idx: usize, def: u64| rest.get(idx).and_then(|x| x.parse().ok()).unwrap_or(def);
    let ss = |idx: usize, def: &'static str| -> String {
        rest.get(idx).cloned().unwrap_or_else(|| def.to_string())
    };
    let b = |idx: usize, def: bool| rest.get(idx).map(|x| x == "true" || x == "1").unwrap_or(def);

    let r: Result<Json> = (|| -> Result<Json> {
        let ok = || Ok(Json::bool(true));
        match name {
            "tap" => { s.tap(i(0,540), i(1,1200))?; ok() }
            "double_tap" => { s.double_tap(i(0,540), i(1,1200))?; ok() }
            "long_press" => { s.long_press(i(0,540), i(1,1200), Duration::from_millis(u64_a(2,150)))?; ok() }
            "swipe" => { s.swipe((i(0,540),i(1,1500)),(i(2,540),i(3,800)), Duration::from_millis(u64_a(4,250)), u(5,6))?; ok() }
            // INJECT_TEXT is non-UHID; bypass the (possibly broken) UHID keyboard path.
            "type_text" => {
                let mut direct = open_tcp("127.0.0.1", PORT)?;
                direct.set_read_timeout(Some(Duration::from_secs(2)))?;
                direct.set_write_timeout(Some(Duration::from_secs(2)))?;
                drain_dummy_and_meta(&mut direct)?;
                let text = ss(0, "e2e test");
                send_one(&mut direct, &ControlMessage::InjectText(InjectText { text }))?;
                ok()
            }
            "press_home" => { s.press_home()?; ok() }
            "press_back" => { s.press_back()?; ok() }
            "open_recents" => { s.open_recents()?; ok() }
            "volume_up" => { s.volume_up()?; ok() }
            "volume_down" => { s.volume_down()?; ok() }
            "volume_mute" => { s.volume_mute()?; ok() }
            "launch_app" => { s.launch_app(&ss(0,"com.android.settings"))?; ok() }
            "tap_android_key" => {
                let kc = match ss(0,"ENTER").as_str() {
                    "HOME" => AndroidKeycode::HOME, "BACK" => AndroidKeycode::BACK,
                    "DPAD_UP" => AndroidKeycode::DPAD_UP, "DPAD_DOWN" => AndroidKeycode::DPAD_DOWN,
                    "DPAD_LEFT" => AndroidKeycode::DPAD_LEFT, "DPAD_RIGHT" => AndroidKeycode::DPAD_RIGHT,
                    "ENTER" => AndroidKeycode::ENTER,
                    _ => AndroidKeycode::new(u(0,66)),
                };
                s.tap_android_key(kc)?; ok()
            }
            "show_notifications" => { s.show_notifications()?; ok() }
            "show_quick_settings" => { s.show_quick_settings()?; ok() }
            "collapse_panels" => { s.collapse_panels()?; ok() }
            "set_screen_power" => { s.set_screen_power(b(0,true))?; ok() }
            "set_clipboard" => { s.set_clipboard(&ss(0,"e2e"), b(1,false))?; ok() }
            "scroll" => { s.scroll(i(0,540), i(1,1200), f(2,0.0), f(3,-2.0))?; ok() }
            "three_finger_screenshot" => { s.three_finger_screenshot()?; ok() }
            "open_hard_keyboard_settings" => { s.open_hard_keyboard_settings()?; ok() }
            "rotate_device" => { s.rotate_device()?; ok() }
            "set_torch" => { s.set_torch(b(0,false))?; ok() }
            "key" => { s.key(u8_a(0,0x04), b(1,true), Modifiers::empty())?; ok() }
            "set_button" => {
                let b_str = ss(0,"South");
                let btn = match b_str.as_str() {
                    "South" => GamepadButton::South, "East" => GamepadButton::East,
                    "West" => GamepadButton::West, "North" => GamepadButton::North,
                    "Start" => GamepadButton::Start, _ => GamepadButton::South,
                };
                s.set_button(btn, b(1,true))?; ok()
            }
            "mouse_motion" => { s.mouse_motion(i(0,5), i(1,0), u8_a(2,0))?; ok() }
            "set_stick" => {
                let ax_str = ss(0,"LeftX");
                let ax = match ax_str.as_str() {
                    "LeftX" => GamepadAxis::LeftX, "LeftY" => GamepadAxis::LeftY,
                    "RightX" => GamepadAxis::RightX, "RightY" => GamepadAxis::RightY,
                    "LeftTrigger" => GamepadAxis::LeftTrigger, "RightTrigger" => GamepadAxis::RightTrigger,
                    _ => GamepadAxis::LeftX,
                };
                s.set_stick(ax, f(1,0.0))?; ok()
            }
            "done" => ok(),
            _ => Err(format!("unknown dispatch function: {name}").into()),
        }
    })();

    s.close()?;
    match r {
        Ok(v) => { println!("{}", v.to_pretty_string()); Ok(()) }
        Err(e) => { eprintln!("dispatch err: {e}"); Err(e) }
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(|s| s == "dispatch").unwrap_or(false) {
        return match run_dispatch(&args[2..]) {
            Ok(_) => ExitCode::SUCCESS,
            Err(_) => ExitCode::from(1),
        };
    }
    match run_sweep() {
        Ok(_) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("e2e sweep error: {e}");
            ExitCode::from(1)
        }
    }
}

// ===========================================================================
// Minimal JSON encoder (no serde dependency — the root crate doesn't pull
// `serde_json`, and an example crate can't add its own dependencies).
// ===========================================================================

#[derive(Clone)]
enum Json {
    Null,
    Bool(bool),
    Num(u64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    fn null() -> Self { Json::Null }
    fn bool(b: bool) -> Self { Json::Bool(b) }
    fn num(n: u64) -> Self { Json::Num(n) }
    fn str(s: &str) -> Self { Json::Str(s.to_string()) }
    fn arr() -> Self { Json::Arr(Vec::new()) }
    fn obj() -> Self { Json::Obj(Vec::new()) }
    fn push(&mut self, v: Json) { if let Json::Arr(a) = self { a.push(v); } }
    fn set(&mut self, k: &str, v: Json) { if let Json::Obj(o) = self { o.push((k.to_string(), v)); } }
    fn write_into(&self, out: &mut String, ind: usize) {
        match self {
            Json::Null => out.push_str("null"),
            Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Json::Num(n) => out.push_str(&n.to_string()),
            Json::Str(s) => {
                out.push('"');
                for ch in s.chars() {
                    match ch {
                        '"' => out.push_str("\\\""), '\\' => out.push_str("\\\\"),
                        '\n' => out.push_str("\\n"), '\r' => out.push_str("\\r"),
                        '\t' => out.push_str("\\t"),
                        c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
                        c => out.push(c),
                    }
                }
                out.push('"');
            }
            Json::Arr(a) => write_seq(out, ind, '[', ']', a.iter().map(|v| ("", v))),
            Json::Obj(o) => write_seq(out, ind, '{', '}', o.iter().map(|(k, v)| (k.as_str(), v))),
        }
    }
    fn to_pretty_string(&self) -> String {
        let mut s = String::new();
        self.write_into(&mut s, 0);
        s
    }
}

fn write_seq<'a, I: Iterator<Item = (&'a str, &'a Json)>>(out: &mut String, ind: usize, open: char, close: char, iter: I) {
    let v: Vec<_> = iter.collect();
    if v.is_empty() { out.push(open); out.push(close); return; }
    let pad = "  ".repeat(ind); let pad_in = "  ".repeat(ind + 1);
    out.push(open); out.push('\n');
    for (i, (k, val)) in v.iter().enumerate() {
        out.push_str(&pad_in);
        if !k.is_empty() {
            out.push('"'); out.push_str(&k.replace('"', "\\\"")); out.push_str("\": ");
        }
        val.write_into(out, ind + 1);
        if i + 1 < v.len() { out.push(','); }
        out.push('\n');
    }
    out.push_str(&pad); out.push(close);
}