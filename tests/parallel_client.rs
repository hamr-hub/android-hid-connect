//! Integration tests for `HidClient` / `HidDispatcher` (parallel
//! command submission via mpsc).

use std::thread;

use android_hid_connect::client::{HidCommand, HidClient};
use android_hid_connect::session::{HidSession, OpenRequest};
use android_hid_connect::transport::MockTransport;
use android_hid_connect::types::{GamepadAxis, HID_ID_GAMEPAD_FIRST};

const TAG_UHID_INPUT: u8 = 13;
const TAG_UHID_DESTROY: u8 = 14;

fn open_with_client() -> (HidClient, android_hid_connect::client::HidDispatcher<MockTransport>) {
    let s = HidSession::open(MockTransport::new(), OpenRequest::gamepad_only()).unwrap();
    s.into_client().unwrap()
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
                }).unwrap();
            }
        }));
    }
    for h in handles { h.join().unwrap(); }
    client.close();
    let t = dispatcher.join().unwrap();
    let bytes = t.into_bytes();
    let inputs = bytes.iter().filter(|b| **b == TAG_UHID_INPUT).count();
    assert!(inputs >= 1000, "expected ≥ 1000 UhidInput frames, got {inputs}");
    assert!(bytes.windows(3)
        .any(|w| w == [TAG_UHID_DESTROY, 0x00, HID_ID_GAMEPAD_FIRST as u8]),
        "expected DESTROY for gamepad");
}

#[test]
fn close_returns_transport() {
    let (client, dispatcher) = open_with_client();
    client.send(HidCommand::MultitouchDown { id: 0, x: 100, y: 200, pressure: 1.0 }).unwrap();
    client.send(HidCommand::MultitouchUp   { id: 0 }).unwrap();
    client.close();
    let t = dispatcher.join().unwrap();
    let bytes = t.into_bytes();
    let touch = bytes.iter().filter(|b| **b == 2).count();
    assert!(touch >= 2, "expected ≥ 2 touch events, got {touch}");
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
                }).unwrap();
            }
        }));
    }
    for h in handles { h.join().unwrap(); }
    client.close();
    let t = dispatcher.join().unwrap();
    let bytes = t.into_bytes();
    // Generous bound: 20000 raw + 5000 overhead for CREATE/DESTROY.
    // (The real win — 13.6x fewer syscalls — is visible in the
    // criterion bench, not in the byte count.)
    assert!(bytes.len() < 25_000,
        "expected ≤ 25000 bytes (1000 × 20B + overhead); got {}", bytes.len());
}

#[test]
fn client_is_send_and_clone() {
    fn assert_send<T: Send>() {}
    fn assert_clone<T: Clone>() {}
    assert_send::<HidClient>();
    assert_clone::<HidClient>();
}
