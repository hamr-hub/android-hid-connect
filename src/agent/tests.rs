use std::io::{self, Cursor, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::time::Duration;

use super::*;
use crate::ai::{FrameSummary, ObjectBox, TextRegion};
use crate::client::{
    AndroidKeyFrame, KeyboardChordFrame, KeyboardFrame, MouseFrame, ANDROID_KEY_BATCH_FRAMES,
    GAMEPAD_BATCH_FRAMES, KEYBOARD_BATCH_FRAMES, KEYBOARD_CHORD_KEYS, MOUSE_BATCH_FRAMES,
    SCROLL_BATCH_FRAMES, TOUCH_BATCH_FRAMES,
};
use crate::device::{
    spawn_latest_frame_summary_receiver, DeviceEvent, DeviceMessage, LatestFrameSummaryObservation,
    LatestFrameSummaryReceiver, LatestFrameSummarySnapshot, TYPE_ACK_CLIPBOARD, TYPE_UHID_OUTPUT,
};
use crate::error::Error;
use crate::session::{GamepadFrameRaw, HidSession, OpenRequest, GAMEPAD_FRAME_BYTES};
use crate::transport::MockTransport;
use crate::types::{
    AndroidKeyAction, AndroidKeycode, ClipboardCopyKey, GamepadAxis, GamepadButton, Modifiers,
    MouseButton, Scancode, TouchAction, TouchPointerId,
};

#[derive(Debug)]
struct TimedOutReader;

impl Read for TimedOutReader {
    fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
        Err(io::Error::new(io::ErrorKind::TimedOut, "synthetic timeout"))
    }
}

fn ack(sequence: u64) -> Vec<u8> {
    let mut bytes = vec![TYPE_ACK_CLIPBOARD];
    bytes.extend(sequence.to_be_bytes());
    bytes
}

fn clipboard(text: &str) -> Vec<u8> {
    let mut bytes = vec![crate::device::TYPE_CLIPBOARD];
    bytes.extend((text.len() as u32).to_be_bytes());
    bytes.extend(text.as_bytes());
    bytes
}

fn frame_summary_envelope(frame_seq: u32) -> Vec<u8> {
    frame_summary_envelope_with(
        frame_seq,
        &[ObjectBox {
            x: 100,
            y: 200,
            w: 301,
            h: 101,
            class_id: 7,
            confidence: 220,
        }],
        &[],
    )
}

fn frame_summary_envelope_with(
    frame_seq: u32,
    objects: &[ObjectBox],
    text_regions: &[TextRegion],
) -> Vec<u8> {
    frame_summary_envelope_full(
        frame_seq,
        {
            let mut flags = crate::ai::FLAG_KEYFRAME;
            if !objects.is_empty() {
                flags |= crate::ai::FLAG_OBJECTS;
            }
            if !text_regions.is_empty() {
                flags |= crate::ai::FLAG_TEXT;
            }
            flags
        },
        &[],
        objects,
        text_regions,
    )
}

fn frame_summary_envelope_full(
    frame_seq: u32,
    flags: u8,
    motion: &[crate::ai::MotionVector],
    objects: &[ObjectBox],
    text_regions: &[TextRegion],
) -> Vec<u8> {
    frame_summary_envelope_full_at(100, frame_seq, flags, motion, objects, text_regions)
}

fn frame_summary_envelope_full_at(
    timestamp_ms: u64,
    frame_seq: u32,
    flags: u8,
    motion: &[crate::ai::MotionVector],
    objects: &[ObjectBox],
    text_regions: &[TextRegion],
) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend(timestamp_ms.to_be_bytes());
    payload.extend(frame_seq.to_be_bytes());
    payload.extend(1000u16.to_be_bytes());
    payload.extend(2000u16.to_be_bytes());
    payload.push(flags);
    payload.extend(0u16.to_be_bytes());
    payload.extend((motion.len() as u16).to_be_bytes());
    for vector in motion {
        payload.extend(vector.x.to_be_bytes());
        payload.extend(vector.y.to_be_bytes());
        payload.extend(vector.dx.to_be_bytes());
        payload.extend(vector.dy.to_be_bytes());
    }
    payload.extend((objects.len() as u16).to_be_bytes());
    for object in objects {
        payload.extend(object.x.to_be_bytes());
        payload.extend(object.y.to_be_bytes());
        payload.extend(object.w.to_be_bytes());
        payload.extend(object.h.to_be_bytes());
        payload.push(object.class_id);
        payload.push(object.confidence);
    }
    payload.push(text_regions.len() as u8);
    for region in text_regions {
        payload.extend(region.x.to_be_bytes());
        payload.extend(region.y.to_be_bytes());
        payload.extend(region.w.to_be_bytes());
        payload.extend(region.h.to_be_bytes());
    }

    let mut bytes = vec![crate::ai::TYPE_FRAME_SUMMARY];
    bytes.extend((payload.len() as u32).to_be_bytes());
    bytes.extend(payload);
    bytes
}

fn ai_stats_envelope() -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend(1_000u64.to_be_bytes());
    payload.extend(10u32.to_be_bytes());
    payload.extend(1u32.to_be_bytes());
    payload.extend(2u32.to_be_bytes());
    payload.extend(300u64.to_be_bytes());
    payload.extend(4.5f32.to_be_bytes());
    payload.extend(60.0f32.to_be_bytes());

    let mut bytes = vec![crate::ai::TYPE_AI_STATS];
    bytes.extend((payload.len() as u32).to_be_bytes());
    bytes.extend(payload);
    bytes
}

fn latest_snapshot_from_envelope(version: u64, envelope: Vec<u8>) -> LatestFrameSummarySnapshot {
    match crate::device::read_device_event(&mut Cursor::new(envelope)).unwrap() {
        DeviceEvent::FrameSummary(summary) => LatestFrameSummarySnapshot { version, summary },
        other => panic!("expected frame summary, got {other:?}"),
    }
}

fn clipboard_ack_stream(sequence: u64) -> Cursor<Vec<u8>> {
    Cursor::new(ack(sequence))
}

fn tcp_agent_with_reader_bytes(
    bytes: Vec<u8>,
) -> (
    AgentControlSession<MockTransport, TcpStream>,
    std::thread::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut sock, _addr) = listener.accept().unwrap();
        Write::write_all(&mut sock, &bytes).unwrap();
    });

    let reader = TcpStream::connect(addr).unwrap();
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    (
        AgentControlSession::from_parts(session, reader).unwrap(),
        server,
    )
}

fn count_uhid_inputs(buf: &[u8]) -> usize {
    let mut i = 0usize;
    let mut n = 0usize;
    while i < buf.len() {
        match buf[i] {
            12 => {
                if i + 8 > buf.len() {
                    break;
                }
                let name_len = buf[i + 7] as usize;
                if i + 8 + name_len + 2 > buf.len() {
                    break;
                }
                let rd_len_idx = i + 8 + name_len;
                let rd_len = u16::from_be_bytes([buf[rd_len_idx], buf[rd_len_idx + 1]]) as usize;
                i += 8 + name_len + 2 + rd_len;
            }
            TYPE_UHID_OUTPUT => break,
            13 => {
                if i + 5 > buf.len() {
                    break;
                }
                let size = u16::from_be_bytes([buf[i + 3], buf[i + 4]]) as usize;
                n += 1;
                i += 5 + size;
            }
            14 => i += 3,
            _ => break,
        }
    }
    n
}

fn count_touch_events(buf: &[u8]) -> usize {
    let mut i = 0usize;
    let mut n = 0usize;
    while i + 32 <= buf.len() {
        if buf[i] == 2 && buf[i + 1] <= 2 {
            n += 1;
            i += 32;
        } else {
            i += 1;
        }
    }
    n
}

fn first_touch_screen_size(buf: &[u8]) -> Option<(u16, u16)> {
    let mut i = 0usize;
    while i + 32 <= buf.len() {
        if buf[i] == 2 && buf[i + 1] <= 2 {
            let width = u16::from_be_bytes([buf[i + 18], buf[i + 19]]);
            let height = u16::from_be_bytes([buf[i + 20], buf[i + 21]]);
            return Some((width, height));
        }
        i += 1;
    }
    None
}

fn first_touch_xy(buf: &[u8]) -> Option<(i32, i32)> {
    let mut i = 0usize;
    while i + 32 <= buf.len() {
        if buf[i] == 2 && buf[i + 1] <= 2 {
            let x = i32::from_be_bytes(buf[i + 10..i + 14].try_into().unwrap());
            let y = i32::from_be_bytes(buf[i + 14..i + 18].try_into().unwrap());
            return Some((x, y));
        }
        i += 1;
    }
    None
}

fn touch_events(buf: &[u8]) -> Vec<(u8, u64, i32, i32)> {
    let mut i = 0usize;
    let mut events = Vec::new();
    while i < buf.len() {
        let Some(len) = control_message_len_at(buf, i) else {
            return events;
        };
        if buf[i] == 2 {
            events.push((
                buf[i + 1],
                u64::from_be_bytes(buf[i + 2..i + 10].try_into().unwrap()),
                i32::from_be_bytes(buf[i + 10..i + 14].try_into().unwrap()),
                i32::from_be_bytes(buf[i + 14..i + 18].try_into().unwrap()),
            ));
        }
        i += len;
    }
    events
}

fn first_scroll_xy(buf: &[u8]) -> Option<(i32, i32)> {
    let mut i = 0usize;
    while i + 21 <= buf.len() {
        let len = control_message_len_at(buf, i)?;
        if buf[i] == 3 {
            let x = i32::from_be_bytes(buf[i + 1..i + 5].try_into().unwrap());
            let y = i32::from_be_bytes(buf[i + 5..i + 9].try_into().unwrap());
            return Some((x, y));
        }
        i += len;
    }
    None
}

fn mouse_input_payloads(buf: &[u8]) -> Vec<[u8; 5]> {
    let mut i = 0usize;
    let mut out = Vec::new();
    while i < buf.len() {
        let Some(len) = control_message_len_at(buf, i) else {
            return out;
        };
        if buf[i] == 13 && i + 10 <= buf.len() {
            let id = u16::from_be_bytes([buf[i + 1], buf[i + 2]]);
            let size = u16::from_be_bytes([buf[i + 3], buf[i + 4]]) as usize;
            if id == crate::types::HID_ID_MOUSE && size == 5 {
                let mut payload = [0u8; 5];
                payload.copy_from_slice(&buf[i + 5..i + 10]);
                out.push(payload);
            }
        }
        i += len;
    }
    out
}

fn contains_touch_point(buf: &[u8], pointer_id: u64, x: i32, y: i32) -> bool {
    let mut i = 0usize;
    while i + 32 <= buf.len() {
        if buf[i] == 2 && buf[i + 1] <= 2 {
            let got_pointer = u64::from_be_bytes(buf[i + 2..i + 10].try_into().unwrap());
            let got_x = i32::from_be_bytes(buf[i + 10..i + 14].try_into().unwrap());
            let got_y = i32::from_be_bytes(buf[i + 14..i + 18].try_into().unwrap());
            if got_pointer == pointer_id && got_x == x && got_y == y {
                return true;
            }
            i += 32;
        } else {
            i += 1;
        }
    }
    false
}

fn first_inject_keycodes(buf: &[u8], count: usize) -> Vec<u32> {
    let mut keycodes = Vec::with_capacity(count);
    let mut i = 0usize;
    while keycodes.len() < count && i + 14 <= buf.len() {
        assert_eq!(buf[i], 0, "expected INJECT_KEYCODE at offset {i}");
        keycodes.push(u32::from_be_bytes([
            buf[i + 2],
            buf[i + 3],
            buf[i + 4],
            buf[i + 5],
        ]));
        i += 14;
    }
    keycodes
}

fn control_message_len_at(buf: &[u8], i: usize) -> Option<usize> {
    if i >= buf.len() {
        return None;
    }
    let len = match buf[i] {
        0 => 14,
        1 => {
            if i + 5 > buf.len() {
                return None;
            }
            let text_len = u32::from_be_bytes(buf[i + 1..i + 5].try_into().unwrap()) as usize;
            5 + text_len
        }
        2 => 32,
        3 => 21,
        4 => 2,
        5 | 6 | 7 | 11 | 15 | 17 | 19 | 20 => 1,
        18 => 2,
        8 => 2,
        9 => {
            if i + 14 > buf.len() {
                return None;
            }
            let text_len = u32::from_be_bytes(buf[i + 10..i + 14].try_into().unwrap()) as usize;
            14 + text_len
        }
        10 => 2,
        12 => {
            if i + 8 > buf.len() {
                return None;
            }
            let name_len = buf[i + 7] as usize;
            if i + 8 + name_len + 2 > buf.len() {
                return None;
            }
            let rd_len_idx = i + 8 + name_len;
            let rd_len = u16::from_be_bytes([buf[rd_len_idx], buf[rd_len_idx + 1]]) as usize;
            8 + name_len + 2 + rd_len
        }
        13 => {
            if i + 5 > buf.len() {
                return None;
            }
            let size = u16::from_be_bytes([buf[i + 3], buf[i + 4]]) as usize;
            5 + size
        }
        14 => 3,
        16 => {
            if i + 2 > buf.len() {
                return None;
            }
            2 + buf[i + 1] as usize
        }
        21 => 5,
        22 => 6,
        23 => 9,
        24 => 1,
        _ => return None,
    };
    (i + len <= buf.len()).then_some(len)
}

fn find_control_message(buf: &[u8], tag: u8) -> Option<&[u8]> {
    let mut i = 0usize;
    while i < buf.len() {
        let len = control_message_len_at(buf, i)?;
        if buf[i] == tag {
            return Some(&buf[i..i + len]);
        }
        i += len;
    }
    None
}

fn count_control_messages(buf: &[u8], tag: u8) -> usize {
    let mut i = 0usize;
    let mut count = 0usize;
    while i < buf.len() {
        let Some(len) = control_message_len_at(buf, i) else {
            return count;
        };
        if buf[i] == tag {
            count += 1;
        }
        i += len;
    }
    count
}

fn control_message_tags(buf: &[u8]) -> Vec<u8> {
    let mut i = 0usize;
    let mut tags = Vec::new();
    while i < buf.len() {
        let Some(len) = control_message_len_at(buf, i) else {
            return tags;
        };
        tags.push(buf[i]);
        i += len;
    }
    tags
}

fn input_and_touch_tags(buf: &[u8]) -> Vec<u8> {
    control_message_tags(buf)
        .into_iter()
        .filter(|tag| matches!(*tag, 2 | 13))
        .collect()
}

fn contains_inject_keycode(buf: &[u8], keycode: u32) -> bool {
    let mut i = 0usize;
    while i < buf.len() {
        let Some(len) = control_message_len_at(buf, i) else {
            return false;
        };
        if buf[i] == 0 && i + 6 <= buf.len() {
            let got = u32::from_be_bytes(buf[i + 2..i + 6].try_into().unwrap());
            if got == keycode {
                return true;
            }
        }
        i += len;
    }
    false
}

#[test]
fn agent_session_reads_device_messages_and_dispatches_control() {
    let session =
        HidSession::open(MockTransport::new(), OpenRequest::gamepad_only_realtime()).unwrap();
    let mut agent = AgentControlSession::from_parts(session, Cursor::new(ack(42))).unwrap();

    assert_eq!(
        agent.recv_device_message().unwrap(),
        DeviceMessage::AckClipboard { sequence: 42 }
    );
    agent
        .client()
        .send_frame_unchecked(GamepadFrameRaw::new(1, 2, 3, 4, 5, 6, 7))
        .unwrap();

    let closed = agent.close().unwrap();
    assert_eq!(count_uhid_inputs(&closed.transport.bytes), 1);
    assert_eq!(closed.reader.position(), 9);
}

#[test]
fn agent_session_reads_native_and_ai_device_events() {
    let session =
        HidSession::open(MockTransport::new(), OpenRequest::gamepad_only_realtime()).unwrap();
    let mut stream = Vec::new();
    stream.extend(ack(42));
    stream.extend(frame_summary_envelope(7));
    stream.extend(ai_stats_envelope());
    let mut agent = AgentControlSession::from_parts(session, Cursor::new(stream)).unwrap();

    assert_eq!(
        agent.recv_device_event().unwrap(),
        DeviceEvent::Native(DeviceMessage::AckClipboard { sequence: 42 })
    );
    match agent.recv_device_event().unwrap() {
        DeviceEvent::FrameSummary(summary) => {
            assert_eq!(summary.frame_seq, 7);
            assert_eq!(summary.objects[0].class_id, 7);
        }
        other => panic!("expected frame summary, got {other:?}"),
    }
    match agent.recv_device_event().unwrap() {
        DeviceEvent::AiStats(stats) => assert_eq!(stats.frames_sampled, 10),
        other => panic!("expected ai stats, got {other:?}"),
    }

    let _closed = agent.close().unwrap();
}

#[test]
fn agent_wait_helpers_skip_unrelated_ai_and_native_events() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let mut stream = Vec::new();
    stream.extend(frame_summary_envelope(1));
    stream.extend(ack(9));
    stream.extend(clipboard("ok"));
    stream.extend(frame_summary_envelope(2));
    stream.extend(ai_stats_envelope());
    let mut agent = AgentControlSession::from_parts(session, Cursor::new(stream)).unwrap();

    agent.wait_for_clipboard_ack(9).unwrap();
    assert_eq!(agent.wait_for_clipboard().unwrap(), "ok");
    assert_eq!(agent.wait_for_frame_summary().unwrap().frame_seq, 2);
    assert_eq!(agent.wait_for_ai_stats().unwrap().frames_sampled, 10);

    let _closed = agent.close().unwrap();
}

#[test]
fn agent_waits_for_frame_predicates_scene_motion_and_stability() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let motion = [crate::ai::MotionVector {
        x: 500,
        y: 900,
        dx: 3,
        dy: -2,
    }];
    let mut stream = Vec::new();
    stream.extend(frame_summary_envelope_full(1, 0, &[], &[], &[]));
    stream.extend(frame_summary_envelope_full(
        2,
        crate::ai::FLAG_SCENE_CHANGE,
        &[],
        &[],
        &[],
    ));
    stream.extend(frame_summary_envelope_full(3, 0, &[], &[], &[]));
    stream.extend(frame_summary_envelope_full(
        4,
        crate::ai::FLAG_MOTION,
        &motion,
        &[],
        &[],
    ));
    stream.extend(frame_summary_envelope_full(
        5,
        crate::ai::FLAG_MOTION,
        &motion,
        &[],
        &[],
    ));
    stream.extend(frame_summary_envelope_full(6, 0, &[], &[], &[]));
    stream.extend(frame_summary_envelope_full(7, 0, &[], &[], &[]));
    stream.extend(frame_summary_envelope_full(8, 0, &[], &[], &[]));
    let mut agent = AgentControlSession::from_parts(session, Cursor::new(stream)).unwrap();

    assert_eq!(agent.wait_for_scene_change().unwrap().frame_seq, 2);
    assert_eq!(
        agent
            .wait_for_frame_summary_matching(|summary| summary.frame_seq >= 4)
            .unwrap()
            .frame_seq,
        4
    );
    assert_eq!(agent.wait_for_motion().unwrap().frame_seq, 5);
    assert_eq!(agent.wait_for_stable_frames(2).unwrap().frame_seq, 7);
    assert_eq!(agent.wait_for_stable_frame().unwrap().frame_seq, 8);
    assert!(matches!(
        agent.wait_for_stable_frames(0),
        Err(Error::SessionLifecycle(
            "stable frame count must be nonzero"
        ))
    ));

    let _closed = agent.close().unwrap();
}

#[test]
fn agent_frame_wait_limits_bound_observed_summaries() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let motion = [crate::ai::MotionVector {
        x: 500,
        y: 900,
        dx: 3,
        dy: -2,
    }];
    let mut stream = Vec::new();
    stream.extend(ack(3));
    stream.extend(frame_summary_envelope_full(1, 0, &[], &[], &[]));
    stream.extend(frame_summary_envelope_full(
        2,
        crate::ai::FLAG_SCENE_CHANGE,
        &[],
        &[],
        &[],
    ));
    stream.extend(frame_summary_envelope_full(
        3,
        crate::ai::FLAG_MOTION,
        &motion,
        &[],
        &[],
    ));
    stream.extend(frame_summary_envelope_full(4, 0, &[], &[], &[]));
    stream.extend(frame_summary_envelope_full(5, 0, &[], &[], &[]));
    stream.extend(frame_summary_envelope_full(6, 0, &[], &[], &[]));
    let mut agent = AgentControlSession::from_parts(session, Cursor::new(stream)).unwrap();

    assert!(agent.wait_for_scene_change_with_limit(0).unwrap().is_none());
    assert_eq!(agent.receiver_mut().unwrap().get_ref().position(), 0);
    assert!(agent.wait_for_scene_change_with_limit(1).unwrap().is_none());
    assert_eq!(
        agent
            .wait_for_scene_change_with_limit(1)
            .unwrap()
            .unwrap()
            .frame_seq,
        2
    );
    assert_eq!(
        agent
            .wait_for_frame_summary_matching_with_limit(1, FrameSummary::is_moving)
            .unwrap()
            .unwrap()
            .frame_seq,
        3
    );
    assert!(agent
        .wait_for_stable_frames_with_limit(2, 1)
        .unwrap()
        .is_none());
    assert_eq!(
        agent
            .wait_for_stable_frames_with_limit(2, 2)
            .unwrap()
            .unwrap()
            .frame_seq,
        6
    );

    let _closed = agent.close().unwrap();
}

#[test]
fn agent_fresh_frame_waits_skip_stale_seq_and_timestamp() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let mut stream = Vec::new();
    stream.extend(frame_summary_envelope_full_at(100, 1, 0, &[], &[], &[]));
    stream.extend(frame_summary_envelope_full_at(120, 2, 0, &[], &[], &[]));
    stream.extend(frame_summary_envelope_full_at(200, 3, 0, &[], &[], &[]));
    stream.extend(frame_summary_envelope_full_at(250, 4, 0, &[], &[], &[]));
    let mut agent = AgentControlSession::from_parts(session, Cursor::new(stream)).unwrap();

    assert!(agent
        .wait_for_frame_summary_after_seq_with_limit(2, 2)
        .unwrap()
        .is_none());
    assert_eq!(
        agent
            .wait_for_frame_summary_after_seq_with_limit(2, 1)
            .unwrap()
            .unwrap()
            .frame_seq,
        3
    );
    let summary = agent.wait_for_frame_summary_after_timestamp(200).unwrap();
    assert_eq!(summary.frame_seq, 4);
    assert_eq!(summary.timestamp_ms, 250);

    let _closed = agent.close().unwrap();
}

#[test]
fn agent_run_actions_and_wait_for_stable_frames_flushes_then_reads() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let motion = [crate::ai::MotionVector {
        x: 10,
        y: 20,
        dx: 1,
        dy: -1,
    }];
    let mut stream = Vec::new();
    stream.extend(frame_summary_envelope_full(
        1,
        crate::ai::FLAG_MOTION,
        &motion,
        &[],
        &[],
    ));
    stream.extend(frame_summary_envelope_full(2, 0, &[], &[], &[]));
    stream.extend(frame_summary_envelope_full(3, 0, &[], &[], &[]));
    let mut agent = AgentControlSession::from_parts(session, Cursor::new(stream)).unwrap();

    let summary = agent
        .run_actions_and_wait_for_stable_frames(&[AgentAction::tap(10, 20)], 2)
        .unwrap();

    assert_eq!(summary.frame_seq, 3);
    let closed = agent.close().unwrap();
    assert_eq!(count_touch_events(&closed.transport.bytes), 2);
}

#[test]
fn agent_run_actions_and_wait_for_fresh_frame_flushes_then_reads() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let mut stream = Vec::new();
    stream.extend(frame_summary_envelope_full_at(100, 9, 0, &[], &[], &[]));
    stream.extend(frame_summary_envelope_full_at(120, 11, 0, &[], &[], &[]));
    let mut agent = AgentControlSession::from_parts(session, Cursor::new(stream)).unwrap();

    let summary = agent
        .run_actions_and_wait_for_frame_summary_after_seq(&[AgentAction::tap(10, 20)], 10)
        .unwrap();

    assert_eq!(summary.frame_seq, 11);
    let closed = agent.close().unwrap();
    assert_eq!(count_touch_events(&closed.transport.bytes), 2);
}

#[test]
fn agent_detaches_latest_frame_receiver_and_keeps_command_path() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let mut stream = Vec::new();
    stream.extend(ack(7));
    stream.extend(frame_summary_envelope_full_at(100, 1, 0, &[], &[], &[]));
    stream.extend(ai_stats_envelope());
    stream.extend(frame_summary_envelope_full_at(160, 2, 0, &[], &[], &[]));
    let stream_len = stream.len() as u64;
    let mut agent = AgentControlSession::from_parts(session, Cursor::new(stream)).unwrap();

    let (latest, pump) = agent.detach_latest_frame_summary_receiver().unwrap();
    agent.run_actions(&[AgentAction::tap(10, 20)]).unwrap();

    let reader = pump.join().unwrap();
    assert_eq!(reader.position(), stream_len);
    let snapshot = latest.snapshot().unwrap();
    assert_eq!(snapshot.version, 2);
    assert_eq!(snapshot.summary.frame_seq, 2);
    assert_eq!(snapshot.summary.timestamp_ms, 160);
    assert!(matches!(
        agent.wait_for_frame_summary().unwrap_err(),
        Error::Transport(_)
    ));

    let report = agent.close_transport_checked().unwrap();
    report.command_result.unwrap();
    assert_eq!(count_touch_events(&report.transport.bytes), 2);
}

#[test]
fn agent_run_actions_and_wait_for_next_latest_frame_uses_post_barrier_version() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (go_tx, go_rx) = mpsc::channel();
    let server = std::thread::spawn(move || {
        let (mut sock, _addr) = listener.accept().unwrap();
        Write::write_all(
            &mut sock,
            &frame_summary_envelope_full_at(100, 1, 0, &[], &[], &[]),
        )
        .unwrap();
        go_rx.recv().unwrap();
        std::thread::sleep(Duration::from_millis(80));
        Write::write_all(
            &mut sock,
            &frame_summary_envelope_full_at(180, 2, 0, &[], &[], &[]),
        )
        .unwrap();
    });

    let reader = TcpStream::connect(addr).unwrap();
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let mut agent = AgentControlSession::from_parts(session, reader).unwrap();
    let (latest, pump) = agent.detach_latest_frame_summary_receiver().unwrap();
    assert_eq!(latest.wait_first().unwrap().summary.frame_seq, 1);

    go_tx.send(()).unwrap();
    let snapshot = agent
        .run_actions_and_wait_for_next_latest_frame_after_seq(
            &[AgentAction::tap(10, 20)],
            &latest,
            1,
        )
        .unwrap();

    assert_eq!(snapshot.summary.frame_seq, 2);
    assert_eq!(snapshot.summary.timestamp_ms, 180);
    let report = agent.close_transport_checked().unwrap();
    report.command_result.unwrap();
    assert_eq!(count_touch_events(&report.transport.bytes), 2);
    pump.join().unwrap();
    server.join().unwrap();
}

#[test]
fn agent_run_actions_and_wait_for_next_latest_frame_timeout_bounds_wait() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (go_tx, go_rx) = mpsc::channel();
    let server = std::thread::spawn(move || {
        let (mut sock, _addr) = listener.accept().unwrap();
        Write::write_all(
            &mut sock,
            &frame_summary_envelope_full_at(100, 1, 0, &[], &[], &[]),
        )
        .unwrap();
        go_rx.recv().unwrap();
        std::thread::sleep(Duration::from_millis(80));
        Write::write_all(
            &mut sock,
            &frame_summary_envelope_full_at(180, 2, 0, &[], &[], &[]),
        )
        .unwrap();
    });

    let reader = TcpStream::connect(addr).unwrap();
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let mut agent = AgentControlSession::from_parts(session, reader).unwrap();
    let (latest, pump) = agent.detach_latest_frame_summary_receiver().unwrap();
    assert_eq!(latest.wait_first().unwrap().summary.frame_seq, 1);

    go_tx.send(()).unwrap();
    assert!(matches!(
        agent
            .run_actions_and_wait_for_next_latest_frame_after_seq_timeout(
                &[AgentAction::tap(10, 20)],
                &latest,
                1,
                Duration::from_millis(5),
            )
            .unwrap_err(),
        Error::AgentTimeout("latest frame summary")
    ));

    let report = agent.close_transport_checked().unwrap();
    report.command_result.unwrap();
    assert_eq!(count_touch_events(&report.transport.bytes), 2);
    pump.join().unwrap();
    server.join().unwrap();
}

#[test]
fn agent_try_run_actions_and_wait_for_next_latest_frame_after_version_uses_cached_boundary() {
    let mut stream = Vec::new();
    stream.extend(frame_summary_envelope_full_at(100, 1, 0, &[], &[], &[]));
    stream.extend(frame_summary_envelope_full_at(180, 2, 0, &[], &[], &[]));
    let (latest, pump) = spawn_latest_frame_summary_receiver(Cursor::new(stream)).unwrap();
    pump.join().unwrap();
    assert_eq!(latest.version(), 2);
    let prior_observation = LatestFrameSummaryObservation::at_version(1);

    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
    let snapshot = agent
        .try_run_actions_and_wait_for_next_latest_frame_after_observation(
            &[AgentAction::tap(10, 20)],
            &latest,
            &prior_observation,
        )
        .unwrap();

    assert_eq!(snapshot.version, 2);
    assert_eq!(snapshot.summary.frame_seq, 2);
    assert_eq!(snapshot.summary.timestamp_ms, 180);
    let closed = agent.close().unwrap();
    assert_eq!(count_touch_events(&closed.transport.bytes), 2);

    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
    let snapshot = agent
        .try_run_actions_and_wait_for_next_latest_frame_matching_after_observation_timeout(
            &[AgentAction::tap(12, 24)],
            &latest,
            &prior_observation,
            Duration::from_secs(1),
            |summary| summary.frame_seq == 2,
        )
        .unwrap();
    assert_eq!(snapshot.version, 2);
    assert_eq!(snapshot.summary.frame_seq, 2);
    let closed = agent.close().unwrap();
    assert_eq!(count_touch_events(&closed.transport.bytes), 2);
}

