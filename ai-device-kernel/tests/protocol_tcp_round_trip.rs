//! TCP-level round-trip tests for the AI Device Kernel wire
//! protocol — see v3 §3.4.
//!
//! These tests don't talk to a real device (that's Phase 6's job).
//! They validate the framing layer end-to-end:
//!
//! ```text
//! host → TCP → daemon → postcard decode → ReplyPayload →
//! postcard encode → TCP → host → postcard decode → typed `ActionResult`
//! ```
//!
//! What this catches that the unit tests can't:
//!
//! - Wire-format ambiguity (varint-length vs fixed-length headers).
//! - Stream fragmentation (a single wire frame may straddle two
//!   `read` calls).
//! - Discriminator-byte collisions across the 4 verbs.
//! - Flush boundaries (we flush our test streams after each write).
//!
//! Run with `cargo test -p ai-device-kernel --test
//! protocol_tcp_round_trip`. CI-friendly: no real device needed.
//!
//! ## Design
//!
//! The tests use `std::net::TcpListener` + a `std::thread` to
//! mimic a daemon without involving Android. The handler is a
//! 50-line stub that decodes a `RequestPayload`, produces a
//! matching `ReplyPayload`, and writes it back in a single frame.
//! The test then decodes the reply and asserts round-trip
//! integrity.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;

use ai_device_kernel::{
    Frame, ReplyPayload, RequestPayload, Verb, Action, ActionId, ActionResult,
    GroundTruth, Predicate, PredicateHandle, PredicateResult, Observation,
    DeviceState, ObservationComponent, FrameFlags,
};

/// Bind a localhost listener on an ephemeral port and return the
/// bound address (so the test knows where to dial).
fn bind_loopback() -> (TcpListener, std::net::SocketAddr) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    (listener, addr)
}

/// Spawn a one-shot server thread that accepts exactly one
/// connection, reads one frame, and writes one reply frame.
fn spawn_one_shot_server(
    listener: TcpListener,
    reply_for: impl Fn(RequestPayload) -> ReplyPayload + Send + 'static,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let (mut sock, _) = listener.accept().expect("accept");
        // Read the first frame (the request).
        let frame = read_frame(&mut sock).expect("read request");
        let request: RequestPayload = frame.decode_request().expect("decode request");
        let reply = reply_for(request);
        let reply_frame = Frame::reply(&reply);
        let encoded = reply_frame.encode();
        sock.write_all(&encoded).expect("write reply");
        sock.flush().expect("flush");
    })
}

/// Read one frame from the stream (blocks until header + payload
/// arrive). Returns the decoded `Frame`.
fn read_frame(sock: &mut TcpStream) -> std::io::Result<Frame> {
    // Read 2-byte header.
    let mut header = [0u8; 2];
    sock.read_exact(&mut header)?;
    let verb = Verb::from_byte(header[0])
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "unknown verb"))?;
    let flags = FrameFlags::from_bits(header[1]);
    // Read varint length (max 10 bytes for usize).
    let mut len_buf = [0u8; 10];
    let mut len_len = 0;
    let payload_len: usize = loop {
        let mut one = [0u8; 1];
        sock.read_exact(&mut one)?;
        len_buf[len_len] = one[0];
        len_len += 1;
        // Decode the varint as we go.
        let mut value: usize = 0;
        let mut shift = 0;
        for byte in &len_buf[..len_len] {
            let cont = byte & 0x80 != 0;
            let chunk = (byte & 0x7F) as usize;
            value |= chunk << shift;
            shift += 7;
            if !cont {
                break;
            }
        }
        if one[0] & 0x80 == 0 {
            break value;
        }
        if len_len >= 10 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "varint > 10 bytes",
            ));
        }
    };
    let mut payload = vec![0u8; payload_len];
    sock.read_exact(&mut payload)?;
    Ok(Frame { verb, flags, payload })
}

#[test]
fn action_request_round_trips_over_loopback_tcp() {
    let (listener, addr) = bind_loopback();
    let _ = spawn_one_shot_server(listener, |req| {
        match req {
            RequestPayload::Action { action, .. } => {
                let landed = !matches!(action, Action::InjectRaw { .. });
                ReplyPayload::Action(ActionResult {
                    id: ActionId(1),
                    landed,
                    ground_truth: GroundTruth::default(),
                    elapsed_ms: 1,
                })
            }
            other => panic!("unexpected request shape: {other:?}"),
        }
    });

    let mut client = TcpStream::connect(addr).expect("connect");
    let request = RequestPayload::Action {
        id: ActionId(1),
        action: Action::TapSelector {
            selector: "Button[id=login]".into(),
            deadline_ms: 1000,
        },
    };
    let frame = Frame::request(&request);
    let encoded = frame.encode();
    client.write_all(&encoded).expect("write request");
    client.flush().expect("flush");

    let reply_frame = read_frame(&mut client).expect("reply");
    assert_eq!(reply_frame.verb, Verb::Action);
    let reply: ReplyPayload = reply_frame.decode_reply().expect("decode reply");
    match reply {
        ReplyPayload::Action(ar) => {
            assert!(ar.landed);
            assert_eq!(ar.elapsed_ms, 1);
            assert_eq!(ar.id, ActionId(1));
        }
        _ => panic!("expected Action reply"),
    }
}

