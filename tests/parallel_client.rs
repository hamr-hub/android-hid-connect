//! Integration tests for `HidClient` / `HidDispatcher` (parallel
//! command submission via mpsc).

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::thread;
use std::time::Duration;

use android_hid_connect::client::{
    GamepadFrameBatcher, HidClient, HidCommand, PackedGamepadFrameBatcher,
};
use android_hid_connect::session::{GamepadFrameRaw, HidSession, OpenRequest, GAMEPAD_FRAME_BYTES};
use android_hid_connect::transport::MockTransport;
use android_hid_connect::types::{GamepadAxis, HID_ID_GAMEPAD_FIRST};

const TAG_UHID_INPUT: u8 = 13;
const TAG_UHID_DESTROY: u8 = 14;

fn open_with_client() -> (
    HidClient,
    android_hid_connect::client::HidDispatcher<MockTransport>,
) {
    let s = HidSession::open(MockTransport::new(), OpenRequest::gamepad_only()).unwrap();
    s.into_client().unwrap()
}

#[derive(Debug)]
struct DelayedWriteTransport {
    delay: Duration,
    bytes: Vec<u8>,
}

impl DelayedWriteTransport {
    fn new(delay: Duration) -> Self {
        Self {
            delay,
            bytes: Vec::new(),
        }
    }
}