#[test]
fn agent_try_run_actions_and_wait_for_next_latest_frame_timeout_bounds_wait() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
    let latest = LatestFrameSummaryReceiver::default();

    let err = agent
        .try_run_actions_and_wait_for_next_latest_frame_after_seq_timeout(
            &[AgentAction::tap(10, 20)],
            &latest,
            0,
            Duration::from_millis(1),
        )
        .unwrap_err();

    assert!(matches!(err, Error::AgentTimeout("latest frame summary")));
    let closed = agent.close().unwrap();
    assert_eq!(count_touch_events(&closed.transport.bytes), 2);
}

#[test]
fn agent_try_run_actions_and_wait_for_next_latest_frame_preflights_without_dispatch() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent =
        AgentControlSession::from_parts_with_bound(session, Cursor::new(Vec::new()), 1).unwrap();
    let latest = LatestFrameSummaryReceiver::default();

    let err = agent
        .try_run_actions_and_wait_for_next_latest_frame_timeout(
            &[AgentAction::tap(10, 20)],
            &latest,
            Duration::from_millis(1),
        )
        .unwrap_err();

    assert!(matches!(
        err,
        Error::SessionLifecycle(TRY_RUN_EXCEEDS_COMMAND_BOUND)
    ));
    let closed = agent.close().unwrap();
    assert!(closed.transport.bytes.is_empty());
}

#[test]
fn agent_try_run_actions_and_wait_for_next_latest_target_after_observation_selects_cached_target() {
    let mut stream = Vec::new();
    stream.extend(frame_summary_envelope_full_at(
        100,
        1,
        crate::ai::FLAG_OBJECTS,
        &[],
        &[ObjectBox {
            x: 100,
            y: 200,
            w: 301,
            h: 101,
            class_id: 7,
            confidence: 230,
        }],
        &[],
    ));
    let (latest, pump) = spawn_latest_frame_summary_receiver(Cursor::new(stream)).unwrap();
    pump.join().unwrap();
    let prior_observation = LatestFrameSummaryObservation::at_version(0);

    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
    let rect = agent
        .try_run_actions_and_wait_for_next_latest_target_rect_after_observation(
            &[AgentAction::tap(10, 20)],
            &latest,
            AgentTargetSelector::object_class_min_confidence(7, 220),
            &prior_observation,
        )
        .unwrap();

    assert_eq!(rect.to_pixels(1000, 2000), (100, 200, 400, 300));
    let closed = agent.close().unwrap();
    assert_eq!(count_touch_events(&closed.transport.bytes), 2);
}

#[test]
fn agent_try_run_actions_and_tap_next_latest_target_at_pointer_timeout_taps_target() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (go_tx, go_rx) = mpsc::channel();
    let server = std::thread::spawn(move || {
        let (mut sock, _addr) = listener.accept().unwrap();
        go_rx.recv().unwrap();
        std::thread::sleep(Duration::from_millis(80));
        Write::write_all(
            &mut sock,
            &frame_summary_envelope_full_at(
                100,
                1,
                crate::ai::FLAG_OBJECTS,
                &[],
                &[ObjectBox {
                    x: 100,
                    y: 200,
                    w: 301,
                    h: 101,
                    class_id: 7,
                    confidence: 230,
                }],
                &[],
            ),
        )
        .unwrap();
    });

    let reader = TcpStream::connect(addr).unwrap();
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let mut agent = AgentControlSession::from_parts(session, reader).unwrap();
    agent.set_screen_size(1000, 2000).unwrap();
    let (latest, pump) = agent.detach_latest_frame_summary_receiver().unwrap();
    let pointer = TouchPointerId::VIRTUAL_FINGER;

    go_tx.send(()).unwrap();
    let rect = agent
        .try_run_actions_and_tap_next_latest_target_at_pointer_timeout(
            &[AgentAction::tap(10, 20)],
            &latest,
            AgentTargetSelector::object_class_min_confidence(7, 220),
            pointer,
            (2_500, 7_500),
            Duration::from_secs(1),
        )
        .unwrap();

    assert_eq!(rect.to_pixels(1000, 2000), (100, 200, 400, 300));
    let report = agent.close_transport_checked().unwrap();
    report.command_result.unwrap();
    let events = touch_events(&report.transport.bytes);
    assert_eq!(events.len(), 4);
    assert_eq!(events[0], (TouchAction::DOWN.value(), 0, 10, 20));
    assert_eq!(events[1], (TouchAction::UP.value(), 0, 10, 20));
    assert_eq!(
        events[2],
        (TouchAction::DOWN.value(), pointer.value(), 175, 275)
    );
    assert_eq!(
        events[3],
        (TouchAction::UP.value(), pointer.value(), 175, 275)
    );
    pump.join().unwrap();
    server.join().unwrap();
}

#[test]
fn agent_try_run_actions_and_wait_for_next_latest_target_preflights_without_dispatch() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent =
        AgentControlSession::from_parts_with_bound(session, Cursor::new(Vec::new()), 1).unwrap();
    let latest = LatestFrameSummaryReceiver::default();

    let err = agent
        .try_run_actions_and_wait_for_next_latest_target_rect_timeout(
            &[AgentAction::tap(10, 20)],
            &latest,
            AgentTargetSelector::best_object(),
            Duration::from_millis(1),
        )
        .unwrap_err();

    assert!(matches!(
        err,
        Error::SessionLifecycle(TRY_RUN_EXCEEDS_COMMAND_BOUND)
    ));
    let closed = agent.close().unwrap();
    assert!(closed.transport.bytes.is_empty());
}

#[test]
fn agent_run_actions_and_wait_for_next_latest_frame_after_version_uses_cached_boundary() {
    let mut stream = Vec::new();
    stream.extend(frame_summary_envelope_full_at(100, 1, 0, &[], &[], &[]));
    stream.extend(frame_summary_envelope_full_at(180, 2, 0, &[], &[], &[]));
    let (latest, pump) = spawn_latest_frame_summary_receiver(Cursor::new(stream)).unwrap();
    pump.join().unwrap();
    assert_eq!(latest.version(), 2);
    let observation = latest.observe();
    assert!(observation.has_snapshot());
    assert_eq!(observation.boundary_version(), 2);
    assert_eq!(observation.summary().unwrap().frame_seq, 2);
    let prior_observation = LatestFrameSummaryObservation::at_version(1);
    assert!(prior_observation.accepts(observation.snapshot().unwrap()));

    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
    let snapshot = agent
        .run_actions_and_wait_for_next_latest_frame_after_version(
            &[AgentAction::tap(10, 20)],
            &latest,
            1,
        )
        .unwrap();

    assert_eq!(snapshot.version, 2);
    assert_eq!(snapshot.summary.frame_seq, 2);
    assert_eq!(snapshot.summary.timestamp_ms, 180);
    let closed = agent.close().unwrap();
    assert_eq!(count_touch_events(&closed.transport.bytes), 2);

    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
    let snapshot = agent
        .run_actions_and_wait_for_next_latest_frame_matching_after_observation_timeout(
            &[AgentAction::tap(12, 24)],
            &latest,
            &prior_observation,
            Duration::from_secs(1),
            |summary| summary.frame_seq == 2,
        )
        .unwrap();
    assert_eq!(snapshot.version, 2);
    assert_eq!(snapshot.summary.frame_seq, 2);
    let closed = agent.close().unwrap();
    assert_eq!(count_touch_events(&closed.transport.bytes), 2);

    let mut stream = Vec::new();
    stream.extend(frame_summary_envelope_full_at(100, 1, 0, &[], &[], &[]));
    stream.extend(frame_summary_envelope_full_at(
        180,
        2,
        crate::ai::FLAG_OBJECTS,
        &[],
        &[ObjectBox {
            x: 100,
            y: 200,
            w: 301,
            h: 101,
            class_id: 7,
            confidence: 230,
        }],
        &[],
    ));
    let (latest, pump) = spawn_latest_frame_summary_receiver(Cursor::new(stream)).unwrap();
    pump.join().unwrap();

    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
    let rect = agent
        .run_actions_and_wait_for_next_latest_target_rect_after_observation(
            &[AgentAction::tap(10, 20)],
            &latest,
            AgentTargetSelector::object_class_min_confidence(7, 220),
            &prior_observation,
        )
        .unwrap();
    assert_eq!(rect.to_pixels(1000, 2000), (100, 200, 400, 300));
    let closed = agent.close().unwrap();
    assert_eq!(count_touch_events(&closed.transport.bytes), 2);

    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
    let rect = agent
        .run_actions_and_tap_next_latest_target_after_observation_timeout(
            &[AgentAction::tap(30, 40)],
            &latest,
            AgentTargetSelector::object_class_min_confidence(7, 220),
            &prior_observation,
            Duration::from_secs(1),
        )
        .unwrap();
    assert_eq!(rect.to_pixels(1000, 2000), (100, 200, 400, 300));
    let closed = agent.close().unwrap();
    let events = touch_events(&closed.transport.bytes);
    assert_eq!(events.len(), 4);
    assert_eq!(events[0], (TouchAction::DOWN.value(), 0, 30, 40));
    assert_eq!(events[1], (TouchAction::UP.value(), 0, 30, 40));
    assert_eq!(events[2], (TouchAction::DOWN.value(), 0, 270, 240));
    assert_eq!(events[3], (TouchAction::UP.value(), 0, 270, 240));

    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
    let empty_latest = LatestFrameSummaryReceiver::default();
    let err = agent
        .run_actions_and_wait_for_next_latest_frame_after_version_timeout(
            &[AgentAction::tap(30, 40)],
            &empty_latest,
            0,
            Duration::from_millis(1),
        )
        .unwrap_err();
    assert!(matches!(err, Error::AgentTimeout("latest frame summary")));
    let closed = agent.close().unwrap();
    assert_eq!(count_touch_events(&closed.transport.bytes), 2);
}

#[test]
fn agent_run_actions_and_tap_next_latest_targets_waits_then_taps() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (go_object_tx, go_object_rx) = mpsc::channel();
    let (go_text_tx, go_text_rx) = mpsc::channel();
    let server = std::thread::spawn(move || {
        let (mut sock, _addr) = listener.accept().unwrap();
        Write::write_all(
            &mut sock,
            &frame_summary_envelope_full_at(100, 1, 0, &[], &[], &[]),
        )
        .unwrap();
        go_object_rx.recv().unwrap();
        std::thread::sleep(Duration::from_millis(80));
        Write::write_all(
            &mut sock,
            &frame_summary_envelope_full_at(
                180,
                2,
                crate::ai::FLAG_OBJECTS,
                &[],
                &[
                    ObjectBox {
                        x: 10,
                        y: 20,
                        w: 11,
                        h: 21,
                        class_id: 3,
                        confidence: 210,
                    },
                    ObjectBox {
                        x: 100,
                        y: 200,
                        w: 301,
                        h: 101,
                        class_id: 7,
                        confidence: 230,
                    },
                ],
                &[],
            ),
        )
        .unwrap();
        go_text_rx.recv().unwrap();
        std::thread::sleep(Duration::from_millis(80));
        Write::write_all(
            &mut sock,
            &frame_summary_envelope_full_at(
                260,
                3,
                crate::ai::FLAG_TEXT,
                &[],
                &[],
                &[
                    TextRegion {
                        x: 100,
                        y: 200,
                        w: 11,
                        h: 11,
                    },
                    TextRegion {
                        x: 700,
                        y: 800,
                        w: 101,
                        h: 101,
                    },
                ],
            ),
        )
        .unwrap();
    });

    let reader = TcpStream::connect(addr).unwrap();
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let mut agent = AgentControlSession::from_parts(session, reader).unwrap();
    agent.set_screen_size(1000, 2000).unwrap();
    let (latest, pump) = agent.detach_latest_frame_summary_receiver().unwrap();
    assert_eq!(latest.wait_first().unwrap().summary.frame_seq, 1);

    let pointer = TouchPointerId::VIRTUAL_FINGER;
    go_object_tx.send(()).unwrap();
    let object = agent
        .run_actions_and_tap_next_latest_target_at_pointer_timeout(
            &[AgentAction::tap(10, 20)],
            &latest,
            AgentTargetSelector::object_class_min_confidence(7, 220),
            pointer,
            (2_500, 7_500),
            Duration::from_secs(1),
        )
        .unwrap();
    go_text_tx.send(()).unwrap();
    let text = agent
        .run_actions_and_tap_next_latest_target_timeout(
            &[AgentAction::tap(30, 40)],
            &latest,
            AgentTargetSelector::largest_text_region(),
            Duration::from_secs(1),
        )
        .unwrap();

    assert_eq!(object.to_pixels(1000, 2000), (100, 200, 400, 300));
    assert_eq!(text.to_pixels(1000, 2000), (700, 800, 800, 900));
    let report = agent.close_transport_checked().unwrap();
    report.command_result.unwrap();
    let events = touch_events(&report.transport.bytes);
    assert_eq!(events.len(), 8);
    assert_eq!(events[0], (TouchAction::DOWN.value(), 0, 10, 20));
    assert_eq!(events[1], (TouchAction::UP.value(), 0, 10, 20));
    assert_eq!(
        events[2],
        (TouchAction::DOWN.value(), pointer.value(), 175, 275)
    );
    assert_eq!(
        events[3],
        (TouchAction::UP.value(), pointer.value(), 175, 275)
    );
    assert_eq!(events[4], (TouchAction::DOWN.value(), 0, 30, 40));
    assert_eq!(events[5], (TouchAction::UP.value(), 0, 30, 40));
    assert_eq!(events[6], (TouchAction::DOWN.value(), 0, 750, 850));
    assert_eq!(events[7], (TouchAction::UP.value(), 0, 750, 850));
    pump.join().unwrap();
    server.join().unwrap();
}

#[test]
fn agent_run_actions_and_wait_for_target_with_limit_flushes_then_reads() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let mut stream = Vec::new();
    stream.extend(frame_summary_envelope_with(
        1,
        &[ObjectBox {
            x: 10,
            y: 20,
            w: 11,
            h: 21,
            class_id: 1,
            confidence: 255,
        }],
        &[],
    ));
    stream.extend(frame_summary_envelope_with(
        2,
        &[ObjectBox {
            x: 100,
            y: 200,
            w: 301,
            h: 101,
            class_id: 6,
            confidence: 230,
        }],
        &[],
    ));
    let mut agent = AgentControlSession::from_parts(session, Cursor::new(stream)).unwrap();

    let missed = agent
        .run_actions_and_wait_for_object_selector_rect_with_limit(
            &[AgentAction::tap(10, 20)],
            AgentObjectSelector::class_min_confidence(6, 220),
            1,
        )
        .unwrap();
    assert!(missed.is_none());
    let rect = agent
        .wait_for_object_selector_rect_with_limit(
            AgentObjectSelector::class_min_confidence(6, 220),
            1,
        )
        .unwrap()
        .unwrap();

    assert_eq!(rect.to_pixels(1000, 2000), (100, 200, 400, 300));
    let closed = agent.close().unwrap();
    assert_eq!(count_touch_events(&closed.transport.bytes), 2);
}

#[test]
fn agent_target_selector_ordered_wait_and_taps_cover_generic_api() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let mut stream = Vec::new();
    stream.extend(frame_summary_envelope_with(1, &[], &[]));
    stream.extend(frame_summary_envelope_with(
        2,
        &[ObjectBox {
            x: 100,
            y: 200,
            w: 301,
            h: 101,
            class_id: 9,
            confidence: 230,
        }],
        &[],
    ));
    stream.extend(frame_summary_envelope_with(
        3,
        &[],
        &[TextRegion {
            x: 700,
            y: 800,
            w: 101,
            h: 101,
        }],
    ));
    stream.extend(frame_summary_envelope_with(
        4,
        &[],
        &[
            TextRegion {
                x: 10,
                y: 20,
                w: 11,
                h: 11,
            },
            TextRegion {
                x: 100,
                y: 200,
                w: 301,
                h: 101,
            },
        ],
    ));
    stream.extend(frame_summary_envelope_with(
        5,
        &[ObjectBox {
            x: 300,
            y: 400,
            w: 201,
            h: 101,
            class_id: 3,
            confidence: 240,
        }],
        &[],
    ));
    let mut agent = AgentControlSession::from_parts(session, Cursor::new(stream)).unwrap();
    let pointer = TouchPointerId::VIRTUAL_FINGER;
    agent.set_screen_size(1000, 2000).unwrap();

    let missed = agent
        .run_actions_and_wait_for_target_rect_with_limit(
            &[AgentAction::tap(1, 2)],
            AgentTargetSelector::object_class_min_confidence(9, 220),
            1,
        )
        .unwrap();
    assert!(missed.is_none());
    let object = agent
        .wait_for_target_rect(AgentTargetSelector::object_class_min_confidence(9, 220))
        .unwrap();
    let text = agent
        .tap_next_target_at_pointer_with_limit(
            AgentTargetSelector::text_region(0),
            pointer,
            10_000,
            0,
            1,
        )
        .unwrap()
        .unwrap();
    let largest_text = agent
        .run_actions_and_tap_next_target_at_with_limit(
            &[AgentAction::tap(30, 40)],
            AgentTargetSelector::largest_text_region(),
            0,
            10_000,
            1,
        )
        .unwrap()
        .unwrap();
    let best = agent
        .run_actions_and_tap_next_target(
            &[AgentAction::tap(50, 60)],
            AgentTargetSelector::best_object(),
        )
        .unwrap();

    assert_eq!(object.to_pixels(1000, 2000), (100, 200, 400, 300));
    assert_eq!(text.to_pixels(1000, 2000), (700, 800, 800, 900));
    assert_eq!(largest_text.to_pixels(1000, 2000), (100, 200, 400, 300));
    assert_eq!(best.to_pixels(1000, 2000), (300, 400, 500, 500));

    let closed = agent.close().unwrap();
    let events = touch_events(&closed.transport.bytes);
    assert_eq!(events.len(), 12);
    assert_eq!(events[0], (TouchAction::DOWN.value(), 0, 1, 2));
    assert_eq!(events[1], (TouchAction::UP.value(), 0, 1, 2));
    assert_eq!(
        events[2],
        (TouchAction::DOWN.value(), pointer.value(), 800, 800)
    );
    assert_eq!(
        events[3],
        (TouchAction::UP.value(), pointer.value(), 800, 800)
    );
    assert_eq!(events[4], (TouchAction::DOWN.value(), 0, 30, 40));
    assert_eq!(events[5], (TouchAction::UP.value(), 0, 30, 40));
    assert_eq!(events[6], (TouchAction::DOWN.value(), 0, 100, 300));
    assert_eq!(events[7], (TouchAction::UP.value(), 0, 100, 300));
    assert_eq!(events[8], (TouchAction::DOWN.value(), 0, 50, 60));
    assert_eq!(events[9], (TouchAction::UP.value(), 0, 50, 60));
    assert_eq!(events[10], (TouchAction::DOWN.value(), 0, 400, 450));
    assert_eq!(events[11], (TouchAction::UP.value(), 0, 400, 450));
}

#[test]
fn agent_bounded_target_taps_cover_object_best_class_and_text_families() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let mut stream = Vec::new();
    stream.extend(frame_summary_envelope_with(
        1,
        &[ObjectBox {
            x: 10,
            y: 20,
            w: 30,
            h: 40,
            class_id: 1,
            confidence: 255,
        }],
        &[],
    ));
    stream.extend(frame_summary_envelope_with(
        2,
        &[
            ObjectBox {
                x: 10,
                y: 20,
                w: 30,
                h: 40,
                class_id: 1,
                confidence: 255,
            },
            ObjectBox {
                x: 100,
                y: 200,
                w: 301,
                h: 101,
                class_id: 5,
                confidence: 220,
            },
        ],
        &[],
    ));
    stream.extend(frame_summary_envelope_with(
        3,
        &[
            ObjectBox {
                x: 10,
                y: 20,
                w: 30,
                h: 40,
                class_id: 2,
                confidence: 100,
            },
            ObjectBox {
                x: 300,
                y: 400,
                w: 201,
                h: 101,
                class_id: 6,
                confidence: 250,
            },
        ],
        &[],
    ));
    stream.extend(frame_summary_envelope_with(
        4,
        &[
            ObjectBox {
                x: 1,
                y: 1,
                w: 10,
                h: 10,
                class_id: 1,
                confidence: 255,
            },
            ObjectBox {
                x: 50,
                y: 60,
                w: 101,
                h: 201,
                class_id: 7,
                confidence: 220,
            },
        ],
        &[],
    ));
    stream.extend(frame_summary_envelope_with(
        5,
        &[],
        &[
            TextRegion {
                x: 10,
                y: 20,
                w: 30,
                h: 40,
            },
            TextRegion {
                x: 500,
                y: 600,
                w: 101,
                h: 201,
            },
        ],
    ));
    let mut agent = AgentControlSession::from_parts(session, Cursor::new(stream)).unwrap();
    let pointer = TouchPointerId::finger(42);

    agent.set_screen_size(1000, 2000).unwrap();
    assert!(agent
        .tap_next_object_at_pointer_with_limit(1, pointer, 2_500, 7_500, 1)
        .unwrap()
        .is_none());
    let indexed = agent
        .tap_next_object_at_pointer_with_limit(1, pointer, 2_500, 7_500, 1)
        .unwrap()
        .unwrap();
    let best = agent.tap_next_best_object_with_limit(1).unwrap().unwrap();
    let class = agent
        .tap_next_object_class_pointer_with_limit(7, pointer, 1)
        .unwrap()
        .unwrap();
    let text = agent
        .tap_next_text_region_at_pointer_with_limit(1, pointer, 10_000, 0, 1)
        .unwrap()
        .unwrap();

    assert_eq!(indexed.to_pixels(1000, 2000), (100, 200, 400, 300));
    assert_eq!(best.to_pixels(1000, 2000), (300, 400, 500, 500));
    assert_eq!(class.to_pixels(1000, 2000), (50, 60, 150, 260));
    assert_eq!(text.to_pixels(1000, 2000), (500, 600, 600, 800));

    let closed = agent.close().unwrap();
    let events = touch_events(&closed.transport.bytes);
    assert_eq!(events.len(), 8);
    assert_eq!(
        events[0],
        (TouchAction::DOWN.value(), pointer.value(), 175, 275)
    );
    assert_eq!(
        events[1],
        (TouchAction::UP.value(), pointer.value(), 175, 275)
    );
    assert_eq!(events[2], (TouchAction::DOWN.value(), 0, 400, 450));
    assert_eq!(events[3], (TouchAction::UP.value(), 0, 400, 450));
    assert_eq!(
        events[4],
        (TouchAction::DOWN.value(), pointer.value(), 100, 160)
    );
    assert_eq!(
        events[5],
        (TouchAction::UP.value(), pointer.value(), 100, 160)
    );
    assert_eq!(
        events[6],
        (TouchAction::DOWN.value(), pointer.value(), 600, 600)
    );
    assert_eq!(
        events[7],
        (TouchAction::UP.value(), pointer.value(), 600, 600)
    );
}

#[test]
fn agent_run_actions_and_tap_next_text_region_with_limit_flushes_then_taps_target() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let mut stream = Vec::new();
    stream.extend(frame_summary_envelope_with(1, &[], &[]));
    stream.extend(frame_summary_envelope_with(
        2,
        &[],
        &[TextRegion {
            x: 100,
            y: 200,
            w: 301,
            h: 101,
        }],
    ));
    let mut agent = AgentControlSession::from_parts(session, Cursor::new(stream)).unwrap();
    let pointer = TouchPointerId::finger(9);

    agent.set_screen_size(1000, 2000).unwrap();
    let rect = agent
        .run_actions_and_tap_next_text_region_at_pointer_with_limit(
            &[AgentAction::tap(10, 20)],
            0,
            pointer,
            10_000,
            0,
            2,
        )
        .unwrap()
        .unwrap();

    assert_eq!(rect.to_pixels(1000, 2000), (100, 200, 400, 300));
    let closed = agent.close().unwrap();
    let events = touch_events(&closed.transport.bytes);
    assert_eq!(events.len(), 4);
    assert_eq!(events[0], (TouchAction::DOWN.value(), 0, 10, 20));
    assert_eq!(events[1], (TouchAction::UP.value(), 0, 10, 20));
    assert_eq!(
        events[2],
        (TouchAction::DOWN.value(), pointer.value(), 400, 200)
    );
    assert_eq!(
        events[3],
        (TouchAction::UP.value(), pointer.value(), 400, 200)
    );
}

#[test]
fn agent_tap_next_object_selector_with_limit_skips_tap_on_miss() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let mut stream = Vec::new();
    stream.extend(frame_summary_envelope_with(
        1,
        &[ObjectBox {
            x: 10,
            y: 20,
            w: 11,
            h: 21,
            class_id: 1,
            confidence: 255,
        }],
        &[],
    ));
    stream.extend(frame_summary_envelope_with(
        2,
        &[ObjectBox {
            x: 100,
            y: 200,
            w: 301,
            h: 101,
            class_id: 6,
            confidence: 230,
        }],
        &[],
    ));
    let mut agent = AgentControlSession::from_parts(session, Cursor::new(stream)).unwrap();

    agent.set_screen_size(1000, 2000).unwrap();
    let missed = agent
        .tap_next_object_selector_at_pointer_with_limit(
            AgentObjectSelector::class_min_confidence(6, 220),
            TouchPointerId::VIRTUAL_FINGER,
            2_500,
            7_500,
            1,
        )
        .unwrap();
    assert!(missed.is_none());
    let rect = agent
        .tap_next_object_selector_at_pointer_with_limit(
            AgentObjectSelector::class_min_confidence(6, 220),
            TouchPointerId::VIRTUAL_FINGER,
            2_500,
            7_500,
            1,
        )
        .unwrap()
        .unwrap();

    assert_eq!(rect.to_pixels(1000, 2000), (100, 200, 400, 300));
    let closed = agent.close().unwrap();
    let events = touch_events(&closed.transport.bytes);
    assert_eq!(events.len(), 2);
    assert_eq!(
        events[0],
        (
            TouchAction::DOWN.value(),
            TouchPointerId::VIRTUAL_FINGER.value(),
            175,
            275
        )
    );
    assert_eq!(
        events[1],
        (
            TouchAction::UP.value(),
            TouchPointerId::VIRTUAL_FINGER.value(),
            175,
            275
        )
    );
}

#[test]
fn agent_run_actions_and_tap_next_largest_text_region_with_limit_taps_on_hit() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let mut stream = Vec::new();
    stream.extend(frame_summary_envelope_with(1, &[], &[]));
    stream.extend(frame_summary_envelope_with(
        2,
        &[],
        &[TextRegion {
            x: 100,
            y: 200,
            w: 301,
            h: 101,
        }],
    ));
    let mut agent = AgentControlSession::from_parts(session, Cursor::new(stream)).unwrap();

    agent.set_screen_size(1000, 2000).unwrap();
    let rect = agent
        .run_actions_and_tap_next_largest_text_region_at_with_limit(
            &[AgentAction::tap(10, 20)],
            0,
            10_000,
            2,
        )
        .unwrap()
        .unwrap();

    assert_eq!(rect.to_pixels(1000, 2000), (100, 200, 400, 300));
    let closed = agent.close().unwrap();
    let events = touch_events(&closed.transport.bytes);
    assert_eq!(events.len(), 4);
    assert_eq!(events[0], (TouchAction::DOWN.value(), 0, 10, 20));
    assert_eq!(events[1], (TouchAction::UP.value(), 0, 10, 20));
    assert_eq!(events[2], (TouchAction::DOWN.value(), 0, 100, 300));
    assert_eq!(events[3], (TouchAction::UP.value(), 0, 100, 300));
}

#[test]
fn agent_run_actions_and_wait_for_object_selector_rect_flushes_then_reads() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let mut stream = Vec::new();
    stream.extend(frame_summary_envelope_with(1, &[], &[]));
    stream.extend(frame_summary_envelope_with(
        2,
        &[ObjectBox {
            x: 120,
            y: 240,
            w: 101,
            h: 201,
            class_id: 8,
            confidence: 230,
        }],
        &[],
    ));
    let mut agent = AgentControlSession::from_parts(session, Cursor::new(stream)).unwrap();

    let rect = agent
        .run_actions_and_wait_for_object_selector_rect(
            &[AgentAction::tap(10, 20)],
            AgentObjectSelector::class_min_confidence(8, 220),
        )
        .unwrap();

    assert_eq!(rect.to_pixels(1000, 2000), (120, 240, 220, 440));
    assert_eq!(rect.center().to_pixels(1000, 2000), (170, 340));
    let closed = agent.close().unwrap();
    assert_eq!(count_touch_events(&closed.transport.bytes), 2);
}

