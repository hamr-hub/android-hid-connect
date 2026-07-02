//! `adk-bench` — in-process micro-benchmark for the AI middleware
//! intent→UHID hot path.
//!
//! Mirrors the `examples/live_autoglm_benchmark.rs` flow but runs
//! entirely in-process over `android_hid_connect::MockTransport`:
//! no real device needed, suitable for CI.
//!
//! ## What it measures
//!
//! Per-action wall-clock latency from `ActionInjector::apply` start
//! to the bytes being collected in the mock transport. Outputs
//! p50 / p95 / p99 / max in microseconds for the canonical 5-step
//! plan:
//!
//! 1. Tap (DOWN+UP)
//! 2. Swipe (60 Hz sample, ~320 ms)
//! 3. TypeText (10 chars)
//! 4. Key press
//! 5. GamepadFrame (raw 15-byte HID report)
//!
//! ## Usage
//!
//! ```text
//! cargo run --release -p ai-device-kernel --bin adk-bench -- --iterations 1000
//! ```
//!
//! ## SLO contract
//!
//! - p99 < 5 ms (target) on the in-process path. The kernel-internal
//!   intent→UHID hot path is < 1 ms per Phase 6.5 baseline; the
//!   overhead here is the mock-transport serialization cost.
//! - Zero allocations in the hot path (`Action::Tap` / `Swipe` /
//!   `Key` / `GamepadFrame`).

use std::time::{Duration, Instant};

use ai_device_kernel::action::Action;
use ai_device_kernel::bridge::ActionInjector;
use android_hid_connect::session::OpenRequest;
use android_hid_connect::transport::MockTransport;

const ITERATIONS_DEFAULT: u64 = 1000;

/// 5-step canonical plan. Each step mirrors a step in the
/// `live_autoglm_benchmark` AutoGLM-Phone 11-step task.
fn canonical_plan() -> Vec<Action> {
    vec![
        // 1. Tap a button (DOWN + UP at one coord)
        Action::Tap {
            x: 540,
            y: 1200,
            deadline_ms: 1000,
        },
        // 2. Swipe across a list (320 ms, ~60 Hz)
        Action::Swipe {
            x1: 540,
            y1: 800,
            x2: 540,
            y2: 200,
            dur_ms: 320,
            deadline_ms: 1000,
        },
        // 3. Type 10 chars
        Action::TypeText {
            text: "abcdefghij".into(),
            deadline_ms: 1000,
        },
        // 4. Press Android BACK (KEYCODE_BACK = 4)
        Action::Key {
            code: 4,
            deadline_ms: 1000,
        },
        // 5. GamepadFrame (raw 15-byte HID report — neutral state)
        Action::GamepadFrame {
            report: [0u8; 15],
            deadline_ms: 1000,
        },
    ]
}

fn run(iterations: u64) -> Vec<Duration> {
    let transport = MockTransport::new();
    // `OpenRequest::all()` opens the keyboard + mouse + gamepad
    // UHID slots — required because the canonical plan exercises
    // all three.
    let mut injector = ActionInjector::open(transport, OpenRequest::all())
        .expect("open ActionInjector over MockTransport");
    let plan = canonical_plan();

    let mut samples = Vec::with_capacity((iterations as usize) * plan.len());
    for _ in 0..iterations {
        for action in &plan {
            let outcome = injector.apply(action);
            // Only collect samples from landed actions so the
            // p99 reflects the happy path, not the
            // not-implemented short-circuit.
            if outcome.landed {
                samples.push(outcome.elapsed);
            }
        }
    }
    samples
}

fn percentile(sorted: &[Duration], pct: f64) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    let idx = ((sorted.len() as f64 - 1.0) * pct).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn print_report(samples: &mut [Duration], iterations: u64, plan_len: usize) {
    samples.sort_unstable();
    let p50 = percentile(samples, 0.50);
    let p95 = percentile(samples, 0.95);
    let p99 = percentile(samples, 0.99);
    let max = samples.last().copied().unwrap_or(Duration::ZERO);
    let total = samples.iter().sum::<Duration>();
    let mean = if samples.is_empty() {
        Duration::ZERO
    } else {
        total / samples.len() as u32
    };

    println!("== adk-bench: intent→UHID hot path ==");
    println!("iterations      : {iterations}");
    println!("plan_len        : {plan_len}");
    println!("landed_samples  : {}", samples.len());
    println!(
        "latency (µs)    : mean {:>5.1}  p50 {:>5.1}  p95 {:>5.1}  p99 {:>5.1}  max {:>5.1}",
        mean.as_micros() as f64,
        p50.as_micros() as f64,
        p95.as_micros() as f64,
        p99.as_micros() as f64,
        max.as_micros() as f64,
    );
    println!("total wall      : {:.2} ms", total.as_secs_f64() * 1000.0);

    // SLO gate: p99 < 5 ms. Emit non-zero exit on regression so
    // CI catches it.
    let slo = Duration::from_micros(5_000);
    if p99 > slo {
        eprintln!(
            "FAIL: p99 = {:.1} µs exceeds SLO {:.1} µs",
            p99.as_micros() as f64,
            slo.as_micros() as f64,
        );
        std::process::exit(2);
    } else {
        println!(
            "OK   : p99 = {:.1} µs within SLO {:.1} µs",
            p99.as_micros() as f64,
            slo.as_micros() as f64,
        );
    }
}

fn parse_args() -> u64 {
    let mut iterations = ITERATIONS_DEFAULT;
    for arg in std::env::args().skip(1) {
        if let Some(rest) = arg.strip_prefix("--iterations=") {
            iterations = rest.parse().unwrap_or(ITERATIONS_DEFAULT);
        } else if arg == "--quick" {
            iterations = 100;
        } else if arg == "--help" || arg == "-h" {
            println!("adk-bench [--iterations=N] [--quick]");
            println!("  default: {ITERATIONS_DEFAULT} iterations");
            println!("  --quick: 100 iterations (for CI smoke)");
            std::process::exit(0);
        }
    }
    iterations
}

fn main() {
    let iterations = parse_args();
    let plan = canonical_plan();
    let started = Instant::now();
    let mut samples = run(iterations);
    let elapsed = started.elapsed();
    print_report(&mut samples, iterations, plan.len());
    println!(
        "wall clock      : {:.2} ms total, {:.1} ns / sample",
        elapsed.as_secs_f64() * 1000.0,
        elapsed.as_nanos() as f64 / samples.len().max(1) as f64
    );
}