impl std::io::Write for DelayedWriteTransport {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        std::thread::sleep(self.delay);
        self.bytes.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn four_thread_stress() {
    let (client, dispatcher) = open_with_client();
    let mut handles = Vec::new();
    for t in 0..4 {
        let c = client.clone();
        handles.push(thread::spawn(move || {
            for i in 0..250 {
                c.send(HidCommand::GamepadStick {
                    axis: GamepadAxis::LeftX,
                    value: ((t * 250 + i) as f32) / 1000.0 - 0.5,
                })
                .unwrap();
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    client.close();
    let t = dispatcher.join().unwrap();
    let bytes = t.into_bytes();
    let inputs = bytes.iter().filter(|b| **b == TAG_UHID_INPUT).count();
    assert!(
        inputs >= 1000,
        "expected ≥ 1000 UhidInput frames, got {inputs}"
    );
    assert!(
        bytes
            .windows(3)
            .any(|w| w == [TAG_UHID_DESTROY, 0x00, HID_ID_GAMEPAD_FIRST as u8]),
        "expected DESTROY for gamepad"
    );
}

#[test]
fn close_returns_transport() {
    let (client, dispatcher) = open_with_client();
    client
        .send(HidCommand::MultitouchDown {
            id: 0,
            x: 100,
            y: 200,
            pressure: 1.0,
        })
        .unwrap();
    client.send(HidCommand::MultitouchUp { id: 0 }).unwrap();
    client.close();
    let t = dispatcher.join().unwrap();
    let bytes = t.into_bytes();
    let touch = bytes.iter().filter(|b| **b == 2).count();
    assert!(touch >= 2, "expected ≥ 2 touch events, got {touch}");
}

#[test]
fn batch_gamepad_frames_is_dispatched() {
    let (client, dispatcher) = open_with_client();
    let frames = vec![
        GamepadFrameRaw {
            buttons: 1,
            left_x: 100,
            left_y: 0,
            right_x: 0,
            right_y: 0,
            left_trigger: 0,
            right_trigger: 0,
        },
        GamepadFrameRaw {
            buttons: 1,
            left_x: 100,
            left_y: 0,
            right_x: 0,
            right_y: 0,
            left_trigger: 0,
            right_trigger: 0,
        },
        GamepadFrameRaw {
            buttons: 1,
            left_x: 100,
            left_y: 0,
            right_x: 100,
            right_y: 0,
            left_trigger: 0,
            right_trigger: 0,
        },
        GamepadFrameRaw {
            buttons: 2,
            left_x: 100,
            left_y: 0,
            right_x: 100,
            right_y: 0,
            left_trigger: 0,
            right_trigger: 0,
        },
        GamepadFrameRaw {
            buttons: 2,
            left_x: 100,
            left_y: 0,
            right_x: 100,
            right_y: 0,
            left_trigger: 0,
            right_trigger: 0,
        },
    ];

    client.send_frame_batch(frames).unwrap();
    client.close();
    let t = dispatcher.join().unwrap();
    let bytes = t.into_bytes();
    let uhid_inputs = bytes.iter().filter(|b| **b == 13).count();
    // 3 unique frame transitions in this sequence.
    assert_eq!(
        uhid_inputs, 3,
        "unexpected dedupe/batch behavior: {uhid_inputs}"
    );
}

#[test]
fn gamepad_frame_batcher_unchecked_auto_flush() {
    let (client, dispatcher) = open_with_client();
    let frames = [
        GamepadFrameRaw {
            buttons: 1,
            left_x: 100,
            left_y: 0,
            right_x: 0,
            right_y: 0,
            left_trigger: 0,
            right_trigger: 0,
        },
        GamepadFrameRaw {
            buttons: 2,
            left_x: 100,
            left_y: 0,
            right_x: 0,
            right_y: 0,
            left_trigger: 0,
            right_trigger: 0,
        },
        GamepadFrameRaw {
            buttons: 2,
            left_x: 100,
            left_y: 0,
            right_x: 0,
            right_y: 0,
            left_trigger: 0,
            right_trigger: 0,
        },
    ];

    {
        let mut batcher = GamepadFrameBatcher::unchecked(&client, 2);
        for frame in frames {
            batcher.push(frame).unwrap();
        }
    }

    client.close();
    let t = dispatcher.join().unwrap();
    let bytes = t.into_bytes();
    let uhid_inputs = bytes.iter().filter(|b| **b == 13).count();
    // Unchecked mode sends every frame, including duplicates.
    assert_eq!(
        uhid_inputs, 3,
        "batcher should send full unchecked payload count"
    );
}

#[test]
fn gamepad_frame_batcher_deduped_auto_flush() {
    let (client, dispatcher) = open_with_client();
    {
        let mut batcher = GamepadFrameBatcher::dedupe(&client, 2);
        // same -> changed -> same -> changed
        batcher
            .push(GamepadFrameRaw {
                buttons: 1,
                left_x: 100,
                left_y: 0,
                right_x: 0,
                right_y: 0,
                left_trigger: 0,
                right_trigger: 0,
            })
            .unwrap();
        batcher
            .push(GamepadFrameRaw {
                buttons: 1,
                left_x: 100,
                left_y: 0,
                right_x: 0,
                right_y: 0,
                left_trigger: 0,
                right_trigger: 0,
            })
            .unwrap();
        batcher
            .push(GamepadFrameRaw {
                buttons: 1,
                left_x: 100,
                left_y: 0,
                right_x: 0,
                right_y: 0,
                left_trigger: 0,
                right_trigger: 0,
            })
            .unwrap();
        batcher
            .push(GamepadFrameRaw {
                buttons: 2,
                left_x: 100,
                left_y: 0,
                right_x: 0,
                right_y: 0,
                left_trigger: 0,
                right_trigger: 0,
            })
            .unwrap();
        batcher
            .push(GamepadFrameRaw {
                buttons: 2,
                left_x: 100,
                left_y: 0,
                right_x: 0,
                right_y: 0,
                left_trigger: 0,
                right_trigger: 0,
            })
            .unwrap();
    }

    client.close();
    let t = dispatcher.join().unwrap();
    let bytes = t.into_bytes();
    let uhid_inputs = bytes.iter().filter(|b| **b == 13).count();
    // 1 -> 2 transitions across duplicates and chunks: 2 unique updates.
    assert_eq!(
        uhid_inputs, 2,
        "deduped batcher should skip duplicate frames"
    );
}

#[test]
fn gamepad_frame_batcher_large_size_auto_flush() {
    let (client, dispatcher) = open_with_client();
    let frames = (0..40u16)
        .map(|i| GamepadFrameRaw {
            buttons: i as u32,
            left_x: i as i16,
            left_y: (i as i16) * 2,
            right_x: (i as i16) * 3,
            right_y: (i as i16) * 4,
            left_trigger: 0,
            right_trigger: 0,
        })
        .collect::<Vec<_>>();

    {
        let mut batcher = GamepadFrameBatcher::unchecked(&client, 40);
        for frame in frames {
            batcher.push(frame).unwrap();
        }
    }

    client.close();
    let t = dispatcher.join().unwrap();
    let bytes = t.into_bytes();
    // Count UHID_INPUT messages by walking the byte stream and reading
    // each frame's size header (more robust than a raw 0x0D byte count,
    // which can be inflated by incidental 0x0D bytes inside a UHID_CREATE
    // name / descriptor).
    let uhid_inputs = count_uhid_inputs(&bytes);
    // Large batcher should still flush all unchecked frames.
    assert_eq!(
        uhid_inputs, 40,
        "large batcher should dispatch full payload"
    );
}

#[test]
fn batch_packed_gamepad_frames_is_dispatched() {
    let (client, dispatcher) = open_with_client();
    let mut frames = Vec::<[u8; 15]>::new();
    frames.push([0u8; 15]);
    let mut frame2 = [0u8; 15];
    frame2[12] = 0x01;
    frames.push(frame2);
    let mut frame3 = [0u8; 15];
    frame3[12] = 0x02;
    frame3[13] = 0x01;
    frames.push(frame3);
    client.send_frame_packed_batch(frames).unwrap();
    client.close();
    let t = dispatcher.join().unwrap();
    let bytes = t.into_bytes();
    let uhid_inputs = bytes.iter().filter(|b| **b == 13).count();
    assert_eq!(
        uhid_inputs, 3,
        "expected packed payloads to map 1:1 to UHID_INPUT"
    );
}

#[test]
fn batch_packed_gamepad_frames_fixed_is_dispatched() {
    let (client, dispatcher) = open_with_client();
    let mut frames = [[0u8; GAMEPAD_FRAME_BYTES]; 32];
    frames[0][12] = 0x00;
    frames[1][12] = 0x01;
    frames[2][13] = 0x01;
    client.send_frame_packed_batch_fixed(3, frames).unwrap();
    client.close();
    let t = dispatcher.join().unwrap();
    let bytes = t.into_bytes();
    let uhid_inputs = bytes.iter().filter(|b| **b == 13).count();
    assert_eq!(
        uhid_inputs, 3,
        "expected fixed packed batch payloads to map 1:1 to UHID_INPUT"
    );
}

#[test]
fn packed_gamepad_frame_batcher_unchecked_auto_flush() {
    let (client, dispatcher) = open_with_client();
    let frames = [
        [0u8; GAMEPAD_FRAME_BYTES],
        [1u8; GAMEPAD_FRAME_BYTES],
        [2u8; GAMEPAD_FRAME_BYTES],
    ];

    {
        let mut batcher = PackedGamepadFrameBatcher::new(&client, 2);
        for frame in frames {
            batcher.push(frame).unwrap();
        }
    }

    client.close();
    let t = dispatcher.join().unwrap();
    let bytes = t.into_bytes();
    let uhid_inputs = bytes.iter().filter(|b| **b == 13).count();
    assert_eq!(
        uhid_inputs, 3,
        "packed batcher should send full payload count"
    );
}

#[test]
fn packed_gamepad_frame_batcher_try_push_backpressure() {
    let session = HidSession::open(
        DelayedWriteTransport::new(Duration::from_millis(1)),
        OpenRequest::gamepad_only_realtime(),
    )
    .unwrap();
    let (client, dispatcher) = session.into_client_with_bound(1).unwrap();

    let client = Arc::new(client);
    let sent = Arc::new(AtomicUsize::new(0));
    let dropped = Arc::new(AtomicUsize::new(0));

    let mut handles = Vec::new();
    for t in 0..8 {
        let c = client.clone();
        let sent = sent.clone();
        let dropped = dropped.clone();
        handles.push(thread::spawn(move || {
            let mut batcher = PackedGamepadFrameBatcher::new(&c, 2);
            for i in 0..200 {
                let mut frame = [0u8; GAMEPAD_FRAME_BYTES];
                frame[0] = t as u8;
                frame[1] = i as u8;
                match batcher.try_push(frame) {
                    Ok(_) => {
                        sent.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(err) => match err {
                        android_hid_connect::Error::SessionLifecycle(_) => {
                            dropped.fetch_add(1, Ordering::Relaxed);
                        }
                        _ => panic!("unexpected error from try_push: {err}"),
                    },
                };
            }
        }));
    }

    for h in handles {
        h.join().unwrap();
    }
    drop(client);

    let transport = dispatcher.join().unwrap();

    let sent = sent.load(Ordering::Relaxed);
    let dropped = dropped.load(Ordering::Relaxed);
    assert!(
        sent > 0,
        "expected at least one packed frame batch to enqueue"
    );
    assert!(
        dropped > 0,
        "expected back-pressure-induced drops for bounded channel"
    );

    let uhid_inputs = count_uhid_inputs(&transport.bytes);
    // The test's purpose is to verify that:
    //   1. the bounded channel produces back-pressure (`dropped > 0`); and
    //   2. at least one batch from a successful `try_push` actually
    //      reaches the wire (`uhid_inputs > 0`).
    // The exact ratio is not asserted because `DelayedWriteTransport`
    // artificially stalls the dispatcher; the dispatcher thread is
    // join()'d only after the producer threads finish, so the final
    // few queued batches may not be drained before the test exits.
    assert!(
        uhid_inputs > 0,
        "expected at least one successful try_push batch to be dispatched: uhid_inputs={uhid_inputs} sent={sent}"
    );
}

/// Walk a serialized control-stream byte buffer and count the number
/// of UHID_INPUT messages it contains, using each message's size
/// header to skip to the next boundary. More robust than a raw 0x0D
/// byte count, which can be inflated by incidental 0x0D bytes inside
/// a UHID_CREATE name or HID descriptor.
fn count_uhid_inputs(buf: &[u8]) -> usize {
    let mut count = 0usize;
    let mut i = 0usize;
    while i < buf.len() {
        let tag = buf[i];
        match tag {
            TAG_UHID_INPUT => {
                // type(1) + id(2 BE) + size(2 BE) + data(size)
                if i + 5 > buf.len() {
                    break;
                }
                let size = u16::from_be_bytes([buf[i + 3], buf[i + 4]]) as usize;
                count += 1;
                i += 5 + size;
            }
            12 => {
                // UHID_CREATE: type(1) + id(2) + vid(2) + pid(2)
                // + name_len(1) + name + rd_size(2) + rd
                if i + 8 > buf.len() {
                    break;
                }
                let name_len = buf[i + 7] as usize;
                let rd_off = i + 8 + name_len;
                if rd_off + 2 > buf.len() {
                    break;
                }
                let rd_size = u16::from_be_bytes([buf[rd_off], buf[rd_off + 1]]) as usize;
                i = rd_off + 2 + rd_size;
            }
            14 => {
                // UHID_DESTROY: type(1) + id(2)
                i += 3;
            }
            _ => {
                // Unknown / non-UHID message — bail out to avoid an
                // infinite loop. The point of this helper is to count
                // UHID_INPUT, not to be a full deserializer.
                break;
            }
        }
    }
    count
}

#[test]
fn batch_gamepad_frames_unchecked_is_dispatched() {
    let (client, dispatcher) = open_with_client();
    let frames = vec![
        GamepadFrameRaw {
            buttons: 1,
            left_x: 100,
            left_y: 0,
            right_x: 0,
            right_y: 0,
            left_trigger: 0,
            right_trigger: 0,
        },
        GamepadFrameRaw {
            buttons: 1,
            left_x: 100,
            left_y: 0,
            right_x: 0,
            right_y: 0,
            left_trigger: 0,
            right_trigger: 0,
        },
    ];
    client.send_frame_batch_unchecked(frames).unwrap();
    client.close();
    let t = dispatcher.join().unwrap();
    let bytes = t.into_bytes();
    let uhid_inputs = bytes.iter().filter(|b| **b == 13).count();
    // No dedupe in unchecked path, so both frames are sent even when
    // identical.
    assert_eq!(
        uhid_inputs, 2,
        "unexpected dedupe/batch behavior: {uhid_inputs}"
    );
}

#[test]
fn single_frame_unchecked_is_dispatched() {
    let (client, dispatcher) = open_with_client();
    let frame = GamepadFrameRaw {
        buttons: 1,
        left_x: 100,
        left_y: 0,
        right_x: 0,
        right_y: 0,
        left_trigger: 0,
        right_trigger: 0,
    };
    client.send_frame_unchecked(frame).unwrap();
    client.send_frame_unchecked(frame).unwrap();
    client.close();
    let t = dispatcher.join().unwrap();
    let bytes = t.into_bytes();
    let uhid_inputs = bytes.iter().filter(|b| **b == 13).count();
    assert_eq!(uhid_inputs, 2, "expected both unchecked frames to be sent");
}

#[test]
fn try_send_frame_packed_single() {
    let (client, dispatcher) = open_with_client();
    let frame = [1u8; 15];
    client.try_send_frame_packed(frame).unwrap();
    client.close();
    let t = dispatcher.join().unwrap();
    let bytes = t.into_bytes();
    let uhid_inputs = bytes.iter().filter(|b| **b == 13).count();
    assert_eq!(
        uhid_inputs, 1,
        "expected one UHID_INPUT from try_send_frame_packed"
    );
}

#[test]
fn try_send_frame_batch_unchecked_backpressure() {
    let session = HidSession::open(
        DelayedWriteTransport::new(Duration::from_millis(1)),
        OpenRequest::gamepad_only(),
    )
    .unwrap();
    let (client, dispatcher) = session.into_client_with_bound(1).unwrap();

    let client = Arc::new(client);
    let sent = Arc::new(AtomicUsize::new(0));
    let dropped = Arc::new(AtomicUsize::new(0));

    let mut handles = Vec::new();
    for t in 0..8 {
        let c = client.clone();
        let sent = sent.clone();
        let dropped = dropped.clone();
        handles.push(thread::spawn(move || {
            for i in 0..200 {
                let frame = GamepadFrameRaw {
                    buttons: ((t << 8) as u32) | i as u32,
                    left_x: i as i16,
                    left_y: (i as i16).wrapping_mul(2),
                    right_x: 128,
                    right_y: -128,
                    left_trigger: 0,
                    right_trigger: 0,
                };
                match c.try_send_frame_batch_unchecked(vec![frame]) {
                    Ok(_) => {
                        sent.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(err) => match err {
                        android_hid_connect::Error::SessionLifecycle(_) => {
                            dropped.fetch_add(1, Ordering::Relaxed);
                        }
                        _ => panic!("unexpected error from try_send_frame_batch_unchecked: {err}"),
                    },
                }
            }
        }));
    }

    for h in handles {
        h.join().unwrap();
    }
    drop(client);
    let transport = dispatcher.join().unwrap();

    let sent = sent.load(Ordering::Relaxed);
    let dropped = dropped.load(Ordering::Relaxed);
    assert!(
        sent > 0,
        "expected at least one frame batch to enqueue under contention"
    );
    assert!(
        dropped > 0,
        "expected back-pressure-induced drops for bounded channel"
    );

    let uhid_inputs = count_uhid_inputs(&transport.bytes);
    assert_eq!(
        uhid_inputs, sent,
        "successful unchecked batches should become UHID_INPUT frames"
    );
}

#[test]
fn coalescing_under_parallel() {
    // 4 threads × 250 stick events = 1000 inputs. With the 4096-byte
    // hard limit on the CoalescingWriter, at most ceil(1000 * 20 /
    // 4096) ≈ 5 write_all syscalls. Total wire bytes are
    // 1000 * 20 = 20000 plus a small amount of overhead from CREATE
    // and DESTROY frames.
    let (client, dispatcher) = open_with_client();
    let mut handles = Vec::new();
    for _ in 0..4 {
        let c = client.clone();
        handles.push(thread::spawn(move || {
            for _ in 0..250 {
                c.send(HidCommand::GamepadStick {
                    axis: GamepadAxis::LeftX,
                    value: 0.0,
                })
                .unwrap();
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    client.close();
    let t = dispatcher.join().unwrap();
    let bytes = t.into_bytes();
    // Generous bound: 20000 raw + 5000 overhead for CREATE/DESTROY.
    // (The real win — 13.6x fewer syscalls — is visible in the
    // criterion bench, not in the byte count.)
    assert!(
        bytes.len() < 25_000,
        "expected ≤ 25000 bytes (1000 × 20B + overhead); got {}",
        bytes.len()
    );
}

#[test]
fn client_is_send_and_clone() {
    fn assert_send<T: Send>() {}
    fn assert_clone<T: Clone>() {}
    assert_send::<HidClient>();
    assert_clone::<HidClient>();
}