#[test]
fn agent_waits_for_next_vision_targets_across_frames() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let mut stream = Vec::new();
    stream.extend(frame_summary_envelope_with(1, &[], &[]));
    stream.extend(frame_summary_envelope_with(
        2,
        &[
            ObjectBox {
                x: 100,
                y: 200,
                w: 301,
                h: 101,
                class_id: 7,
                confidence: 220,
            },
            ObjectBox {
                x: 500,
                y: 600,
                w: 11,
                h: 21,
                class_id: 3,
                confidence: 230,
            },
        ],
        &[],
    ));
    stream.extend(frame_summary_envelope_with(
        3,
        &[ObjectBox {
            x: 300,
            y: 400,
            w: 301,
            h: 101,
            class_id: 2,
            confidence: 210,
        }],
        &[],
    ));
    stream.extend(frame_summary_envelope_with(
        4,
        &[],
        &[
            TextRegion {
                x: 10,
                y: 20,
                w: 11,
                h: 21,
            },
            TextRegion {
                x: 700,
                y: 800,
                w: 101,
                h: 101,
            },
        ],
    ));
    let mut agent = AgentControlSession::from_parts(session, Cursor::new(stream)).unwrap();

    assert_eq!(
        agent
            .wait_for_best_object_rect()
            .unwrap()
            .center()
            .to_pixels(1000, 2000),
        (505, 610)
    );
    assert_eq!(
        agent
            .wait_for_best_object_class_rect(2)
            .unwrap()
            .center()
            .to_pixels(1000, 2000),
        (450, 450)
    );
    assert_eq!(
        agent
            .wait_for_largest_text_region_rect()
            .unwrap()
            .center()
            .to_pixels(1000, 2000),
        (750, 850)
    );

    let _closed = agent.close().unwrap();
}

#[test]
fn agent_waits_for_object_selector_across_frames() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let mut stream = Vec::new();
    stream.extend(frame_summary_envelope_with(
        1,
        &[ObjectBox {
            x: 100,
            y: 200,
            w: 101,
            h: 101,
            class_id: 2,
            confidence: 180,
        }],
        &[],
    ));
    stream.extend(frame_summary_envelope_with(
        2,
        &[ObjectBox {
            x: 500,
            y: 600,
            w: 11,
            h: 21,
            class_id: 3,
            confidence: 255,
        }],
        &[],
    ));
    stream.extend(ack(17));
    stream.extend(frame_summary_envelope_with(
        3,
        &[ObjectBox {
            x: 300,
            y: 400,
            w: 301,
            h: 101,
            class_id: 2,
            confidence: 230,
        }],
        &[],
    ));
    let mut agent = AgentControlSession::from_parts(session, Cursor::new(stream)).unwrap();

    let rect = agent
        .wait_for_object_selector_rect(AgentObjectSelector::class_min_confidence(2, 220))
        .unwrap();
    assert_eq!(rect.center().to_pixels(1000, 2000), (450, 450));

    let _closed = agent.close().unwrap();
}

#[test]
fn agent_tap_next_vision_targets_emit_touch_events() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let mut stream = Vec::new();
    stream.extend(frame_summary_envelope_with(
        1,
        &[ObjectBox {
            x: 100,
            y: 200,
            w: 301,
            h: 101,
            class_id: 7,
            confidence: 220,
        }],
        &[],
    ));
    stream.extend(frame_summary_envelope_with(
        2,
        &[],
        &[TextRegion {
            x: 700,
            y: 800,
            w: 101,
            h: 101,
        }],
    ));
    let mut agent = AgentControlSession::from_parts(session, Cursor::new(stream)).unwrap();

    agent.set_screen_size(1000, 2000).unwrap();
    let object = agent.tap_next_best_object().unwrap();
    let text = agent.tap_next_largest_text_region().unwrap();

    assert_eq!(object.center().to_pixels(1000, 2000), (250, 250));
    assert_eq!(text.center().to_pixels(1000, 2000), (750, 850));

    let closed = agent.close().unwrap();
    let events = touch_events(&closed.transport.bytes);
    assert_eq!(events.len(), 4);
    assert_eq!(events[0], (TouchAction::DOWN.value(), 0, 250, 250));
    assert_eq!(events[1], (TouchAction::UP.value(), 0, 250, 250));
    assert_eq!(events[2], (TouchAction::DOWN.value(), 0, 750, 850));
    assert_eq!(events[3], (TouchAction::UP.value(), 0, 750, 850));
}

#[test]
fn agent_tap_next_object_selector_emits_touch() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let mut stream = Vec::new();
    stream.extend(frame_summary_envelope_with(
        1,
        &[ObjectBox {
            x: 100,
            y: 200,
            w: 101,
            h: 101,
            class_id: 4,
            confidence: 219,
        }],
        &[],
    ));
    stream.extend(frame_summary_envelope_with(
        2,
        &[ObjectBox {
            x: 700,
            y: 800,
            w: 101,
            h: 101,
            class_id: 4,
            confidence: 220,
        }],
        &[],
    ));
    let mut agent = AgentControlSession::from_parts(session, Cursor::new(stream)).unwrap();

    agent.set_screen_size(1000, 2000).unwrap();
    let rect = agent
        .tap_next_object_selector(AgentObjectSelector::class_min_confidence(4, 220))
        .unwrap();

    assert_eq!(rect.center().to_pixels(1000, 2000), (750, 850));
    let closed = agent.close().unwrap();
    let events = touch_events(&closed.transport.bytes);
    assert_eq!(events.len(), 2);
    assert_eq!(events[0], (TouchAction::DOWN.value(), 0, 750, 850));
    assert_eq!(events[1], (TouchAction::UP.value(), 0, 750, 850));
}

#[test]
fn agent_run_actions_and_tap_next_object_selector_at_pointer_flushes_then_taps_target() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let mut stream = Vec::new();
    stream.extend(frame_summary_envelope_with(1, &[], &[]));
    stream.extend(frame_summary_envelope_with(
        2,
        &[ObjectBox {
            x: 100,
            y: 200,
            w: 301,
            h: 101,
            class_id: 6,
            confidence: 230,
        }],
        &[],
    ));
    let pointer = TouchPointerId::VIRTUAL_FINGER;
    let mut agent = AgentControlSession::from_parts(session, Cursor::new(stream)).unwrap();

    agent.set_screen_size(1000, 2000).unwrap();
    let rect = agent
        .run_actions_and_tap_next_object_selector_at_pointer(
            &[AgentAction::tap(10, 20)],
            AgentObjectSelector::class_min_confidence(6, 220),
            pointer,
            2_500,
            7_500,
        )
        .unwrap();

    assert_eq!(rect.to_pixels(1000, 2000), (100, 200, 400, 300));
    let closed = agent.close().unwrap();
    let events = touch_events(&closed.transport.bytes);
    assert_eq!(events.len(), 4);
    assert_eq!(events[0], (TouchAction::DOWN.value(), 0, 10, 20));
    assert_eq!(events[1], (TouchAction::UP.value(), 0, 10, 20));
    assert_eq!(
        events[2],
        (TouchAction::DOWN.value(), pointer.value(), 175, 275)
    );
    assert_eq!(
        events[3],
        (TouchAction::UP.value(), pointer.value(), 175, 275)
    );
}

#[test]
fn agent_run_actions_and_tap_next_object_class_at_flushes_then_taps_target() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let mut stream = Vec::new();
    stream.extend(frame_summary_envelope_with(
        1,
        &[ObjectBox {
            x: 10,
            y: 20,
            w: 11,
            h: 21,
            class_id: 8,
            confidence: 255,
        }],
        &[],
    ));
    stream.extend(frame_summary_envelope_with(
        2,
        &[ObjectBox {
            x: 100,
            y: 200,
            w: 301,
            h: 101,
            class_id: 9,
            confidence: 220,
        }],
        &[],
    ));
    let mut agent = AgentControlSession::from_parts(session, Cursor::new(stream)).unwrap();

    agent.set_screen_size(1000, 2000).unwrap();
    let rect = agent
        .run_actions_and_tap_next_object_class_at(&[AgentAction::tap(10, 20)], 9, 10_000, 0)
        .unwrap();

    assert_eq!(rect.to_pixels(1000, 2000), (100, 200, 400, 300));
    let closed = agent.close().unwrap();
    let events = touch_events(&closed.transport.bytes);
    assert_eq!(events.len(), 4);
    assert_eq!(events[0], (TouchAction::DOWN.value(), 0, 10, 20));
    assert_eq!(events[1], (TouchAction::UP.value(), 0, 10, 20));
    assert_eq!(events[2], (TouchAction::DOWN.value(), 0, 400, 200));
    assert_eq!(events[3], (TouchAction::UP.value(), 0, 400, 200));
}

#[test]
fn agent_tap_next_object_anchor_helpers_emit_relative_touch_events() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let mut stream = Vec::new();
    stream.extend(frame_summary_envelope_with(
        1,
        &[ObjectBox {
            x: 100,
            y: 200,
            w: 301,
            h: 101,
            class_id: 1,
            confidence: 210,
        }],
        &[],
    ));
    stream.extend(frame_summary_envelope_with(
        2,
        &[ObjectBox {
            x: 700,
            y: 800,
            w: 101,
            h: 101,
            class_id: 2,
            confidence: 230,
        }],
        &[],
    ));
    stream.extend(frame_summary_envelope_with(
        3,
        &[ObjectBox {
            x: 100,
            y: 200,
            w: 301,
            h: 101,
            class_id: 4,
            confidence: 220,
        }],
        &[],
    ));
    stream.extend(frame_summary_envelope_with(
        4,
        &[ObjectBox {
            x: 100,
            y: 200,
            w: 301,
            h: 101,
            class_id: 5,
            confidence: 220,
        }],
        &[],
    ));
    let mut agent = AgentControlSession::from_parts(session, Cursor::new(stream)).unwrap();

    agent.set_screen_size(1000, 2000).unwrap();
    let indexed = agent.tap_next_object_at(0, 0, 10_000).unwrap();
    let best = agent.tap_next_best_object_at(10_000, 0).unwrap();
    let class = agent.tap_next_object_class_at(4, 5_000, 5_000).unwrap();
    let selected = agent
        .tap_next_object_selector_at(
            AgentObjectSelector::class_min_confidence(5, 220),
            2_500,
            7_500,
        )
        .unwrap();

    assert_eq!(indexed.to_pixels(1000, 2000), (100, 200, 400, 300));
    assert_eq!(best.to_pixels(1000, 2000), (700, 800, 800, 900));
    assert_eq!(class.center().to_pixels(1000, 2000), (250, 250));
    assert_eq!(
        selected
            .try_point_at_basis_points(2_500, 7_500)
            .unwrap()
            .to_pixels(1000, 2000),
        (175, 275)
    );

    let closed = agent.close().unwrap();
    let events = touch_events(&closed.transport.bytes);
    assert_eq!(events.len(), 8);
    assert_eq!(events[0], (TouchAction::DOWN.value(), 0, 100, 300));
    assert_eq!(events[2], (TouchAction::DOWN.value(), 0, 800, 800));
    assert_eq!(events[4], (TouchAction::DOWN.value(), 0, 250, 250));
    assert_eq!(events[6], (TouchAction::DOWN.value(), 0, 175, 275));
}

#[test]
fn agent_tap_next_text_anchor_helpers_emit_relative_touch_events() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let mut stream = Vec::new();
    stream.extend(frame_summary_envelope_with(
        1,
        &[],
        &[TextRegion {
            x: 700,
            y: 800,
            w: 101,
            h: 101,
        }],
    ));
    stream.extend(frame_summary_envelope_with(
        2,
        &[],
        &[
            TextRegion {
                x: 10,
                y: 20,
                w: 11,
                h: 21,
            },
            TextRegion {
                x: 100,
                y: 200,
                w: 301,
                h: 101,
            },
        ],
    ));
    let mut agent = AgentControlSession::from_parts(session, Cursor::new(stream)).unwrap();

    agent.set_screen_size(1000, 2000).unwrap();
    let indexed = agent.tap_next_text_region_at(0, 10_000, 0).unwrap();
    let largest = agent.tap_next_largest_text_region_at(0, 10_000).unwrap();

    assert_eq!(indexed.to_pixels(1000, 2000), (700, 800, 800, 900));
    assert_eq!(largest.to_pixels(1000, 2000), (100, 200, 400, 300));

    let closed = agent.close().unwrap();
    let events = touch_events(&closed.transport.bytes);
    assert_eq!(events.len(), 4);
    assert_eq!(events[0], (TouchAction::DOWN.value(), 0, 800, 800));
    assert_eq!(events[2], (TouchAction::DOWN.value(), 0, 100, 300));
}

#[test]
fn agent_tap_next_pointer_vision_targets_emit_typed_pointer_events() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let mut stream = Vec::new();
    stream.extend(frame_summary_envelope_with(
        1,
        &[ObjectBox {
            x: 100,
            y: 200,
            w: 301,
            h: 101,
            class_id: 7,
            confidence: 220,
        }],
        &[],
    ));
    stream.extend(frame_summary_envelope_with(
        2,
        &[],
        &[TextRegion {
            x: 700,
            y: 800,
            w: 101,
            h: 101,
        }],
    ));
    let pointer = TouchPointerId::VIRTUAL_FINGER;
    let mut agent = AgentControlSession::from_parts(session, Cursor::new(stream)).unwrap();

    agent.set_screen_size(1000, 2000).unwrap();
    let object = agent
        .tap_next_best_object_at_pointer(pointer, 2_500, 7_500)
        .unwrap();
    let text = agent.tap_next_largest_text_region_pointer(pointer).unwrap();

    assert_eq!(object.to_pixels(1000, 2000), (100, 200, 400, 300));
    assert_eq!(text.center().to_pixels(1000, 2000), (750, 850));

    let closed = agent.close().unwrap();
    let events = touch_events(&closed.transport.bytes);
    assert_eq!(events.len(), 4);
    assert!(events
        .iter()
        .all(|(_, pointer_id, _, _)| *pointer_id == pointer.value()));
    assert_eq!(
        events[0],
        (TouchAction::DOWN.value(), pointer.value(), 175, 275)
    );
    assert_eq!(
        events[2],
        (TouchAction::DOWN.value(), pointer.value(), 750, 850)
    );
}

#[test]
fn agent_latest_snapshot_target_helpers_select_and_tap_without_waiting() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let snapshot = latest_snapshot_from_envelope(
        9,
        frame_summary_envelope_full(
            3,
            crate::ai::FLAG_OBJECTS | crate::ai::FLAG_TEXT,
            &[],
            &[
                ObjectBox {
                    x: 10,
                    y: 20,
                    w: 11,
                    h: 21,
                    class_id: 3,
                    confidence: 210,
                },
                ObjectBox {
                    x: 100,
                    y: 200,
                    w: 301,
                    h: 101,
                    class_id: 7,
                    confidence: 230,
                },
            ],
            &[
                TextRegion {
                    x: 700,
                    y: 800,
                    w: 101,
                    h: 101,
                },
                TextRegion {
                    x: 100,
                    y: 200,
                    w: 301,
                    h: 101,
                },
            ],
        ),
    );
    let pointer = TouchPointerId::VIRTUAL_FINGER;
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
    agent.set_screen_size(1000, 2000).unwrap();

    assert!(AgentTargetSelector::object_class_min_confidence(7, 220).is_present(&snapshot.summary));
    assert!(!AgentTargetSelector::object_class(99).is_present(&snapshot.summary));
    let best = agent
        .latest_target_rect(&snapshot, AgentTargetSelector::best_object())
        .unwrap()
        .unwrap();
    let indexed_text = agent
        .latest_target_rect(&snapshot, AgentTargetSelector::text_region(0))
        .unwrap()
        .unwrap();
    let observation = LatestFrameSummaryObservation::from_snapshot(snapshot.clone());
    let empty_observation = LatestFrameSummaryObservation::at_version(0);
    let observed_best = agent
        .latest_observation_target_rect(&observation, AgentTargetSelector::best_object())
        .unwrap()
        .unwrap();
    let object = agent
        .tap_latest_object_selector_at_pointer(
            &snapshot,
            AgentObjectSelector::class_min_confidence(7, 220),
            pointer,
            2_500,
            7_500,
        )
        .unwrap()
        .unwrap();
    let text = agent
        .tap_latest_target_at(
            &snapshot,
            AgentTargetSelector::largest_text_region(),
            0,
            10_000,
        )
        .unwrap()
        .unwrap();
    let indexed_text_tap = agent
        .tap_latest_target_pointer(&snapshot, AgentTargetSelector::text_region(0), pointer)
        .unwrap()
        .unwrap();
    let observed_center = agent
        .tap_latest_observation_target(
            &observation,
            AgentTargetSelector::object_class_min_confidence(7, 220),
        )
        .unwrap()
        .unwrap();
    let observed_anchor = agent
        .tap_latest_observation_target_at(
            &observation,
            AgentTargetSelector::text_region(0),
            10_000,
            0,
        )
        .unwrap()
        .unwrap();
    let observed_pointer = agent
        .tap_latest_observation_target_at_pointer(
            &observation,
            AgentTargetSelector::largest_text_region(),
            pointer,
            0,
            10_000,
        )
        .unwrap()
        .unwrap();
    assert!(agent
        .tap_latest_object_selector(&snapshot, AgentObjectSelector::class_id(99))
        .unwrap()
        .is_none());
    assert!(agent
        .tap_latest_target(&snapshot, AgentTargetSelector::object_class(99))
        .unwrap()
        .is_none());
    assert!(agent
        .latest_observation_target_rect(&empty_observation, AgentTargetSelector::best_object())
        .unwrap()
        .is_none());
    assert!(agent
        .tap_latest_observation_target(&observation, AgentTargetSelector::object_class(99))
        .unwrap()
        .is_none());
    assert!(agent
        .tap_latest_observation_target_pointer(
            &empty_observation,
            AgentTargetSelector::text_region(0),
            pointer,
        )
        .unwrap()
        .is_none());

    assert_eq!(best.to_pixels(1000, 2000), (100, 200, 400, 300));
    assert_eq!(observed_best.to_pixels(1000, 2000), (100, 200, 400, 300));
    assert_eq!(object.to_pixels(1000, 2000), (100, 200, 400, 300));
    assert_eq!(indexed_text.to_pixels(1000, 2000), (700, 800, 800, 900));
    assert_eq!(text.to_pixels(1000, 2000), (100, 200, 400, 300));
    assert_eq!(indexed_text_tap.to_pixels(1000, 2000), (700, 800, 800, 900));
    assert_eq!(observed_center.to_pixels(1000, 2000), (100, 200, 400, 300));
    assert_eq!(observed_anchor.to_pixels(1000, 2000), (700, 800, 800, 900));
    assert_eq!(observed_pointer.to_pixels(1000, 2000), (100, 200, 400, 300));
    let closed = agent.close().unwrap();
    let events = touch_events(&closed.transport.bytes);
    assert_eq!(events.len(), 12);
    assert_eq!(
        events[0],
        (TouchAction::DOWN.value(), pointer.value(), 175, 275)
    );
    assert_eq!(
        events[1],
        (TouchAction::UP.value(), pointer.value(), 175, 275)
    );
    assert_eq!(events[2], (TouchAction::DOWN.value(), 0, 100, 300));
    assert_eq!(events[3], (TouchAction::UP.value(), 0, 100, 300));
    assert_eq!(
        events[4],
        (TouchAction::DOWN.value(), pointer.value(), 750, 850)
    );
    assert_eq!(
        events[5],
        (TouchAction::UP.value(), pointer.value(), 750, 850)
    );
    assert_eq!(events[6], (TouchAction::DOWN.value(), 0, 250, 250));
    assert_eq!(events[7], (TouchAction::UP.value(), 0, 250, 250));
    assert_eq!(events[8], (TouchAction::DOWN.value(), 0, 800, 800));
    assert_eq!(events[9], (TouchAction::UP.value(), 0, 800, 800));
    assert_eq!(
        events[10],
        (TouchAction::DOWN.value(), pointer.value(), 100, 300)
    );
    assert_eq!(
        events[11],
        (TouchAction::UP.value(), pointer.value(), 100, 300)
    );
}

#[test]
fn agent_clone_client_can_send_from_worker() {
    let session =
        HidSession::open(MockTransport::new(), OpenRequest::gamepad_only_realtime()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
    let client = agent.clone_client();

    let worker = std::thread::spawn(move || {
        client
            .send_frame_unchecked(GamepadFrameRaw::new(2, 0, 0, 0, 0, 0, 0))
            .unwrap();
    });
    worker.join().unwrap();

    let closed = agent.close().unwrap();
    assert_eq!(count_uhid_inputs(&closed.transport.bytes), 1);
}

#[test]
fn agent_flush_surfaces_prior_dispatch_error() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

    agent.type_text("needs keyboard").unwrap();
    let err = agent.flush().unwrap_err();
    assert!(matches!(err, Error::SessionLifecycle("keyboard not open")));

    let closed = agent.close().unwrap();
    assert!(closed.transport.bytes.is_empty());
}

#[test]
fn agent_type_text_strict_surfaces_unsupported_char() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::kbd_only()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

    agent.type_text_strict("a中b").unwrap();
    let err = agent.flush().unwrap_err();
    assert!(matches!(
        err,
        Error::SessionLifecycle("unsupported char in type_text_strict")
    ));

    let closed = agent.close().unwrap();
    assert_eq!(
        count_uhid_inputs(&closed.transport.bytes),
        2,
        "strict text should stop at the first unsupported character"
    );
}

#[test]
fn agent_keyboard_tap_helpers_emit_uhid_reports() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::kbd_only()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

    agent.tap_scancode(Scancode::A, Modifiers::LSHIFT).unwrap();
    agent
        .key_scancode(Scancode::B, true, Modifiers::empty())
        .unwrap();
    agent
        .key_scancode(Scancode::B, false, Modifiers::empty())
        .unwrap();

    let closed = agent.close().unwrap();
    assert_eq!(
        count_uhid_inputs(&closed.transport.bytes),
        4,
        "tap_scancode emits down/up and key_scancode emits one report per edge"
    );
}

#[test]
fn agent_try_keyboard_helpers_use_nonblocking_checked_dispatch() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::kbd_only()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

    agent
        .try_tap_scancode(Scancode::A, Modifiers::LSHIFT)
        .unwrap();
    agent
        .try_key_scancode(Scancode::B, true, Modifiers::empty())
        .unwrap();
    agent
        .try_key_scancode(Scancode::B, false, Modifiers::empty())
        .unwrap();
    agent
        .try_key(Scancode::C.to_u8(), true, Modifiers::empty())
        .unwrap();
    agent
        .try_key(Scancode::C.to_u8(), false, Modifiers::empty())
        .unwrap();
    agent
        .try_tap_key(Scancode::D.to_u8(), Modifiers::LCTRL)
        .unwrap();
    agent
        .try_scancode_chord(&[Scancode::K, Scancode::C], Modifiers::LCTRL)
        .unwrap();

    let closed = agent.close().unwrap();
    assert_eq!(count_uhid_inputs(&closed.transport.bytes), 12);
}

#[test]
fn agent_try_keyboard_preflights_command_bound_without_dispatch() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent =
        AgentControlSession::from_parts_with_bound(session, Cursor::new(Vec::new()), 1).unwrap();

    let err = agent
        .try_tap_scancode(Scancode::A, Modifiers::LSHIFT)
        .unwrap_err();

    assert!(matches!(
        err,
        Error::SessionLifecycle(TRY_KEY_EXCEEDS_COMMAND_BOUND)
    ));
    let closed = agent.close().unwrap();
    assert!(closed.transport.bytes.is_empty());
}

#[test]
fn agent_close_checked_reports_error_and_recovers_resources() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(ack(5))).unwrap();

    agent.type_text("needs keyboard").unwrap();
    agent
        .client()
        .send(crate::client::HidCommand::MultitouchDown {
            id: 0,
            x: 10,
            y: 20,
            pressure: 1.0,
        })
        .unwrap();
    let report = agent.close_checked().unwrap();

    assert!(matches!(
        report.command_result,
        Err(Error::SessionLifecycle("keyboard not open"))
    ));
    assert_eq!(count_touch_events(&report.closed.transport.bytes), 1);
    assert_eq!(report.closed.reader.position(), 0);
}

#[test]
fn agent_intent_helpers_emit_touch_text_and_launch_commands() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

    agent.tap(10, 20).unwrap();
    agent.swipe((0, 0), (30, 60), 3).unwrap();
    agent.type_text("hi").unwrap();
    agent.launch_app("com.android.settings").unwrap();
    agent.set_screen_power(false).unwrap();

    let closed = agent.close().unwrap();
    let bytes = closed.transport.bytes;
    assert_eq!(count_touch_events(&bytes), 7);
    assert!(bytes.contains(&1), "InjectText tag should be present");
    assert!(bytes.contains(&16), "StartApp tag should be present");
    assert!(bytes.contains(&10), "SetDisplayPower tag should be present");
}

#[test]
fn agent_point_converts_normalized_coordinates() {
    assert_eq!(AgentPoint::CENTER.to_pixels(1080, 2400), (540, 1200));
    assert_eq!(AgentPoint::BOTTOM_RIGHT.to_pixels(1080, 2400), (1079, 2399));
    assert_eq!(
        AgentPoint::try_from_basis_points(5_000, 2_500)
            .unwrap()
            .to_pixels(1080, 2400),
        (540, 600)
    );
    assert_eq!(
        AgentPoint::try_from_unit(0.25, 0.75)
            .unwrap()
            .to_pixels(1000, 2000),
        (250, 1499)
    );
    assert!(matches!(
        AgentPoint::try_from_unit(1.1, 0.5),
        Err(Error::SessionLifecycle("normalized point out of range"))
    ));
    assert!(matches!(
        AgentPoint::try_from_basis_points(10_001, 0),
        Err(Error::SessionLifecycle("normalized point out of range"))
    ));
}

#[test]
fn agent_rect_converts_detection_boxes_to_normalized_targets() {
    assert_eq!(AgentRect::FULL_SCREEN.center(), AgentPoint::CENTER);
    assert_eq!(
        AgentRect::try_from_basis_points(2_500, 2_500, 7_500, 7_500)
            .unwrap()
            .center()
            .to_pixels(1000, 2000),
        (500, 1000)
    );

    let object = ObjectBox {
        x: 100,
        y: 200,
        w: 301,
        h: 101,
        class_id: 7,
        confidence: 220,
    };
    let rect = AgentRect::try_from_object_box(object, 1000, 2000).unwrap();
    assert_eq!(rect.to_pixels(1000, 2000), (100, 200, 400, 300));
    assert_eq!(rect.center().to_pixels(1000, 2000), (250, 250));

    let text = TextRegion {
        x: 10,
        y: 20,
        w: 11,
        h: 21,
    };
    let rect = AgentRect::try_from_text_region(text, 100, 200).unwrap();
    assert_eq!(rect.center().to_pixels(100, 200), (15, 30));

    assert!(matches!(
        AgentRect::try_from_pixels(990, 0, 20, 10, 1000, 2000),
        Err(Error::SessionLifecycle("agent rectangle out of range"))
    ));
    assert!(matches!(
        AgentRect::try_from_basis_points(0, 0, 10_001, 1),
        Err(Error::SessionLifecycle("normalized point out of range"))
    ));
}

#[test]
fn agent_rect_points_at_relative_anchors() {
    let rect = AgentRect::try_from_pixels(100, 200, 301, 101, 1000, 2000).unwrap();

    assert_eq!(
        rect.try_point_at_basis_points(0, 0)
            .unwrap()
            .to_pixels(1000, 2000),
        (100, 200)
    );
    assert_eq!(
        rect.try_point_at_basis_points(10_000, 10_000)
            .unwrap()
            .to_pixels(1000, 2000),
        (400, 300)
    );
    assert_eq!(
        rect.try_point_at_basis_points(2_500, 7_500)
            .unwrap()
            .to_pixels(1000, 2000),
        (175, 275)
    );
    assert_eq!(
        rect.try_point_at_unit(0.25, 0.75)
            .unwrap()
            .to_pixels(1000, 2000),
        (175, 275)
    );

    let reversed = AgentRect {
        left: rect.right,
        top: rect.bottom,
        right: rect.left,
        bottom: rect.top,
    };
    assert_eq!(
        reversed
            .try_point_at_basis_points(0, 0)
            .unwrap()
            .to_pixels(1000, 2000),
        (100, 200)
    );
    assert!(matches!(
        rect.try_point_at_basis_points(10_001, 0),
        Err(Error::SessionLifecycle("normalized point out of range"))
    ));
    assert!(matches!(
        rect.try_point_at_unit(f32::NAN, 0.5),
        Err(Error::SessionLifecycle("normalized point out of range"))
    ));
}

#[test]
fn agent_rect_selects_targets_from_frame_summary() {
    let summary = FrameSummary {
        timestamp_ms: 1,
        frame_seq: 2,
        width: 1000,
        height: 2000,
        flags: crate::ai::FLAG_OBJECTS | crate::ai::FLAG_TEXT,
        features: Vec::new(),
        motion: Vec::new(),
        objects: vec![
            ObjectBox {
                x: 10,
                y: 20,
                w: 11,
                h: 21,
                class_id: 1,
                confidence: 200,
            },
            ObjectBox {
                x: 100,
                y: 200,
                w: 101,
                h: 101,
                class_id: 2,
                confidence: 220,
            },
            ObjectBox {
                x: 300,
                y: 400,
                w: 301,
                h: 101,
                class_id: 2,
                confidence: 220,
            },
            ObjectBox {
                x: 500,
                y: 600,
                w: 11,
                h: 21,
                class_id: 3,
                confidence: 230,
            },
        ],
        text_regions: vec![
            TextRegion {
                x: 10,
                y: 20,
                w: 11,
                h: 21,
            },
            TextRegion {
                x: 700,
                y: 800,
                w: 101,
                h: 101,
            },
        ],
    };

    assert_eq!(
        AgentRect::try_from_frame_object(&summary, 1)
            .unwrap()
            .unwrap()
            .center()
            .to_pixels(1000, 2000),
        (150, 250)
    );
    assert!(AgentRect::try_from_frame_object(&summary, 99)
        .unwrap()
        .is_none());
    assert_eq!(
        AgentRect::try_from_best_object(&summary)
            .unwrap()
            .unwrap()
            .center()
            .to_pixels(1000, 2000),
        (505, 610)
    );
    assert_eq!(
        AgentRect::try_from_best_object_class(&summary, 2)
            .unwrap()
            .unwrap()
            .center()
            .to_pixels(1000, 2000),
        (450, 450)
    );
    assert!(AgentRect::try_from_best_object_class(&summary, 9)
        .unwrap()
        .is_none());
    assert_eq!(
        AgentRect::try_from_frame_text_region(&summary, 0)
            .unwrap()
            .unwrap()
            .center()
            .to_pixels(1000, 2000),
        (15, 30)
    );
    assert_eq!(
        AgentRect::try_from_largest_text_region(&summary)
            .unwrap()
            .unwrap()
            .center()
            .to_pixels(1000, 2000),
        (750, 850)
    );

    let bad_summary = FrameSummary {
        width: 0,
        objects: vec![summary.objects[0]],
        ..summary
    };
    assert!(matches!(
        AgentRect::try_from_best_object(&bad_summary),
        Err(Error::SessionLifecycle("agent rectangle out of range"))
    ));
}

