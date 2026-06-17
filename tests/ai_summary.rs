//! Integration tests for the AI summary wire protocol.
//!
//! Verifies:
//!   1. `ControlMessage::AiConfig / AiQuery / AiPause` serialize to the
//!      correct byte layout (types 22/23/24, no length prefix on the
//!      control socket).
//!   2. `FrameSummary::parse` round-trips a server-shaped payload.
//!   3. `read_device_envelope` skips unknown types forward-compatibly.
//!   4. `DeviceEnvelope` is correctly disambiguated.

use android_hid_connect::ai::{
    read_device_envelope, AiStats, DeviceEnvelope, FrameSummary, MotionVector, ObjectBox,
    TextRegion, FLAG_KEYFRAME, FLAG_MOTION, FLAG_OBJECTS, FLAG_SCENE_CHANGE,
};
use android_hid_connect::control::message::{
    AiConfig, AiQuery, ControlMessage, AI_FLAG_FEATURES, AI_FLAG_KEYFRAMES, AI_FLAG_MOTION,
    AI_FLAG_OBJECTS,
};

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

#[test]
fn ai_config_serializes_to_tag_22() {
    let msg = ControlMessage::AiConfig(AiConfig {
        flags: AI_FLAG_KEYFRAMES | AI_FLAG_FEATURES | AI_FLAG_MOTION | AI_FLAG_OBJECTS,
        sample_interval_ms: 200,
        feature_dim: 64,
    });
    let v = msg.serialize().unwrap();
    // type(1) + flags(1) + sample(2) + dim(2) = 6
    assert_eq!(v.len(), 6);
    assert_eq!(v[0], 22);
    assert_eq!(v[1], 0x0F);
    assert_eq!(&v[2..4], &be_u16(200));
    assert_eq!(&v[4..6], &be_u16(64));
}

#[test]
fn ai_query_serializes_to_tag_23() {
    let msg = ControlMessage::AiQuery(AiQuery {
        since_timestamp_ms: 1_700_000_000_000,
    });
    let v = msg.serialize().unwrap();
    assert_eq!(v.len(), 9);
    assert_eq!(v[0], 23);
    assert_eq!(&v[1..9], &be_u64(1_700_000_000_000));
}

#[test]
fn ai_pause_is_tag_only() {
    let v = ControlMessage::AiPause.serialize().unwrap();
    assert_eq!(v, vec![24]);
}

#[test]
fn frame_summary_round_trip_full() {
    let payload = {
        let mut b = Vec::new();
        b.extend(be_u64(123_456_789));
        b.extend(be_u32(7));
        b.extend(be_u16(1080));
        b.extend(be_u16(2400));
        b.push(FLAG_KEYFRAME | FLAG_SCENE_CHANGE | FLAG_MOTION | FLAG_OBJECTS);
        // 3 features
        b.extend(be_u16(3));
        b.extend(be_f32(0.1));
        b.extend(be_f32(0.5));
        b.extend(be_f32(0.9));
        // 2 motion vectors
        b.extend(be_u16(2));
        b.extend(be_u16(100));
        b.extend(be_u16(200));
        b.extend(be_i16(5));
        b.extend(be_i16(-3));
        b.extend(be_u16(300));
        b.extend(be_u16(400));
        b.extend(be_i16(0));
        b.extend(be_i16(7));
        // 1 object box
        b.extend(be_u16(1));
        b.extend(be_u16(50));
        b.extend(be_u16(60));
        b.extend(be_u16(200));
        b.extend(be_u16(40));
        b.push(0);
        b.push(200);
        // 1 text region
        b.push(1);
        b.extend(be_u16(10));
        b.extend(be_u16(20));
        b.extend(be_u16(30));
        b.extend(be_u16(40));
        b
    };
    let s = FrameSummary::parse(&payload).unwrap();
    assert_eq!(s.timestamp_ms, 123_456_789);
    assert_eq!(s.frame_seq, 7);
    assert!(s.is_keyframe());
    assert!(s.is_scene_change());
    assert_eq!(s.features.len(), 3);
    assert!((s.features[1] - 0.5).abs() < 1e-6);
    assert_eq!(
        s.motion,
        vec![
            MotionVector {
                x: 100,
                y: 200,
                dx: 5,
                dy: -3
            },
            MotionVector {
                x: 300,
                y: 400,
                dx: 0,
                dy: 7
            },
        ]
    );
    assert_eq!(
        s.objects,
        vec![ObjectBox {
            x: 50,
            y: 60,
            w: 200,
            h: 40,
            class_id: 0,
            confidence: 200
        }]
    );
    assert_eq!(
        s.text_regions,
        vec![TextRegion {
            x: 10,
            y: 20,
            w: 30,
            h: 40
        }]
    );
}

