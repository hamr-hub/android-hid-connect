//! In-process E2E test for the AI summary protocol.
//!
//! Spins up a TCP listener that:
//!   1. Accepts a connection.
//!   2. Reads control messages (validates they parse correctly).
//!   3. Sends 3 synthetic FRAME_SUMMARY envelopes back.
//!   4. Replies to an AI_QUERY with an AI_STATS envelope.
//!
//! The client-side uses the real `read_device_envelope` + serializer
//! paths, so this exercises the full byte-level protocol.
//!
//! Run with: `cargo test --test ai_summary_e2e -- --nocapture`

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::Duration;

use android_hid_connect::ai::{
    read_device_envelope, AiStats, DeviceEnvelope, FrameSummary, MotionVector, ObjectBox,
    FLAG_KEYFRAME, FLAG_MOTION, FLAG_OBJECTS,
};
use android_hid_connect::control::message::{
    AiConfig, AiQuery, ControlMessage, AI_FLAG_FEATURES, AI_FLAG_KEYFRAMES, AI_FLAG_MOTION,
};

/// Server-side mock: writes one dummy byte + 64-byte device meta,
/// then reads control messages and replies with AI frames + stats.
fn run_mock_server(mut sock: TcpStream) {
    // Drain the incoming control messages — we just verify they parse.
    sock.set_read_timeout(Some(Duration::from_millis(200))).ok();

    // Consume the scrcpy protocol prefix: 1 dummy byte + 64-byte device
    // meta. These are out-of-band, not control messages.
    let mut prefix = vec![0u8; 65];
    let _ = sock.read_exact(&mut prefix);

    // Read up to 16 control messages.
    let mut got_ai_config = false;
    let mut got_ai_query = false;
    for _ in 0..16 {
        let mut len_byte = [0u8; 1];
        if sock.read_exact(&mut len_byte).is_err() {
            break;
        }
        // Reconstruct the message by reading the rest based on the tag.
        let tag = len_byte[0];
        let rest: Vec<u8> = match tag {
            22 => {
                // AI_CONFIG: u8 flags + u16 sample + u16 dim
                let mut b = vec![0u8; 5];
                if sock.read_exact(&mut b).is_err() {
                    break;
                }
                b.insert(0, tag);
                b
            }
            23 => {
                // AI_QUERY: u64 since
                let mut b = vec![0u8; 8];
                if sock.read_exact(&mut b).is_err() {
                    break;
                }
                b.insert(0, tag);
                b
            }
            24 => vec![tag],
            _ => {
                // Other tags we don't care about for this test; just
                // bail.
                break;
            }
        };
        if tag == 22 {
            got_ai_config = true;
        }
        if tag == 23 {
            got_ai_query = true;
        }
        eprintln!("  [mock] got control msg tag={tag}, len={}", rest.len());
    }
    assert!(got_ai_config, "mock server never saw AiConfig");
    assert!(got_ai_query, "mock server never saw AiQuery");

    // Send 3 frame summaries.
    for i in 0..3u32 {
        let frame = synthetic_frame(i);
        let env = build_frame_envelope(&frame);
        sock.write_all(&env).unwrap();
    }
    // Send stats.
    let stats = AiStats {
        uptime_ms: 1_000,
        frames_sampled: 3,
        frames_skipped_keyframe: 0,
        frames_yolo_inferred: 3,
        bytes_summary_emitted: 720,
        avg_latency_ms: 4.2,
        current_fps: 5.0,
    };
    let env = build_stats_envelope(&stats);
    sock.write_all(&env).unwrap();
}

fn be_u16(v: u16) -> [u8; 2] {
    v.to_be_bytes()
}
fn be_u32(v: u32) -> [u8; 4] {
    v.to_be_bytes()
}
fn be_u64(v: u64) -> [u8; 8] {
    v.to_be_bytes()
}
fn be_i16(v: i16) -> [u8; 2] {
    v.to_be_bytes()
}
fn be_f32(v: f32) -> [u8; 4] {
    v.to_be_bytes()
}

