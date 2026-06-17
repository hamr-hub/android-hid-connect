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

criterion_group!(
    benches,
    bench_keyboard_input,
    bench_serialize,
    bench_send_to_mock,
    bench_coalesced_burst,
    bench_100_msg_raw_vs_coalesced,
);
criterion_main!(benches);
