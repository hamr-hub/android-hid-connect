//! `AiSummary` — parse `DEVICE_MSG_TYPE_FRAME_SUMMARY` and
//! `DEVICE_MSG_TYPE_AI_STATS` from the device-side scrcpy-AI server.
//!
//! The on-device server (see `scrcpy-ai-server/src/main/java/com/
//! genymobile/scrcpy/ai/`) emits compact per-frame summaries that an
//! LLM agent can consume directly without ever decoding raw H.264:
//!
//!   - **Keyframe flag** — "this frame is a scene change"
//!   - **Feature vector** — 64-D RGB histogram + lightness moments +
//!     edge density + center-vs-periphery contrast
//!   - **Motion vectors** — sparse 32×32 grid of `(x, y, dx, dy)`
//!   - **Object detections** — `[x, y, w, h, class, confidence]` per box
//!   - **Text regions** — `[x, y, w, h]` per high-contrast block
//!
//! This module is purely about *parsing* the wire format. Sending
//! AI control messages (TYPE_AI_CONFIG / TYPE_AI_QUERY / TYPE_AI_PAUSE)
//! is done by the regular `transport::send_one` path with a typed
//! `ControlMessage` (see [`crate::control::message::ControlMessage`]).

use crate::error::{Error, Result};

/// On-wire tags (must match `AiProtocol.java`).
pub const TYPE_FRAME_SUMMARY: u8 = 3;
pub const TYPE_AI_STATS: u8 = 4;

/// Flag bits carried in `FrameSummary.flags`.
pub const FLAG_KEYFRAME: u8 = 1 << 0;
pub const FLAG_SCENE_CHANGE: u8 = 1 << 1;
pub const FLAG_MOTION: u8 = 1 << 2;
pub const FLAG_OBJECTS: u8 = 1 << 3;
pub const FLAG_TEXT: u8 = 1 << 4;
pub const FLAG_HAS_JPEG: u8 = 1 << 5;

/// One motion vector emitted by `MotionTracker`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MotionVector {
    pub x: u16,
    pub y: u16,
    pub dx: i16,
    pub dy: i16,
}

/// One object detection (stub for now; TFLite YOLO later).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObjectBox {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
    pub class_id: u8,
    /// Confidence in `[0, 255]`.
    pub confidence: u8,
}

/// One text region (high-contrast rectangle).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextRegion {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

/// Parsed `DEVICE_MSG_TYPE_FRAME_SUMMARY` payload.
#[derive(Debug, Clone, PartialEq)]
pub struct FrameSummary {
    pub timestamp_ms: u64,
    pub frame_seq: u32,
    pub width: u16,
    pub height: u16,
    pub flags: u8,
    pub features: Vec<f32>,
    pub motion: Vec<MotionVector>,
    pub objects: Vec<ObjectBox>,
    pub text_regions: Vec<TextRegion>,
}

impl FrameSummary {
    /// True iff the FLAG_KEYFRAME bit is set.
    #[inline]
    pub fn is_keyframe(&self) -> bool {
        self.flags & FLAG_KEYFRAME != 0
    }
    /// True iff the FLAG_SCENE_CHANGE bit is set.
    #[inline]
    pub fn is_scene_change(&self) -> bool {
        self.flags & FLAG_SCENE_CHANGE != 0
    }
    /// True iff any object boxes were emitted on this frame.
    #[inline]
    pub fn has_objects(&self) -> bool {
        !self.objects.is_empty()
    }
    /// True iff any motion vectors were emitted on this frame.
    #[inline]
    pub fn is_moving(&self) -> bool {
        !self.motion.is_empty()
    }

    /// Render a one-line human-readable summary for LLM prompts.
    pub fn describe(&self) -> String {
        let mut s = format!(
            "frame#{} ts={}ms {}x{} flags=0x{:02x}",
            self.frame_seq, self.timestamp_ms, self.width, self.height, self.flags
        );
        if self.is_keyframe() {
            s.push_str(" KEYFRAME");
        }
        if self.is_scene_change() {
            s.push_str(" SCENE_CHANGE");
        }
        if self.is_moving() {
            s.push_str(&format!(" MOTION({})", self.motion.len()));
        }
        if self.has_objects() {
            s.push_str(&format!(" OBJECTS({})", self.objects.len()));
        }
        if !self.text_regions.is_empty() {
            s.push_str(&format!(" TEXT({})", self.text_regions.len()));
        }
        if !self.features.is_empty() {
            s.push_str(&format!(" features_dim={}", self.features.len()));
        }
        s
    }

