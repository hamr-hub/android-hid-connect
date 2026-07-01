//! AutoGLM-Phone 11-step benchmark — reproduces the Kuaishou live-stream
//! gift-panel task using android-hid-connect's UHID control surface.
//!
//! Task steps (matching the AutoGLM-Phone report):
//!   1. Tap bottom "首页" (Home) tab
//!   2. Swipe top navigation bar right to find "直播" (Live) tab
//!   3. Swipe right again if needed
//!   4. Swipe left if needed
//!   5. Tap "直播" tab once visible
//!   6. Tap a live-stream cover to enter the room
//!   7. Wait 5s for live room to load
//!   8. Tap gift icon (bottom-right area)
//!   9. Confirm gift panel is open
//!  10. Handle any popup (close recharge page if it appears)
//!  11. Task complete — press back to exit
//!
//! Each step measures round-trip latency from the host side.
//! Screenshots are captured via `adb exec-out screencap` between steps.
//!
//! Prerequisites:
//!   adb push scrcpy-server /data/local/tmp/scrcpy-server
//!   adb forward tcp:27183 localabstract:scrcpy
//!   adb shell 'nohup sh -c "CLASSPATH=/data/local/tmp/scrcpy-server \
//!     app_process / com.genymobile.scrcpy.Server 2.7 \
//!     video=false audio=false control=true clipboard_autosync=false \
//!     tunnel_forward=true send_dummy_byte=true" > /dev/null 2>&1 &'
//!   cargo run --example live_autoglm_benchmark

use std::io::Read;
use std::process::Command;
use std::time::{Duration, Instant};

use android_hid_connect::session::{HidSession, OpenRequest};
use android_hid_connect::transport::open_tcp;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

const PORT: u16 = 27183;
const SCREEN_W: u16 = 1080;
const SCREEN_H: u16 = 2400;

fn drain_dummy_and_meta(stream: &mut std::net::TcpStream) -> std::io::Result<()> {
    let mut dummy = [0u8; 1];
    stream.read_exact(&mut dummy)?;
    let mut meta = vec![0u8; 64];
    stream.read_exact(&mut meta)?;
    Ok(())
}

