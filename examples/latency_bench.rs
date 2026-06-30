//! Real-device latency / throughput benchmark for android-hid-connect.
//!
//! Drives a running scrcpy-server (port 27183 by default — set up via
//! `adb push scrcpy-server /data/local/tmp/scrcpy-server` and `adb shell
//! CLASSPATH=/data/local/tmp/scrcpy-server app_process /
//! com.genymobile.scrcpy.Server 2.7 ...`) and reports per-message
//! write-side latency percentiles + throughput for:
//!
//!   - INJECT_TOUCH_EVENT (DOWN)        — single absolute touch frame
//!   - INJECT_TOUCH_EVENT (UP)          — release counterpart
//!   - UHID_INPUT keyboard (8 bytes)    — UHID keyboard edge report
//!   - UHID_INPUT gamepad (15 bytes)    — UHID gamepad edge report
//!
//! Each case reports min / p50 / p90 / p95 / p99 / mean / max in ms, plus
//! overall throughput in commands/sec and MB/sec. Output is JSON to
//! stdout so it is easy to diff against the same workload run against
//! `hs bench`.
//!
//! Usage:
//!   cargo run --release --example latency_bench -- [port] [iterations]

use std::env;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use android_hid_connect::control::message::{
    ControlMessage, InjectTouchEvent,
};
use android_hid_connect::hid::gamepad::GamepadHid;
use android_hid_connect::hid::keyboard::KeyboardHid;
use android_hid_connect::types::{GamepadAxis, Modifiers};
use android_hid_connect::HidDevice;

const DEFAULT_PORT: u16 = 27183;
const DEFAULT_ITERATIONS: u32 = 100;

#[derive(Default)]
struct Samples {
    durations: Vec<Duration>,
    bytes: usize,
}

impl Samples {
    fn push(&mut self, d: Duration, b: usize) {
        self.durations.push(d);
        self.bytes += b;
    }

    fn percentiles(&mut self) -> Report {
        self.durations.sort();
        let n = self.durations.len();
        let min = self.durations.first().copied().unwrap_or_default();
        let max = self.durations.last().copied().unwrap_or_default();
        let p50 = pct(&self.durations, 0.50);
        let p90 = pct(&self.durations, 0.90);
        let p95 = pct(&self.durations, 0.95);
        let p99 = pct(&self.durations, 0.99);
        let mean = if n > 0 {
            self.durations.iter().sum::<Duration>() / n as u32
        } else {
            Duration::ZERO
        };
        let total = self.durations.iter().sum::<Duration>();
        Report {
            n: n as u32,
            min_ms: ms(min),
            p50_ms: ms(p50),
            p90_ms: ms(p90),
            p95_ms: ms(p95),
            p99_ms: ms(p99),
            max_ms: ms(max),
            mean_ms: ms(mean),
            total_ms: ms(total),
            bytes: self.bytes,
        }
    }
}

#[derive(Default)]
struct Report {
    n: u32,
    min_ms: f64,
    p50_ms: f64,
    p90_ms: f64,
    p95_ms: f64,
    p99_ms: f64,
    max_ms: f64,
    mean_ms: f64,
    total_ms: f64,
    bytes: usize,
}

impl Report {
    fn to_json(&self, label: &str) -> String {
        let elapsed_s = self.total_ms / 1000.0;
        let throughput = if elapsed_s > 0.0 {
            self.n as f64 / elapsed_s
        } else {
            0.0
        };
        let mbps = if elapsed_s > 0.0 {
            (self.bytes as f64) / elapsed_s / 1_000_000.0
        } else {
            0.0
        };
        format!(
            r#"{{"label":"{label}","n":{n},"min_ms":{min:.3},"p50_ms":{p50:.3},"p90_ms":{p90:.3},"p95_ms":{p95:.3},"p99_ms":{p99:.3},"max_ms":{max:.3},"mean_ms":{mean:.3},"total_ms":{total:.3},"bytes":{bytes},"throughput_cmd_per_s":{tp:.1},"throughput_mb_per_s":{mb:.2}}}"#,
            label = label,
            n = self.n,
            min = self.min_ms,
            p50 = self.p50_ms,
            p90 = self.p90_ms,
            p95 = self.p95_ms,
            p99 = self.p99_ms,
            max = self.max_ms,
            mean = self.mean_ms,
            total = self.total_ms,
            bytes = self.bytes,
            tp = throughput,
            mb = mbps,
        )
    }
}