#[test]
fn envelope_decode_frame() {
    let mut envelope = vec![3u8]; // type = FRAME_SUMMARY
    let payload = {
        let mut b = Vec::new();
        b.extend(be_u64(1));
        b.extend(be_u32(1));
        b.extend(be_u16(100));
        b.extend(be_u16(100));
        b.push(FLAG_KEYFRAME);
        b.extend(be_u16(0));
        b.extend(be_u16(0));
        b.extend(be_u16(0));
        b.push(0);
        b
    };
    envelope.extend(be_u32(payload.len() as u32));
    envelope.extend(payload);

    let env = read_device_envelope(&mut envelope.as_slice()).unwrap();
    match env {
        Some(DeviceEnvelope::Frame(s)) => {
            assert!(s.is_keyframe());
            assert_eq!(s.features.len(), 0);
        }
        _ => panic!("expected Frame envelope"),
    }
}

#[test]
fn envelope_decode_stats() {
    let mut envelope = vec![4u8]; // type = AI_STATS
    let payload = {
        let mut b = Vec::new();
        b.extend(be_u64(60_000));
        b.extend(be_u32(300));
        b.extend(be_u32(12));
        b.extend(be_u32(50));
        b.extend(be_u64(24_000));
        b.extend(be_f32(4.5));
        b.extend(be_f32(5.2));
        b
    };
    envelope.extend(be_u32(payload.len() as u32));
    envelope.extend(payload);

    let env = read_device_envelope(&mut envelope.as_slice()).unwrap();
    match env {
        Some(DeviceEnvelope::Stats(st)) => {
            let expected = AiStats {
                uptime_ms: 60_000,
                frames_sampled: 300,
                frames_skipped_keyframe: 12,
                frames_yolo_inferred: 50,
                bytes_summary_emitted: 24_000,
                avg_latency_ms: 4.5,
                current_fps: 5.2,
            };
            assert_eq!(st, expected);
        }
        _ => panic!("expected Stats envelope"),
    }
}

#[test]
fn envelope_skips_unknown_types() {
    let mut envelope = vec![99u8]; // unknown
    envelope.extend(be_u32(0));
    let env = read_device_envelope(&mut envelope.as_slice()).unwrap();
    assert!(env.is_none());
}

#[test]
fn frame_summary_describe_for_llm() {
    let mut b = Vec::new();
    b.extend(be_u64(0));
    b.extend(be_u32(1));
    b.extend(be_u16(1080));
    b.extend(be_u16(2400));
    b.push(FLAG_KEYFRAME | FLAG_MOTION);
    b.extend(be_u16(0));
    b.extend(be_u16(1));
    b.extend(be_u16(100));
    b.extend(be_u16(100));
    b.extend(be_i16(5));
    b.extend(be_i16(-5));
    b.extend(be_u16(0));
    b.push(0);
    let s = FrameSummary::parse(&b).unwrap();
    let d = s.describe();
    assert!(d.contains("KEYFRAME"));
    assert!(d.contains("MOTION(1)"));
    assert!(!d.contains("OBJECTS"));
    assert!(!d.contains("SCENE_CHANGE"));
}
