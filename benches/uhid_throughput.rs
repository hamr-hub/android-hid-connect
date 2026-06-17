//! UHID throughput micro-benchmark.
//!
//! Run with: `cargo bench --bench uhid_throughput`
//!
//! Measures how many `UHID_INPUT` messages we can serialize per second
//! when the message bytes are dropped on the floor (no I/O). This is
//! the upper bound on the rate the library can pump the host-side
//! scrcpy control socket.

use criterion::{criterion_group, criterion_main, Criterion};
use android_hid_connect::transport::send_one;
use android_hid_connect::transport::MockTransport;
use android_hid_connect::types::HID_MAX_SIZE;
use android_hid_connect::control::message::{ControlMessage, UhidInput};
use android_hid_connect::KeyboardHid;
use std::hint::black_box;

fn bench_keyboard_input(c: &mut Criterion) {
    let mut kbd = KeyboardHid::new();
    // The first key_event requires open_message()'s state to be set; we
    // don't send open here because we're only measuring the hot path.
    c.bench_function("keyboard inject_key (no I/O)", |b| {
        b.iter(|| {
            let m = kbd.key_event(0x04, true, android_hid_connect::Modifiers::empty()).unwrap();
            std::hint::black_box(m);
        });
    });
}

fn bench_serialize(c: &mut Criterion) {
    let mut data = [0u8; HID_MAX_SIZE];
    data[0] = 0x02; // LSHIFT
    data[2] = 0x04; // A
    let msg = ControlMessage::UhidInput(UhidInput {
        id: 1, size: 8, data,
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
    let msg = ControlMessage::UhidInput(UhidInput { id: 1, size: 8, data });
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
    let msg = ControlMessage::UhidInput(UhidInput { id: 1, size: 8, data });
    c.bench_function("coalesced 100-input burst", |b| {
        b.iter(|| {
            let mut w = CoalescingWriter::with_limits(
                MockTransport::new(),
                Duration::from_millis(1),
                4096,
            );
            for _ in 0..100 {
                w.push(black_box(&msg)).unwrap();
            }
            w.flush_now().unwrap();
            std::hint::black_box(w);
        });
    });
}

criterion_group!(
    benches,
    bench_keyboard_input,
    bench_serialize,
    bench_send_to_mock,
    bench_coalesced_burst,
);
criterion_main!(benches);