fn build_frame_envelope(s: &FrameSummary) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend(be_u64(s.timestamp_ms));
    body.extend(be_u32(s.frame_seq));
    body.extend(be_u16(s.width));
    body.extend(be_u16(s.height));
    body.push(s.flags);
    body.extend(be_u16(s.features.len() as u16));
    for &f in &s.features {
        body.extend(be_f32(f));
    }
    body.extend(be_u16(s.motion.len() as u16));
    for v in &s.motion {
        body.extend(be_u16(v.x));
        body.extend(be_u16(v.y));
        body.extend(be_i16(v.dx));
        body.extend(be_i16(v.dy));
    }
    body.extend(be_u16(s.objects.len() as u16));
    for o in &s.objects {
        body.extend(be_u16(o.x));
        body.extend(be_u16(o.y));
        body.extend(be_u16(o.w));
        body.extend(be_u16(o.h));
        body.push(o.class_id);
        body.push(o.confidence);
    }
    body.push(s.text_regions.len() as u8);
    for t in &s.text_regions {
        body.extend(be_u16(t.x));
        body.extend(be_u16(t.y));
        body.extend(be_u16(t.w));
        body.extend(be_u16(t.h));
    }
    let mut env = vec![3u8]; // TYPE_FRAME_SUMMARY
    env.extend(be_u32(body.len() as u32));
    env.extend(body);
    env
}

fn build_stats_envelope(s: &AiStats) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend(be_u64(s.uptime_ms));
    body.extend(be_u32(s.frames_sampled));
    body.extend(be_u32(s.frames_skipped_keyframe));
    body.extend(be_u32(s.frames_yolo_inferred));
    body.extend(be_u64(s.bytes_summary_emitted));
    body.extend(be_f32(s.avg_latency_ms));
    body.extend(be_f32(s.current_fps));
    let mut env = vec![4u8]; // TYPE_AI_STATS
    env.extend(be_u32(body.len() as u32));
    env.extend(body);
    env
}

fn synthetic_frame(seq: u32) -> FrameSummary {
    FrameSummary {
        timestamp_ms: 1_700_000_000_000 + (seq as u64) * 200,
        frame_seq: seq,
        width: 1080,
        height: 2400,
        flags: if seq == 0 {
            FLAG_KEYFRAME | FLAG_MOTION
        } else {
            FLAG_OBJECTS | FLAG_MOTION
        },
        features: vec![0.1, 0.2, 0.3, 0.4],
        motion: vec![MotionVector {
            x: 540,
            y: 1200,
            dx: 5,
            dy: -3,
        }],
        objects: vec![ObjectBox {
            x: 100,
            y: 200,
            w: 300,
            h: 80,
            class_id: 0,
            confidence: 220,
        }],
        text_regions: vec![],
    }
}

#[test]
fn ai_protocol_round_trip() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();

    let server_thread = thread::spawn(move || {
        let (sock, _) = listener.accept().unwrap();
        run_mock_server(sock);
    });

    let mut client = TcpStream::connect(addr).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    client
        .set_write_timeout(Some(Duration::from_secs(2)))
        .unwrap();

    // Send dummy byte + device meta (the AI server is mock; this
    // mirrors what scrcpy-server expects).
    client.write_all(&[0u8]).unwrap();
    client.write_all(&[0u8; 64]).unwrap();

    // Send AiConfig.
    let cfg = ControlMessage::AiConfig(AiConfig {
        flags: AI_FLAG_KEYFRAMES | AI_FLAG_FEATURES | AI_FLAG_MOTION,
        sample_interval_ms: 200,
        feature_dim: 64,
    });
    client.write_all(&cfg.serialize().unwrap()).unwrap();

    // Send AiQuery.
    let q = ControlMessage::AiQuery(AiQuery {
        since_timestamp_ms: 0,
    });
    client.write_all(&q.serialize().unwrap()).unwrap();

    // Read 4 envelopes (3 frames + 1 stats).
    let mut frames = 0;
    let mut stats_seen = false;
    for _ in 0..4 {
        match read_device_envelope(&mut client) {
            Ok(Some(DeviceEnvelope::Frame(s))) => {
                frames += 1;
                eprintln!("  [client] got frame#{}: {}", s.frame_seq, s.describe());
                assert!(!s.features.is_empty());
                assert!(!s.motion.is_empty());
                if s.frame_seq == 0 {
                    assert!(s.is_keyframe());
                }
            }
            Ok(Some(DeviceEnvelope::Stats(st))) => {
                stats_seen = true;
                eprintln!("  [client] got stats: {}", st.describe());
                assert_eq!(st.frames_sampled, 3);
            }
            Ok(None) => panic!("unexpected None envelope"),
            Err(e) => panic!("read error: {e}"),
        }
    }
    assert_eq!(frames, 3, "expected 3 frame summaries");
    assert!(stats_seen, "expected an AI_STATS envelope");

    server_thread.join().unwrap();
}