    /// Parse from the byte payload that follows the type+length
    /// prefix. The caller has already consumed the 1-byte type tag
    /// and the 4-byte big-endian length, and is passing only the
    /// `payload` slice.
    pub fn parse(payload: &[u8]) -> Result<Self> {
        let mut cur = std::io::Cursor::new(payload);
        use std::io::Read;
        let timestamp_ms = read_u64_be(&mut cur).map_err(io_err)?;
        let frame_seq = read_u32_be(&mut cur).map_err(io_err)?;
        let width = read_u16_be(&mut cur).map_err(io_err)?;
        let height = read_u16_be(&mut cur).map_err(io_err)?;
        let flags = read_u8(&mut cur).map_err(io_err)?;
        let feature_dim = read_u16_be(&mut cur).map_err(io_err)? as usize;
        let mut features = Vec::with_capacity(feature_dim);
        for _ in 0..feature_dim {
            features.push(read_f32_be(&mut cur).map_err(io_err)?);
        }
        let num_motion = read_u16_be(&mut cur).map_err(io_err)? as usize;
        let mut motion = Vec::with_capacity(num_motion);
        for _ in 0..num_motion {
            motion.push(MotionVector {
                x: read_u16_be(&mut cur).map_err(io_err)?,
                y: read_u16_be(&mut cur).map_err(io_err)?,
                dx: read_i16_be(&mut cur).map_err(io_err)?,
                dy: read_i16_be(&mut cur).map_err(io_err)?,
            });
        }
        let num_obj = read_u16_be(&mut cur).map_err(io_err)? as usize;
        let mut objects = Vec::with_capacity(num_obj);
        for _ in 0..num_obj {
            objects.push(ObjectBox {
                x: read_u16_be(&mut cur).map_err(io_err)?,
                y: read_u16_be(&mut cur).map_err(io_err)?,
                w: read_u16_be(&mut cur).map_err(io_err)?,
                h: read_u16_be(&mut cur).map_err(io_err)?,
                class_id: read_u8(&mut cur).map_err(io_err)?,
                confidence: read_u8(&mut cur).map_err(io_err)?,
            });
        }
        let num_text = read_u8(&mut cur).map_err(io_err)? as usize;
        let mut text_regions = Vec::with_capacity(num_text);
        for _ in 0..num_text {
            text_regions.push(TextRegion {
                x: read_u16_be(&mut cur).map_err(io_err)?,
                y: read_u16_be(&mut cur).map_err(io_err)?,
                w: read_u16_be(&mut cur).map_err(io_err)?,
                h: read_u16_be(&mut cur).map_err(io_err)?,
            });
        }
        // Drain any trailing bytes (forward-compat padding).
        let mut sink = [0u8; 64];
        while cur.position() < payload.len() as u64 {
            let n = Read::read(&mut cur, &mut sink).unwrap_or(0);
            if n == 0 {
                break;
            }
        }
        Ok(FrameSummary {
            timestamp_ms,
            frame_seq,
            width,
            height,
            flags,
            features,
            motion,
            objects,
            text_regions,
        })
    }
}

/// Parsed `DEVICE_MSG_TYPE_AI_STATS` payload.
#[derive(Debug, Clone, PartialEq)]
pub struct AiStats {
    pub uptime_ms: u64,
    pub frames_sampled: u32,
    pub frames_skipped_keyframe: u32,
    pub frames_yolo_inferred: u32,
    pub bytes_summary_emitted: u64,
    pub avg_latency_ms: f32,
    pub current_fps: f32,
}

impl AiStats {
    pub fn parse(payload: &[u8]) -> Result<Self> {
        let mut cur = std::io::Cursor::new(payload);
        Ok(AiStats {
            uptime_ms: read_u64_be(&mut cur).map_err(io_err)?,
            frames_sampled: read_u32_be(&mut cur).map_err(io_err)?,
            frames_skipped_keyframe: read_u32_be(&mut cur).map_err(io_err)?,
            frames_yolo_inferred: read_u32_be(&mut cur).map_err(io_err)?,
            bytes_summary_emitted: read_u64_be(&mut cur).map_err(io_err)?,
            avg_latency_ms: read_f32_be(&mut cur).map_err(io_err)?,
            current_fps: read_f32_be(&mut cur).map_err(io_err)?,
        })
    }