#[test]
fn agent_object_selector_filters_class_and_confidence() {
    let summary = FrameSummary {
        timestamp_ms: 1,
        frame_seq: 2,
        width: 1000,
        height: 2000,
        flags: crate::ai::FLAG_OBJECTS,
        features: Vec::new(),
        motion: Vec::new(),
        objects: vec![
            ObjectBox {
                x: 100,
                y: 200,
                w: 101,
                h: 101,
                class_id: 2,
                confidence: 220,
            },
            ObjectBox {
                x: 300,
                y: 400,
                w: 301,
                h: 101,
                class_id: 2,
                confidence: 220,
            },
            ObjectBox {
                x: 500,
                y: 600,
                w: 11,
                h: 21,
                class_id: 3,
                confidence: 230,
            },
        ],
        text_regions: Vec::new(),
    };

    assert_eq!(
        AgentObjectSelector::ANY.select(&summary).unwrap().class_id,
        3
    );
    assert_eq!(
        AgentObjectSelector::class_min_confidence(2, 220)
            .select_rect(&summary)
            .unwrap()
            .unwrap()
            .center()
            .to_pixels(1000, 2000),
        (450, 450)
    );
    assert!(AgentObjectSelector::class_id(2).matches(summary.objects[0]));
    assert!(!AgentObjectSelector::min_confidence(231).matches(summary.objects[2]));
    assert!(AgentRect::try_from_best_object_matching(
        &summary,
        AgentObjectSelector::ANY
            .with_class_id(2)
            .with_min_confidence(221),
    )
    .unwrap()
    .is_none());
}

#[test]
fn agent_normalized_touch_helpers_use_tracked_screen_size() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

    agent.set_screen_size(1080, 2400).unwrap();
    agent.tap_point(AgentPoint::CENTER).unwrap();

    let closed = agent.close().unwrap();
    assert_eq!(first_touch_xy(&closed.transport.bytes), Some((540, 1200)));
    assert_eq!(
        first_touch_screen_size(&closed.transport.bytes),
        Some((1080, 2400))
    );
}

#[test]
fn agent_rect_touch_helpers_use_center_point() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
    let rect = AgentRect::try_from_pixels(100, 200, 301, 101, 1000, 2000).unwrap();

    agent.set_screen_size(1000, 2000).unwrap();
    agent.tap_rect(rect).unwrap();

    let closed = agent.close().unwrap();
    assert_eq!(first_touch_xy(&closed.transport.bytes), Some((250, 250)));
    assert_eq!(
        first_touch_screen_size(&closed.transport.bytes),
        Some((1000, 2000))
    );
}

#[test]
fn agent_rect_anchor_touch_helpers_use_relative_points() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
    let rect = AgentRect::try_from_pixels(100, 200, 301, 101, 1000, 2000).unwrap();

    agent.set_screen_size(1000, 2000).unwrap();
    agent.tap_rect_at(rect, 2_500, 7_500).unwrap();
    agent
        .tap_rect_at_pointer(TouchPointerId::VIRTUAL_FINGER, rect, 10_000, 0)
        .unwrap();

    let closed = agent.close().unwrap();
    let events = touch_events(&closed.transport.bytes);
    assert_eq!(events.len(), 4);
    assert_eq!(events[0], (TouchAction::DOWN.value(), 0, 175, 275));
    assert_eq!(
        events[2],
        (
            TouchAction::DOWN.value(),
            TouchPointerId::VIRTUAL_FINGER.value(),
            400,
            200,
        )
    );
}

#[test]
fn agent_try_tap_rect_anchor_pointer_uses_nonblocking_checked_dispatch() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
    let rect = AgentRect::try_from_pixels(100, 200, 301, 101, 1000, 2000).unwrap();

    agent.set_screen_size(1000, 2000).unwrap();
    agent
        .try_tap_rect_at_pointer(TouchPointerId::VIRTUAL_FINGER, rect, 2_500, 7_500)
        .unwrap();

    let closed = agent.close().unwrap();
    let events = touch_events(&closed.transport.bytes);
    assert_eq!(events.len(), 2);
    assert_eq!(
        events[0],
        (
            TouchAction::DOWN.value(),
            TouchPointerId::VIRTUAL_FINGER.value(),
            175,
            275,
        )
    );
    assert_eq!(
        events[1],
        (
            TouchAction::UP.value(),
            TouchPointerId::VIRTUAL_FINGER.value(),
            175,
            275,
        )
    );
}

#[test]
fn agent_try_tap_preflights_command_bound_without_dispatch() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent =
        AgentControlSession::from_parts_with_bound(session, Cursor::new(Vec::new()), 1).unwrap();

    let err = agent.try_tap(10, 20).unwrap_err();

    assert!(matches!(
        err,
        Error::SessionLifecycle(TRY_TAP_EXCEEDS_COMMAND_BOUND)
    ));
    let closed = agent.close().unwrap();
    assert!(closed.transport.bytes.is_empty());
}

#[test]
fn agent_try_double_tap_rect_anchor_pointer_uses_nonblocking_checked_dispatch() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
    let rect = AgentRect::try_from_pixels(100, 200, 301, 101, 1000, 2000).unwrap();

    agent.set_screen_size(1000, 2000).unwrap();
    agent
        .try_double_tap_rect_at_pointer(TouchPointerId::VIRTUAL_FINGER, rect, 2_500, 7_500)
        .unwrap();

    let closed = agent.close().unwrap();
    let events = touch_events(&closed.transport.bytes);
    assert_eq!(events.len(), 4);
    assert_eq!(
        events[0],
        (
            TouchAction::DOWN.value(),
            TouchPointerId::VIRTUAL_FINGER.value(),
            175,
            275,
        )
    );
    assert_eq!(
        events[1],
        (
            TouchAction::UP.value(),
            TouchPointerId::VIRTUAL_FINGER.value(),
            175,
            275,
        )
    );
    assert_eq!(events[2], events[0]);
    assert_eq!(events[3], events[1]);
}

#[test]
fn agent_try_double_tap_preflights_command_bound_without_dispatch() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent =
        AgentControlSession::from_parts_with_bound(session, Cursor::new(Vec::new()), 1).unwrap();

    let err = agent.try_double_tap(10, 20).unwrap_err();

    assert!(matches!(
        err,
        Error::SessionLifecycle(TRY_DOUBLE_TAP_EXCEEDS_COMMAND_BOUND)
    ));
    let closed = agent.close().unwrap();
    assert!(closed.transport.bytes.is_empty());
}

#[test]
fn agent_run_actions_batches_normalized_touch_actions() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

    agent.set_screen_size(1080, 2400).unwrap();
    agent
        .run_actions(&[
            AgentAction::tap_point(AgentPoint::CENTER),
            AgentAction::swipe_points(
                AgentPoint::try_from_basis_points(0, 0).unwrap(),
                AgentPoint::try_from_basis_points(10_000, 10_000).unwrap(),
                2,
            ),
            AgentAction::double_tap_point(AgentPoint::try_from_unit(0.25, 0.25).unwrap()),
        ])
        .unwrap();

    let closed = agent.close().unwrap();
    assert_eq!(count_touch_events(&closed.transport.bytes), 2 + 4 + 4);
    assert_eq!(control_message_tags(&closed.transport.bytes), vec![2; 10]);
}

#[test]
fn agent_run_actions_batches_rect_touch_actions() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
    let rect = AgentRect::try_from_basis_points(4_000, 4_000, 6_000, 6_000).unwrap();

    agent.set_screen_size(1000, 2000).unwrap();
    agent
        .run_actions(&[
            AgentAction::tap_rect(rect),
            AgentAction::double_tap_rect(rect),
        ])
        .unwrap();

    let closed = agent.close().unwrap();
    assert_eq!(control_message_tags(&closed.transport.bytes), vec![2; 6]);
    assert_eq!(first_touch_xy(&closed.transport.bytes), Some((500, 1000)));
}

#[test]
fn agent_run_actions_batches_rect_anchor_actions() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
    let pointer = TouchPointerId::GENERIC_FINGER;
    let rect = AgentRect::try_from_pixels(100, 200, 301, 101, 1000, 2000).unwrap();

    agent.set_screen_size(1000, 2000).unwrap();
    agent
        .run_actions(&[
            AgentAction::tap_rect_at(rect, 0, 0),
            AgentAction::tap_rect_at_pointer(pointer, rect, 10_000, 0),
            AgentAction::double_tap_rect_at(rect, 2_500, 7_500),
            AgentAction::double_tap_rect_at_pointer(pointer, rect, 10_000, 10_000),
        ])
        .unwrap();

    let closed = agent.close().unwrap();
    let events = touch_events(&closed.transport.bytes);
    assert_eq!(events.len(), 2 + 2 + 4 + 4);
    assert_eq!(control_message_tags(&closed.transport.bytes), vec![2; 12]);
    assert!(events.contains(&(TouchAction::DOWN.value(), 0, 100, 200)));
    assert!(events.contains(&(TouchAction::DOWN.value(), pointer.value(), 400, 200)));
    assert!(events.contains(&(TouchAction::DOWN.value(), 0, 175, 275)));
    assert!(events.contains(&(TouchAction::DOWN.value(), pointer.value(), 400, 300)));
}

#[test]
fn agent_rect_swipe_helpers_use_relative_points() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
    let pointer = TouchPointerId::VIRTUAL_FINGER;
    let rect = AgentRect::try_from_pixels(100, 200, 301, 101, 1000, 2000).unwrap();

    agent.set_screen_size(1000, 2000).unwrap();
    agent
        .swipe_rect(rect, (0, 5_000), (10_000, 5_000), 2)
        .unwrap();
    agent
        .swipe_rect_pointer(pointer, rect, (2_500, 0), (2_500, 10_000), 1)
        .unwrap();

    let closed = agent.close().unwrap();
    let events = touch_events(&closed.transport.bytes);
    assert_eq!(events.len(), 4 + 3);
    assert_eq!(events[0], (TouchAction::DOWN.value(), 0, 100, 250));
    assert_eq!(events[2], (TouchAction::MOVE.value(), 0, 400, 250));
    assert_eq!(events[3], (TouchAction::UP.value(), 0, 400, 250));
    assert_eq!(
        events[4],
        (TouchAction::DOWN.value(), pointer.value(), 175, 200)
    );
    assert_eq!(
        events[6],
        (TouchAction::UP.value(), pointer.value(), 175, 300)
    );
}

#[test]
fn agent_run_actions_batches_rect_swipe_actions() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
    let pointer = TouchPointerId::GENERIC_FINGER;
    let rect = AgentRect::try_from_pixels(100, 200, 301, 101, 1000, 2000).unwrap();

    agent.set_screen_size(1000, 2000).unwrap();
    agent
        .run_actions(&[
            AgentAction::tap_rect_at(rect, 0, 0),
            AgentAction::swipe_rect(rect, (0, 5_000), (10_000, 5_000), 2),
            AgentAction::swipe_rect_pointer(pointer, rect, (2_500, 0), (2_500, 10_000), 1),
            AgentAction::double_tap_rect_at(rect, 10_000, 10_000),
        ])
        .unwrap();

    let closed = agent.close().unwrap();
    let events = touch_events(&closed.transport.bytes);
    assert_eq!(events.len(), 2 + 4 + 3 + 4);
    assert_eq!(control_message_tags(&closed.transport.bytes), vec![2; 13]);
    assert!(events.contains(&(TouchAction::DOWN.value(), 0, 100, 250)));
    assert!(events.contains(&(TouchAction::UP.value(), 0, 400, 250)));
    assert!(events.contains(&(TouchAction::DOWN.value(), pointer.value(), 175, 200)));
    assert!(events.contains(&(TouchAction::UP.value(), pointer.value(), 175, 300)));
}

#[test]
fn agent_try_queue_actions_batches_rect_swipes_with_tiny_bound() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent =
        AgentControlSession::from_parts_with_bound(session, Cursor::new(Vec::new()), 1).unwrap();

    agent
        .try_queue_actions(&[AgentAction::swipe_rect(
            AgentRect::FULL_SCREEN,
            (0, 5_000),
            (10_000, 5_000),
            2,
        )])
        .unwrap();

    let closed = agent.close().unwrap();
    let events = touch_events(&closed.transport.bytes);
    assert_eq!(events.len(), 4);
    assert_eq!(events[0], (TouchAction::DOWN.value(), 0, 0, 960));
    assert_eq!(events[3], (TouchAction::UP.value(), 0, 1079, 960));
}

#[test]
fn agent_run_actions_batches_pointer_touch_actions() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
    let pointer = TouchPointerId::GENERIC_FINGER;
    let rect = AgentRect::try_from_basis_points(4_000, 4_000, 6_000, 6_000).unwrap();

    agent.set_screen_size(1000, 2000).unwrap();
    agent
        .run_actions(&[
            AgentAction::tap_pointer(pointer, 10, 20),
            AgentAction::tap_point_pointer(pointer, AgentPoint::CENTER),
            AgentAction::tap_rect_pointer(pointer, rect),
            AgentAction::swipe_points_pointer(
                pointer,
                AgentPoint::try_from_basis_points(0, 0).unwrap(),
                AgentPoint::try_from_basis_points(10_000, 10_000).unwrap(),
                2,
            ),
            AgentAction::double_tap_rect_pointer(pointer, rect),
        ])
        .unwrap();

    let closed = agent.close().unwrap();
    let events = touch_events(&closed.transport.bytes);
    assert_eq!(events.len(), 2 + 2 + 2 + 4 + 4);
    assert_eq!(control_message_tags(&closed.transport.bytes), vec![2; 14]);
    assert!(events
        .iter()
        .all(|(_, pointer_id, _, _)| *pointer_id == pointer.value()));
    assert!(events.contains(&(TouchAction::DOWN.value(), pointer.value(), 500, 1000)));
    assert!(events.contains(&(TouchAction::UP.value(), pointer.value(), 999, 1999)));
}

#[test]
fn agent_pinch_helper_emits_two_pointer_touch_path() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

    agent
        .pinch((100, 1200), (240, 1200), (980, 1200), (840, 1200), 3)
        .unwrap();

    let closed = agent.close().unwrap();
    let events = touch_events(&closed.transport.bytes);
    assert_eq!(events.len(), 2 + 3 * 2 + 2);
    assert_eq!(control_message_tags(&closed.transport.bytes), vec![2; 10]);
    assert_eq!(events[0], (TouchAction::DOWN.value(), 0, 100, 1200));
    assert_eq!(events[1], (TouchAction::DOWN.value(), 1, 980, 1200));
    assert_eq!(events[8], (TouchAction::UP.value(), 0, 240, 1200));
    assert_eq!(events[9], (TouchAction::UP.value(), 1, 840, 1200));
    assert_eq!(events.iter().filter(|(_, id, _, _)| *id == 0).count(), 5);
    assert_eq!(events.iter().filter(|(_, id, _, _)| *id == 1).count(), 5);
}

#[test]
fn agent_run_actions_batches_normalized_pinch_with_adjacent_touch_actions() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
    let first_from = AgentPoint::try_from_basis_points(4_000, 5_000).unwrap();
    let first_to = AgentPoint::try_from_basis_points(3_000, 5_000).unwrap();
    let second_from = AgentPoint::try_from_basis_points(6_000, 5_000).unwrap();
    let second_to = AgentPoint::try_from_basis_points(7_000, 5_000).unwrap();

    agent.set_screen_size(1000, 2000).unwrap();
    agent
        .run_actions(&[
            AgentAction::tap_point(AgentPoint::CENTER),
            AgentAction::pinch_points(first_from, first_to, second_from, second_to, 2),
            AgentAction::double_tap_point(AgentPoint::CENTER),
        ])
        .unwrap();

    let closed = agent.close().unwrap();
    let events = touch_events(&closed.transport.bytes);
    assert_eq!(events.len(), 2 + 2 + 2 * 2 + 2 + 4);
    assert_eq!(control_message_tags(&closed.transport.bytes), vec![2; 14]);

    let (x0, y0) = first_from.to_pixels(1000, 2000);
    let (x1, y1) = second_to.to_pixels(1000, 2000);
    assert!(events.contains(&(TouchAction::DOWN.value(), 0, x0, y0)));
    assert!(events.contains(&(TouchAction::UP.value(), 1, x1, y1)));
}

#[test]
fn agent_try_queue_actions_batches_pinch_with_tiny_bound() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent =
        AgentControlSession::from_parts_with_bound(session, Cursor::new(Vec::new()), 1).unwrap();

    agent
        .try_queue_actions(&[AgentAction::pinch(
            (10, 20),
            (20, 20),
            (50, 20),
            (40, 20),
            1,
        )])
        .unwrap();

    let closed = agent.close().unwrap();
    let events = touch_events(&closed.transport.bytes);
    assert_eq!(events.len(), 6);
    assert_eq!(events[0], (TouchAction::DOWN.value(), 0, 10, 20));
    assert_eq!(events[5], (TouchAction::UP.value(), 1, 40, 20));
}

#[test]
fn agent_normalized_scroll_helpers_use_tracked_screen_size() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

    agent.set_screen_size(720, 1280).unwrap();
    agent
        .scroll_point_with_buttons(AgentPoint::CENTER, 0.0, -16.0, 0x11)
        .unwrap();
    agent
        .run_actions(&[AgentAction::scroll_point(
            AgentPoint::try_from_basis_points(2_500, 7_500).unwrap(),
            0,
            16,
        )])
        .unwrap();

    let closed = agent.close().unwrap();
    let bytes = closed.transport.bytes;
    assert_eq!(control_message_tags(&bytes), vec![3, 3]);
    assert_eq!(first_scroll_xy(&bytes), Some((360, 640)));
    let second = &bytes[21..42];
    assert_eq!(i32::from_be_bytes(second[1..5].try_into().unwrap()), 180);
    assert_eq!(i32::from_be_bytes(second[5..9].try_into().unwrap()), 959);
}

#[test]
fn agent_rect_scroll_helpers_use_center_point() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
    let rect = AgentRect::try_from_pixels(100, 200, 301, 101, 1000, 2000).unwrap();

    agent.set_screen_size(1000, 2000).unwrap();
    agent
        .scroll_rect_with_buttons(rect, 0.0, -16.0, 0x11)
        .unwrap();
    agent
        .run_actions(&[AgentAction::scroll_rect(rect, 0, 16)])
        .unwrap();

    let closed = agent.close().unwrap();
    assert_eq!(control_message_tags(&closed.transport.bytes), vec![3, 3]);
    assert_eq!(first_scroll_xy(&closed.transport.bytes), Some((250, 250)));
    let second = &closed.transport.bytes[21..42];
    assert_eq!(i32::from_be_bytes(second[1..5].try_into().unwrap()), 250);
    assert_eq!(i32::from_be_bytes(second[5..9].try_into().unwrap()), 250);
}

#[test]
fn agent_rect_anchor_scroll_helpers_use_relative_point() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
    let rect = AgentRect::try_from_pixels(100, 200, 301, 101, 1000, 2000).unwrap();

    agent.set_screen_size(1000, 2000).unwrap();
    agent
        .scroll_rect_at_with_buttons(rect, 2_500, 7_500, 0.0, -16.0, 0x11)
        .unwrap();
    agent
        .run_actions(&[
            AgentAction::scroll_rect_at(rect, 10_000, 0, 0, 16),
            AgentAction::scroll_rect_at_with_buttons(rect, 0, 10_000, 0, 8, 0x22),
        ])
        .unwrap();

    let closed = agent.close().unwrap();
    assert_eq!(control_message_tags(&closed.transport.bytes), vec![3, 3, 3]);
    assert_eq!(first_scroll_xy(&closed.transport.bytes), Some((175, 275)));
    let second = &closed.transport.bytes[21..42];
    assert_eq!(i32::from_be_bytes(second[1..5].try_into().unwrap()), 400);
    assert_eq!(i32::from_be_bytes(second[5..9].try_into().unwrap()), 200);
    let third = &closed.transport.bytes[42..63];
    assert_eq!(i32::from_be_bytes(third[1..5].try_into().unwrap()), 100);
    assert_eq!(i32::from_be_bytes(third[5..9].try_into().unwrap()), 300);
    assert_eq!(u32::from_be_bytes(third[17..21].try_into().unwrap()), 0x22);
}

#[test]
fn agent_try_scroll_rect_anchor_uses_nonblocking_checked_dispatch() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
    let rect = AgentRect::try_from_pixels(100, 200, 301, 101, 1000, 2000).unwrap();

    agent.set_screen_size(1000, 2000).unwrap();
    agent
        .try_scroll_rect_at_with_buttons(rect, 2_500, 7_500, 0.0, -16.0, 0x11)
        .unwrap();

    let closed = agent.close().unwrap();
    let bytes = closed.transport.bytes;
    assert_eq!(control_message_tags(&bytes), vec![3]);
    assert_eq!(first_scroll_xy(&bytes), Some((175, 275)));
    assert_eq!(u32::from_be_bytes(bytes[17..21].try_into().unwrap()), 0x11);
}

#[test]
fn agent_try_scroll_preflights_command_bound_without_dispatch() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent =
        AgentControlSession::from_parts_with_bound(session, Cursor::new(Vec::new()), 1).unwrap();

    let err = agent.try_scroll(10, 20, 0.0, -16.0).unwrap_err();

    assert!(matches!(
        err,
        Error::SessionLifecycle(TRY_SCROLL_EXCEEDS_COMMAND_BOUND)
    ));
    let closed = agent.close().unwrap();
    assert!(closed.transport.bytes.is_empty());
}

#[test]
fn agent_try_queue_actions_rejects_normalized_timed_actions() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

    let err = agent
        .try_queue_actions(&[AgentAction::long_press_point(
            AgentPoint::CENTER,
            Duration::from_millis(1),
        )])
        .unwrap_err();

    assert!(matches!(
        err,
        Error::SessionLifecycle("timed action requires queue_actions or run_actions")
    ));
}

#[test]
fn agent_try_queue_actions_rejects_rect_timed_actions() {
    for action in [
        AgentAction::long_press_rect(AgentRect::FULL_SCREEN, Duration::from_millis(1)),
        AgentAction::long_press_rect_at(
            AgentRect::FULL_SCREEN,
            5_000,
            5_000,
            Duration::from_millis(1),
        ),
    ] {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

        let err = agent.try_queue_actions(&[action]).unwrap_err();
        assert!(matches!(
            err,
            Error::SessionLifecycle("timed action requires queue_actions or run_actions")
        ));
    }
}

#[test]
fn agent_try_queue_actions_rejects_pointer_timed_actions() {
    for action in [
        AgentAction::long_press_pointer(
            TouchPointerId::VIRTUAL_FINGER,
            10,
            20,
            Duration::from_millis(1),
        ),
        AgentAction::long_press_point_pointer(
            TouchPointerId::VIRTUAL_FINGER,
            AgentPoint::CENTER,
            Duration::from_millis(1),
        ),
        AgentAction::long_press_rect_pointer(
            TouchPointerId::VIRTUAL_FINGER,
            AgentRect::FULL_SCREEN,
            Duration::from_millis(1),
        ),
        AgentAction::long_press_rect_at_pointer(
            TouchPointerId::VIRTUAL_FINGER,
            AgentRect::FULL_SCREEN,
            5_000,
            5_000,
            Duration::from_millis(1),
        ),
    ] {
        let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
        let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

        let err = agent.try_queue_actions(&[action]).unwrap_err();

        assert!(matches!(
            err,
            Error::SessionLifecycle("timed action requires queue_actions or run_actions")
        ));
        let closed = agent.close().unwrap();
        assert!(closed.transport.bytes.is_empty());
    }
}

#[test]
fn agent_android_intent_helpers_emit_control_messages() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

    agent.press_home().unwrap();
    agent.press_back().unwrap();
    agent.open_recents().unwrap();
    agent.volume_up().unwrap();
    agent.volume_down().unwrap();
    agent.volume_mute().unwrap();
    agent.show_notifications().unwrap();
    agent.show_quick_settings().unwrap();
    agent.collapse_panels().unwrap();
    agent.rotate_device().unwrap();
    agent.resize_display(720, 1280).unwrap();
    agent.set_torch(true).unwrap();
    agent.camera_zoom_in().unwrap();
    agent.camera_zoom_out().unwrap();
    agent.open_hard_keyboard_settings().unwrap();
    agent.reset_video().unwrap();

    let closed = agent.close().unwrap();
    let bytes = closed.transport.bytes;
    assert_eq!(
        first_inject_keycodes(&bytes, 6),
        vec![3, 4, 187, 24, 25, 164]
    );
    for tag in [5, 6, 7, 11, 18, 19, 20, 15, 17] {
        assert!(bytes.contains(&tag), "missing control tag {tag}");
    }
    let resize = bytes
        .windows(5)
        .find(|frame| frame[0] == 21)
        .expect("RESIZE_DISPLAY frame");
    assert_eq!(u16::from_be_bytes([resize[1], resize[2]]), 720);
    assert_eq!(u16::from_be_bytes([resize[3], resize[4]]), 1280);
}

#[test]
fn agent_try_control_helpers_use_nonblocking_checked_dispatch() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

    agent.try_set_screen_power(false).unwrap();
    agent.try_show_notifications().unwrap();
    agent.try_show_quick_settings().unwrap();
    agent.try_collapse_panels().unwrap();
    agent.try_rotate_device().unwrap();
    agent.try_resize_display(720, 1280).unwrap();
    agent.try_set_torch(true).unwrap();
    agent.try_camera_zoom_in().unwrap();
    agent.try_camera_zoom_out().unwrap();
    agent.try_open_hard_keyboard_settings().unwrap();
    agent.try_reset_video().unwrap();

    let closed = agent.close().unwrap();
    let bytes = closed.transport.bytes;
    assert_eq!(
        control_message_tags(&bytes),
        vec![10, 5, 6, 7, 11, 21, 18, 19, 20, 15, 17]
    );
    assert_eq!(find_control_message(&bytes, 10), Some(&[10, 0][..]));
    assert_eq!(find_control_message(&bytes, 18), Some(&[18, 1][..]));
    let resize = find_control_message(&bytes, 21).expect("RESIZE_DISPLAY frame");
    assert_eq!(u16::from_be_bytes([resize[1], resize[2]]), 720);
    assert_eq!(u16::from_be_bytes([resize[3], resize[4]]), 1280);
}

#[test]
fn agent_try_set_screen_size_updates_local_metadata_after_checked_dispatch() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

    agent.try_set_screen_size(720, 1280).unwrap();
    assert_eq!(agent.screen_size(), (720, 1280));
    agent.try_tap_point(AgentPoint::CENTER).unwrap();

    let closed = agent.close().unwrap();
    assert_eq!(
        first_touch_screen_size(&closed.transport.bytes),
        Some((720, 1280))
    );
    assert_eq!(first_touch_xy(&closed.transport.bytes), Some((360, 640)));
}

#[test]
fn agent_try_control_preflights_command_bound_without_dispatch() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent =
        AgentControlSession::from_parts_with_bound(session, Cursor::new(Vec::new()), 1).unwrap();

    let err = agent.try_set_screen_power(false).unwrap_err();

    assert!(matches!(
        err,
        Error::SessionLifecycle(TRY_CONTROL_EXCEEDS_COMMAND_BOUND)
    ));
    let closed = agent.close().unwrap();
    assert!(closed.transport.bytes.is_empty());
}

#[test]
fn agent_try_launch_app_preflights_oversized_name_without_dispatch() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

    let err = agent.try_launch_app("x".repeat(256)).unwrap_err();

    assert!(matches!(
        err,
        Error::SessionLifecycle(LAUNCH_APP_NAME_TOO_LONG)
    ));
    let closed = agent.close().unwrap();
    assert!(closed.transport.bytes.is_empty());
}

#[test]
fn agent_ai_extension_helpers_emit_control_messages() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
    let flags = crate::control::AI_FLAG_FEATURES | crate::control::AI_FLAG_TEXT;

    agent.configure_ai(flags, 33, 64).unwrap();
    agent.query_ai(1234).unwrap();
    agent.pause_ai().unwrap();

    let closed = agent.close().unwrap();
    let bytes = closed.transport.bytes;
    assert_eq!(
        find_control_message(&bytes, 22).expect("AI_CONFIG frame"),
        &[22, flags, 0, 33, 0, 64]
    );
    let query = find_control_message(&bytes, 23).expect("AI_QUERY frame");
    assert_eq!(u64::from_be_bytes(query[1..9].try_into().unwrap()), 1234);
    assert_eq!(find_control_message(&bytes, 24), Some(&[24][..]));
}