fn pct(samples: &[Duration], q: f64) -> Duration {
    let idx = ((samples.len() as f64 * q).ceil() as usize)
        .saturating_sub(1)
        .min(samples.len().saturating_sub(1));
    samples[idx]
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn read_control_prefix(stream: &mut TcpStream) -> std::io::Result<()> {
    // 1 dummy byte + 64-byte device name (raw, no length prefix).
    let mut buf = [0u8; 65];
    stream.read_exact(&mut buf)?;
    Ok(())
}

fn print_table_row(label: &str, r: &Report) {
    let elapsed_s = r.total_ms / 1000.0;
    let throughput = if elapsed_s > 0.0 {
        r.n as f64 / elapsed_s
    } else {
        0.0
    };
    eprintln!(
        "{:<28} {:>8} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9} {:>10}",
        "command", "n", "min ms", "p50 ms", "p95 ms", "p99 ms", "max ms", "mean ms", "cmd/s"
    );
    eprintln!(
        "{:<28} {:>8} {:>9.3} {:>9.3} {:>9.3} {:>9.3} {:>9.3} {:>9.3} {:>10.1}",
        label, r.n, r.min_ms, r.p50_ms, r.p95_ms, r.p99_ms, r.max_ms, r.mean_ms, throughput
    );
}

fn main() -> std::process::ExitCode {
    let args: Vec<String> = env::args().collect();
    let port: u16 = args
        .get(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_PORT);
    let iters: u32 = args
        .get(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_ITERATIONS);

    eprintln!("== android-hid-connect latency bench ==");
    eprintln!("connecting to 127.0.0.1:{port} ...");

    let mut stream = match TcpStream::connect(("127.0.0.1", port)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("connect failed: {e}");
            return std::process::ExitCode::from(2);
        }
    };
    stream.set_nodelay(true).ok();

    if let Err(e) = read_control_prefix(&mut stream) {
        eprintln!("read_control_prefix failed: {e}");
        return std::process::ExitCode::from(2);
    }

    // ---- Case A: INJECT_TOUCH_EVENT DOWN ----
    let mut samples_a = Samples::default();
    let down_msg = ControlMessage::InjectTouchEvent(InjectTouchEvent {
        action: 0, // DOWN
        pointer_id: 0xFFFFFFFFFFFF0001u64,
        x: 540,
        y: 1200,
        screen_w: 1080,
        screen_h: 2400,
        pressure: 1.0,
        action_button: 0,
        buttons: 0,
    });
    let mut scratch = Vec::with_capacity(64);
    for _ in 0..iters {
        let t0 = Instant::now();
        scratch.clear();
        down_msg.serialize_into(&mut scratch).unwrap();
        let n = scratch.len();
        stream.write_all(&scratch).unwrap();
        stream.flush().unwrap();
        samples_a.push(t0.elapsed(), n);
    }

    // ---- Case B: INJECT_TOUCH_EVENT UP ----
    let mut samples_b = Samples::default();
    let up_msg = ControlMessage::InjectTouchEvent(InjectTouchEvent {
        action: 1, // UP
        pointer_id: 0xFFFFFFFFFFFF0001u64,
        x: 540,
        y: 1200,
        screen_w: 1080,
        screen_h: 2400,
        pressure: 1.0,
        action_button: 0,
        buttons: 0,
    });
    for _ in 0..iters {
        let t0 = Instant::now();
        scratch.clear();
        up_msg.serialize_into(&mut scratch).unwrap();
        let n = scratch.len();
        stream.write_all(&scratch).unwrap();
        stream.flush().unwrap();
        samples_b.push(t0.elapsed(), n);
    }

    // ---- Case C: UHID keyboard edge report (8 bytes) ----
    let mut kbd = KeyboardHid::new();
    let key_msg = kbd.key_event(0x04, true, Modifiers::empty()).unwrap();
    let mut samples_c = Samples::default();
    for _ in 0..iters {
        let t0 = Instant::now();
        scratch.clear();
        key_msg.serialize_into(&mut scratch).unwrap();
        let n = scratch.len();
        stream.write_all(&scratch).unwrap();
        stream.flush().unwrap();
        samples_c.push(t0.elapsed(), n);
    }

    // ---- Case D: UHID gamepad edge report (15 bytes) ----
    // Open UHID gamepad first (so server accepts subsequent INPUT).
    let mut gp = GamepadHid::new();
    let (_hid_id, open_msg) = gp.open(1, Some("BenchPad")).unwrap();
    scratch.clear();
    open_msg.serialize_into(&mut scratch).unwrap();
    stream.write_all(&scratch).unwrap();
    stream.flush().unwrap();

    let gamepad_msg = gp
        .axis_event(1, GamepadAxis::LeftX, 16384)
        .unwrap();
    let mut samples_d = Samples::default();
    for _ in 0..iters {
        let t0 = Instant::now();
        scratch.clear();
        gamepad_msg.serialize_into(&mut scratch).unwrap();
        let n = scratch.len();
        stream.write_all(&scratch).unwrap();
        stream.flush().unwrap();
        samples_d.push(t0.elapsed(), n);
    }

    let report_a = samples_a.percentiles();
    let report_b = samples_b.percentiles();
    let report_c = samples_c.percentiles();
    let report_d = samples_d.percentiles();

    eprintln!();
    eprintln!(
        "{:<28} {:>8} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9} {:>10}",
        "command", "n", "min ms", "p50 ms", "p95 ms", "p99 ms", "max ms", "mean ms", "cmd/s"
    );
    print_table_row("inject_touch_event DOWN", &report_a);
    print_table_row("inject_touch_event UP", &report_b);
    print_table_row("uhid_input keyboard", &report_c);
    print_table_row("uhid_input gamepad", &report_d);

    println!(
        "[{}]",
        [
            report_a.to_json("inject_touch_event DOWN"),
            report_b.to_json("inject_touch_event UP"),
            report_c.to_json("uhid_input keyboard"),
            report_d.to_json("uhid_input gamepad"),
        ]
        .join(",")
    );
    std::process::ExitCode::SUCCESS
}