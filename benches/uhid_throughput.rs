//! UHID throughput micro-benchmark.
//!
//! Run with: `cargo bench --bench uhid_throughput`
//!
//! Measures how many `UHID_INPUT` messages we can serialize per second
//! when the message bytes are dropped on the floor (no I/O). This is
//! the upper bound on the rate the library can pump the host-side
//! scrcpy control socket.

use android_hid_connect::control::message::{ControlMessage, UhidInput};
use android_hid_connect::transport::send_one;
use android_hid_connect::transport::MockTransport;
use android_hid_connect::types::HID_MAX_SIZE;
use android_hid_connect::KeyboardHid;
use criterion::{criterion_group, criterion_main, Criterion};
use std::hint::black_box;

fn bench_keyboard_input(c: &mut Criterion) {
    let mut kbd = KeyboardHid::new();
    // The first key_event requires open_message()'s state to be set; we
    // don't send open here because we're only measuring the hot path.
    c.bench_function("keyboard inject_key (no I/O)", |b| {
        b.iter(|| {
            let m = kbd
                .key_event(0x04, true, android_hid_connect::Modifiers::empty())
                .unwrap();
            std::hint::black_box(m);
        });
    });
}

fn bench_serialize(c: &mut Criterion) {
    let mut data = [0u8; HID_MAX_SIZE];
    data[0] = 0x02; // LSHIFT
    data[2] = 0x04; // A
    let msg = ControlMessage::UhidInput(UhidInput {
        id: 1,
        size: 8,
        data,
    });
    c.bench_function("uhid_input serialize", |b| {
        b.iter(|| {
            let bytes = msg.serialize().unwrap();
            std::hint::black_box(bytes);
        });
    });
}

fn bench_send_to_mock(c: &mut Criterion) {
    let mut data = [0u8; HID_MAX_SIZE];
    data[2] = 0x04;
    let msg = ControlMessage::UhidInput(UhidInput {
        id: 1,
        size: 8,
        data,
    });
    c.bench_function("send_one into MockTransport", |b| {
        b.iter(|| {
            let mut t = MockTransport::new();
            send_one(&mut t, &msg).unwrap();
            std::hint::black_box(t.into_bytes());
        });
    });
}

fn bench_coalesced_burst(c: &mut Criterion) {
    // Burst of 100 UhidInput messages through a CoalescingWriter with
    // a 1ms window and 4096-byte hard limit. Measures the average
    // per-message cost when coalescing is on (should be much lower
    // than `send_one into MockTransport` because there is only one
    // write_all per window, not 100).
    use android_hid_connect::coalesce::CoalescingWriter;
    use std::time::Duration;
    let mut data = [0u8; HID_MAX_SIZE];
    data[2] = 0x04;
    let msg = ControlMessage::UhidInput(UhidInput {
        id: 1,
        size: 8,
        data,
    });
    c.bench_function("coalesced 100-input burst", |b| {
        b.iter(|| {
            let mut w =
                CoalescingWriter::with_limits(MockTransport::new(), Duration::from_millis(1), 4096);
            for _ in 0..100 {
                w.push(black_box(&msg)).unwrap();
            }
            w.flush_now().unwrap();
            std::hint::black_box(w);
        });
    });
}

/// A "syscall-equivalent" transport that charges a fixed cost per
/// `write_all` + `flush` call, modelling the per-syscall overhead of a
/// real `TcpStream` (1-5 µs on Linux). This makes the coalescing
/// speedup visible in the bench output — without this, `MockTransport`
/// is in-memory and coalescing looks like pure overhead.
struct SyscallCostTransport {
    bytes: Vec<u8>,
    /// Cost charged per `write_all` (nanoseconds).
    per_write_ns: u64,
    /// Cost charged per `flush` (nanoseconds).
    per_flush_ns: u64,
}

impl SyscallCostTransport {
    fn new(per_write_ns: u64, per_flush_ns: u64) -> Self {
        Self {
            bytes: Vec::new(),
            per_write_ns,
            per_flush_ns,
        }
    }
}