#[test]
fn agent_try_ai_helpers_use_nonblocking_checked_dispatch() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
    let flags = crate::control::AI_FLAG_FEATURES | crate::control::AI_FLAG_TEXT;

    agent.try_configure_ai(flags, 33, 64).unwrap();
    agent.try_query_ai(1234).unwrap();
    agent.try_pause_ai().unwrap();

    let closed = agent.close().unwrap();
    let bytes = closed.transport.bytes;
    assert_eq!(control_message_tags(&bytes), vec![22, 23, 24]);
    assert_eq!(
        find_control_message(&bytes, 22).expect("AI_CONFIG frame"),
        &[22, flags, 0, 33, 0, 64]
    );
    let query = find_control_message(&bytes, 23).expect("AI_QUERY frame");
    assert_eq!(u64::from_be_bytes(query[1..9].try_into().unwrap()), 1234);
}

#[test]
fn agent_try_ai_preflights_command_bound_without_dispatch() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent =
        AgentControlSession::from_parts_with_bound(session, Cursor::new(Vec::new()), 1).unwrap();

    let err = agent.try_query_ai(1234).unwrap_err();

    assert!(matches!(
        err,
        Error::SessionLifecycle(TRY_AI_EXCEEDS_COMMAND_BOUND)
    ));
    let closed = agent.close().unwrap();
    assert!(closed.transport.bytes.is_empty());
}

#[test]
fn agent_try_clipboard_and_launch_helpers_use_nonblocking_checked_dispatch() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

    agent.try_launch_app("com.example.app").unwrap();
    agent.try_set_clipboard("clip", false).unwrap();
    agent.try_set_clipboard_sequenced(7, "seq", true).unwrap();
    agent
        .try_request_clipboard_key(ClipboardCopyKey::CUT)
        .unwrap();

    let closed = agent.close().unwrap();
    let bytes = closed.transport.bytes;
    assert_eq!(control_message_tags(&bytes), vec![16, 9, 9, 8]);
    let launch = find_control_message(&bytes, 16).expect("START_APP frame");
    assert_eq!(launch[1] as usize, "com.example.app".len());
    assert_eq!(&launch[2..], b"com.example.app");

    let first_clipboard = find_control_message(&bytes, 9).expect("SET_CLIPBOARD frame");
    assert_eq!(
        u64::from_be_bytes(first_clipboard[1..9].try_into().unwrap()),
        0
    );
    assert_eq!(first_clipboard[9], 0);
    assert_eq!(
        u32::from_be_bytes(first_clipboard[10..14].try_into().unwrap()),
        4
    );
    assert_eq!(&first_clipboard[14..], b"clip");

    assert_eq!(count_control_messages(&bytes, 9), 2);
    let request = find_control_message(&bytes, 8).expect("GET_CLIPBOARD frame");
    assert_eq!(request, &[8, ClipboardCopyKey::CUT.value()]);
}

#[test]
fn agent_try_clipboard_preflights_command_bound_without_dispatch() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent =
        AgentControlSession::from_parts_with_bound(session, Cursor::new(Vec::new()), 1).unwrap();

    let err = agent.try_request_clipboard(0).unwrap_err();

    assert!(matches!(
        err,
        Error::SessionLifecycle(TRY_CLIPBOARD_EXCEEDS_COMMAND_BOUND)
    ));
    let closed = agent.close().unwrap();
    assert!(closed.transport.bytes.is_empty());
}

#[test]
fn query_ai_and_wait_stats_sends_query_and_skips_unrelated_events() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let mut stream = frame_summary_envelope(1);
    stream.extend(ai_stats_envelope());
    let mut agent = AgentControlSession::from_parts(session, Cursor::new(stream)).unwrap();

    let stats = agent
        .query_ai_and_wait_stats(0x0102_0304_0506_0708)
        .unwrap();

    assert_eq!(stats.frames_sampled, 10);
    let closed = agent.close().unwrap();
    let query = find_control_message(&closed.transport.bytes, 23).expect("AI_QUERY frame");
    assert_eq!(
        u64::from_be_bytes(query[1..9].try_into().unwrap()),
        0x0102_0304_0506_0708
    );
}

#[test]
fn run_actions_and_query_ai_and_wait_stats_flushes_then_reads() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let mut stream = frame_summary_envelope(1);
    stream.extend(ai_stats_envelope());
    let mut agent = AgentControlSession::from_parts(session, Cursor::new(stream)).unwrap();

    let stats = agent
        .run_actions_and_query_ai_and_wait_stats(&[AgentAction::tap(10, 20)], 0x1112_1314_1516_1718)
        .unwrap();

    assert_eq!(stats.frames_sampled, 10);
    let closed = agent.close().unwrap();
    assert_eq!(control_message_tags(&closed.transport.bytes), [2, 2, 23]);
    let query = find_control_message(&closed.transport.bytes, 23).expect("AI_QUERY frame");
    assert_eq!(
        u64::from_be_bytes(query[1..9].try_into().unwrap()),
        0x1112_1314_1516_1718
    );
}

#[test]
fn agent_mouse_helpers_emit_uhid_mouse_reports() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::mouse_only()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

    agent
        .mouse_motion_buttons(10, -5, &[MouseButton::Left])
        .unwrap();
    agent.mouse_button_state(&[MouseButton::Right]).unwrap();
    agent.mouse_scroll(0.0, 2.0).unwrap();

    let closed = agent.close().unwrap();
    let payloads = mouse_input_payloads(&closed.transport.bytes);
    assert_eq!(payloads.len(), 3);
    assert_eq!(
        payloads[0],
        [MouseButton::Left as u8, 10, (-5i8) as u8, 0, 0]
    );
    assert_eq!(payloads[1], [MouseButton::Right as u8, 0, 0, 0, 0]);
    assert_eq!(payloads[2], [0, 0, 0, 2, 0]);
}

#[test]
fn agent_try_mouse_helpers_use_nonblocking_checked_dispatch() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::mouse_only()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

    agent
        .try_mouse_motion_buttons(10, -5, &[MouseButton::Left])
        .unwrap();
    agent.try_mouse_button_state(&[MouseButton::Right]).unwrap();
    agent.try_mouse_scroll(0.0, 2.0).unwrap();

    let closed = agent.close().unwrap();
    let payloads = mouse_input_payloads(&closed.transport.bytes);
    assert_eq!(payloads.len(), 3);
    assert_eq!(
        payloads[0],
        [MouseButton::Left as u8, 10, (-5i8) as u8, 0, 0]
    );
    assert_eq!(payloads[1], [MouseButton::Right as u8, 0, 0, 0, 0]);
    assert_eq!(payloads[2], [0, 0, 0, 2, 0]);
}

#[test]
fn agent_try_mouse_preflights_command_bound_without_dispatch() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent =
        AgentControlSession::from_parts_with_bound(session, Cursor::new(Vec::new()), 1).unwrap();

    let err = agent
        .try_mouse_motion_buttons(10, -5, &[MouseButton::Left])
        .unwrap_err();

    assert!(matches!(
        err,
        Error::SessionLifecycle(TRY_MOUSE_EXCEEDS_COMMAND_BOUND)
    ));
    let closed = agent.close().unwrap();
    assert!(closed.transport.bytes.is_empty());
}

#[test]
fn agent_mouse_actions_cover_batch_and_scroll() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::mouse_only()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
    let frames = [
        MouseFrame::motion(1, 2, MouseButton::Left as u8),
        MouseFrame::motion(-3, -4, 0),
    ];

    agent
        .run_actions(&[
            AgentAction::mouse_motion(5, 6, 0),
            AgentAction::mouse_buttons(MouseButton::Middle as u8),
            AgentAction::mouse_scroll(0, 3),
            AgentAction::try_mouse_batch(&frames).unwrap(),
        ])
        .unwrap();

    let closed = agent.close().unwrap();
    let payloads = mouse_input_payloads(&closed.transport.bytes);
    assert_eq!(payloads.len(), 5);
    assert_eq!(payloads[0], [0, 5, 6, 0, 0]);
    assert_eq!(payloads[1], [MouseButton::Middle as u8, 0, 0, 0, 0]);
    assert_eq!(payloads[2], [0, 0, 0, 3, 0]);
    assert_eq!(payloads[3], [MouseButton::Left as u8, 1, 2, 0, 0]);
    assert_eq!(payloads[4], [0, (-3i8) as u8, (-4i8) as u8, 0, 0]);
}

#[test]
fn agent_mouse_batch_rejects_oversized_slices() {
    let frames = vec![MouseFrame::EMPTY; MOUSE_BATCH_FRAMES + 1];
    assert!(matches!(
        AgentAction::try_mouse_batch(&frames),
        Err(Error::SessionLifecycle("mouse batch too large"))
    ));
}

#[test]
fn agent_run_actions_batches_consecutive_mouse_actions() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::mouse_only()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
    let frames = [
        MouseFrame::motion(1, 2, MouseButton::Left as u8),
        MouseFrame::motion(-3, -4, 0),
    ];

    agent
        .run_actions(&[
            AgentAction::mouse_motion(5, 6, 0),
            AgentAction::mouse_button_state(&[MouseButton::Middle]),
            AgentAction::try_mouse_batch(&frames).unwrap(),
        ])
        .unwrap();

    let closed = agent.close().unwrap();
    let payloads = mouse_input_payloads(&closed.transport.bytes);
    assert_eq!(payloads.len(), 4);
    assert_eq!(payloads[0], [0, 5, 6, 0, 0]);
    assert_eq!(payloads[1], [MouseButton::Middle as u8, 0, 0, 0, 0]);
    assert_eq!(payloads[2], [MouseButton::Left as u8, 1, 2, 0, 0]);
    assert_eq!(payloads[3], [0, (-3i8) as u8, (-4i8) as u8, 0, 0]);
}

#[test]
fn agent_run_actions_flushes_mouse_before_touch_actions() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::mouse_only()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

    agent
        .run_actions(&[
            AgentAction::mouse_motion(5, 6, 0),
            AgentAction::tap(10, 20),
            AgentAction::mouse_buttons(MouseButton::Right as u8),
        ])
        .unwrap();

    let closed = agent.close().unwrap();
    assert_eq!(
        input_and_touch_tags(&closed.transport.bytes),
        vec![13, 2, 2, 13]
    );
}

#[test]
fn agent_try_queue_actions_batches_mouse_with_tiny_bound() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::mouse_only()).unwrap();
    let agent =
        AgentControlSession::from_parts_with_bound(session, Cursor::new(Vec::new()), 1).unwrap();

    agent
        .try_queue_actions(&[
            AgentAction::mouse_motion(5, 6, 0),
            AgentAction::mouse_button_state(&[MouseButton::Middle]),
        ])
        .unwrap();

    let closed = agent.close().unwrap();
    assert_eq!(mouse_input_payloads(&closed.transport.bytes).len(), 2);
}

#[test]
fn agent_back_or_screen_on_helpers_emit_control_message() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

    agent.back_or_screen_on(AndroidKeyAction::UP).unwrap();
    agent
        .run_actions(&[AgentAction::back_or_screen_on(AndroidKeyAction::DOWN)])
        .unwrap();

    let closed = agent.close().unwrap();
    let bytes = closed.transport.bytes;
    assert_eq!(control_message_tags(&bytes), vec![4, 4]);
    assert_eq!(&bytes, &[4, 1, 4, 0]);
}

#[test]
fn agent_try_back_or_screen_on_uses_nonblocking_checked_dispatch() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

    agent.try_back_or_screen_on(AndroidKeyAction::UP).unwrap();

    let closed = agent.close().unwrap();
    assert_eq!(closed.transport.bytes, vec![4, 1]);
}

#[test]
fn agent_typed_android_keycode_helpers_emit_keycodes() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

    agent.press_android_key(AndroidKeycode::POWER).unwrap();
    agent
        .inject_android_key_event(AndroidKeyAction::UP, AndroidKeycode::ENTER, 2, 3)
        .unwrap();
    agent.release_android_key(AndroidKeycode::MENU).unwrap();
    agent
        .run_actions(&[
            AgentAction::press_android_key(AndroidKeycode::BACK),
            AgentAction::inject_android_key_event(AndroidKeyAction::UP, AndroidKeycode::MENU, 2, 3),
            AgentAction::release_android_key(AndroidKeycode::POWER),
        ])
        .unwrap();

    let closed = agent.close().unwrap();
    let bytes = closed.transport.bytes;
    assert_eq!(control_message_tags(&bytes), vec![0, 0, 0, 0, 0, 0]);
    assert_eq!(
        first_inject_keycodes(&bytes, 6),
        vec![26, 66, 82, 4, 82, 26]
    );
    let menu = &bytes[56..70];
    assert_eq!(menu[1], 1);
    assert_eq!(u32::from_be_bytes(menu[2..6].try_into().unwrap()), 82);
    assert_eq!(u32::from_be_bytes(menu[6..10].try_into().unwrap()), 2);
    assert_eq!(u32::from_be_bytes(menu[10..14].try_into().unwrap()), 3);
}

#[test]
fn agent_try_android_key_helpers_use_nonblocking_checked_dispatch() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

    agent.try_press_android_key(AndroidKeycode::POWER).unwrap();
    agent
        .try_inject_android_key_event(AndroidKeyAction::UP, AndroidKeycode::ENTER, 2, 3)
        .unwrap();
    agent.try_release_android_key(AndroidKeycode::MENU).unwrap();
    agent
        .try_tap_android_key_with_metastate(AndroidKeycode::ENTER, 3)
        .unwrap();
    agent.try_tap_android_keycode(82, 0x40).unwrap();
    agent.try_press_home().unwrap();
    agent.try_press_back().unwrap();
    agent.try_open_recents().unwrap();
    agent.try_volume_up().unwrap();
    agent.try_volume_down().unwrap();
    agent.try_volume_mute().unwrap();

    let closed = agent.close().unwrap();
    let bytes = closed.transport.bytes;
    assert_eq!(control_message_tags(&bytes), vec![0; 13]);
    assert_eq!(
        first_inject_keycodes(&bytes, 13),
        vec![26, 66, 82, 66, 66, 82, 82, 3, 4, 187, 24, 25, 164]
    );
}

#[test]
fn agent_try_android_key_preflights_command_bound_without_dispatch() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent =
        AgentControlSession::from_parts_with_bound(session, Cursor::new(Vec::new()), 1).unwrap();

    let err = agent.try_press_home().unwrap_err();

    assert!(matches!(
        err,
        Error::SessionLifecycle(TRY_ANDROID_KEY_EXCEEDS_COMMAND_BOUND)
    ));
    let closed = agent.close().unwrap();
    assert!(closed.transport.bytes.is_empty());
}

#[test]
fn agent_android_key_tap_helpers_emit_down_up_keycodes() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

    agent
        .tap_android_key_with_metastate(AndroidKeycode::ENTER, 3)
        .unwrap();
    agent
        .run_actions(&[
            AgentAction::tap_android_key(AndroidKeycode::BACK),
            AgentAction::tap_android_keycode(82, 0x40),
        ])
        .unwrap();

    let closed = agent.close().unwrap();
    let bytes = closed.transport.bytes;
    assert_eq!(control_message_tags(&bytes), vec![0, 0, 0, 0, 0, 0]);
    assert_eq!(first_inject_keycodes(&bytes, 6), vec![66, 66, 4, 4, 82, 82]);

    let events: Vec<_> = bytes
        .chunks_exact(14)
        .map(|frame| {
            (
                frame[1],
                u32::from_be_bytes(frame[2..6].try_into().unwrap()),
                u32::from_be_bytes(frame[10..14].try_into().unwrap()),
            )
        })
        .collect();
    assert_eq!(
        events,
        vec![
            (0, 66, 3),
            (1, 66, 3),
            (0, 4, 0),
            (1, 4, 0),
            (0, 82, 0x40),
            (1, 82, 0x40)
        ]
    );
}

#[test]
fn agent_android_key_batch_action_dispatches_fixed_batch() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
    let frames = [
        AndroidKeyFrame::down(AndroidKeycode::ENTER, 3),
        AndroidKeyFrame::up(AndroidKeycode::ENTER, 3),
        AndroidKeyFrame::typed(AndroidKeyAction::UP, AndroidKeycode::MENU, 2, 4),
    ];

    agent
        .run_actions(&[AgentAction::try_android_key_batch(&frames).unwrap()])
        .unwrap();

    let closed = agent.close().unwrap();
    assert_eq!(control_message_tags(&closed.transport.bytes), vec![0, 0, 0]);
    assert_eq!(
        first_inject_keycodes(&closed.transport.bytes, 3),
        vec![66, 66, 82]
    );
}

#[test]
fn agent_run_actions_batches_consecutive_android_key_actions() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
    let frames = [
        AndroidKeyFrame::down(AndroidKeycode::MENU, 0),
        AndroidKeyFrame::up(AndroidKeycode::MENU, 0),
    ];

    agent
        .run_actions(&[
            AgentAction::tap_android_key(AndroidKeycode::ENTER),
            AgentAction::PressBack,
            AgentAction::try_android_key_batch(&frames).unwrap(),
        ])
        .unwrap();

    let closed = agent.close().unwrap();
    let bytes = closed.transport.bytes;
    assert_eq!(control_message_tags(&bytes), vec![0, 0, 0, 0, 0]);
    assert_eq!(first_inject_keycodes(&bytes, 5), vec![66, 66, 4, 82, 82]);
}

#[test]
fn agent_run_actions_flushes_android_keys_before_touch_actions() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

    agent
        .run_actions(&[
            AgentAction::tap_android_key(AndroidKeycode::ENTER),
            AgentAction::tap(10, 20),
            AgentAction::tap_android_key(AndroidKeycode::BACK),
        ])
        .unwrap();

    let closed = agent.close().unwrap();
    assert_eq!(
        control_message_tags(&closed.transport.bytes),
        vec![0, 0, 2, 2, 0, 0]
    );
}

#[test]
fn agent_try_queue_actions_batches_android_keys_with_tiny_bound() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent =
        AgentControlSession::from_parts_with_bound(session, Cursor::new(Vec::new()), 1).unwrap();

    agent
        .try_queue_actions(&[
            AgentAction::tap_android_key(AndroidKeycode::ENTER),
            AgentAction::PressBack,
            AgentAction::tap_android_keycode(82, 0x40),
        ])
        .unwrap();

    let closed = agent.close().unwrap();
    assert_eq!(control_message_tags(&closed.transport.bytes), vec![0; 5]);
    assert_eq!(
        first_inject_keycodes(&closed.transport.bytes, 5),
        vec![66, 66, 4, 82, 82]
    );
}

#[test]
fn agent_android_key_batch_constructor_rejects_oversized_slices() {
    let frames = vec![AndroidKeyFrame::EMPTY; ANDROID_KEY_BATCH_FRAMES + 1];
    assert!(matches!(
        AgentAction::try_android_key_batch(&frames),
        Err(Error::SessionLifecycle("android key batch too large"))
    ));
}

#[test]
fn agent_screen_size_affects_touch_frames() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

    agent.set_screen_size(1440, 3120).unwrap();
    agent.tap(10, 20).unwrap();

    let closed = agent.close().unwrap();
    assert_eq!(
        first_touch_screen_size(&closed.transport.bytes),
        Some((1440, 3120))
    );
}

#[test]
fn agent_composite_gesture_helpers_use_batched_touch_frames() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

    agent.set_screen_size(1440, 3120).unwrap();
    assert_eq!(agent.screen_size(), (1440, 3120));
    agent.double_tap(10, 20).unwrap();
    agent.long_press(30, 40, Duration::from_millis(0)).unwrap();
    agent.three_finger_screenshot().unwrap();

    let closed = agent.close().unwrap();
    let bytes = closed.transport.bytes;
    assert_eq!(count_touch_events(&bytes), 4 + 2 + 36);
    assert_eq!(first_touch_screen_size(&bytes), Some((1440, 3120)));
    assert!(
        contains_touch_point(&bytes, 0, 360, 780),
        "three-finger path should use agent-local 1440x3120 dimensions"
    );
}

#[test]
fn agent_cancel_touch_helpers_emit_action_cancel() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

    agent.cancel_touch(5).unwrap();
    agent.run_actions(&[AgentAction::cancel_touch(6)]).unwrap();

    let closed = agent.close().unwrap();
    let bytes = closed.transport.bytes;
    assert_eq!(control_message_tags(&bytes), vec![2, 2]);
    assert_eq!(bytes[1], 3);
    assert_eq!(u64::from_be_bytes(bytes[2..10].try_into().unwrap()), 5);
    assert_eq!(bytes[33], 3);
    assert_eq!(u64::from_be_bytes(bytes[34..42].try_into().unwrap()), 6);
}

#[test]
fn agent_typed_touch_pointer_helpers_preserve_scrcpy_reserved_ids() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
    let pointer = TouchPointerId::VIRTUAL_FINGER;
    let custom = [
        AgentTouchFrame::down_pointer(pointer, 30, 40, u16::MAX),
        AgentTouchFrame::move_pointer_to(pointer, 35, 45, 32768),
        AgentTouchFrame::up_pointer(pointer, 35, 45),
    ];

    agent.set_screen_size(1000, 2000).unwrap();
    agent.tap_pointer(pointer, 10, 20).unwrap();
    agent
        .tap_point_pointer(pointer, AgentPoint::CENTER)
        .unwrap();
    agent
        .run_actions(&[
            AgentAction::try_touch_frames(&custom).unwrap(),
            AgentAction::cancel_touch_pointer(pointer),
        ])
        .unwrap();

    let closed = agent.close().unwrap();
    let events = touch_events(&closed.transport.bytes);
    assert_eq!(events.len(), 8);
    assert!(events
        .iter()
        .all(|(_, pointer_id, _, _)| *pointer_id == pointer.value()));
    assert_eq!(
        events[0],
        (TouchAction::DOWN.value(), pointer.value(), 10, 20)
    );
    assert_eq!(
        events[1],
        (TouchAction::UP.value(), pointer.value(), 10, 20)
    );
    assert_eq!(
        events[2],
        (TouchAction::DOWN.value(), pointer.value(), 500, 1000)
    );
    assert_eq!(events[7].0, TouchAction::CANCEL.value());
}

#[test]
fn agent_touch_frame_batch_action_batches_with_adjacent_touch_actions() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
    let custom = [
        AgentTouchFrame::down(2, 100, 200, u16::MAX),
        AgentTouchFrame::move_to(2, 110, 210, 32768),
        AgentTouchFrame::up(2, 110, 210),
    ];

    agent
        .run_actions(&[
            AgentAction::tap(10, 20),
            AgentAction::try_touch_frames(&custom).unwrap(),
            AgentAction::cancel_touch(2),
        ])
        .unwrap();

    let closed = agent.close().unwrap();
    let bytes = closed.transport.bytes;
    assert_eq!(control_message_tags(&bytes), vec![2; 6]);

    let custom_down = &bytes[64..96];
    assert_eq!(custom_down[1], TouchAction::DOWN.value());
    assert_eq!(
        u64::from_be_bytes(custom_down[2..10].try_into().unwrap()),
        2
    );
    assert_eq!(
        i32::from_be_bytes(custom_down[10..14].try_into().unwrap()),
        100
    );
    assert_eq!(
        i32::from_be_bytes(custom_down[14..18].try_into().unwrap()),
        200
    );
    assert_eq!(
        u16::from_be_bytes(custom_down[22..24].try_into().unwrap()),
        u16::MAX
    );

    let custom_move = &bytes[96..128];
    assert_eq!(custom_move[1], TouchAction::MOVE.value());
    assert_eq!(
        u16::from_be_bytes(custom_move[22..24].try_into().unwrap()),
        32768
    );

    let custom_up = &bytes[128..160];
    assert_eq!(custom_up[1], TouchAction::UP.value());
    assert_eq!(u16::from_be_bytes(custom_up[22..24].try_into().unwrap()), 0);

    let custom_cancel = &bytes[160..192];
    assert_eq!(custom_cancel[1], TouchAction::CANCEL.value());
}

#[test]
fn agent_touch_frame_batch_rejects_oversized_or_malformed_batches() {
    let frame = AgentTouchFrame::move_to(0, 1, 2, 32768);
    let frames = vec![frame; TOUCH_BATCH_FRAMES + 1];
    assert!(matches!(
        AgentAction::try_touch_frames(&frames),
        Err(Error::SessionLifecycle("touch frame batch too large"))
    ));

    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
    let malformed = AgentAction::touch_frames_fixed(
        TOUCH_BATCH_FRAMES + 1,
        [AgentTouchFrame::EMPTY; TOUCH_BATCH_FRAMES],
    );
    let err = agent.run_actions(&[malformed]).unwrap_err();
    assert!(matches!(
        err,
        Error::SessionLifecycle("touch frame batch length overflow")
    ));

    let _closed = agent.close().unwrap();
}

#[test]
fn agent_run_actions_executes_plan_with_one_checked_boundary() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::kbd_only()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

    agent
        .run_actions(&[
            AgentAction::SetScreenSize {
                width: 1440,
                height: 3120,
            },
            AgentAction::tap(10, 20),
            AgentAction::swipe((0, 0), (30, 60), 3),
            AgentAction::type_text("hi"),
            AgentAction::launch_app("com.android.settings"),
            AgentAction::PressBack,
            AgentAction::SetScreenPower { on: false },
        ])
        .unwrap();

    assert_eq!(agent.screen_size(), (1440, 3120));
    let closed = agent.close().unwrap();
    let bytes = closed.transport.bytes;
    assert_eq!(count_touch_events(&bytes), 7);
    assert_eq!(first_touch_screen_size(&bytes), Some((1440, 3120)));
    assert!(contains_inject_keycode(&bytes, 4));
    assert!(
        count_control_messages(&bytes, 13) >= 4,
        "type_text should emit keyboard UHID_INPUT reports"
    );
    assert!(
        find_control_message(&bytes, 16).is_some(),
        "StartApp tag should be present"
    );
    assert!(
        find_control_message(&bytes, 10).is_some(),
        "SetDisplayPower tag should be present"
    );
}

#[test]
fn agent_run_actions_batches_consecutive_touch_actions() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

    agent
        .run_actions(&[
            AgentAction::tap(10, 20),
            AgentAction::swipe((0, 0), (30, 60), 3),
            AgentAction::DoubleTap { x: 40, y: 50 },
        ])
        .unwrap();

    let closed = agent.close().unwrap();
    let bytes = closed.transport.bytes;
    assert_eq!(count_control_messages(&bytes, 2), 2 + 5 + 4);
    assert_eq!(control_message_tags(&bytes), vec![2; 11]);
}

#[test]
fn agent_run_actions_flushes_touch_before_non_touch_actions() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

    agent
        .run_actions(&[
            AgentAction::tap(10, 20),
            AgentAction::PressBack,
            AgentAction::tap(30, 40),
        ])
        .unwrap();

    let closed = agent.close().unwrap();
    assert_eq!(
        control_message_tags(&closed.transport.bytes),
        vec![2, 2, 0, 2, 2]
    );
}

#[test]
fn agent_try_queue_actions_enqueues_without_checked_wait() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

    agent
        .try_queue_actions(&[
            AgentAction::SetScreenSize {
                width: 1440,
                height: 3120,
            },
            AgentAction::tap(10, 20),
            AgentAction::PressBack,
            AgentAction::tap(30, 40),
            AgentAction::SetScreenPower { on: true },
        ])
        .unwrap();

    assert_eq!(agent.screen_size(), (1440, 3120));
    let closed = agent.close().unwrap();
    assert_eq!(
        control_message_tags(&closed.transport.bytes),
        vec![2, 2, 0, 2, 2, 10]
    );
    assert_eq!(
        first_touch_screen_size(&closed.transport.bytes),
        Some((1440, 3120))
    );
}

#[test]
fn agent_action_preflight_classifies_try_queueable_plans() {
    let rect = AgentRect::FULL_SCREEN;
    let ready = [
        AgentAction::tap(10, 20),
        AgentAction::swipe((10, 20), (30, 40), 2),
        AgentAction::scroll_rect(rect, 0, -16),
        AgentAction::Flush,
    ];
    let mixed = [
        AgentAction::tap(10, 20),
        AgentAction::wait(Duration::from_millis(1)),
        AgentAction::PressBack,
    ];

    assert!(AgentAction::all_try_queueable(&ready));
    assert_eq!(AgentAction::first_non_try_queueable(&ready), None);
    assert_eq!(AgentAction::try_queueable_prefix_len(&ready), ready.len());
    assert_eq!(AgentAction::first_blocking_timing(&ready), None);
    assert_eq!(AgentAction::blocking_timing_prefix_len(&ready), ready.len());
    assert!(!AgentAction::all_try_queueable(&mixed));
    assert_eq!(AgentAction::first_non_try_queueable(&mixed), Some(1));
    assert_eq!(AgentAction::try_queueable_prefix_len(&mixed), 1);
    assert_eq!(AgentAction::first_blocking_timing(&mixed), Some(1));
    assert_eq!(AgentAction::blocking_timing_prefix_len(&mixed), 1);
    assert_eq!(
        AgentAction::first_try_queue_error(&mixed),
        Some((1, TIMED_ACTION_REQUIRES_BLOCKING))
    );
    assert!(matches!(
        AgentAction::validate_try_queue_plan(&mixed),
        Err(Error::SessionLifecycle(TIMED_ACTION_REQUIRES_BLOCKING))
    ));
    assert!(mixed[1].requires_blocking_timing());
    assert!(!mixed[1].can_try_queue());
    assert!(AgentAction::long_press_rect(rect, Duration::from_millis(1)).requires_blocking_timing());
}