#[test]
fn query_request_round_trips_over_loopback_tcp() {
    let (listener, addr) = bind_loopback();
    let _ = spawn_one_shot_server(listener, |req| {
        match req {
            RequestPayload::Query { .. } => ReplyPayload::Query(Observation {
                seq: 1,
                timestamp_ms: 100,
                a11y: None,
                frame: None,
                state: DeviceState::unknown(100),
                events: vec![],
            }),
            other => panic!("unexpected: {other:?}"),
        }
    });

    let mut client = TcpStream::connect(addr).expect("connect");
    let request = RequestPayload::Query {
        a11y: true,
        frame: false,
        state: true,
    };
    let frame = Frame::request(&request);
    client.write_all(&frame.encode()).expect("write");
    client.flush().expect("flush");

    let reply_frame = read_frame(&mut client).expect("reply");
    let reply: ReplyPayload = reply_frame.decode_reply().expect("decode");
    match reply {
        ReplyPayload::Query(obs) => {
            assert_eq!(obs.seq, 1);
            assert_eq!(obs.timestamp_ms, 100);
        }
        _ => panic!("expected Query reply"),
    }
}

#[test]
fn plan_request_round_trips_over_loopback_tcp() {
    let (listener, addr) = bind_loopback();
    let _ = spawn_one_shot_server(listener, |req| {
        match req {
            RequestPayload::Plan { id, .. } => {
                use ai_device_kernel::{PlanResult, StepResult};
                ReplyPayload::Plan(PlanResult {
                    plan_id: id,
                    steps: vec![StepResult {
                        step_id: ai_device_kernel::StepId(0),
                        index: 0,
                        action_result: ActionResult {
                            id: ActionId(1),
                            landed: true,
                            ground_truth: GroundTruth::default(),
                            elapsed_ms: 1,
                        },
                        landed: true,
                        error: None,
                    }],
                    final_observation: Observation {
                        seq: 0,
                        timestamp_ms: 0,
                        a11y: None,
                        frame: None,
                        state: DeviceState::unknown(0),
                        events: vec![],
                    },
                    total_elapsed_ms: 1,
                    all_landed: true,
                })
            }
            other => panic!("unexpected: {other:?}"),
        }
    });

    let mut client = TcpStream::connect(addr).expect("connect");
    let request = RequestPayload::Plan {
        id: ai_device_kernel::PlanId(7),
        plan: ai_device_kernel::Plan::new(vec![ai_device_kernel::PlanStep::new(
            Action::Wait {
                predicate: Predicate::SelectorMatches {
                    selector: "Button".into(),
                    timeout_ms: 500,
                },
                deadline_ms: 500,
            },
        )]),
    };
    let frame = Frame::request(&request);
    client.write_all(&frame.encode()).expect("write");
    client.flush().expect("flush");

    let reply_frame = read_frame(&mut client).expect("reply");
    assert_eq!(reply_frame.verb, Verb::Plan);
    let reply: ReplyPayload = reply_frame.decode_reply().expect("decode");
    match reply {
        ReplyPayload::Plan(pr) => {
            assert_eq!(pr.plan_id, ai_device_kernel::PlanId(7));
            assert!(pr.all_landed);
            assert_eq!(pr.steps.len(), 1);
        }
        _ => panic!("expected Plan reply"),
    }
}

#[test]
fn observe_request_accepts_filter_round_trip() {
    use ai_device_kernel::EventKind;

    let (listener, addr) = bind_loopback();
    let _ = spawn_one_shot_server(listener, |req| {
        // Echo back an EndOfStream marker so the host stops
        // reading (server-stream simulation).
        match req {
            RequestPayload::Observe { .. } => {
                ReplyPayload::EndOfStream { final_seq: 0 }
            }
            other => panic!("unexpected: {other:?}"),
        }
    });

    let mut client = TcpStream::connect(addr).expect("connect");
    let request = RequestPayload::Observe {
        since_seq: 0,
        filter: vec![EventKind::ActivityResumed, EventKind::SceneChangeDetected],
    };
    let frame = Frame::request(&request);
    client.write_all(&frame.encode()).expect("write");
    client.flush().expect("flush");

    let reply_frame = read_frame(&mut client).expect("reply");
    assert_eq!(reply_frame.verb, Verb::EndOfStream);
    let reply: ReplyPayload = reply_frame.decode_reply().expect("decode");
    match reply {
        ReplyPayload::EndOfStream { final_seq } => assert_eq!(final_seq, 0),
        _ => panic!("expected EndOfStream"),
    }
}