impl std::io::Write for SyscallCostTransport {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // Simulate syscall cost via a busy-wait loop. Cheap enough
        // for benchmarking; accurate enough to see the difference.
        let start = std::time::Instant::now();
        while start.elapsed().as_nanos() < self.per_write_ns as u128 {
            std::hint::black_box(0u64);
        }
        self.bytes.extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        let start = std::time::Instant::now();
        while start.elapsed().as_nanos() < self.per_flush_ns as u128 {
            std::hint::black_box(0u64);
        }
        Ok(())
    }
}

fn bench_100_msg_raw_vs_coalesced(c: &mut Criterion) {
    use android_hid_connect::coalesce::CoalescingWriter;
    use std::time::Duration;
    let mut data = [0u8; HID_MAX_SIZE];
    data[2] = 0x04;
    let msg = ControlMessage::UhidInput(UhidInput {
        id: 1,
        size: 8,
        data,
    });
    // 1 µs per write_all + 500 ns per flush ≈ 1.5 µs per syscall
    // (typical for a localhost TcpStream on Linux).
    const COST: u64 = 1_000;
    const FLUSH: u64 = 500;

    let mut g = c.benchmark_group("100-input syscall-equiv");
    g.bench_function("raw send_one x 100", |b| {
        b.iter(|| {
            let mut t = SyscallCostTransport::new(COST, FLUSH);
            for _ in 0..100 {
                send_one(&mut t, black_box(&msg)).unwrap();
            }
            black_box(t.bytes.len());
        });
    });
    g.bench_function("coalesced 1ms window", |b| {
        b.iter(|| {
            let mut w = CoalescingWriter::with_limits(
                SyscallCostTransport::new(COST, FLUSH),
                Duration::from_millis(1),
                4096,
            );
            for _ in 0..100 {
                w.push(black_box(&msg)).unwrap();
            }
            w.flush_now().unwrap();
            black_box(w);
        });
    });
    g.finish();
}