#[test]
fn agent_action_preflight_classifies_structural_plan_errors() {
    let ready = AgentAction::tap_rect_at(AgentRect::FULL_SCREEN, 10_000, 0);
    assert_eq!(ready.structural_error(), None);
    ready.validate_structure().unwrap();

    let malformed_strict_text = AgentAction::type_text_strict("ok中");
    assert_eq!(
        malformed_strict_text.structural_error(),
        Some(STRICT_TEXT_UNSUPPORTED)
    );
    assert!(matches!(
        malformed_strict_text.validate_structure(),
        Err(Error::SessionLifecycle(STRICT_TEXT_UNSUPPORTED))
    ));

    let oversized_app_name = AgentAction::launch_app("a".repeat(256));
    assert_eq!(
        oversized_app_name.structural_error(),
        Some(LAUNCH_APP_NAME_TOO_LONG)
    );
    assert!(matches!(
        oversized_app_name.validate_structure(),
        Err(Error::SessionLifecycle(LAUNCH_APP_NAME_TOO_LONG))
    ));

    let malformed_anchor = AgentAction::tap_rect_at(AgentRect::FULL_SCREEN, 10_001, 0);
    assert_eq!(
        malformed_anchor.structural_error(),
        Some("normalized point out of range")
    );
    assert!(matches!(
        malformed_anchor.validate_structure(),
        Err(Error::SessionLifecycle("normalized point out of range"))
    ));

    let malformed_chord = AgentAction::keyboard_chord_fixed(KeyboardChordFrame::new(
        (KEYBOARD_CHORD_KEYS + 1) as u8,
        [Scancode::A.to_u8(); KEYBOARD_CHORD_KEYS],
        Modifiers::LCTRL,
    ));
    assert_eq!(
        malformed_chord.structural_error(),
        Some("keyboard chord length overflow")
    );

    let malformed_batch = AgentAction::touch_frames_fixed(
        TOUCH_BATCH_FRAMES + 1,
        [AgentTouchFrame::EMPTY; TOUCH_BATCH_FRAMES],
    );
    let actions = [AgentAction::tap(10, 20), malformed_batch];
    assert_eq!(
        AgentAction::first_structural_error(&actions),
        Some((1, "touch frame batch length overflow"))
    );
    assert_eq!(AgentAction::first_non_try_queueable(&actions), Some(1));
    assert_eq!(AgentAction::try_queueable_prefix_len(&actions), 1);
    assert_eq!(AgentAction::first_blocking_timing(&actions), None);
    assert_eq!(
        AgentAction::blocking_timing_prefix_len(&actions),
        actions.len()
    );
    assert!(matches!(
        AgentAction::validate_plan_structure(&actions),
        Err(Error::SessionLifecycle("touch frame batch length overflow"))
    ));
    assert!(matches!(
        AgentAction::validate_try_queue_plan(&actions),
        Err(Error::SessionLifecycle("touch frame batch length overflow"))
    ));

    let strict_text_actions = [
        AgentAction::tap(10, 20),
        AgentAction::type_text_strict("a中"),
    ];
    assert_eq!(
        AgentAction::first_structural_error(&strict_text_actions),
        Some((1, STRICT_TEXT_UNSUPPORTED))
    );
    assert!(matches!(
        AgentAction::validate_plan_structure(&strict_text_actions),
        Err(Error::SessionLifecycle(STRICT_TEXT_UNSUPPORTED))
    ));

    let launch_app_actions = [
        AgentAction::tap(10, 20),
        AgentAction::launch_app("a".repeat(256)),
    ];
    assert_eq!(
        AgentAction::first_structural_error(&launch_app_actions),
        Some((1, LAUNCH_APP_NAME_TOO_LONG))
    );
    assert!(matches!(
        AgentAction::validate_plan_structure(&launch_app_actions),
        Err(Error::SessionLifecycle(LAUNCH_APP_NAME_TOO_LONG))
    ));
}

#[test]
fn agent_action_try_queue_preflight_reports_first_error_in_plan_order() {
    let malformed = AgentAction::key_batch_fixed(
        KEYBOARD_BATCH_FRAMES + 1,
        [KeyboardFrame::EMPTY; KEYBOARD_BATCH_FRAMES],
    );
    let wait = AgentAction::wait(Duration::from_millis(0));

    let malformed_first = [AgentAction::tap(10, 20), malformed.clone(), wait.clone()];
    assert_eq!(
        AgentAction::first_try_queue_error(&malformed_first),
        Some((1, "keyboard batch length overflow"))
    );
    assert_eq!(
        AgentAction::first_blocking_timing(&malformed_first),
        Some(2)
    );
    assert_eq!(AgentAction::blocking_timing_prefix_len(&malformed_first), 2);
    assert!(matches!(
        AgentAction::validate_try_queue_plan(&malformed_first),
        Err(Error::SessionLifecycle("keyboard batch length overflow"))
    ));

    let timed_first = [AgentAction::tap(10, 20), wait, malformed];
    assert_eq!(
        AgentAction::first_try_queue_error(&timed_first),
        Some((1, TIMED_ACTION_REQUIRES_BLOCKING))
    );
    assert_eq!(AgentAction::first_blocking_timing(&timed_first), Some(1));
    assert_eq!(AgentAction::blocking_timing_prefix_len(&timed_first), 1);
    assert!(matches!(
        AgentAction::validate_try_queue_plan(&timed_first),
        Err(Error::SessionLifecycle(TIMED_ACTION_REQUIRES_BLOCKING))
    ));
}

#[test]
fn agent_action_plan_summary_reports_boundaries_and_dispatch_pressure() {
    let rect = AgentRect::FULL_SCREEN;
    let ready = [
        AgentAction::tap(10, 20),
        AgentAction::swipe((10, 20), (30, 40), 2),
        AgentAction::scroll_rect(rect, 0, -16),
        AgentAction::Flush,
    ];
    let summary = AgentAction::plan_summary(&ready);

    assert_eq!(summary.action_count, ready.len());
    assert!(summary.is_structurally_valid());
    assert!(summary.all_try_queueable());
    assert!(!summary.has_blocking_timing());
    assert_eq!(summary.first_structural_error, None);
    assert_eq!(summary.first_try_queue_error, None);
    assert_eq!(summary.first_blocking_timing, None);
    assert_eq!(summary.try_queueable_prefix_len, ready.len());
    assert_eq!(summary.blocking_timing_prefix_len, ready.len());
    assert_eq!(summary.estimated_queue_dispatch_commands, 3);
    assert_eq!(summary.estimated_run_dispatch_commands, 4);
    assert_eq!(summary.estimated_try_queue_dispatch_commands, 3);
    assert_eq!(summary.estimated_try_run_dispatch_commands, 4);
    assert_eq!(summary.estimated_try_queue_prefix_dispatch_commands, 3);
    assert_eq!(summary.estimated_try_run_prefix_dispatch_commands, 4);
    assert_eq!(summary.first_try_queue_prefix_error, None);
    assert!(summary.can_try_queue_prefix());
    assert!(!summary.has_blocking_suffix());
    assert_eq!(summary.blocking_suffix_len(), 0);
    assert!(summary.queue_dispatch_fits_bound(3));
    assert!(!summary.queue_dispatch_fits_bound(2));
    assert!(summary.run_dispatch_fits_bound(4));
    assert!(!summary.run_dispatch_fits_bound(3));
    assert!(summary.try_queue_dispatch_fits_bound(3));
    assert!(!summary.try_queue_dispatch_fits_bound(2));
    assert!(summary.try_run_dispatch_fits_bound(4));
    assert!(!summary.try_run_dispatch_fits_bound(3));
    assert!(summary.try_queue_prefix_dispatch_fits_bound(3));
    assert!(!summary.try_queue_prefix_dispatch_fits_bound(2));
    assert!(summary.try_run_prefix_dispatch_fits_bound(4));
    assert!(!summary.try_run_prefix_dispatch_fits_bound(3));

    let touch_heavy = [
        AgentAction::touch_frames_fixed(
            TOUCH_BATCH_FRAMES,
            [AgentTouchFrame::EMPTY; TOUCH_BATCH_FRAMES],
        ),
        AgentAction::tap(10, 20),
    ];
    let summary = AgentPlanSummary::analyze(&touch_heavy);
    assert_eq!(summary.estimated_queue_dispatch_commands, 2);
    assert_eq!(summary.estimated_run_dispatch_commands, 3);
    assert_eq!(summary.estimated_try_run_dispatch_commands, 3);
    assert_eq!(summary.estimated_try_run_prefix_dispatch_commands, 3);
    assert!(summary.try_queue_dispatch_fits_bound(2));
    assert!(!summary.try_queue_dispatch_fits_bound(1));
    assert!(summary.try_run_dispatch_fits_bound(3));
    assert!(!summary.try_run_dispatch_fits_bound(2));
    assert!(summary.try_run_prefix_dispatch_fits_bound(3));
    assert!(!summary.try_run_prefix_dispatch_fits_bound(2));
}

#[test]
fn agent_action_plan_summary_reports_blocking_prefix_pressure() {
    let malformed = AgentAction::key_batch_fixed(
        KEYBOARD_BATCH_FRAMES + 1,
        [KeyboardFrame::EMPTY; KEYBOARD_BATCH_FRAMES],
    );
    let actions = [
        AgentAction::tap(10, 20),
        AgentAction::wait(Duration::from_millis(0)),
        malformed,
    ];
    let summary = AgentAction::plan_summary(&actions);

    assert!(!summary.is_structurally_valid());
    assert!(!summary.all_try_queueable());
    assert!(summary.has_blocking_timing());
    assert_eq!(
        summary.first_structural_error,
        Some((2, "keyboard batch length overflow"))
    );
    assert_eq!(
        summary.first_try_queue_error,
        Some((1, TIMED_ACTION_REQUIRES_BLOCKING))
    );
    assert_eq!(summary.first_try_queue_prefix_error, None);
    assert_eq!(summary.first_blocking_timing, Some(1));
    assert_eq!(summary.try_queueable_prefix_len, 1);
    assert_eq!(summary.blocking_timing_prefix_len, 1);
    assert!(summary.can_try_queue_prefix());
    assert!(summary.has_blocking_suffix());
    assert_eq!(summary.blocking_suffix_len(), 2);
    assert_eq!(summary.estimated_queue_dispatch_commands, 0);
    assert_eq!(summary.estimated_run_dispatch_commands, 0);
    assert_eq!(summary.estimated_try_queue_dispatch_commands, 0);
    assert_eq!(summary.estimated_try_run_dispatch_commands, 0);
    assert_eq!(summary.estimated_try_queue_prefix_dispatch_commands, 1);
    assert_eq!(summary.estimated_try_run_prefix_dispatch_commands, 2);
    assert!(!summary.queue_dispatch_fits_bound(usize::MAX));
    assert!(!summary.run_dispatch_fits_bound(usize::MAX));
    assert!(!summary.try_queue_dispatch_fits_bound(usize::MAX));
    assert!(!summary.try_run_dispatch_fits_bound(usize::MAX));
    assert!(summary.try_queue_prefix_dispatch_fits_bound(1));
    assert!(!summary.try_queue_prefix_dispatch_fits_bound(0));
    assert!(summary.try_run_prefix_dispatch_fits_bound(2));
    assert!(!summary.try_run_prefix_dispatch_fits_bound(1));

    let blocking_first = AgentAction::plan_summary(&[
        AgentAction::wait(Duration::from_millis(0)),
        AgentAction::tap(10, 20),
    ]);
    assert_eq!(blocking_first.blocking_timing_prefix_len, 0);
    assert_eq!(
        blocking_first.estimated_try_queue_prefix_dispatch_commands,
        0
    );
    assert_eq!(blocking_first.estimated_try_run_prefix_dispatch_commands, 1);
    assert!(blocking_first.try_queue_prefix_dispatch_fits_bound(0));
    assert!(blocking_first.try_run_prefix_dispatch_fits_bound(1));
    assert!(!blocking_first.try_run_prefix_dispatch_fits_bound(0));
}

#[test]
fn agent_action_plan_summary_zeroes_invalid_prefix_dispatch() {
    let malformed = AgentAction::touch_frames_fixed(
        TOUCH_BATCH_FRAMES + 1,
        [AgentTouchFrame::EMPTY; TOUCH_BATCH_FRAMES],
    );
    let actions = [AgentAction::tap(10, 20), malformed];
    let summary = AgentAction::plan_summary(&actions);

    assert_eq!(
        summary.first_structural_error,
        Some((1, "touch frame batch length overflow"))
    );
    assert_eq!(
        summary.first_try_queue_error,
        Some((1, "touch frame batch length overflow"))
    );
    assert_eq!(
        summary.first_try_queue_prefix_error,
        Some((1, "touch frame batch length overflow"))
    );
    assert_eq!(summary.first_blocking_timing, None);
    assert_eq!(summary.try_queueable_prefix_len, 1);
    assert_eq!(summary.blocking_timing_prefix_len, actions.len());
    assert!(!summary.can_try_queue_prefix());
    assert!(!summary.has_blocking_suffix());
    assert_eq!(summary.blocking_suffix_len(), 0);
    assert_eq!(summary.estimated_queue_dispatch_commands, 0);
    assert_eq!(summary.estimated_run_dispatch_commands, 0);
    assert_eq!(summary.estimated_try_queue_dispatch_commands, 0);
    assert_eq!(summary.estimated_try_run_dispatch_commands, 0);
    assert_eq!(summary.estimated_try_queue_prefix_dispatch_commands, 0);
    assert_eq!(summary.estimated_try_run_prefix_dispatch_commands, 0);
    assert!(!summary.try_queue_prefix_dispatch_fits_bound(usize::MAX));
    assert!(!summary.try_run_prefix_dispatch_fits_bound(usize::MAX));
    assert!(!summary.try_run_dispatch_fits_bound(usize::MAX));
}

#[test]
fn agent_action_plan_summary_counts_gamepad_mode_switch_flushes() {
    let frame = GamepadFrameRaw::new(1, 2, 3, 4, 5, 6, 7);
    let mut frames = [GamepadFrameRaw::new(0, 0, 0, 0, 0, 0, 0); GAMEPAD_BATCH_FRAMES];
    frames[0] = frame;
    frames[1] = frame;
    let actions = [
        AgentAction::gamepad_frame(frame),
        AgentAction::gamepad_frame_unchecked(frame),
        AgentAction::gamepad_packed_frame(frame.pack()),
        AgentAction::gamepad_packed_frame_batch_fixed(
            2,
            [[0u8; GAMEPAD_FRAME_BYTES]; GAMEPAD_BATCH_FRAMES],
        ),
        AgentAction::gamepad_frame_batch_fixed(2, frames),
    ];
    let summary = AgentAction::plan_summary(&actions);

    assert!(summary.is_structurally_valid());
    assert_eq!(summary.estimated_queue_dispatch_commands, 4);
    assert_eq!(summary.estimated_run_dispatch_commands, 5);
    assert_eq!(summary.estimated_try_queue_dispatch_commands, 4);
    assert_eq!(summary.estimated_try_run_dispatch_commands, 5);
    assert_eq!(summary.estimated_try_queue_prefix_dispatch_commands, 4);
    assert_eq!(summary.estimated_try_run_prefix_dispatch_commands, 5);
    assert!(summary.try_queue_dispatch_fits_bound(4));
    assert!(!summary.try_queue_dispatch_fits_bound(3));
    assert!(summary.try_run_dispatch_fits_bound(5));
    assert!(!summary.try_run_dispatch_fits_bound(4));
    assert!(summary.try_run_prefix_dispatch_fits_bound(5));
    assert!(!summary.try_run_prefix_dispatch_fits_bound(4));
}

#[test]
fn agent_action_bounded_try_queue_prefix_splits_by_command_bound() {
    let actions = [
        AgentAction::tap(10, 20),
        AgentAction::Flush,
        AgentAction::tap(30, 40),
    ];

    let prefix = AgentAction::bounded_try_queue_prefix(&actions, 2);
    assert_eq!(prefix.action_count, actions.len());
    assert_eq!(prefix.accepted_actions, 2);
    assert_eq!(prefix.estimated_dispatch_commands, 2);
    assert_eq!(prefix.command_bound, 2);
    assert!(!prefix.is_full_plan());
    assert!(!prefix.is_empty());
    assert_eq!(prefix.remaining_actions(), 1);
    assert!(prefix.accepted_dispatch_fits_bound());
    assert_eq!(prefix.accepted_range(), 0..2);
    assert_eq!(prefix.remaining_range(), 2..3);
    assert_eq!(prefix.accepted_slice(&actions), Some(&actions[..2]));
    assert_eq!(prefix.remaining_slice(&actions), Some(&actions[2..]));
    assert_eq!(
        prefix.split_slice(&actions),
        Some((&actions[..2], &actions[2..]))
    );
    assert_eq!(prefix.accepted_slice(&actions[..2]), None);
    assert_eq!(
        prefix.stop,
        AgentPlanBoundedPrefixStop::CommandBound {
            index: 2,
            required_dispatch_commands: 3,
        }
    );
    assert!(prefix.stop.is_command_bound());
    assert!(!prefix.stop.is_end_of_plan());
    assert_eq!(prefix.stop.index(), Some(2));
    assert_eq!(prefix.stop.required_dispatch_commands(), Some(3));
    assert_eq!(prefix.stop.error(), None);

    let full = AgentAction::bounded_try_queue_prefix(&actions, 3);
    assert_eq!(full.accepted_actions, actions.len());
    assert_eq!(full.estimated_dispatch_commands, 3);
    assert_eq!(full.stop, AgentPlanBoundedPrefixStop::EndOfPlan);
    assert!(full.is_full_plan());
    assert!(full.stop.is_end_of_plan());
    assert_eq!(full.stop.index(), None);
    assert_eq!(full.stop.required_dispatch_commands(), None);
    assert_eq!(full.remaining_actions(), 0);
    assert_eq!(full.accepted_range(), 0..3);
    assert_eq!(full.remaining_range(), 3..3);
    assert_eq!(
        full.split_slice(&actions),
        Some((&actions[..], &actions[3..]))
    );
}

#[test]
fn agent_action_bounded_try_queue_prefix_preserves_batching_pressure() {
    let actions = [
        AgentAction::tap(10, 20),
        AgentAction::tap(30, 40),
        AgentAction::tap(50, 60),
    ];

    let prefix = AgentAction::bounded_try_queue_prefix(&actions, 1);
    assert_eq!(prefix.accepted_actions, actions.len());
    assert_eq!(prefix.estimated_dispatch_commands, 1);
    assert_eq!(prefix.stop, AgentPlanBoundedPrefixStop::EndOfPlan);
}

#[test]
fn agent_action_bounded_try_queue_prefix_stops_at_static_rejection() {
    let malformed = AgentAction::touch_frames_fixed(
        TOUCH_BATCH_FRAMES + 1,
        [AgentTouchFrame::EMPTY; TOUCH_BATCH_FRAMES],
    );
    let actions = [AgentAction::tap(10, 20), malformed, AgentAction::Flush];
    let prefix = AgentAction::bounded_try_queue_prefix(&actions, usize::MAX);

    assert_eq!(prefix.accepted_actions, 1);
    assert_eq!(prefix.estimated_dispatch_commands, 1);
    assert_eq!(
        prefix.stop,
        AgentPlanBoundedPrefixStop::TryQueueError {
            index: 1,
            error: "touch frame batch length overflow",
        }
    );
    assert!(prefix.stop.is_try_queue_error());
    assert_eq!(prefix.stop.index(), Some(1));
    assert_eq!(
        prefix.stop.error(),
        Some("touch frame batch length overflow")
    );
    assert_eq!(prefix.stop.required_dispatch_commands(), None);
    assert_eq!(prefix.remaining_actions(), 2);
}

#[test]
fn agent_action_bounded_try_queue_prefix_stops_at_blocking_timing() {
    let actions = [
        AgentAction::tap(10, 20),
        AgentAction::wait(Duration::from_millis(0)),
        AgentAction::tap(30, 40),
    ];
    let prefix = AgentAction::bounded_try_queue_prefix(&actions, usize::MAX);

    assert_eq!(prefix.accepted_actions, 1);
    assert_eq!(prefix.estimated_dispatch_commands, 1);
    assert_eq!(
        prefix.stop,
        AgentPlanBoundedPrefixStop::BlockingTiming { index: 1 }
    );
    assert!(prefix.stop.is_blocking_timing());
    assert_eq!(prefix.stop.index(), Some(1));
    assert_eq!(prefix.stop.error(), None);
    assert_eq!(prefix.stop.required_dispatch_commands(), None);
    assert_eq!(prefix.remaining_actions(), 2);
}

#[test]
fn agent_action_bounded_try_queue_prefix_allows_zero_command_actions() {
    let actions = [
        AgentAction::touch_frames_fixed(0, [AgentTouchFrame::EMPTY; TOUCH_BATCH_FRAMES]),
        AgentAction::Flush,
    ];
    let prefix = AgentAction::bounded_try_queue_prefix(&actions, 0);

    assert_eq!(prefix.accepted_actions, 1);
    assert_eq!(prefix.estimated_dispatch_commands, 0);
    assert_eq!(
        prefix.stop,
        AgentPlanBoundedPrefixStop::CommandBound {
            index: 1,
            required_dispatch_commands: 1,
        }
    );
    assert!(prefix.accepted_dispatch_fits_bound());
}

#[test]
fn agent_action_bounded_try_run_prefix_reserves_checked_barrier() {
    let actions = [
        AgentAction::tap(10, 20),
        AgentAction::Flush,
        AgentAction::tap(30, 40),
    ];

    let prefix = AgentAction::bounded_try_run_prefix(&actions, 3);
    assert_eq!(prefix.command_bound, 3);
    assert_eq!(prefix.accepted_actions, 2);
    assert_eq!(prefix.estimated_dispatch_commands, 2);
    assert_eq!(prefix.estimated_checked_dispatch_commands(), 3);
    assert!(prefix.checked_dispatch_fits_bound());
    assert_eq!(
        prefix.stop,
        AgentPlanBoundedPrefixStop::CommandBound {
            index: 2,
            required_dispatch_commands: 4,
        }
    );

    let full = AgentAction::bounded_try_run_prefix(&actions, 4);
    assert_eq!(full.command_bound, 4);
    assert_eq!(full.accepted_actions, actions.len());
    assert_eq!(full.estimated_dispatch_commands, 3);
    assert_eq!(full.estimated_checked_dispatch_commands(), 4);
    assert_eq!(full.stop, AgentPlanBoundedPrefixStop::EndOfPlan);
    assert!(full.checked_dispatch_fits_bound());

    let no_barrier = AgentAction::bounded_try_run_prefix(&actions, 0);
    assert_eq!(no_barrier.command_bound, 0);
    assert_eq!(no_barrier.accepted_actions, 0);
    assert_eq!(no_barrier.estimated_checked_dispatch_commands(), 1);
    assert!(!no_barrier.checked_dispatch_fits_bound());
    assert_eq!(
        no_barrier.stop,
        AgentPlanBoundedPrefixStop::CommandBound {
            index: 0,
            required_dispatch_commands: 1,
        }
    );
}

#[test]
fn agent_try_queue_actions_bounded_prefix_dispatches_command_bound_prefix() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
    let actions = [
        AgentAction::tap(10, 20),
        AgentAction::Flush,
        AgentAction::tap(30, 40),
    ];

    assert_eq!(agent.command_bound(), DEFAULT_AGENT_COMMAND_BOUND);
    let prefix = agent.try_queue_actions_bounded_prefix(&actions, 2).unwrap();

    assert_eq!(prefix.accepted_actions, 2);
    assert_eq!(prefix.estimated_dispatch_commands, 2);
    assert_eq!(
        prefix.stop,
        AgentPlanBoundedPrefixStop::CommandBound {
            index: 2,
            required_dispatch_commands: 3,
        }
    );
    let closed = agent.close().unwrap();
    assert_eq!(control_message_tags(&closed.transport.bytes), vec![2, 2]);

    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent =
        AgentControlSession::from_parts_with_bound(session, Cursor::new(Vec::new()), 2).unwrap();
    assert_eq!(agent.command_bound(), 2);

    let prefix = agent
        .try_queue_actions_bounded_prefix_with_session_bound(&actions)
        .unwrap();

    assert_eq!(prefix.command_bound, 2);
    assert_eq!(prefix.accepted_actions, 2);
    assert_eq!(prefix.estimated_dispatch_commands, 2);
    assert_eq!(
        prefix.stop,
        AgentPlanBoundedPrefixStop::CommandBound {
            index: 2,
            required_dispatch_commands: 3,
        }
    );
    let closed = agent.close().unwrap();
    assert_eq!(control_message_tags(&closed.transport.bytes), vec![2, 2]);
}

#[test]
fn agent_try_run_actions_bounded_prefix_dispatches_checked_command_bound_prefix() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
    let actions = [
        AgentAction::tap(10, 20),
        AgentAction::Flush,
        AgentAction::tap(30, 40),
    ];

    let prefix = agent.try_run_actions_bounded_prefix(&actions, 3).unwrap();

    assert_eq!(prefix.command_bound, 3);
    assert_eq!(prefix.accepted_actions, 2);
    assert_eq!(prefix.estimated_dispatch_commands, 2);
    assert_eq!(prefix.estimated_checked_dispatch_commands(), 3);
    assert_eq!(
        prefix.stop,
        AgentPlanBoundedPrefixStop::CommandBound {
            index: 2,
            required_dispatch_commands: 4,
        }
    );
    let closed = agent.close().unwrap();
    assert_eq!(control_message_tags(&closed.transport.bytes), vec![2, 2]);

    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent =
        AgentControlSession::from_parts_with_bound(session, Cursor::new(Vec::new()), 3).unwrap();

    let prefix = agent
        .try_run_actions_bounded_prefix_with_session_bound(&actions)
        .unwrap();

    assert_eq!(prefix.command_bound, 3);
    assert_eq!(prefix.accepted_actions, 2);
    assert!(prefix.checked_dispatch_fits_bound());
    let closed = agent.close().unwrap();
    assert_eq!(control_message_tags(&closed.transport.bytes), vec![2, 2]);
}

#[test]
fn agent_try_run_actions_bounded_prefix_rejects_malformed_suffix_without_dispatch() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
    let malformed = AgentAction::key_batch_fixed(
        KEYBOARD_BATCH_FRAMES + 1,
        [KeyboardFrame::EMPTY; KEYBOARD_BATCH_FRAMES],
    );

    let err = agent
        .try_run_actions_bounded_prefix(
            &[AgentAction::tap(10, 20), AgentAction::Flush, malformed],
            3,
        )
        .unwrap_err();

    assert!(matches!(
        err,
        Error::SessionLifecycle("keyboard batch length overflow")
    ));
    let closed = agent.close().unwrap();
    assert!(closed.transport.bytes.is_empty());
}

#[test]
fn agent_bounded_try_queue_prefix_with_session_bound_is_pure() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent =
        AgentControlSession::from_parts_with_bound(session, Cursor::new(Vec::new()), 2).unwrap();
    let actions = [
        AgentAction::tap(10, 20),
        AgentAction::Flush,
        AgentAction::tap(30, 40),
    ];

    let prefix = agent.bounded_try_queue_prefix_with_session_bound(&actions);

    assert_eq!(prefix.command_bound, 2);
    assert_eq!(prefix.accepted_actions, 2);
    assert_eq!(prefix.estimated_dispatch_commands, 2);
    assert_eq!(
        prefix.stop,
        AgentPlanBoundedPrefixStop::CommandBound {
            index: 2,
            required_dispatch_commands: 3,
        }
    );
    let checked_prefix = agent.bounded_try_run_prefix_with_session_bound(&actions);
    assert_eq!(checked_prefix.command_bound, 2);
    assert_eq!(checked_prefix.accepted_actions, 1);
    assert_eq!(checked_prefix.estimated_dispatch_commands, 1);
    assert_eq!(checked_prefix.estimated_checked_dispatch_commands(), 2);
    assert_eq!(
        checked_prefix.stop,
        AgentPlanBoundedPrefixStop::CommandBound {
            index: 1,
            required_dispatch_commands: 3,
        }
    );
    let closed = agent.close().unwrap();
    assert!(closed.transport.bytes.is_empty());
}

#[test]
fn agent_try_queue_actions_bounded_prefix_returns_blocking_boundary() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
    let actions = [
        AgentAction::tap(10, 20),
        AgentAction::wait(Duration::from_millis(0)),
        AgentAction::tap(30, 40),
    ];

    let prefix = agent
        .try_queue_actions_bounded_prefix(&actions, usize::MAX)
        .unwrap();

    assert_eq!(prefix.accepted_actions, 1);
    assert_eq!(
        prefix.stop,
        AgentPlanBoundedPrefixStop::BlockingTiming { index: 1 }
    );
    let closed = agent.close().unwrap();
    assert_eq!(control_message_tags(&closed.transport.bytes), vec![2, 2]);
}

#[test]
fn agent_try_queue_actions_bounded_prefix_rejects_malformed_suffix_without_dispatch() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
    let malformed = AgentAction::touch_frames_fixed(
        TOUCH_BATCH_FRAMES + 1,
        [AgentTouchFrame::EMPTY; TOUCH_BATCH_FRAMES],
    );

    let err = agent
        .try_queue_actions_bounded_prefix(&[AgentAction::tap(10, 20), malformed], usize::MAX)
        .unwrap_err();

    assert!(matches!(
        err,
        Error::SessionLifecycle("touch frame batch length overflow")
    ));
    let closed = agent.close().unwrap();
    assert!(closed.transport.bytes.is_empty());
}

#[test]
fn agent_try_queue_actions_bounded_prefix_rejects_malformed_after_command_bound() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
    let malformed = AgentAction::key_batch_fixed(
        KEYBOARD_BATCH_FRAMES + 1,
        [KeyboardFrame::EMPTY; KEYBOARD_BATCH_FRAMES],
    );

    let err = agent
        .try_queue_actions_bounded_prefix(
            &[AgentAction::tap(10, 20), AgentAction::Flush, malformed],
            1,
        )
        .unwrap_err();

    assert!(matches!(
        err,
        Error::SessionLifecycle("keyboard batch length overflow")
    ));
    let closed = agent.close().unwrap();
    assert!(closed.transport.bytes.is_empty());
}

