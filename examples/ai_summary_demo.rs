//! `ai_summary_demo` — consume on-device AI summaries from a Rust LLM
//! agent's perspective.
//!
//! Demonstrates the full pipeline:
//!   1. Open a scrcpy-server control socket (mock or live).
//!   2. Send `AiConfig` to enable keyframe + features + motion +
//!      objects on the device.
//!   3. Read DEVICE_MSG envelopes and print human-readable summaries
//!      to stdout — exactly the format an LLM would consume.
//!
//! Two modes:
//!
//! - Live mode:  connect to `127.0.0.1:27183` (default) and a real
//!   scrcpy-ai-server running on the device.
//! - Mock mode:  pass `--mock` to drive the same parser with a
//!   scripted payload generator (no device needed). Useful for CI /
//!   offline LLM-agent development.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use android_hid_connect::ai::{read_device_envelope, AiStats, DeviceEnvelope, FrameSummary};
use android_hid_connect::control::message::{
    AiConfig, AiQuery, ControlMessage, AI_FLAG_FEATURES, AI_FLAG_KEYFRAMES, AI_FLAG_MOTION,
    AI_FLAG_OBJECTS, AI_FLAG_TEXT,
};
use android_hid_connect::transport::open_tcp;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

const PORT: u16 = 27183;

fn drain_dummy_and_meta(stream: &mut TcpStream) -> std::io::Result<()> {
    let mut dummy = [0u8; 1];
    stream.read_exact(&mut dummy)?;
    let mut meta = vec![0u8; 64];
    stream.read_exact(&mut meta)?;
    Ok(())
}

fn enable_ai(stream: &mut TcpStream) -> Result<()> {
    let cfg = ControlMessage::AiConfig(AiConfig {
        flags: AI_FLAG_KEYFRAMES
            | AI_FLAG_FEATURES
            | AI_FLAG_MOTION
            | AI_FLAG_OBJECTS
            | AI_FLAG_TEXT,
        sample_interval_ms: 200,
        feature_dim: 64,
    });
    let bytes = cfg.serialize()?;
    stream.write_all(&bytes)?;
    stream.flush()?;
    println!(
        "  >> sent AiConfig: keyframes+features+motion+objects+text, 200ms interval, 64-D features"
    );
    Ok(())
}

fn query_stats(stream: &mut TcpStream) -> Result<()> {
    let q = ControlMessage::AiQuery(AiQuery {
        since_timestamp_ms: 0,
    });
    let bytes = q.serialize()?;
    stream.write_all(&bytes)?;
    stream.flush()?;
    println!("  >> sent AiQuery(since=0)");
    Ok(())
}

/// Print one frame summary in a form directly suitable for an LLM
/// system prompt. Designed to fit on a single line per frame so the
/// LLM can scan quickly.
fn print_frame(s: &FrameSummary) {
    println!("  FRAME {}", s.describe());
    if !s.features.is_empty() {
        let preview: Vec<String> = s
            .features
            .iter()
            .take(8)
            .map(|f| format!("{:.2}", f))
            .collect();
        println!("         features[:8] = [{}]", preview.join(" "));
    }
    for v in s.motion.iter().take(3) {
        println!(
            "         motion @({},{}) -> ({:+},{:+})",
            v.x, v.y, v.dx, v.dy
        );
    }
    for o in s.objects.iter().take(3) {
        println!(
            "         object class={} conf={} box=({},{},{},{})",
            o.class_id, o.confidence, o.x, o.y, o.w, o.h
        );
    }
    for t in s.text_regions.iter().take(3) {
        println!("         text region=({},{},{},{})", t.x, t.y, t.w, t.h);
    }
}