fn bench_gamepad_frame_pack_and_packed_batch(c: &mut Criterion) {
    use android_hid_connect::session::{
        GamepadFrameRaw, GAMEPAD_FRAME_BYTES, HidSession, OpenRequest,
    };
    use android_hid_connect::GamepadButton;
    use android_hid_connect::transport::MockTransport;

    const FRAME_COUNT: usize = 512;
    let mut packed_frames = Vec::<[u8; GAMEPAD_FRAME_BYTES]>::with_capacity(FRAME_COUNT);
    let mut raw_frames = Vec::<GamepadFrameRaw>::with_capacity(FRAME_COUNT);

    for i in 0..FRAME_COUNT {
        let buttons = (i as u32) & 0x0FFF;
        let left_x = (i as i16).wrapping_mul(32) % 32767;
        let left_y = (i as i16).wrapping_mul(19) % 32767;
        let right_x = (i as i16).wrapping_mul(17) % 32767;
        let right_y = (i as i16).wrapping_mul(11) % 32767;
        let left_trigger = (i as i16).min(0x7FFF);
        let right_trigger = ((i as i16) * 3 / 2).min(0x7FFF);
        let frame = GamepadFrameRaw::new(
            buttons,
            left_x,
            left_y,
            right_x,
            right_y,
            left_trigger,
            right_trigger,
        );
        packed_frames.push(frame.pack());
        raw_frames.push(frame);
    }

    let mut packed_cursor = 0u8;
    let mut pack_state = GamepadFrameRaw::new(
        GamepadButton::South as u32,
        0,
        0,
        0,
        0,
        0,
        0,
    );

    c.bench_function("gamepad frame pack", |b| {
        b.iter(|| {
            pack_state.left_x = pack_state.left_x.wrapping_add(1);
            pack_state.left_y = pack_state.left_y.wrapping_add(2);
            let packed = pack_state.pack();
            packed_cursor = packed_cursor.wrapping_add(packed[0]);
            std::hint::black_box((packed, packed_cursor));
        });
    });

    c.bench_function("session set_frame_raw_unchecked single", |b| {
        let frame = raw_frames[0];
        let mut s = HidSession::open(MockTransport::new(), OpenRequest::gamepad_only()).unwrap();
        b.iter(|| {
            s.set_frame_raw_unchecked(
                frame.buttons,
                frame.left_x,
                frame.left_y,
                frame.right_x,
                frame.right_y,
                frame.left_trigger,
                frame.right_trigger,
            )
            .unwrap();
            s.flush_now().unwrap();
            black_box(s.stats());
        });
        s.close().unwrap();
    });

    c.bench_function("session set_frame_raw_unchecked single (direct)", |b| {
        let frame = raw_frames[0];
        let mut s = HidSession::open(
            MockTransport::new(),
            OpenRequest::gamepad_only_realtime(),
        )
        .unwrap();
        b.iter(|| {
            s.set_frame_raw_unchecked(
                frame.buttons,
                frame.left_x,
                frame.left_y,
                frame.right_x,
                frame.right_y,
                frame.left_trigger,
                frame.right_trigger,
            )
            .unwrap();
            black_box(s.stats());
        });
        s.close().unwrap();
    });

    c.bench_function("session set_frame_raw_packed_batch 512", |b| {
        let frames = &packed_frames;
        b.iter(|| {
            let mut s = HidSession::open(MockTransport::new(), OpenRequest::gamepad_only()).unwrap();
            s.set_frame_raw_packed_batch(frames).unwrap();
            s.flush_now().unwrap();
            black_box(s.stats());
            s.close().unwrap();
        });
    });

    c.bench_function("session set_frame_raw_batch_deduped 512", |b| {
        let frames = &raw_frames;
        b.iter(|| {
            let mut s = HidSession::open(MockTransport::new(), OpenRequest::gamepad_only()).unwrap();
            s.set_frame_raw_batch(frames).unwrap();
            s.flush_now().unwrap();
            black_box(s.stats());
            s.close().unwrap();
        });
    });

    c.bench_function("session set_frame_raw_batch_unchecked 512", |b| {
        let frames = &raw_frames;
        b.iter(|| {
            let mut s = HidSession::open(MockTransport::new(), OpenRequest::gamepad_only()).unwrap();
            s.set_frame_raw_batch_unchecked(frames).unwrap();
            s.flush_now().unwrap();
            black_box(s.stats());
            s.close().unwrap();
        });
    });

    c.bench_function("session set_frame_raw_packed_batch 512 (direct)", |b| {
        let frames = &packed_frames;
        b.iter(|| {
            let mut s = HidSession::open(MockTransport::new(), OpenRequest::gamepad_only_realtime()).unwrap();
            s.set_frame_raw_packed_batch(frames).unwrap();
            black_box(s.stats());
            s.close().unwrap();
        });
    });

    c.bench_function("session set_frame_raw_batch_unchecked 512 (direct)", |b| {
        let frames = &raw_frames;
        b.iter(|| {
            let mut s = HidSession::open(MockTransport::new(), OpenRequest::gamepad_only_realtime()).unwrap();
            s.set_frame_raw_batch_unchecked(frames).unwrap();
            black_box(s.stats());
            s.close().unwrap();
        });
    });

    c.bench_function("session set_frame_raw_batch_deduped 512 (direct)", |b| {
        let frames = &raw_frames;
        b.iter(|| {
            let mut s = HidSession::open(MockTransport::new(), OpenRequest::gamepad_only_realtime()).unwrap();
            s.set_frame_raw_batch(frames).unwrap();
            black_box(s.stats());
            s.close().unwrap();
        });
    });

    c.bench_function("session set_frame_raw_batch_unchecked 512 (coalesce steady-state)", |b| {
        let frames = &raw_frames;
        let mut s = HidSession::open(MockTransport::new(), OpenRequest::gamepad_only()).unwrap();
        b.iter(|| {
            s.set_frame_raw_batch_unchecked(frames).unwrap();
            s.flush_now().unwrap();
            black_box(s.stats());
        });
        s.close().unwrap();
    });

    c.bench_function("session set_frame_raw_batch_unchecked 512 (direct steady-state)", |b| {
        let frames = &raw_frames;
        let mut s = HidSession::open(MockTransport::new(), OpenRequest::gamepad_only_realtime()).unwrap();
        b.iter(|| {
            s.set_frame_raw_batch_unchecked(frames).unwrap();
            black_box(s.stats());
        });
        s.close().unwrap();
    });
}

