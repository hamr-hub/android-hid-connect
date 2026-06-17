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

criterion_group!(benches, bench_keyboard_input, bench_serialize, bench_send_to_mock);
criterion_main!(benches);