#[test]
fn agent_try_queue_actions_bounded_prefix_rejects_malformed_after_blocking_boundary() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
    let malformed = AgentAction::key_batch_fixed(
        KEYBOARD_BATCH_FRAMES + 1,
        [KeyboardFrame::EMPTY; KEYBOARD_BATCH_FRAMES],
    );

    let err = agent
        .try_queue_actions_bounded_prefix(
            &[
                AgentAction::tap(10, 20),
                AgentAction::wait(Duration::from_millis(0)),
                malformed,
            ],
            usize::MAX,
        )
        .unwrap_err();

    assert!(matches!(
        err,
        Error::SessionLifecycle("keyboard batch length overflow")
    ));
    let closed = agent.close().unwrap();
    assert!(closed.transport.bytes.is_empty());
}

#[test]
fn agent_try_queue_actions_bounded_prefix_handles_blocking_first_without_dispatch() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

    let prefix = agent
        .try_queue_actions_bounded_prefix(
            &[
                AgentAction::wait(Duration::from_millis(0)),
                AgentAction::tap(10, 20),
            ],
            usize::MAX,
        )
        .unwrap();

    assert_eq!(prefix.accepted_actions, 0);
    assert!(prefix.is_empty());
    assert_eq!(
        prefix.stop,
        AgentPlanBoundedPrefixStop::BlockingTiming { index: 0 }
    );
    let closed = agent.close().unwrap();
    assert!(closed.transport.bytes.is_empty());
}

#[test]
fn agent_try_queue_actions_preflights_timed_actions_without_dispatch() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

    let err = agent
        .try_queue_actions(&[
            AgentAction::tap(10, 20),
            AgentAction::wait(Duration::from_millis(0)),
        ])
        .unwrap_err();

    assert!(matches!(
        err,
        Error::SessionLifecycle("timed action requires queue_actions or run_actions")
    ));
    let closed = agent.close().unwrap();
    assert!(closed.transport.bytes.is_empty());
}

#[test]
fn agent_queue_actions_preflights_structural_errors_without_dispatch() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

    let malformed = AgentAction::touch_frames_fixed(
        TOUCH_BATCH_FRAMES + 1,
        [AgentTouchFrame::EMPTY; TOUCH_BATCH_FRAMES],
    );
    let err = agent
        .queue_actions(&[AgentAction::tap(10, 20), malformed])
        .unwrap_err();

    assert!(matches!(
        err,
        Error::SessionLifecycle("touch frame batch length overflow")
    ));
    let closed = agent.close().unwrap();
    assert!(closed.transport.bytes.is_empty());
}

#[test]
fn agent_try_queue_actions_preflights_structural_errors_without_dispatch() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

    let malformed = AgentAction::key_batch_fixed(
        KEYBOARD_BATCH_FRAMES + 1,
        [KeyboardFrame::EMPTY; KEYBOARD_BATCH_FRAMES],
    );
    let err = agent
        .try_queue_actions(&[AgentAction::tap(10, 20), malformed])
        .unwrap_err();

    assert!(matches!(
        err,
        Error::SessionLifecycle("keyboard batch length overflow")
    ));
    let closed = agent.close().unwrap();
    assert!(closed.transport.bytes.is_empty());
}

#[test]
fn agent_queue_actions_preflights_oversized_launch_app_without_dispatch() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

    let err = agent
        .queue_actions(&[
            AgentAction::tap(10, 20),
            AgentAction::launch_app("a".repeat(256)),
        ])
        .unwrap_err();

    assert!(matches!(
        err,
        Error::SessionLifecycle(LAUNCH_APP_NAME_TOO_LONG)
    ));
    let closed = agent.close().unwrap();
    assert!(closed.transport.bytes.is_empty());
}

#[test]
fn agent_try_run_actions_executes_plan_with_checked_boundary() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

    agent
        .try_run_actions(&[AgentAction::tap(10, 20), AgentAction::tap(30, 40)])
        .unwrap();

    let closed = agent.close().unwrap();
    assert_eq!(
        control_message_tags(&closed.transport.bytes),
        vec![2, 2, 2, 2]
    );
}

#[test]
fn agent_try_run_actions_preflights_timed_actions_without_dispatch() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

    let err = agent
        .try_run_actions(&[
            AgentAction::tap(10, 20),
            AgentAction::wait(Duration::from_millis(0)),
        ])
        .unwrap_err();

    assert!(matches!(
        err,
        Error::SessionLifecycle("timed action requires queue_actions or run_actions")
    ));
    let closed = agent.close().unwrap();
    assert!(closed.transport.bytes.is_empty());
}

#[test]
fn agent_try_run_actions_preflights_command_bound_without_dispatch() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent =
        AgentControlSession::from_parts_with_bound(session, Cursor::new(Vec::new()), 1).unwrap();

    let err = agent
        .try_run_actions(&[AgentAction::tap(10, 20)])
        .unwrap_err();

    assert!(matches!(
        err,
        Error::SessionLifecycle(TRY_RUN_EXCEEDS_COMMAND_BOUND)
    ));
    let closed = agent.close().unwrap();
    assert!(closed.transport.bytes.is_empty());
}

#[test]
fn agent_try_run_actions_surfaces_dispatch_error_after_valid_work() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

    let err = agent
        .try_run_actions(&[
            AgentAction::GamepadButton {
                button: GamepadButton::South,
                pressed: true,
            },
            AgentAction::tap(10, 20),
        ])
        .unwrap_err();

    assert!(matches!(err, Error::SessionLifecycle("gamepad not open")));
    let closed = agent.close().unwrap();
    assert_eq!(control_message_tags(&closed.transport.bytes), vec![2, 2]);
}

#[test]
fn agent_try_queue_actions_prefix_stops_before_blocking_action() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

    let actions = [
        AgentAction::tap(10, 20),
        AgentAction::PressBack,
        AgentAction::wait(Duration::from_millis(0)),
        AgentAction::tap(30, 40),
    ];
    let sent = agent.try_queue_actions_prefix(&actions).unwrap();

    assert_eq!(sent, 2);
    let closed = agent.close().unwrap();
    assert_eq!(control_message_tags(&closed.transport.bytes), vec![2, 2, 0]);
}

#[test]
fn agent_try_queue_actions_prefix_handles_blocking_first_action() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

    let sent = agent
        .try_queue_actions_prefix(&[
            AgentAction::wait(Duration::from_millis(0)),
            AgentAction::tap(10, 20),
        ])
        .unwrap();

    assert_eq!(sent, 0);
    let closed = agent.close().unwrap();
    assert!(closed.transport.bytes.is_empty());
}

#[test]
fn agent_try_queue_actions_prefix_rejects_structural_error_before_dispatch() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

    let malformed = AgentAction::key_batch_fixed(
        KEYBOARD_BATCH_FRAMES + 1,
        [KeyboardFrame::EMPTY; KEYBOARD_BATCH_FRAMES],
    );
    let err = agent
        .try_queue_actions_prefix(&[AgentAction::tap(10, 20), malformed])
        .unwrap_err();

    assert!(matches!(
        err,
        Error::SessionLifecycle("keyboard batch length overflow")
    ));
    let closed = agent.close().unwrap();
    assert!(closed.transport.bytes.is_empty());
}

#[test]
fn agent_try_queue_actions_prefix_leaves_blocking_suffix_uninspected() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

    let malformed = AgentAction::key_batch_fixed(
        KEYBOARD_BATCH_FRAMES + 1,
        [KeyboardFrame::EMPTY; KEYBOARD_BATCH_FRAMES],
    );
    let sent = agent
        .try_queue_actions_prefix(&[
            AgentAction::tap(10, 20),
            AgentAction::wait(Duration::from_millis(0)),
            malformed,
        ])
        .unwrap();

    assert_eq!(sent, 1);
    let closed = agent.close().unwrap();
    assert_eq!(control_message_tags(&closed.transport.bytes), vec![2, 2]);
}

#[test]
fn agent_try_run_actions_prefix_stops_before_blocking_action() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

    let actions = [
        AgentAction::tap(10, 20),
        AgentAction::PressBack,
        AgentAction::wait(Duration::from_millis(0)),
        AgentAction::tap(30, 40),
    ];
    let sent = agent.try_run_actions_prefix(&actions).unwrap();

    assert_eq!(sent, 2);
    let closed = agent.close().unwrap();
    assert_eq!(control_message_tags(&closed.transport.bytes), vec![2, 2, 0]);
}

#[test]
fn agent_try_run_actions_prefix_handles_blocking_first_action() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

    let sent = agent
        .try_run_actions_prefix(&[
            AgentAction::wait(Duration::from_millis(0)),
            AgentAction::tap(10, 20),
        ])
        .unwrap();

    assert_eq!(sent, 0);
    let closed = agent.close().unwrap();
    assert!(closed.transport.bytes.is_empty());
}

#[test]
fn agent_try_run_actions_prefix_rejects_structural_error_before_dispatch() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

    let malformed = AgentAction::key_batch_fixed(
        KEYBOARD_BATCH_FRAMES + 1,
        [KeyboardFrame::EMPTY; KEYBOARD_BATCH_FRAMES],
    );
    let err = agent
        .try_run_actions_prefix(&[AgentAction::tap(10, 20), malformed])
        .unwrap_err();

    assert!(matches!(
        err,
        Error::SessionLifecycle("keyboard batch length overflow")
    ));
    let closed = agent.close().unwrap();
    assert!(closed.transport.bytes.is_empty());
}

#[test]
fn agent_try_run_actions_prefix_leaves_blocking_suffix_uninspected() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

    let malformed = AgentAction::key_batch_fixed(
        KEYBOARD_BATCH_FRAMES + 1,
        [KeyboardFrame::EMPTY; KEYBOARD_BATCH_FRAMES],
    );
    let sent = agent
        .try_run_actions_prefix(&[
            AgentAction::tap(10, 20),
            AgentAction::wait(Duration::from_millis(0)),
            malformed,
        ])
        .unwrap();

    assert_eq!(sent, 1);
    let closed = agent.close().unwrap();
    assert_eq!(control_message_tags(&closed.transport.bytes), vec![2, 2]);
}

#[test]
fn agent_try_run_actions_prefix_preflights_command_bound_without_dispatch() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent =
        AgentControlSession::from_parts_with_bound(session, Cursor::new(Vec::new()), 1).unwrap();

    let err = agent
        .try_run_actions_prefix(&[
            AgentAction::tap(10, 20),
            AgentAction::wait(Duration::from_millis(0)),
        ])
        .unwrap_err();

    assert!(matches!(
        err,
        Error::SessionLifecycle(TRY_RUN_EXCEEDS_COMMAND_BOUND)
    ));
    let closed = agent.close().unwrap();
    assert!(closed.transport.bytes.is_empty());
}

#[test]
fn agent_try_run_actions_prefix_surfaces_dispatch_error_after_prefix_work() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

    let err = agent
        .try_run_actions_prefix(&[
            AgentAction::GamepadButton {
                button: GamepadButton::South,
                pressed: true,
            },
            AgentAction::wait(Duration::from_millis(0)),
        ])
        .unwrap_err();

    assert!(matches!(err, Error::SessionLifecycle("gamepad not open")));
    let closed = agent.close().unwrap();
    assert!(closed.transport.bytes.is_empty());
}

#[test]
fn agent_run_actions_surfaces_dispatch_error_after_valid_work() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

    let err = agent
        .run_actions(&[
            AgentAction::Key {
                scancode: 0x04,
                pressed: true,
                mods: Modifiers::empty(),
            },
            AgentAction::tap(10, 20),
        ])
        .unwrap_err();

    assert!(matches!(err, Error::SessionLifecycle("keyboard not open")));
    let closed = agent.close().unwrap();
    assert_eq!(count_touch_events(&closed.transport.bytes), 2);
}

#[test]
fn agent_run_actions_type_text_strict_preflights_unsupported_char_without_dispatch() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::kbd_only()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

    let err = agent
        .run_actions(&[
            AgentAction::type_text_strict("A中"),
            AgentAction::type_text("z"),
        ])
        .unwrap_err();

    assert!(matches!(
        err,
        Error::SessionLifecycle("unsupported char in type_text_strict")
    ));
    let closed = agent.close().unwrap();
    assert_eq!(count_uhid_inputs(&closed.transport.bytes), 0);
}

#[test]
fn agent_actions_cover_typed_keyboard_tap_helpers() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::kbd_only()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

    agent
        .run_actions(&[
            AgentAction::tap_scancode(Scancode::A, Modifiers::LSHIFT),
            AgentAction::key_scancode(Scancode::B, true, Modifiers::empty()),
            AgentAction::key_scancode(Scancode::B, false, Modifiers::empty()),
        ])
        .unwrap();

    let closed = agent.close().unwrap();
    assert_eq!(count_uhid_inputs(&closed.transport.bytes), 4);
}

#[test]
fn agent_run_actions_batches_consecutive_keyboard_actions() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::kbd_only()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

    agent
        .run_actions(&[
            AgentAction::tap_scancode(Scancode::A, Modifiers::LSHIFT),
            AgentAction::key_scancode(Scancode::B, true, Modifiers::empty()),
            AgentAction::key_scancode(Scancode::B, false, Modifiers::empty()),
        ])
        .unwrap();

    let closed = agent.close().unwrap();
    assert_eq!(count_uhid_inputs(&closed.transport.bytes), 4);
    assert_eq!(input_and_touch_tags(&closed.transport.bytes), vec![13; 4]);
}

#[test]
fn agent_run_actions_flushes_keyboard_before_touch_actions() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::kbd_only()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

    agent
        .run_actions(&[
            AgentAction::tap_scancode(Scancode::A, Modifiers::LSHIFT),
            AgentAction::tap(10, 20),
            AgentAction::tap_scancode(Scancode::B, Modifiers::empty()),
        ])
        .unwrap();

    let closed = agent.close().unwrap();
    assert_eq!(
        input_and_touch_tags(&closed.transport.bytes),
        vec![13, 13, 2, 2, 13, 13]
    );
}

#[test]
fn agent_try_queue_actions_batches_keyboard_with_tiny_bound() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::kbd_only()).unwrap();
    let agent =
        AgentControlSession::from_parts_with_bound(session, Cursor::new(Vec::new()), 1).unwrap();

    agent
        .try_queue_actions(&[
            AgentAction::tap_scancode(Scancode::A, Modifiers::LSHIFT),
            AgentAction::key_scancode(Scancode::B, true, Modifiers::empty()),
            AgentAction::key_scancode(Scancode::B, false, Modifiers::empty()),
        ])
        .unwrap();

    let closed = agent.close().unwrap();
    assert_eq!(count_uhid_inputs(&closed.transport.bytes), 4);
}

#[test]
fn agent_keyboard_frame_batch_action_dispatches_fixed_batch() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::kbd_only()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
    let frames = [
        KeyboardFrame::scancode_down(Scancode::A, Modifiers::LSHIFT),
        KeyboardFrame::scancode_up(Scancode::A),
        KeyboardFrame::scancode_down(Scancode::B, Modifiers::empty()),
        KeyboardFrame::scancode_up(Scancode::B),
    ];

    agent
        .run_actions(&[AgentAction::try_key_batch(&frames).unwrap()])
        .unwrap();

    let closed = agent.close().unwrap();
    assert_eq!(count_uhid_inputs(&closed.transport.bytes), frames.len());
}

#[test]
fn agent_keyboard_chord_action_dispatches_fixed_batch() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::kbd_only()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

    agent
        .run_actions(&[
            AgentAction::ctrl_scancode(Scancode::C),
            AgentAction::ctrl_shift_scancode(Scancode::V),
        ])
        .unwrap();

    let closed = agent.close().unwrap();
    assert_eq!(count_uhid_inputs(&closed.transport.bytes), 4);
    assert_eq!(input_and_touch_tags(&closed.transport.bytes), vec![13; 4]);
}

#[test]
fn agent_run_actions_batches_keyboard_chords_with_adjacent_keys() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::kbd_only()).unwrap();
    let agent =
        AgentControlSession::from_parts_with_bound(session, Cursor::new(Vec::new()), 1).unwrap();

    agent
        .try_queue_actions(&[
            AgentAction::tap_scancode(Scancode::A, Modifiers::LSHIFT),
            AgentAction::try_scancode_chord(&[Scancode::K, Scancode::C], Modifiers::LCTRL).unwrap(),
            AgentAction::key_scancode(Scancode::B, true, Modifiers::empty()),
            AgentAction::key_scancode(Scancode::B, false, Modifiers::empty()),
        ])
        .unwrap();

    let closed = agent.close().unwrap();
    assert_eq!(count_uhid_inputs(&closed.transport.bytes), 8);
    assert_eq!(input_and_touch_tags(&closed.transport.bytes), vec![13; 8]);
}

#[test]
fn agent_keyboard_batch_constructor_rejects_oversized_slices() {
    let frames = vec![KeyboardFrame::EMPTY; KEYBOARD_BATCH_FRAMES + 1];
    assert!(matches!(
        AgentAction::try_key_batch(&frames),
        Err(Error::SessionLifecycle("keyboard batch too large"))
    ));
}

#[test]
fn agent_keyboard_chord_constructor_rejects_invalid_slices() {
    let frames = vec![Scancode::A; crate::client::KEYBOARD_CHORD_KEYS + 1];
    assert!(matches!(
        AgentAction::try_scancode_chord(&frames, Modifiers::LCTRL),
        Err(Error::SessionLifecycle("keyboard chord too large"))
    ));
    assert!(matches!(
        AgentAction::try_scancode_chord(&[Scancode::LeftCtrl], Modifiers::empty()),
        Err(Error::SessionLifecycle(
            "keyboard chord keys must be non-modifier scancodes"
        ))
    ));

    let malformed = AgentAction::keyboard_chord_fixed(KeyboardChordFrame::new(
        (crate::client::KEYBOARD_CHORD_KEYS + 1) as u8,
        [Scancode::A.to_u8(); crate::client::KEYBOARD_CHORD_KEYS],
        Modifiers::LCTRL,
    ));
    let session = HidSession::open(MockTransport::new(), OpenRequest::kbd_only()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
    assert!(matches!(
        agent.run_actions(&[malformed]),
        Err(Error::SessionLifecycle("keyboard chord length overflow"))
    ));
}

#[test]
fn agent_queue_actions_defers_errors_until_checked_boundary() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

    agent
        .queue_actions(&[
            AgentAction::Key {
                scancode: 0x04,
                pressed: true,
                mods: Modifiers::empty(),
            },
            AgentAction::tap(10, 20),
        ])
        .unwrap();
    let report = agent.close_checked().unwrap();

    assert!(matches!(
        report.command_result,
        Err(Error::SessionLifecycle("keyboard not open"))
    ));
    assert_eq!(count_touch_events(&report.closed.transport.bytes), 2);
}