    /// One-line summary for dashboards.
    pub fn describe(&self) -> String {
        format!(
            "uptime={}s sampled={} yolo={} bytes={} avg={:.1}ms fps={:.1}",
            self.uptime_ms / 1000,
            self.frames_sampled,
            self.frames_yolo_inferred,
            self.bytes_summary_emitted,
            self.avg_latency_ms,
            self.current_fps,
        )
    }
}

// === big-endian readers ===

fn io_err(e: std::io::Error) -> Error {
    Error::Transport(format!("{e}"))
}

fn read_u8(cur: &mut std::io::Cursor<&[u8]>) -> std::io::Result<u8> {
    let mut b = [0u8; 1];
    std::io::Read::read_exact(cur, &mut b)?;
    Ok(b[0])
}
fn read_u16_be(cur: &mut std::io::Cursor<&[u8]>) -> std::io::Result<u16> {
    let mut b = [0u8; 2];
    std::io::Read::read_exact(cur, &mut b)?;
    Ok(u16::from_be_bytes(b))
}
fn read_i16_be(cur: &mut std::io::Cursor<&[u8]>) -> std::io::Result<i16> {
    let mut b = [0u8; 2];
    std::io::Read::read_exact(cur, &mut b)?;
    Ok(i16::from_be_bytes(b))
}
fn read_u32_be(cur: &mut std::io::Cursor<&[u8]>) -> std::io::Result<u32> {
    let mut b = [0u8; 4];
    std::io::Read::read_exact(cur, &mut b)?;
    Ok(u32::from_be_bytes(b))
}
fn read_u64_be(cur: &mut std::io::Cursor<&[u8]>) -> std::io::Result<u64> {
    let mut b = [0u8; 8];
    std::io::Read::read_exact(cur, &mut b)?;
    Ok(u64::from_be_bytes(b))
}
fn read_f32_be(cur: &mut std::io::Cursor<&[u8]>) -> std::io::Result<f32> {
    let mut b = [0u8; 4];
    std::io::Read::read_exact(cur, &mut b)?;
    Ok(f32::from_be_bytes(b))
}

/// Parse the next device-msg from a `Read + Write` byte stream. This
/// is the full device-msg envelope: type(1) + length(4 BE) + payload.
/// Returns `Ok(None)` for unknown / unsupported types (so the caller
/// can skip forward).
pub fn read_device_envelope<R: std::io::Read>(r: &mut R) -> Result<Option<DeviceEnvelope>> {
    let mut header = [0u8; 5];
    if let Err(e) = std::io::Read::read_exact(r, &mut header) {
        if e.kind() == std::io::ErrorKind::UnexpectedEof {
            return Ok(None);
        }
        return Err(Error::Transport(format!("{e}")));
    }
    let ty = header[0];
    let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    let mut payload = vec![0u8; len];
    std::io::Read::read_exact(r, &mut payload).map_err(|e| Error::Transport(format!("{e}")))?;
    match ty {
        TYPE_FRAME_SUMMARY => Ok(Some(DeviceEnvelope::Frame(FrameSummary::parse(&payload)?))),
        TYPE_AI_STATS => Ok(Some(DeviceEnvelope::Stats(AiStats::parse(&payload)?))),
        _ => Ok(None),
    }
}

/// One parsed device-msg envelope.
#[derive(Debug, Clone, PartialEq)]
pub enum DeviceEnvelope {
    Frame(FrameSummary),
    Stats(AiStats),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_be_u16(v: u16) -> Vec<u8> {
        v.to_be_bytes().to_vec()
    }
    fn write_be_u32(v: u32) -> Vec<u8> {
        v.to_be_bytes().to_vec()
    }
    fn write_be_u64(v: u64) -> Vec<u8> {
        v.to_be_bytes().to_vec()
    }
    fn write_be_i16(v: i16) -> Vec<u8> {
        v.to_be_bytes().to_vec()
    }
    fn write_be_f32(v: f32) -> Vec<u8> {
        v.to_be_bytes().to_vec()
    }