fn print_stats(st: &AiStats) {
    println!("  STATS {}", st.describe());
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let mock_mode = args.iter().any(|a| a == "--mock");

    println!("== android-hid-connect AI summary consumer ==");
    let mut stream = open_tcp("127.0.0.1", PORT)?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;

    if mock_mode {
        println!("  MOCK MODE: not actually connecting; running scripted payload generator.");
        let mut frames = 0;
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(2) {
            let f = synthetic_frame_summary(frames);
            print_frame(&f);
            frames += 1;
            std::thread::sleep(Duration::from_millis(200));
        }
        let s = synthetic_ai_stats(frames as usize);
        print_stats(&s);
        println!(
            "\n== Mock summary: {} frames synthesized in {:?}",
            frames,
            start.elapsed()
        );
        return Ok(());
    }

    drain_dummy_and_meta(&mut stream)?;
    println!("  connected, device meta drained");
    enable_ai(&mut stream)?;
    query_stats(&mut stream)?;

    let start = Instant::now();
    let mut frames = 0usize;
    let mut stats_seen = 0usize;
    while start.elapsed() < Duration::from_secs(10) {
        match read_device_envelope(&mut stream) {
            Ok(Some(DeviceEnvelope::Frame(s))) => {
                frames += 1;
                if frames <= 5 || s.is_keyframe() || s.has_objects() {
                    print_frame(&s);
                }
            }
            Ok(Some(DeviceEnvelope::Stats(st))) => {
                stats_seen += 1;
                print_stats(&st);
            }
            Ok(None) => {
                // unknown type; skip and continue
            }
            Err(e) => {
                eprintln!("  ERR  read_device_envelope: {e}");
                break;
            }
        }
        // Query every 2s so we get periodic stats.
        if start.elapsed().as_millis() % 2000 < 50 {
            let _ = query_stats(&mut stream);
        }
    }
    println!(
        "\n== Done. Consumed {} frames + {} stats envelopes in {:?} ==",
        frames,
        stats_seen,
        start.elapsed()
    );
    Ok(())
}

// === Mock synthesis (no device required) ===

fn synthetic_frame_summary(seq: u32) -> FrameSummary {
    use android_hid_connect::ai::{
        MotionVector, ObjectBox, TextRegion, FLAG_KEYFRAME, FLAG_MOTION, FLAG_OBJECTS,
        FLAG_SCENE_CHANGE,
    };
    let t = seq as f32 * 0.1;
    let features: Vec<f32> = (0..64)
        .map(|i| {
            let v = ((t + i as f32) * 0.13).sin().abs();
            (v * 1000.0) as u32 as f32 / 1000.0
        })
        .collect();
    let motion = if seq.is_multiple_of(3) {
        vec![MotionVector {
            x: 540,
            y: 1200,
            dx: (t.sin() * 10.0) as i16,
            dy: (t.cos() * 8.0) as i16,
        }]
    } else {
        vec![]
    };
    let objects = if seq.is_multiple_of(5) {
        vec![ObjectBox {
            x: 100,
            y: 200,
            w: 300,
            h: 80,
            class_id: 0,
            confidence: 220,
        }]
    } else {
        vec![]
    };
    let text_regions = if seq.is_multiple_of(7) {
        vec![TextRegion {
            x: 50,
            y: 100,
            w: 200,
            h: 50,
        }]
    } else {
        vec![]
    };
    let mut flags = 0;
    if seq.is_multiple_of(10) {
        flags |= FLAG_SCENE_CHANGE;
    }
    if seq == 0 || seq.is_multiple_of(10) {
        flags |= FLAG_KEYFRAME;
    }
    if !motion.is_empty() {
        flags |= FLAG_MOTION;
    }
    if !objects.is_empty() {
        flags |= FLAG_OBJECTS;
    }
    FrameSummary {
        timestamp_ms: 1_700_000_000_000 + (seq as u64) * 200,
        frame_seq: seq,
        width: 1080,
        height: 2400,
        flags,
        features,
        motion,
        objects,
        text_regions,
    }
}

fn synthetic_ai_stats(sampled: usize) -> AiStats {
    AiStats {
        uptime_ms: 2_000,
        frames_sampled: sampled as u32,
        frames_skipped_keyframe: 0,
        frames_yolo_inferred: sampled as u32,
        bytes_summary_emitted: sampled as u64 * 240,
        avg_latency_ms: 4.5,
        current_fps: 5.0,
    }
}