#[test]
fn agent_actions_cover_gamepad_and_clipboard_commands() {
    let session =
        HidSession::open(MockTransport::new(), OpenRequest::gamepad_only_realtime()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

    agent
        .run_actions(&[
            AgentAction::GamepadButton {
                button: GamepadButton::South,
                pressed: true,
            },
            AgentAction::GamepadButtons {
                buttons: GamepadButton::South as u32 | GamepadButton::DpadUp as u32,
            },
            AgentAction::GamepadFrameUnchecked {
                frame: GamepadFrameRaw::new(1, 2, 3, 4, 5, 6, 7),
            },
            AgentAction::set_clipboard("agent plan", false),
            AgentAction::request_clipboard_key(ClipboardCopyKey::COPY),
            AgentAction::configure_ai(
                crate::control::AI_FLAG_KEYFRAMES | crate::control::AI_FLAG_OBJECTS,
                16,
                0,
            ),
            AgentAction::query_ai(0x0102_0304_0506_0708),
            AgentAction::pause_ai(),
        ])
        .unwrap();

    let closed = agent.close().unwrap();
    let bytes = closed.transport.bytes;
    assert_eq!(count_uhid_inputs(&bytes), 3);
    assert!(
        find_control_message(&bytes, 9).is_some(),
        "SET_CLIPBOARD tag should be present"
    );
    let get_clipboard = find_control_message(&bytes, 8).expect("GET_CLIPBOARD frame");
    assert_eq!(get_clipboard[1], 1);
    let ai_config = find_control_message(&bytes, 22).expect("AI_CONFIG frame");
    assert_eq!(
        ai_config,
        &[
            22,
            crate::control::AI_FLAG_KEYFRAMES | crate::control::AI_FLAG_OBJECTS,
            0,
            16,
            0,
            0
        ]
    );
    let ai_query = find_control_message(&bytes, 23).expect("AI_QUERY frame");
    assert_eq!(
        u64::from_be_bytes(ai_query[1..9].try_into().unwrap()),
        0x0102_0304_0506_0708
    );
    assert_eq!(find_control_message(&bytes, 24), Some(&[24][..]));
}

#[test]
fn agent_gamepad_helpers_emit_uhid_reports() {
    let session =
        HidSession::open(MockTransport::new(), OpenRequest::gamepad_only_realtime()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

    agent.send_button(GamepadButton::South, true).unwrap();
    agent.send_button(GamepadButton::South, false).unwrap();
    agent
        .send_buttons(GamepadButton::South as u32 | GamepadButton::DpadUp as u32)
        .unwrap();
    agent.send_stick_raw(GamepadAxis::LeftX, 123).unwrap();
    agent.send_stick(GamepadAxis::RightY, 0.5).unwrap();
    agent.send_left_stick_raw(7, -7).unwrap();
    agent.send_right_stick_raw(-8, 8).unwrap();
    agent.send_triggers_raw(9, 10).unwrap();
    agent.send_sticks_raw(11, 12, 13, 14, 15, 16).unwrap();
    agent
        .send_frame_unchecked(GamepadFrameRaw::new(1, 2, 3, 4, 5, 6, 7))
        .unwrap();
    agent
        .send_frame_packed(GamepadFrameRaw::new(2, 3, 4, 5, 6, 7, 8).pack())
        .unwrap();

    let closed = agent.close().unwrap();
    assert_eq!(count_uhid_inputs(&closed.transport.bytes), 11);
}

#[test]
fn agent_try_gamepad_helpers_use_nonblocking_checked_dispatch() {
    let session =
        HidSession::open(MockTransport::new(), OpenRequest::gamepad_only_realtime()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

    agent.try_send_button(GamepadButton::South, true).unwrap();
    agent.try_send_button(GamepadButton::South, false).unwrap();
    agent
        .try_send_buttons(GamepadButton::South as u32 | GamepadButton::DpadUp as u32)
        .unwrap();
    agent.try_send_stick_raw(GamepadAxis::LeftX, 123).unwrap();
    agent.try_send_stick(GamepadAxis::RightY, 0.5).unwrap();
    agent.try_send_left_stick_raw(7, -7).unwrap();
    agent.try_send_right_stick_raw(-8, 8).unwrap();
    agent.try_send_triggers_raw(9, 10).unwrap();
    agent.try_send_sticks_raw(11, 12, 13, 14, 15, 16).unwrap();
    agent
        .try_send_frame_unchecked(GamepadFrameRaw::new(1, 2, 3, 4, 5, 6, 7))
        .unwrap();
    agent
        .try_send_frame(GamepadFrameRaw::new(2, 3, 4, 5, 6, 7, 8))
        .unwrap();
    agent
        .try_send_frame_packed(GamepadFrameRaw::new(3, 4, 5, 6, 7, 8, 9).pack())
        .unwrap();

    let closed = agent.close().unwrap();
    assert_eq!(count_uhid_inputs(&closed.transport.bytes), 12);
}

#[test]
fn agent_try_gamepad_fixed_batches_use_nonblocking_checked_dispatch() {
    let session =
        HidSession::open(MockTransport::new(), OpenRequest::gamepad_only_realtime()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
    let mut frames = [GamepadFrameRaw::new(0, 0, 0, 0, 0, 0, 0); GAMEPAD_BATCH_FRAMES];
    frames[0] = GamepadFrameRaw::new(1, 10, 20, 30, 40, 50, 60);
    frames[1] = GamepadFrameRaw::new(2, 11, 21, 31, 41, 51, 61);
    let mut packed = [[0u8; GAMEPAD_FRAME_BYTES]; GAMEPAD_BATCH_FRAMES];
    packed[0] = GamepadFrameRaw::new(3, 12, 22, 32, 42, 52, 62).pack();
    packed[1] = GamepadFrameRaw::new(4, 13, 23, 33, 43, 53, 63).pack();

    agent.try_send_frame_batch_fixed(2, frames).unwrap();
    agent
        .try_send_frame_batch_fixed_unchecked(2, frames)
        .unwrap();
    agent.try_send_frame_packed_batch_fixed(2, packed).unwrap();

    let closed = agent.close().unwrap();
    assert_eq!(count_uhid_inputs(&closed.transport.bytes), 6);
}

#[test]
fn agent_try_gamepad_preflights_command_bound_without_dispatch() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent =
        AgentControlSession::from_parts_with_bound(session, Cursor::new(Vec::new()), 1).unwrap();

    let err = agent
        .try_send_button(GamepadButton::South, true)
        .unwrap_err();

    assert!(matches!(
        err,
        Error::SessionLifecycle(TRY_GAMEPAD_EXCEEDS_COMMAND_BOUND)
    ));
    let closed = agent.close().unwrap();
    assert!(closed.transport.bytes.is_empty());
}

#[test]
fn agent_gamepad_frame_batch_action_dispatches_fixed_batch() {
    let session =
        HidSession::open(MockTransport::new(), OpenRequest::gamepad_only_realtime()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
    let frames = [
        GamepadFrameRaw::new(1, 10, 20, 30, 40, 50, 60),
        GamepadFrameRaw::new(2, 11, 21, 31, 41, 51, 61),
        GamepadFrameRaw::new(3, 12, 22, 32, 42, 52, 62),
    ];

    let action = AgentAction::try_gamepad_frame_batch(&frames).unwrap();
    agent.run_actions(&[action]).unwrap();

    let closed = agent.close().unwrap();
    assert_eq!(count_uhid_inputs(&closed.transport.bytes), 3);
}

#[test]
fn agent_gamepad_unchecked_batch_preserves_duplicate_frames() {
    let session =
        HidSession::open(MockTransport::new(), OpenRequest::gamepad_only_realtime()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
    let frame = GamepadFrameRaw::new(1, 2, 3, 4, 5, 6, 7);
    let action = AgentAction::try_gamepad_frame_batch_unchecked(&[frame, frame]).unwrap();

    agent.run_actions(&[action]).unwrap();

    let closed = agent.close().unwrap();
    assert_eq!(count_uhid_inputs(&closed.transport.bytes), 2);
}

#[test]
fn agent_gamepad_packed_batch_action_dispatches_fixed_batch() {
    let session =
        HidSession::open(MockTransport::new(), OpenRequest::gamepad_only_realtime()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
    let frames = [
        GamepadFrameRaw::new(1, 10, 20, 30, 40, 50, 60).pack(),
        GamepadFrameRaw::new(2, 11, 21, 31, 41, 51, 61).pack(),
    ];

    let action = AgentAction::try_gamepad_packed_frame_batch(&frames).unwrap();
    agent.run_actions(&[action]).unwrap();

    let closed = agent.close().unwrap();
    assert_eq!(count_uhid_inputs(&closed.transport.bytes), 2);
}

#[test]
fn agent_run_actions_batches_consecutive_gamepad_unchecked_frames() {
    let session =
        HidSession::open(MockTransport::new(), OpenRequest::gamepad_only_realtime()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
    let batch = [
        GamepadFrameRaw::new(2, 11, 21, 31, 41, 51, 61),
        GamepadFrameRaw::new(3, 12, 22, 32, 42, 52, 62),
    ];

    agent
        .run_actions(&[
            AgentAction::gamepad_frame_unchecked(GamepadFrameRaw::new(1, 10, 20, 30, 40, 50, 60)),
            AgentAction::try_gamepad_frame_batch_unchecked(&batch).unwrap(),
            AgentAction::gamepad_frame_unchecked(GamepadFrameRaw::new(4, 13, 23, 33, 43, 53, 63)),
        ])
        .unwrap();

    let closed = agent.close().unwrap();
    assert_eq!(count_uhid_inputs(&closed.transport.bytes), 4);
    assert_eq!(input_and_touch_tags(&closed.transport.bytes), vec![13; 4]);
}

#[test]
fn agent_run_actions_flushes_gamepad_before_touch_actions() {
    let session =
        HidSession::open(MockTransport::new(), OpenRequest::gamepad_only_realtime()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

    agent
        .run_actions(&[
            AgentAction::gamepad_frame_unchecked(GamepadFrameRaw::new(1, 2, 3, 4, 5, 6, 7)),
            AgentAction::tap(10, 20),
            AgentAction::gamepad_frame_unchecked(GamepadFrameRaw::new(8, 9, 10, 11, 12, 13, 14)),
        ])
        .unwrap();

    let closed = agent.close().unwrap();
    assert_eq!(
        input_and_touch_tags(&closed.transport.bytes),
        vec![13, 2, 2, 13]
    );
}

#[test]
fn agent_try_queue_actions_batches_gamepad_with_tiny_bound() {
    let session =
        HidSession::open(MockTransport::new(), OpenRequest::gamepad_only_realtime()).unwrap();
    let agent =
        AgentControlSession::from_parts_with_bound(session, Cursor::new(Vec::new()), 1).unwrap();

    agent
        .try_queue_actions(&[
            AgentAction::gamepad_frame_unchecked(GamepadFrameRaw::new(1, 10, 20, 30, 40, 50, 60)),
            AgentAction::gamepad_frame_unchecked(GamepadFrameRaw::new(2, 11, 21, 31, 41, 51, 61)),
            AgentAction::gamepad_frame_unchecked(GamepadFrameRaw::new(3, 12, 22, 32, 42, 52, 62)),
        ])
        .unwrap();

    let closed = agent.close().unwrap();
    assert_eq!(count_uhid_inputs(&closed.transport.bytes), 3);
}

#[test]
fn agent_run_actions_flushes_gamepad_on_mode_switch() {
    let session =
        HidSession::open(MockTransport::new(), OpenRequest::gamepad_only_realtime()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
    let packed = GamepadFrameRaw::new(2, 11, 21, 31, 41, 51, 61).pack();

    agent
        .run_actions(&[
            AgentAction::gamepad_frame_unchecked(GamepadFrameRaw::new(1, 10, 20, 30, 40, 50, 60)),
            AgentAction::gamepad_packed_frame(packed),
            AgentAction::gamepad_frame_unchecked(GamepadFrameRaw::new(3, 12, 22, 32, 42, 52, 62)),
        ])
        .unwrap();

    let closed = agent.close().unwrap();
    assert_eq!(count_uhid_inputs(&closed.transport.bytes), 3);
    assert_eq!(input_and_touch_tags(&closed.transport.bytes), vec![13; 3]);
}

#[test]
fn agent_gamepad_batch_constructors_reject_oversized_slices() {
    let frame = GamepadFrameRaw::new(1, 2, 3, 4, 5, 6, 7);
    let frames = vec![frame; GAMEPAD_BATCH_FRAMES + 1];
    assert!(matches!(
        AgentAction::try_gamepad_frame_batch(&frames),
        Err(Error::SessionLifecycle("gamepad frame batch too large"))
    ));
    assert!(matches!(
        AgentAction::try_gamepad_frame_batch_unchecked(&frames),
        Err(Error::SessionLifecycle("gamepad frame batch too large"))
    ));

    let packed = vec![[0u8; GAMEPAD_FRAME_BYTES]; GAMEPAD_BATCH_FRAMES + 1];
    assert!(matches!(
        AgentAction::try_gamepad_packed_frame_batch(&packed),
        Err(Error::SessionLifecycle(
            "gamepad packed frame batch too large"
        ))
    ));
}

#[test]
fn agent_typed_clipboard_copy_key_helpers_emit_get_clipboard() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let mut agent =
        AgentControlSession::from_parts(session, Cursor::new(clipboard("typed"))).unwrap();

    agent.request_clipboard_key(ClipboardCopyKey::CUT).unwrap();
    let text = agent
        .get_clipboard_and_wait_key(ClipboardCopyKey::COPY)
        .unwrap();
    assert_eq!(text, "typed");

    let closed = agent.close().unwrap();
    assert_eq!(closed.transport.bytes, vec![8, 2, 8, 1]);
}

#[test]
fn agent_scroll_helpers_emit_inject_scroll_with_screen_size() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

    agent.set_screen_size(720, 1280).unwrap();
    agent
        .scroll_with_buttons(100, 200, 8.0, -16.0, 0x11)
        .unwrap();
    agent
        .run_actions(&[AgentAction::scroll(300, 400, 0, 16)])
        .unwrap();

    let closed = agent.close().unwrap();
    let bytes = closed.transport.bytes;
    assert_eq!(control_message_tags(&bytes), vec![3, 3]);
    let first = &bytes[0..21];
    assert_eq!(i32::from_be_bytes(first[1..5].try_into().unwrap()), 100);
    assert_eq!(i32::from_be_bytes(first[5..9].try_into().unwrap()), 200);
    assert_eq!(u16::from_be_bytes(first[9..11].try_into().unwrap()), 720);
    assert_eq!(u16::from_be_bytes(first[11..13].try_into().unwrap()), 1280);
    assert_eq!(
        u16::from_be_bytes(first[13..15].try_into().unwrap()),
        0x4000
    );
    assert_eq!(
        u16::from_be_bytes(first[15..17].try_into().unwrap()),
        0x8000
    );
    assert_eq!(u32::from_be_bytes(first[17..21].try_into().unwrap()), 0x11);
    let second = &bytes[21..42];
    assert_eq!(i32::from_be_bytes(second[1..5].try_into().unwrap()), 300);
    assert_eq!(i32::from_be_bytes(second[5..9].try_into().unwrap()), 400);
    assert_eq!(u16::from_be_bytes(second[9..11].try_into().unwrap()), 720);
    assert_eq!(u16::from_be_bytes(second[11..13].try_into().unwrap()), 1280);
    assert_eq!(u16::from_be_bytes(second[13..15].try_into().unwrap()), 0);
    assert_eq!(
        u16::from_be_bytes(second[15..17].try_into().unwrap()),
        0x7FFF
    );
}

#[test]
fn agent_scroll_batch_action_dispatches_fixed_batch() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
    let frames = [
        AgentScrollFrame::new(100, 200, 8, -16, 0x11),
        AgentScrollFrame::scroll(300, 400, 0, 16),
        AgentScrollFrame::new(500, 600, -1, 1, 0x22),
    ];

    agent
        .run_actions(&[AgentAction::try_scroll_batch(&frames).unwrap()])
        .unwrap();

    let closed = agent.close().unwrap();
    let bytes = closed.transport.bytes;
    assert_eq!(control_message_tags(&bytes), vec![3, 3, 3]);
    let first = &bytes[0..21];
    assert_eq!(i32::from_be_bytes(first[1..5].try_into().unwrap()), 100);
    assert_eq!(i32::from_be_bytes(first[5..9].try_into().unwrap()), 200);
    assert_eq!(u32::from_be_bytes(first[17..21].try_into().unwrap()), 0x11);
    let third = &bytes[42..63];
    assert_eq!(i32::from_be_bytes(third[1..5].try_into().unwrap()), 500);
    assert_eq!(u32::from_be_bytes(third[17..21].try_into().unwrap()), 0x22);
}

#[test]
fn agent_run_actions_batches_consecutive_scroll_actions() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();
    let frames = [
        AgentScrollFrame::scroll(300, 400, 0, 16),
        AgentScrollFrame::new(500, 600, -1, 1, 0x22),
    ];

    agent
        .run_actions(&[
            AgentAction::scroll(100, 200, 8, -16),
            AgentAction::scroll_with_buttons(150, 250, 1, -1, 0x11),
            AgentAction::try_scroll_batch(&frames).unwrap(),
        ])
        .unwrap();

    let closed = agent.close().unwrap();
    let bytes = closed.transport.bytes;
    assert_eq!(control_message_tags(&bytes), vec![3, 3, 3, 3]);
    let xs: Vec<_> = bytes
        .chunks_exact(21)
        .map(|frame| i32::from_be_bytes(frame[1..5].try_into().unwrap()))
        .collect();
    assert_eq!(xs, vec![100, 150, 300, 500]);
}

#[test]
fn agent_run_actions_flushes_scroll_before_touch_actions() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent = AgentControlSession::from_parts(session, Cursor::new(Vec::new())).unwrap();

    agent
        .run_actions(&[
            AgentAction::scroll(100, 200, 0, 1),
            AgentAction::tap(10, 20),
            AgentAction::scroll(300, 400, 0, -1),
        ])
        .unwrap();

    let closed = agent.close().unwrap();
    assert_eq!(
        control_message_tags(&closed.transport.bytes),
        vec![3, 2, 2, 3]
    );
}

#[test]
fn agent_try_queue_actions_batches_scroll_with_tiny_bound() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let agent =
        AgentControlSession::from_parts_with_bound(session, Cursor::new(Vec::new()), 1).unwrap();
    let frames = [
        AgentScrollFrame::scroll(300, 400, 0, 16),
        AgentScrollFrame::new(500, 600, -1, 1, 0x22),
    ];

    agent
        .try_queue_actions(&[
            AgentAction::scroll(100, 200, 8, -16),
            AgentAction::scroll_with_buttons(150, 250, 1, -1, 0x11),
            AgentAction::try_scroll_batch(&frames).unwrap(),
        ])
        .unwrap();

    let closed = agent.close().unwrap();
    assert_eq!(control_message_tags(&closed.transport.bytes), vec![3; 4]);
}

#[test]
fn agent_scroll_batch_constructor_rejects_oversized_slices() {
    let frames = vec![AgentScrollFrame::EMPTY; SCROLL_BATCH_FRAMES + 1];
    assert!(matches!(
        AgentAction::try_scroll_batch(&frames),
        Err(Error::SessionLifecycle("scroll batch too large"))
    ));
}

#[test]
fn set_clipboard_and_wait_ack_uses_matching_sequence() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let mut agent = AgentControlSession::from_parts(session, clipboard_ack_stream(1)).unwrap();

    let sequence = agent
        .set_clipboard_and_wait_ack("agent-copy", false)
        .unwrap();
    assert_eq!(sequence, 1);

    let closed = agent.close().unwrap();
    let bytes = closed.transport.bytes;
    let set_clipboard = bytes
        .iter()
        .position(|b| *b == 9)
        .expect("SET_CLIPBOARD frame");
    assert_eq!(
        u64::from_be_bytes(
            bytes[set_clipboard + 1..set_clipboard + 9]
                .try_into()
                .unwrap()
        ),
        1
    );
    assert_eq!(closed.reader.position(), 9);
}

#[test]
fn wait_for_clipboard_ack_skips_unrelated_messages() {
    let mut stream = ack(7);
    stream.extend(ack(8));
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let mut agent = AgentControlSession::from_parts(session, Cursor::new(stream)).unwrap();

    agent.wait_for_clipboard_ack(8).unwrap();
    let closed = agent.close().unwrap();
    assert_eq!(closed.reader.position(), 18);
}

#[test]
fn get_clipboard_and_wait_returns_clipboard_payload() {
    let mut stream = ack(99);
    stream.extend(clipboard("device text"));
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let mut agent = AgentControlSession::from_parts(session, Cursor::new(stream)).unwrap();

    let text = agent.get_clipboard_and_wait(1).unwrap();
    assert_eq!(text, "device text");

    let closed = agent.close().unwrap();
    let bytes = closed.transport.bytes;
    let get_clipboard = bytes
        .iter()
        .position(|b| *b == 8)
        .expect("GET_CLIPBOARD frame");
    assert_eq!(bytes[get_clipboard + 1], 1);
    assert_eq!(closed.reader.position(), 9 + 5 + "device text".len() as u64);
}

#[test]
fn run_actions_and_get_clipboard_and_wait_key_flushes_then_reads() {
    let mut stream = ack(99);
    stream.extend(clipboard("copied text"));
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let mut agent = AgentControlSession::from_parts(session, Cursor::new(stream)).unwrap();

    let text = agent
        .run_actions_and_get_clipboard_and_wait_key(
            &[AgentAction::tap(10, 20)],
            ClipboardCopyKey::COPY,
        )
        .unwrap();
    assert_eq!(text, "copied text");

    let closed = agent.close().unwrap();
    let bytes = closed.transport.bytes;
    let tags = control_message_tags(&bytes);
    assert_eq!(tags, [2, 2, 8]);
    let get_clipboard = find_control_message(&bytes, 8).expect("GET_CLIPBOARD frame");
    assert_eq!(get_clipboard, &[8, ClipboardCopyKey::COPY.value()]);
    assert_eq!(closed.reader.position(), 9 + 5 + "copied text".len() as u64);
}

#[test]
fn run_actions_and_set_clipboard_and_wait_ack_uses_matching_sequence() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let mut agent = AgentControlSession::from_parts(session, clipboard_ack_stream(1)).unwrap();

    let sequence = agent
        .run_actions_and_set_clipboard_and_wait_ack(
            &[AgentAction::tap(10, 20)],
            "queued clipboard",
            true,
        )
        .unwrap();
    assert_eq!(sequence, 1);

    let closed = agent.close().unwrap();
    let bytes = closed.transport.bytes;
    let tags = control_message_tags(&bytes);
    assert_eq!(tags, [2, 2, 9]);
    let set_clipboard = find_control_message(&bytes, 9).expect("SET_CLIPBOARD frame");
    assert_eq!(
        u64::from_be_bytes(set_clipboard[1..9].try_into().unwrap()),
        sequence
    );
    assert_eq!(set_clipboard[9], 1);
    assert_eq!(closed.reader.position(), 9);
}

#[test]
fn wait_for_clipboard_skips_ack_messages() {
    let mut stream = ack(1);
    stream.extend(ack(2));
    stream.extend(clipboard("later"));
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let mut agent = AgentControlSession::from_parts(session, Cursor::new(stream)).unwrap();

    assert_eq!(agent.wait_for_clipboard().unwrap(), "later");
}

#[test]
fn wait_for_clipboard_maps_reader_timeout_to_agent_timeout() {
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let mut agent = AgentControlSession::from_parts(session, TimedOutReader).unwrap();

    assert!(matches!(
        agent.wait_for_clipboard().unwrap_err(),
        Error::AgentTimeout("clipboard")
    ));
}

#[test]
fn tcp_tap_next_object_selector_at_timeout_emits_relative_touch_and_restores_timeout() {
    let stream = frame_summary_envelope_with(
        1,
        &[ObjectBox {
            x: 100,
            y: 200,
            w: 301,
            h: 101,
            class_id: 7,
            confidence: 230,
        }],
        &[],
    );
    let (mut agent, server) = tcp_agent_with_reader_bytes(stream);

    agent.set_screen_size(1000, 2000).unwrap();
    let rect = agent
        .tap_next_object_selector_at_timeout(
            AgentObjectSelector::class_min_confidence(7, 220),
            2_500,
            7_500,
            Duration::from_secs(1),
        )
        .unwrap();

    assert_eq!(rect.to_pixels(1000, 2000), (100, 200, 400, 300));
    let closed = agent.close().unwrap();
    server.join().unwrap();
    assert_eq!(closed.reader.read_timeout().unwrap(), None);
    let events = touch_events(&closed.transport.bytes);
    assert_eq!(events.len(), 2);
    assert_eq!(events[0], (TouchAction::DOWN.value(), 0, 175, 275));
    assert_eq!(events[1], (TouchAction::UP.value(), 0, 175, 275));
}

#[test]
fn tcp_tap_next_text_region_at_timeout_emits_relative_touch_and_restores_timeout() {
    let mut stream = Vec::new();
    stream.extend(frame_summary_envelope_with(
        1,
        &[],
        &[TextRegion {
            x: 700,
            y: 800,
            w: 101,
            h: 101,
        }],
    ));
    stream.extend(frame_summary_envelope_with(
        2,
        &[],
        &[
            TextRegion {
                x: 10,
                y: 20,
                w: 11,
                h: 21,
            },
            TextRegion {
                x: 100,
                y: 200,
                w: 301,
                h: 101,
            },
        ],
    ));
    let (mut agent, server) = tcp_agent_with_reader_bytes(stream);

    agent.set_screen_size(1000, 2000).unwrap();
    let indexed = agent
        .tap_next_text_region_at_timeout(0, 10_000, 0, Duration::from_secs(1))
        .unwrap();
    let largest = agent
        .tap_next_largest_text_region_at_timeout(0, 10_000, Duration::from_secs(1))
        .unwrap();

    assert_eq!(indexed.to_pixels(1000, 2000), (700, 800, 800, 900));
    assert_eq!(largest.to_pixels(1000, 2000), (100, 200, 400, 300));
    let closed = agent.close().unwrap();
    server.join().unwrap();
    assert_eq!(closed.reader.read_timeout().unwrap(), None);
    let events = touch_events(&closed.transport.bytes);
    assert_eq!(events.len(), 4);
    assert_eq!(events[0], (TouchAction::DOWN.value(), 0, 800, 800));
    assert_eq!(events[2], (TouchAction::DOWN.value(), 0, 100, 300));
}

#[test]
fn tcp_tap_next_pointer_timeout_helpers_emit_typed_pointer_and_restore_timeout() {
    let mut stream = Vec::new();
    stream.extend(frame_summary_envelope_with(
        1,
        &[ObjectBox {
            x: 100,
            y: 200,
            w: 301,
            h: 101,
            class_id: 7,
            confidence: 230,
        }],
        &[],
    ));
    stream.extend(frame_summary_envelope_with(
        2,
        &[],
        &[TextRegion {
            x: 700,
            y: 800,
            w: 101,
            h: 101,
        }],
    ));
    let (mut agent, server) = tcp_agent_with_reader_bytes(stream);
    let pointer = TouchPointerId::VIRTUAL_FINGER;

    agent.set_screen_size(1000, 2000).unwrap();
    let object = agent
        .tap_next_object_selector_at_pointer_timeout(
            AgentObjectSelector::class_min_confidence(7, 220),
            pointer,
            2_500,
            7_500,
            Duration::from_secs(1),
        )
        .unwrap();
    let text = agent
        .tap_next_largest_text_region_pointer_timeout(pointer, Duration::from_secs(1))
        .unwrap();

    assert_eq!(object.to_pixels(1000, 2000), (100, 200, 400, 300));
    assert_eq!(text.center().to_pixels(1000, 2000), (750, 850));
    let closed = agent.close().unwrap();
    server.join().unwrap();
    assert_eq!(closed.reader.read_timeout().unwrap(), None);
    let events = touch_events(&closed.transport.bytes);
    assert_eq!(events.len(), 4);
    assert!(events
        .iter()
        .all(|(_, pointer_id, _, _)| *pointer_id == pointer.value()));
    assert_eq!(
        events[0],
        (TouchAction::DOWN.value(), pointer.value(), 175, 275)
    );
    assert_eq!(
        events[2],
        (TouchAction::DOWN.value(), pointer.value(), 750, 850)
    );
}

#[test]
fn tcp_agent_target_selector_timeout_helpers_restore_timeout() {
    let mut stream = Vec::new();
    stream.extend(frame_summary_envelope_with(
        1,
        &[ObjectBox {
            x: 100,
            y: 200,
            w: 301,
            h: 101,
            class_id: 7,
            confidence: 230,
        }],
        &[],
    ));
    stream.extend(frame_summary_envelope_with(
        2,
        &[],
        &[TextRegion {
            x: 700,
            y: 800,
            w: 101,
            h: 101,
        }],
    ));
    stream.extend(frame_summary_envelope_with(
        3,
        &[],
        &[
            TextRegion {
                x: 10,
                y: 20,
                w: 11,
                h: 21,
            },
            TextRegion {
                x: 100,
                y: 200,
                w: 301,
                h: 101,
            },
        ],
    ));
    stream.extend(frame_summary_envelope_with(
        4,
        &[ObjectBox {
            x: 300,
            y: 400,
            w: 201,
            h: 101,
            class_id: 3,
            confidence: 240,
        }],
        &[],
    ));
    let (mut agent, server) = tcp_agent_with_reader_bytes(stream);
    let pointer = TouchPointerId::VIRTUAL_FINGER;

    agent.set_screen_size(1000, 2000).unwrap();
    let object = agent
        .wait_for_target_rect_timeout(
            AgentTargetSelector::object_class_min_confidence(7, 220),
            Duration::from_secs(1),
        )
        .unwrap();
    let text = agent
        .tap_next_target_at_pointer_timeout(
            AgentTargetSelector::text_region(0),
            pointer,
            10_000,
            0,
            Duration::from_secs(1),
        )
        .unwrap();
    let largest_text = agent
        .run_actions_and_wait_for_target_rect_timeout(
            &[AgentAction::tap(10, 20)],
            AgentTargetSelector::largest_text_region(),
            Duration::from_secs(1),
        )
        .unwrap();
    let best = agent
        .run_actions_and_tap_next_target_at_pointer_timeout(
            &[AgentAction::tap(30, 40)],
            AgentTargetSelector::best_object(),
            pointer,
            0,
            10_000,
            Duration::from_secs(1),
        )
        .unwrap();

    assert_eq!(object.to_pixels(1000, 2000), (100, 200, 400, 300));
    assert_eq!(text.to_pixels(1000, 2000), (700, 800, 800, 900));
    assert_eq!(largest_text.to_pixels(1000, 2000), (100, 200, 400, 300));
    assert_eq!(best.to_pixels(1000, 2000), (300, 400, 500, 500));
    let closed = agent.close().unwrap();
    server.join().unwrap();
    assert_eq!(closed.reader.read_timeout().unwrap(), None);
    let events = touch_events(&closed.transport.bytes);
    assert_eq!(events.len(), 8);
    assert_eq!(
        events[0],
        (TouchAction::DOWN.value(), pointer.value(), 800, 800)
    );
    assert_eq!(
        events[1],
        (TouchAction::UP.value(), pointer.value(), 800, 800)
    );
    assert_eq!(events[2], (TouchAction::DOWN.value(), 0, 10, 20));
    assert_eq!(events[3], (TouchAction::UP.value(), 0, 10, 20));
    assert_eq!(events[4], (TouchAction::DOWN.value(), 0, 30, 40));
    assert_eq!(events[5], (TouchAction::UP.value(), 0, 30, 40));
    assert_eq!(
        events[6],
        (TouchAction::DOWN.value(), pointer.value(), 300, 500)
    );
    assert_eq!(
        events[7],
        (TouchAction::UP.value(), pointer.value(), 300, 500)
    );
}

#[test]
fn tcp_run_actions_and_tap_next_largest_text_region_at_timeout_taps_and_restores_timeout() {
    let stream = frame_summary_envelope_with(
        1,
        &[],
        &[TextRegion {
            x: 100,
            y: 200,
            w: 301,
            h: 101,
        }],
    );
    let (mut agent, server) = tcp_agent_with_reader_bytes(stream);

    agent.set_screen_size(1000, 2000).unwrap();
    let rect = agent
        .run_actions_and_tap_next_largest_text_region_at_timeout(
            &[AgentAction::tap(10, 20)],
            0,
            10_000,
            Duration::from_secs(1),
        )
        .unwrap();

    assert_eq!(rect.to_pixels(1000, 2000), (100, 200, 400, 300));
    let closed = agent.close().unwrap();
    server.join().unwrap();
    assert_eq!(closed.reader.read_timeout().unwrap(), None);
    let events = touch_events(&closed.transport.bytes);
    assert_eq!(events.len(), 4);
    assert_eq!(events[0], (TouchAction::DOWN.value(), 0, 10, 20));
    assert_eq!(events[1], (TouchAction::UP.value(), 0, 10, 20));
    assert_eq!(events[2], (TouchAction::DOWN.value(), 0, 100, 300));
    assert_eq!(events[3], (TouchAction::UP.value(), 0, 100, 300));
}

#[test]
fn tcp_run_actions_and_tap_next_text_region_pointer_timeout_taps_and_restores_timeout() {
    let stream = frame_summary_envelope_with(
        1,
        &[],
        &[TextRegion {
            x: 700,
            y: 800,
            w: 101,
            h: 101,
        }],
    );
    let (mut agent, server) = tcp_agent_with_reader_bytes(stream);
    let pointer = TouchPointerId::VIRTUAL_FINGER;

    agent.set_screen_size(1000, 2000).unwrap();
    let rect = agent
        .run_actions_and_tap_next_text_region_pointer_timeout(
            &[AgentAction::tap(10, 20)],
            0,
            pointer,
            Duration::from_secs(1),
        )
        .unwrap();

    assert_eq!(rect.to_pixels(1000, 2000), (700, 800, 800, 900));
    let closed = agent.close().unwrap();
    server.join().unwrap();
    assert_eq!(closed.reader.read_timeout().unwrap(), None);
    let events = touch_events(&closed.transport.bytes);
    assert_eq!(events.len(), 4);
    assert_eq!(events[0], (TouchAction::DOWN.value(), 0, 10, 20));
    assert_eq!(events[1], (TouchAction::UP.value(), 0, 10, 20));
    assert_eq!(
        events[2],
        (TouchAction::DOWN.value(), pointer.value(), 750, 850)
    );
    assert_eq!(
        events[3],
        (TouchAction::UP.value(), pointer.value(), 750, 850)
    );
}

#[test]
fn tcp_run_actions_and_wait_for_scene_change_timeout_restores_timeout() {
    let mut stream = Vec::new();
    stream.extend(frame_summary_envelope_full(1, 0, &[], &[], &[]));
    stream.extend(frame_summary_envelope_full(
        2,
        crate::ai::FLAG_SCENE_CHANGE,
        &[],
        &[],
        &[],
    ));
    let (mut agent, server) = tcp_agent_with_reader_bytes(stream);

    let summary = agent
        .run_actions_and_wait_for_scene_change_timeout(
            &[AgentAction::tap(10, 20)],
            Duration::from_secs(1),
        )
        .unwrap();

    assert_eq!(summary.frame_seq, 2);
    let closed = agent.close().unwrap();
    server.join().unwrap();
    assert_eq!(closed.reader.read_timeout().unwrap(), None);
    assert_eq!(count_touch_events(&closed.transport.bytes), 2);
}

#[test]
fn tcp_run_actions_and_wait_for_fresh_frame_timeout_restores_timeout() {
    let mut stream = Vec::new();
    stream.extend(frame_summary_envelope_full_at(100, 4, 0, &[], &[], &[]));
    stream.extend(frame_summary_envelope_full_at(120, 6, 0, &[], &[], &[]));
    let (mut agent, server) = tcp_agent_with_reader_bytes(stream);

    let summary = agent
        .run_actions_and_wait_for_frame_summary_after_seq_timeout(
            &[AgentAction::tap(10, 20)],
            5,
            Duration::from_secs(1),
        )
        .unwrap();

    assert_eq!(summary.frame_seq, 6);
    let closed = agent.close().unwrap();
    server.join().unwrap();
    assert_eq!(closed.reader.read_timeout().unwrap(), None);
    assert_eq!(count_touch_events(&closed.transport.bytes), 2);
}

#[test]
fn tcp_run_actions_and_wait_for_largest_text_region_timeout_restores_timeout() {
    let stream = frame_summary_envelope_with(
        1,
        &[],
        &[TextRegion {
            x: 100,
            y: 200,
            w: 301,
            h: 101,
        }],
    );
    let (mut agent, server) = tcp_agent_with_reader_bytes(stream);

    let rect = agent
        .run_actions_and_wait_for_largest_text_region_rect_timeout(
            &[AgentAction::tap(10, 20)],
            Duration::from_secs(1),
        )
        .unwrap();

    assert_eq!(rect.to_pixels(1000, 2000), (100, 200, 400, 300));
    assert_eq!(rect.center().to_pixels(1000, 2000), (250, 250));
    let closed = agent.close().unwrap();
    server.join().unwrap();
    assert_eq!(closed.reader.read_timeout().unwrap(), None);
    assert_eq!(count_touch_events(&closed.transport.bytes), 2);
}

#[test]
fn tcp_wait_timeout_restores_previous_read_timeout() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (_sock, _addr) = listener.accept().unwrap();
        std::thread::sleep(Duration::from_millis(80));
    });

    let reader = TcpStream::connect(addr).unwrap();
    let session = HidSession::open(MockTransport::new(), OpenRequest::none()).unwrap();
    let mut agent = AgentControlSession::from_parts(session, reader).unwrap();

    assert!(matches!(
        agent
            .wait_for_clipboard_timeout(Duration::from_millis(5))
            .unwrap_err(),
        Error::AgentTimeout("clipboard")
    ));

    let closed = agent.close().unwrap();
    assert_eq!(closed.reader.read_timeout().unwrap(), None);
    server.join().unwrap();
}

#[test]
fn tcp_run_actions_and_get_clipboard_timeout_taps_requests_and_restores_timeout() {
    let (mut agent, server) = tcp_agent_with_reader_bytes(clipboard("tcp copied"));

    let text = agent
        .run_actions_and_get_clipboard_and_wait_timeout(
            &[AgentAction::tap(10, 20)],
            ClipboardCopyKey::CUT.value(),
            Duration::from_secs(1),
        )
        .unwrap();

    assert_eq!(text, "tcp copied");
    let closed = agent.close().unwrap();
    assert_eq!(closed.reader.read_timeout().unwrap(), None);
    let tags = control_message_tags(&closed.transport.bytes);
    assert_eq!(tags, [2, 2, 8]);
    let get_clipboard =
        find_control_message(&closed.transport.bytes, 8).expect("GET_CLIPBOARD frame");
    assert_eq!(get_clipboard, &[8, ClipboardCopyKey::CUT.value()]);
    server.join().unwrap();
}

#[test]
fn tcp_query_ai_and_wait_stats_timeout_restores_timeout() {
    let (mut agent, server) = tcp_agent_with_reader_bytes(ai_stats_envelope());

    let stats = agent
        .query_ai_and_wait_stats_timeout(0x0102_0304_0506_0708, Duration::from_secs(1))
        .unwrap();

    assert_eq!(stats.frames_sampled, 10);
    let closed = agent.close().unwrap();
    assert_eq!(closed.reader.read_timeout().unwrap(), None);
    let query = find_control_message(&closed.transport.bytes, 23).expect("AI_QUERY frame");
    assert_eq!(
        u64::from_be_bytes(query[1..9].try_into().unwrap()),
        0x0102_0304_0506_0708
    );
    server.join().unwrap();
}

#[test]
fn tcp_run_actions_and_query_ai_and_wait_stats_timeout_restores_timeout() {
    let (mut agent, server) = tcp_agent_with_reader_bytes(ai_stats_envelope());

    let stats = agent
        .run_actions_and_query_ai_and_wait_stats_timeout(
            &[AgentAction::tap(10, 20)],
            0x2122_2324_2526_2728,
            Duration::from_secs(1),
        )
        .unwrap();

    assert_eq!(stats.frames_sampled, 10);
    let closed = agent.close().unwrap();
    assert_eq!(closed.reader.read_timeout().unwrap(), None);
    assert_eq!(control_message_tags(&closed.transport.bytes), [2, 2, 23]);
    let query = find_control_message(&closed.transport.bytes, 23).expect("AI_QUERY frame");
    assert_eq!(
        u64::from_be_bytes(query[1..9].try_into().unwrap()),
        0x2122_2324_2526_2728
    );
    server.join().unwrap();
}