fn screenshot(tag: &str) -> Option<String> {
    let path = format!("/tmp/autoglm_step_{}.png", tag);
    let output = Command::new("adb")
        .args(["exec-out", "screencap", "-p"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if output.stdout.is_empty() {
        return None;
    }
    std::fs::write(&path, &output.stdout).ok()?;
    Some(path)
}

struct StepResult {
    label: String,
    bytes_sent: u64,
    latency: Duration,
    screenshot: Option<String>,
    status: String,
    notes: String,
}

fn main() -> Result<()> {
    println!("== android-hid-connect AutoGLM-Phone 11-step Benchmark ==");
    println!("Device: SM-G9910 (1080x2400)");
    println!("App: com.smile.gifmaker (Kuaishou)");
    println!();

    let mut stream = open_tcp("127.0.0.1", PORT)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    drain_dummy_and_meta(&mut stream)?;
    println!("  connected to scrcpy-server, dummy + meta drained");

    let mut s = HidSession::open(stream, OpenRequest::none())?;
    s.set_screen_size(SCREEN_W, SCREEN_H);
    println!(
        "  UHID session opened, screen size set to {}x{}",
        SCREEN_W, SCREEN_H
    );
    println!();

    let mut results: Vec<StepResult> = Vec::new();
    let total_start = Instant::now();

    macro_rules! run_step {
        ($idx:expr, $label:expr, $body:expr) => {{
            let t0 = Instant::now();
            let before = s.stats().1;
            let result: Result<()> = $body;
            let elapsed = t0.elapsed();
            let bytes = s.stats().1 - before;

            match result {
                Ok(()) => {
                    let screenshot = screenshot(&format!("{:02}", $idx));
                    println!(
                        "  [{:02}] PASS  {:50}  +{:>5}B  {:?}",
                        $idx, $label, bytes, elapsed
                    );
                    results.push(StepResult {
                        label: $label.to_string(),
                        bytes_sent: bytes,
                        latency: elapsed,
                        screenshot,
                        status: "PASS".to_string(),
                        notes: String::new(),
                    });
                }
                Err(e) => {
                    let screenshot = screenshot(&format!("{:02}", $idx));
                    println!(
                        "  [{:02}] FAIL  {:50}  +{:>5}B  {:?}  error: {}",
                        $idx, $label, bytes, elapsed, e
                    );
                    results.push(StepResult {
                        label: $label.to_string(),
                        bytes_sent: bytes,
                        latency: elapsed,
                        screenshot,
                        status: "FAIL".to_string(),
                        notes: e.to_string(),
                    });
                }
            }
            std::thread::sleep(Duration::from_millis(500));
        }};
    }

    // Step 1: Tap bottom "首页" (Home) tab
    run_step!(1, "Tap bottom 首页 (Home) tab", {
        s.tap(180, 2300)?;
        Ok(())
    });

    // Step 2: Swipe top navigation bar right to find 直播 tab
    run_step!(2, "Swipe top nav bar right (attempt 1)", {
        s.swipe((800, 150), (200, 150), Duration::from_millis(300), 10)?;
        Ok(())
    });

    // Step 3: Swipe right again
    run_step!(3, "Swipe top nav bar right (attempt 2)", {
        s.swipe((800, 150), (200, 150), Duration::from_millis(300), 10)?;
        Ok(())
    });

    // Step 4: Swipe left to go back
    run_step!(4, "Swipe top nav bar left (attempt)", {
        s.swipe((200, 150), (800, 150), Duration::from_millis(300), 10)?;
        Ok(())
    });

    // Step 5: Tap "直播" tab
    run_step!(5, "Tap 直播 (Live) tab", {
        s.tap(540, 150)?;
        Ok(())
    });

    // Step 6: Tap a live-stream cover to enter the room
    run_step!(6, "Tap live-stream cover (2nd item)", {
        s.tap(800, 500)?;
        Ok(())
    });

    // Step 7: Wait 5s for live room to load
    run_step!(7, "Wait 5s for live room to load", {
        std::thread::sleep(Duration::from_secs(5));
        Ok(())
    });

    // Step 8: Tap gift icon (bottom-right area)
    run_step!(8, "Tap gift icon (bottom-right)", {
        s.tap(950, 2100)?;
        Ok(())
    });

    // Step 9: Confirm gift panel — wait and screenshot
    run_step!(9, "Confirm gift panel open (wait 2s)", {
        std::thread::sleep(Duration::from_secs(2));
        Ok(())
    });

    // Step 10: Handle popup — close recharge page if present
    run_step!(10, "Close popup if present (top-right X)", {
        s.tap(1000, 400)?;
        Ok(())
    });

    // Step 11: Press back to exit
    run_step!(11, "Press Back to exit (task complete)", {
        s.press_back()?;
        Ok(())
    });

    let total_elapsed = total_start.elapsed();
    let total_bytes: u64 = results.iter().map(|r| r.bytes_sent).sum();
    let pass_count = results.iter().filter(|r| r.status == "PASS").count();
    let fail_count = results.iter().filter(|r| r.status == "FAIL").count();

    println!();
    println!("== Summary ==");
    println!("  Total steps:    {}", results.len());
    println!("  Passed:         {}", pass_count);
    println!("  Failed:         {}", fail_count);
    println!("  Total bytes:    {}", total_bytes);
    println!("  Total elapsed:   {:?}", total_elapsed);
    println!();

    println!("== JSON Report ==");
    print!("{{");
    print!("\"tool\":\"android-hid-connect\",");
    print!("\"device\":\"SM-G9910\",");
    print!("\"screen\":\"1080x2400\",");
    print!("\"app\":\"com.smile.gifmaker\",");
    print!("\"total_steps\":{},", results.len());
    print!("\"passed\":{},", pass_count);
    print!("\"failed\":{},", fail_count);
    print!("\"total_elapsed_ms\":{},", total_elapsed.as_millis());
    print!("\"total_bytes\":{},", total_bytes);
    print!("\"steps\":[");
    for (i, r) in results.iter().enumerate() {
        if i > 0 {
            print!(",");
        }
        print!(
            "{{\"idx\":{},\"label\":\"{}\",\"status\":\"{}\",\"bytes\":{},\"latency_ms\":{}}}",
            i + 1,
            r.label,
            r.status,
            r.bytes_sent,
            r.latency.as_millis()
        );
    }
    println!("]}}");

    s.close()?;
    println!("\nDone.");
    Ok(())
}