    fn build_frame_summary() -> Vec<u8> {
        let mut b = Vec::new();
        b.extend(write_be_u64(1_700_000_000_000));
        b.extend(write_be_u32(42));
        b.extend(write_be_u16(1080));
        b.extend(write_be_u16(2400));
        b.push(FLAG_KEYFRAME | FLAG_MOTION | FLAG_OBJECTS);
        b.extend(write_be_u16(4));
        b.extend(write_be_f32(0.1));
        b.extend(write_be_f32(0.2));
        b.extend(write_be_f32(0.3));
        b.extend(write_be_f32(0.4));
        // 2 motion vectors
        b.extend(write_be_u16(2));
        b.extend(write_be_u16(100));
        b.extend(write_be_u16(200));
        b.extend(write_be_i16(5));
        b.extend(write_be_i16(-3));
        b.extend(write_be_u16(300));
        b.extend(write_be_u16(400));
        b.extend(write_be_i16(0));
        b.extend(write_be_i16(7));
        // 1 object box
        b.extend(write_be_u16(1));
        b.extend(write_be_u16(50));
        b.extend(write_be_u16(60));
        b.extend(write_be_u16(200));
        b.extend(write_be_u16(40));
        b.push(0);
        b.push(200);
        // 0 text regions
        b.push(0);
        b
    }

    #[test]
    fn parse_round_trip() {
        let payload = build_frame_summary();
        let s = FrameSummary::parse(&payload).unwrap();
        assert_eq!(s.timestamp_ms, 1_700_000_000_000);
        assert_eq!(s.frame_seq, 42);
        assert_eq!(s.width, 1080);
        assert_eq!(s.height, 2400);
        assert!(s.is_keyframe());
        assert!(s.is_moving());
        assert!(s.has_objects());
        assert_eq!(s.features, vec![0.1, 0.2, 0.3, 0.4]);
        assert_eq!(s.motion.len(), 2);
        assert_eq!(
            s.motion[0],
            MotionVector {
                x: 100,
                y: 200,
                dx: 5,
                dy: -3
            }
        );
        assert_eq!(s.objects.len(), 1);
        assert_eq!(
            s.objects[0],
            ObjectBox {
                x: 50,
                y: 60,
                w: 200,
                h: 40,
                class_id: 0,
                confidence: 200,
            }
        );
    }

    #[test]
    fn parse_empty_features_and_vectors() {
        let mut b = Vec::new();
        b.extend(write_be_u64(0));
        b.extend(write_be_u32(1));
        b.extend(write_be_u16(100));
        b.extend(write_be_u16(100));
        b.push(FLAG_KEYFRAME);
        b.extend(write_be_u16(0)); // feature_dim
        b.extend(write_be_u16(0)); // num_motion
        b.extend(write_be_u16(0)); // num_objects
        b.push(0); // num_text
        let s = FrameSummary::parse(&b).unwrap();
        assert!(s.is_keyframe());
        assert!(!s.is_moving());
        assert!(!s.has_objects());
        assert!(s.features.is_empty());
    }

    #[test]
    fn parse_stats() {
        let mut b = Vec::new();
        b.extend(write_be_u64(60_000)); // uptime
        b.extend(write_be_u32(300)); // sampled
        b.extend(write_be_u32(12)); // skipped
        b.extend(write_be_u32(50)); // yolo
        b.extend(write_be_u64(24_000)); // bytes
        b.extend(write_be_f32(4.5)); // avg_latency
        b.extend(write_be_f32(5.2)); // current_fps
        let st = AiStats::parse(&b).unwrap();
        assert_eq!(st.uptime_ms, 60_000);
        assert_eq!(st.frames_sampled, 300);
        assert!((st.avg_latency_ms - 4.5).abs() < 1e-6);
    }

    #[test]
    fn envelope_unknown_type_returns_none() {
        let mut b = Vec::new();
        b.push(99); // unknown
        b.extend(write_be_u32(0));
        let env = read_device_envelope(&mut b.as_slice()).unwrap();
        assert!(env.is_none());
    }

    #[test]
    fn describe_format() {
        let payload = build_frame_summary();
        let s = FrameSummary::parse(&payload).unwrap();
        let d = s.describe();
        assert!(d.contains("KEYFRAME"));
        assert!(d.contains("MOTION(2)"));
        assert!(d.contains("OBJECTS(1)"));
        assert!(d.contains("features_dim=4"));
    }
}