#[test]
fn predicate_result_round_trips() {
    let result = PredicateResult::Matched {
        handle: PredicateHandle(7),
        elapsed_ms: 1234,
    };
    let bytes = postcard::to_allocvec(&result).expect("encode");
    let decoded: PredicateResult = postcard::from_bytes(&bytes).expect("decode");
    assert_eq!(decoded, result);
}

#[test]
fn frame_verb_distinct_postcard_tags() {
    // 4 verbs + EndOfStream. Each verb byte maps to a unique
    // payload tag in the RequestPayload enum.
    let verbs_and_payloads: Vec<(Verb, RequestPayload)> = vec![
        (Verb::Action, RequestPayload::Action {
            id: ActionId(0),
            action: Action::Tap { x: 0, y: 0, deadline_ms: 0 },
        }),
        (Verb::Plan, RequestPayload::Plan {
            id: ai_device_kernel::PlanId(0),
            plan: ai_device_kernel::Plan::new(vec![]),
        }),
        (Verb::Observe, RequestPayload::Observe {
            since_seq: 0,
            filter: vec![],
        }),
        (Verb::Query, RequestPayload::Query {
            a11y: true,
            frame: false,
            state: false,
        }),
    ];
    let n = verbs_and_payloads.len();
    let mut tags_seen: Vec<u8> = Vec::new();
    for (verb, payload) in &verbs_and_payloads {
        assert_eq!(payload.verb(), *verb);
        let bytes = postcard::to_allocvec(payload).expect("encode");
        // The postcard tag is the first byte for enum encodings.
        tags_seen.push(bytes[0]);
    }
    let unique: std::collections::HashSet<_> = tags_seen.iter().copied().collect();
    assert_eq!(
        unique.len(),
        n,
        "duplicate RequestPayload postcard tags: {tags_seen:?}"
    );
}

#[test]
fn large_payload_framing_survives_fragmentation() {
    // 4 KiB observation JSON in a Query — well over the unit-test
    // size. Confirms the varint length prefix handles multi-byte
    // lengths and the stream decoder waits for the full payload.
    let (listener, addr) = bind_loopback();
    let _ = spawn_one_shot_server(listener, |req| {
        // Lenient server — whatever request verb, just echo back
        // a matching stub reply so the wire framing is what gets
        // tested (not server semantics).
        match req {
            RequestPayload::Query { .. } => ReplyPayload::Query(Observation {
                seq: 0,
                timestamp_ms: 0,
                a11y: None,
                frame: None,
                state: DeviceState::unknown(0),
                events: vec![],
            }),
            // The Action-with-4KiB-selector case sends an Action
            // reply; we synthesise one here.
            RequestPayload::Action { id, .. } => ReplyPayload::Action(ActionResult {
                id,
                landed: true,
                ground_truth: GroundTruth::default(),
                elapsed_ms: 1,
            }),
            _ => ReplyPayload::EndOfStream { final_seq: 0 },
        }
    });

    let mut client = TcpStream::connect(addr).expect("connect");
    let big_selector = "x".repeat(4096);
    let request = RequestPayload::Action {
        id: ActionId(0),
        action: Action::TapSelector {
            selector: big_selector,
            deadline_ms: 1000,
        },
    };
    let frame = Frame::request(&request);
    let encoded = frame.encode();
    eprintln!("encoded size = {}", encoded.len());
    assert!(
        encoded.len() > 1024,
        "encoded size {encoded_len} should exceed 1 KiB",
        encoded_len = encoded.len(),
    );
    client.write_all(&encoded).expect("write");
    client.flush().expect("flush");

    let reply_frame = read_frame(&mut client).expect("reply");
    assert_eq!(reply_frame.verb, Verb::Action);
    let _reply: ReplyPayload = reply_frame.decode_reply().expect("decode");
}

#[test]
fn observation_component_round_trips() {
    let all = [
        ObservationComponent::A11y,
        ObservationComponent::Frame,
        ObservationComponent::State,
        ObservationComponent::Events,
        ObservationComponent::ForceKeyframe,
    ];
    for c in &all {
        let bytes = postcard::to_allocvec(c).expect("encode");
        let decoded: ObservationComponent =
            postcard::from_bytes(&bytes).expect("decode");
        assert_eq!(decoded, *c);
    }
}