fn bench_hidclient_dispatch_overhead(c: &mut Criterion) {
    use android_hid_connect::client::GamepadFrameBatcher;
    use android_hid_connect::session::{GamepadFrameRaw, HidSession, OpenRequest};
    use android_hid_connect::transport::MockTransport;

    let frame = GamepadFrameRaw::new(0x0001, 100, 0, 0, 0, 0, 0);
    let batch_frames: Vec<GamepadFrameRaw> = (0..32u16)
        .map(|i| GamepadFrameRaw::new(i as u32, i as i16, i as i16 * 2, i as i16 * 3, i as i16 * 4, 0, 0))
        .collect();

    c.bench_function("session set_frame_raw_unchecked one frame (steady-state)", |b| {
        let mut s = HidSession::open(MockTransport::new(), OpenRequest::gamepad_only_realtime()).unwrap();
        b.iter(|| {
            s.set_frame_raw_unchecked(
                frame.buttons,
                frame.left_x,
                frame.left_y,
                frame.right_x,
                frame.right_y,
                frame.left_trigger,
                frame.right_trigger,
            )
            .unwrap();
            black_box(s.stats());
        });
        s.close().unwrap();
    });

    c.bench_function("client send_frame_unchecked one frame (steady-state)", |b| {
        let (client, dispatcher) = HidSession::open(
            MockTransport::new(),
            OpenRequest::gamepad_only_realtime(),
        )
        .unwrap()
        .into_client_with_bound(65536)
        .unwrap();

        b.iter(|| {
            client.send_frame_unchecked(black_box(frame)).unwrap();
        });

        client.close();
        let t = dispatcher.join().unwrap();
        black_box(t.into_bytes());
    });

    c.bench_function("client gamepad frame batcher unchecked 32", |b| {
        let (client, dispatcher) = HidSession::open(
            MockTransport::new(),
            OpenRequest::gamepad_only_realtime(),
        )
        .unwrap()
        .into_client_with_bound(65536)
        .unwrap();

        b.iter(|| {
            let mut batcher = GamepadFrameBatcher::unchecked(&client, 32);
            batcher.push_many(batch_frames.iter().copied()).unwrap();
        });

        client.close();
        let t = dispatcher.join().unwrap();
        black_box(t.into_bytes());
    });

    c.bench_function("client gamepad frame packed batch fixed 32", |b| {
        use android_hid_connect::session::GAMEPAD_FRAME_BYTES;
        use android_hid_connect::client::PackedGamepadFrameBatcher;

        let mut packed_frames = [[0u8; GAMEPAD_FRAME_BYTES]; 32];
        for i in 0..packed_frames.len() {
            packed_frames[i][0] = (i as u8).wrapping_mul(3);
        }

        let (client, dispatcher) = HidSession::open(
            MockTransport::new(),
            OpenRequest::gamepad_only_realtime(),
        )
        .unwrap()
        .into_client_with_bound(65536)
        .unwrap();

        b.iter(|| {
            let frames = packed_frames;
            client.send_frame_packed_batch_fixed(32, frames).unwrap();
            black_box(client);
        });

        client.close();
        let t = dispatcher.join().unwrap();
        black_box(t.into_bytes());
    });

    c.bench_function("client gamepad frame packed batcher 32", |b| {
        use android_hid_connect::client::PackedGamepadFrameBatcher;
        use android_hid_connect::session::GAMEPAD_FRAME_BYTES;

        let mut packed_frames = Vec::with_capacity(32);
        for i in 0u8..32 {
            let mut f = [0u8; GAMEPAD_FRAME_BYTES];
            f[0] = i.wrapping_mul(2);
            packed_frames.push(f);
        }

        let (client, dispatcher) = HidSession::open(
            MockTransport::new(),
            OpenRequest::gamepad_only_realtime(),
        )
        .unwrap()
        .into_client_with_bound(65536)
        .unwrap();

        b.iter(|| {
            let mut batcher = PackedGamepadFrameBatcher::new(&client, 32);
            batcher.push_many(packed_frames.iter().copied()).unwrap();
        });

        client.close();
        let t = dispatcher.join().unwrap();
        black_box(t.into_bytes());
    });
}

criterion_group!(
    benches,
    bench_keyboard_input,
    bench_serialize,
    bench_send_to_mock,
    bench_coalesced_burst,
    bench_100_msg_raw_vs_coalesced,
    bench_gamepad_frame_pack_and_packed_batch,
    bench_hidclient_dispatch_overhead,
);
criterion_main!(benches);
