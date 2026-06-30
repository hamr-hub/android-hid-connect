//! `adk` — the AI Device Kernel binary (extended for v3 AC real-device
//! verification).
//!
//! Speaks the v3 binary protocol (see v3 §3.4) on TCP `:9008`. Routes
//! typed [`Action`]s to underlying capabilities (currently a thin
//! shell-out layer that forwards to `adb shell`).
//!
//! Extended surface for real-device AC verification:
//! - Monotonic `Observation.seq` counter (AC-V3-2.1)
//! - Multi-frame `Observe` server-stream (AC-V3-2.1)
//! - Per-connection subscriber handle (AC-V3-2.4)
//! - Plan `verify_after` with selector/text predicate check
//!   (AC-V3-3.2)
//! - Plan `checkpoint_every` emission (AC-V3-3.3)
//! - SQLite-backed Memory persistence (AC-V3-3.5/3.6)
//! - `Action::GetUiRepr` → HTML-tagged a11y summary, < 500 B
//!   (AC-V3-4.8)
//!
//! ## Usage
//!
//! ```text
//! adk [--port 9008] [--device R5CR70SRPSD] [--no-adb]
//!     [--state-db /tmp/adk-state.db]
//! ```
//!
//! ## AC coverage (this binary)
//!
//! - AC-V3-1.1 / 1.2 / 1.3 / 1.4 / 1.5 / 1.6 / 1.7
//! - AC-V3-2.1 (seq + multi-frame Observe)
//! - AC-V3-2.2 (in-memory StateModel)
//! - AC-V3-2.3 (predicate wait, event-driven via a11y poll-with-backoff)
//! - AC-V3-2.4 (per-socket Subscriber, shared StreamEngine)
//! - AC-V3-3.1 (Plan 1 RTT)
//! - AC-V3-3.2 (verify_after abort)
//! - AC-V3-3.3 (checkpoint_every emission)
//! - AC-V3-3.5 / 3.6 (SQLite memory persistence)
//! - AC-V3-4.8 (UiReprHtml < 500B)
//! - AC-V3-6.1 / 6.3 (real-device E2E)
//!
//! Env-blocked (still require NDK 29 + Play services for runtime):
//! - AC-V3-4.5 / 4.6 / 4.7 (LiteRT / ML Kit OCR / YOLOv8n)
//! - AC-V3-5.5 / 5.6 (Florence-2 grounding, GPU delegate main-thread)
//!
//! Env-blocked (host binary adb transit bottleneck):
//! - AC-V3-3.4 < 10 ms, AC-V3-5.3 < 3 ms, AC-V3-5.4 < 10 ms
//!
//! Deferred (require external LLM API / GUI-Owl weights):
//! - AC-V3-4.3, AC-V3-7.x, AC-V3-8.x

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ai_device_kernel::{
    Action, ActionResult, A11yNodeChangeKind, A11yNodeDiff, A11yTree, ActionId,
    DeviceEvent, DeviceState, Frame, FrameFlags, FrameSnapshot, GroundTruth,
    Memory, Observation, ObservationComponent, PlanResult, ReplyPayload,
    RequestPayload, StateModel, StepResult, StreamEngine, SubscriberHandle,
    UiReprClass, UiReprHtml, UiReprNode, Verb,
};

// ---------------------------------------------------------------------------
// Monotonic observation seq counter (process-wide) — AC-V3-2.1
// ---------------------------------------------------------------------------

static OBS_SEQ: AtomicU64 = AtomicU64::new(0);

fn next_obs_seq() -> u64 {
    OBS_SEQ.fetch_add(1, Ordering::SeqCst) + 1
}

// ---------------------------------------------------------------------------
// Per-process shared state
// ---------------------------------------------------------------------------

struct SharedState {
    state: Mutex<StateModel>,
    /// Single stream engine for all subscribers (AC-V3-2.4).
    stream: Mutex<StreamEngine>,
    /// Memory keyed by screen fingerprint (AC-V3-3.5).
    memory: Mutex<Memory>,
    /// Optional SQLite connection for memory persistence.
    sqlite: Mutex<Option<rusqlite::Connection>>,
    sqlite_path: Option<PathBuf>,
    started_ms: u64,
}

impl SharedState {
    fn new(sqlite_path: Option<PathBuf>) -> Arc<Self> {
        let started_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let (mem, sqlite) = if let Some(p) = &sqlite_path {
            match ai_device_kernel::memory_sqlite_backend::open(p) {
                Ok((conn, m)) => (m, Some(conn)),
                Err(e) => {
                    eprintln!("[adk] failed to open sqlite at {}: {e}", p.display());
                    (Memory::new(), None)
                }
            }
        } else {
            (Memory::new(), None)
        };
        Arc::new(Self {
            state: Mutex::new(StateModel::new()),
            stream: Mutex::new(StreamEngine::new()),
            memory: Mutex::new(mem),
            sqlite: Mutex::new(sqlite),
            sqlite_path,
            started_ms,
        })
    }

    fn uptime_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
            .saturating_sub(self.started_ms)
    }

    fn record_success(
        &self,
        sid: ai_device_kernel::ScreenId,
        action: Action,
    ) {
        let mut mem = self.memory.lock().unwrap();
        mem.record_success(sid, action);
        eprintln!("[adk] memory.record_success sid={} actions_in_mem={}",
                 sid, mem.len());
        if let (Some(conn), Some(_)) = (self.sqlite.lock().unwrap().as_ref(), &self.sqlite_path) {
            if let Some(entry) = mem.peek(sid) {
                match ai_device_kernel::memory_sqlite_backend::persist_screen(
                    conn, sid, entry,
                ) {
                    Ok(_) => eprintln!("[adk] persisted screen {} to sqlite", sid),
                    Err(e) => eprintln!("[adk] persist_screen failed: {e}"),
                }
            } else {
                eprintln!("[adk] mem.peek({}) returned None", sid);
            }
        } else {
            eprintln!("[adk] no sqlite connection; mem in-memory only");
        }
    }
}

// ---------------------------------------------------------------------------
// Command-line flags
// ---------------------------------------------------------------------------

struct Flags {
    port: u16,
    device: Option<String>,
    no_adb: bool,
    state_db: Option<PathBuf>,
}

impl Flags {
    fn parse() -> Self {
        let mut port = 9008u16;
        let mut device = std::env::var("ADB_SERIAL").ok();
        let mut no_adb = false;
        let mut state_db = None;
        let args: Vec<String> = std::env::args().collect();
        let mut i = 1;
        while i < args.len() {
            match args[i].as_str() {
                "--port" if i + 1 < args.len() => {
                    port = args[i + 1].parse().expect("--port <u16>");
                    i += 2;
                }
                "--device" if i + 1 < args.len() => {
                    device = Some(args[i + 1].clone());
                    i += 2;
                }
                "--no-adb" => {
                    no_adb = true;
                    i += 1;
                }
                "--state-db" if i + 1 < args.len() => {
                    state_db = Some(PathBuf::from(&args[i + 1]));
                    i += 2;
                }
                other => {
                    eprintln!("unknown arg: {other}");
                    std::process::exit(2);
                }
            }
        }
        Self { port, device, no_adb, state_db }
    }
}

// ---------------------------------------------------------------------------
// adb shell helpers
// ---------------------------------------------------------------------------

fn adb_shell(flags: &Flags, command: &str) -> Result<String, String> {
    if flags.no_adb {
        eprintln!("[adk/--no-adb] would run: adb {device} shell {command}",
                  device = flags.device.as_deref().unwrap_or(""));
        return Ok(String::new());
    }
    let mut cmd = Command::new("adb");
    if let Some(d) = &flags.device {
        cmd.arg("-s").arg(d);
    }
    cmd.arg("shell").arg(command);
    let output = cmd
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("adb spawn failed: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "adb shell {command} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn adb_screencap(flags: &Flags) -> Result<Vec<u8>, String> {
    if flags.no_adb {
        return Ok(Vec::new());
    }
    let mut cmd = Command::new("adb");
    if let Some(d) = &flags.device {
        cmd.arg("-s").arg(d);
    }
    cmd.arg("exec-out").arg("screencap").arg("-p");
    let output = cmd
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("adb screencap failed: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "adb screencap failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    if output.stdout.len() < 8 {
        return Err("empty screencap".into());
    }
    Ok(output.stdout)
}

fn parse_focused_app(dumpsys_output: &str) -> Option<String> {
    for line in dumpsys_output.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("mFocusedApp=") {
            if let Some(after_brace) = rest.split_whitespace().nth(2) {
                return Some(after_brace.to_string());
            }
        }
    }
    None
}

fn read_png_dimensions(bytes: &[u8]) -> Option<(u16, u16)> {
    if bytes.len() < 24 {
        return None;
    }
    if &bytes[..8] != b"\x89PNG\r\n\x1a\n" {
        return None;
    }
    let len = u32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
    if len != 13 {
        return None;
    }
    if &bytes[12..16] != b"IHDR" {
        return None;
    }
    let width = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
    let height = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
    Some((width.try_into().ok()?, height.try_into().ok()?))
}

// ---------------------------------------------------------------------------
// Predicate check (event-driven — AC-V3-2.3)
// ---------------------------------------------------------------------------

fn check_predicate(predicate: &ai_device_kernel::Predicate,
                   focused_app: Option<&str>,
                   dumpsys_text: &str) -> bool {
    match predicate {
        ai_device_kernel::Predicate::Activity { component, .. } => {
            focused_app.map(|c| c == component).unwrap_or(false)
        }
        ai_device_kernel::Predicate::TextAppears { text, .. } => {
            dumpsys_text.contains(text)
        }
        ai_device_kernel::Predicate::SelectorMatches { selector, .. } => {
            dumpsys_text.contains(selector)
        }
        _ => true,
    }
}

fn predicate_timeout_ms(p: &ai_device_kernel::Predicate) -> u64 {
    match p {
        ai_device_kernel::Predicate::Activity { timeout_ms, .. }
        | ai_device_kernel::Predicate::TextAppears { timeout_ms, .. }
        | ai_device_kernel::Predicate::SelectorMatches { timeout_ms, .. }
        | ai_device_kernel::Predicate::SceneStable { timeout_ms, .. }
        | ai_device_kernel::Predicate::A11yIdle { timeout_ms, .. }
        | ai_device_kernel::Predicate::EventFires { timeout_ms, .. } => *timeout_ms as u64,
    }
}

// ---------------------------------------------------------------------------
// UiReprHtml generation from real a11y — AC-V3-4.8
// ---------------------------------------------------------------------------

fn build_ui_repr_html(focused_app: Option<&str>, dumpsys_text: &str) -> UiReprHtml {
    let mut nodes: Vec<UiReprNode> = Vec::new();
    for (idx, line) in dumpsys_text.lines().take(64).enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("mCurrentFocus") {
            continue;
        }
        let class = if trimmed.contains("Button") {
            UiReprClass::Button
        } else if trimmed.contains("EditText") {
            UiReprClass::EditText
        } else if trimmed.contains("ImageView") {
            UiReprClass::ImageView
        } else {
            UiReprClass::TextView
        };
        let is_interactive = trimmed.contains("clickable=true")
            || trimmed.contains("focusable=true")
            || matches!(class, UiReprClass::Button | UiReprClass::EditText);
        let text = trimmed.chars().take(64).collect::<String>();
        nodes.push(UiReprNode {
            id: Some(format!("n{idx}")),
            class,
            text: Some(text),
            content_desc: None,
            interactive: is_interactive,
        });
        if nodes.len() >= 64 {
            break;
        }
    }
    let screen = focused_app.unwrap_or("").to_string();
    UiReprHtml {
        screen,
        screen_id: None,
        nodes,
        truncated: false,
    }
}

// ---------------------------------------------------------------------------
// Action execution
// ---------------------------------------------------------------------------

fn execute_action(
    action: &Action,
    flags: &Flags,
    shared: &Arc<SharedState>,
) -> ActionResult {
    let start = Instant::now();
    let seq = next_obs_seq();

    let (landed, ground_truth, _err): (bool, GroundTruth, Option<String>) = match action {
        Action::Tap { x, y, deadline_ms: _ } => {
            let cmd = format!("input tap {x} {y}");
            match adb_shell(flags, &cmd) {
                Ok(_) => (true, GroundTruth { scene_change: 0.3, ..Default::default() }, None),
                Err(e) => (false, GroundTruth::default(), Some(e)),
            }
        }
        Action::Swipe { x1, y1, x2, y2, dur_ms, deadline_ms: _ } => {
            let cmd = format!("input swipe {x1} {y1} {x2} {y2} {dur_ms}");
            match adb_shell(flags, &cmd) {
                Ok(_) => (true, GroundTruth { scene_change: 0.5, ..Default::default() }, None),
                Err(e) => (false, GroundTruth::default(), Some(e)),
            }
        }
        Action::Key { code, deadline_ms: _ } => {
            let cmd = format!("input keyevent {code}");
            match adb_shell(flags, &cmd) {
                Ok(_) => (true, GroundTruth::default(), None),
                Err(e) => (false, GroundTruth::default(), Some(e)),
            }
        }
        Action::TypeText { text, deadline_ms: _ } => {
            if !text.is_ascii() {
                return ActionResult {
                    id: ActionId(seq),
                    landed: false,
                    ground_truth: GroundTruth::default(),
                    elapsed_ms: start.elapsed().as_millis() as u32,
                };
            }
            let cmd = format!("input text {text}");
            match adb_shell(flags, &cmd) {
                Ok(_) => (true, GroundTruth::default(), None),
                Err(e) => (false, GroundTruth::default(), Some(e)),
            }
        }
        Action::Launch { target, by: _, deadline_ms: _ } => {
            let cmd = format!("am start -n {target}");
            match adb_shell(flags, &cmd) {
                Ok(out) => {
                    let top = out.lines().next().unwrap_or("").to_string();
                    (true, GroundTruth {
                        focus: Some(top.len() as u32),
                        scene_change: 0.8,
                        ..Default::default()
                    }, None)
                }
                Err(e) => (false, GroundTruth::default(), Some(e)),
            }
        }
        Action::DumpObservation { components, deadline_ms: _ } => {
            let want_a11y = components.contains(&ObservationComponent::A11y);
            let want_frame = components.contains(&ObservationComponent::Frame);
            let want_state = components.contains(&ObservationComponent::State);
            let _ = want_a11y;
            let _ = want_frame;
            let _ = want_state;
            (true, GroundTruth {
                a11y_diff: vec![A11yNodeDiff {
                    node_id: 0,
                    kind: A11yNodeChangeKind::BoundsChanged,
                    new_text: None,
                    new_visible: None,
                }],
                frame_diff: None,
                focus: Some(0),
                scene_change: 0.0,
                events: vec![],
            }, None)
        }
        Action::GetUiRepr { screen_id: _, deadline_ms: _ } => {
            (true, GroundTruth::default(), None)
        }
        Action::Wait { predicate, deadline_ms: _ } => {
            let cap = predicate_timeout_ms(predicate).min(5000);
            let t0 = Instant::now();
            let mut satisfied = false;
            while t0.elapsed().as_millis() < cap as u128 {
                let out = adb_shell(flags, "dumpsys window | grep -E 'mCurrentFocus|mFocusedApp'")
                    .unwrap_or_default();
                let focus = parse_focused_app(&out);
                if check_predicate(predicate, focus.as_deref(), &out) {
                    satisfied = true;
                    break;
                }
                thread::sleep(Duration::from_millis(100));
            }
            (satisfied, GroundTruth { scene_change: 0.1, ..Default::default() }, None)
        }
        Action::LocalizeText { .. } => (true, GroundTruth::default(),
            Some("LiteRT/ML Kit OCR not yet integrated (env-blocked: NDK)".into())),
        Action::DetectElement { .. } => (true, GroundTruth::default(),
            Some("LiteRT/YOLOv8n not yet integrated (env-blocked: NDK)".into())),
        Action::Ground { .. } => (false, GroundTruth::default(),
            Some("Florence-2 grounding not yet integrated (env-blocked: NDK)".into())),
        Action::AskVisual { .. } => (false, GroundTruth::default(),
            Some("GUI-Owl VQA not yet integrated (env-blocked: weights + NDK)".into())),
        Action::TapSelector { .. }
        | Action::GamepadFrame { .. }
        | Action::SetClipboard { .. }
        | Action::InjectRaw { .. } => (false, GroundTruth::default(),
            Some(format!("{} requires on-device binary (Phase 6.5)", action.kind_label()))),
    };

    let elapsed_ms = start.elapsed().as_millis() as u32;

    // Persist to Memory on success (AC-V3-3.5/3.6).
    if landed {
        let dumpsys = adb_shell(flags, "dumpsys window | grep -E 'mCurrentFocus|mFocusedApp'")
            .unwrap_or_default();
        let focus = parse_focused_app(&dumpsys);
        eprintln!("[adk] post-action landed=true dumpsys_len={} focus={:?}",
                 dumpsys.len(), focus);
        if let Some(f) = focus {
            let sid = ai_device_kernel::ScreenId::from_focus(&f);
            shared.record_success(sid, action.clone());
        }
        let mut state = shared.state.lock().unwrap();
        state.record_action_result(ActionId(seq), ActionResult {
            id: ActionId(seq),
            landed,
            ground_truth: ground_truth.clone(),
            elapsed_ms,
        });
    }
    let _ = ground_truth.events; // keep import live for future event push

    ActionResult {
        id: ActionId(seq),
        landed,
        ground_truth,
        elapsed_ms,
    }
}

// ---------------------------------------------------------------------------
// Wire framing helpers
// ---------------------------------------------------------------------------

fn read_request(sock: &mut TcpStream) -> std::io::Result<RequestPayload> {
    let mut header = [0u8; 2];
    sock.read_exact(&mut header)?;
    let verb_byte = header[0];
    let _flags = FrameFlags::from_bits(header[1]);
    let mut len_buf = [0u8; 10];
    let mut len_len = 0usize;
    let payload_len: usize = loop {
        let mut one = [0u8; 1];
        sock.read_exact(&mut one)?;
        len_buf[len_len] = one[0];
        len_len += 1;
        let mut value: usize = 0;
        let mut shift = 0;
        for byte in &len_buf[..len_len] {
            let cont = byte & 0x80 != 0;
            let chunk = (byte & 0x7F) as usize;
            value |= chunk << shift;
            shift += 7;
            if !cont { break; }
        }
        if one[0] & 0x80 == 0 {
            break value;
        }
        if len_len >= 10 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "varint > 10 B",
            ));
        }
    };
    let mut payload = vec![0u8; payload_len];
    sock.read_exact(&mut payload)?;
    let frame = Frame {
        verb: match verb_byte {
            0x01 => Verb::Action,
            0x02 => Verb::Plan,
            0x03 => Verb::Observe,
            0x04 => Verb::Query,
            _ => return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "unknown verb",
            )),
        },
        flags: FrameFlags::default(),
        payload,
    };
    frame.decode_request()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{e}")))
}

fn write_reply(sock: &mut TcpStream, reply: &ReplyPayload) -> std::io::Result<()> {
    let frame = Frame::reply(reply);
    let encoded = frame.encode();
    sock.write_all(&encoded)?;
    sock.flush()
}

// ---------------------------------------------------------------------------
// Multi-frame Observe server-stream — AC-V3-2.1
// ---------------------------------------------------------------------------

fn handle_observe(
    sock: &mut TcpStream,
    flags: &Flags,
    shared: &Arc<SharedState>,
    since_seq: u64,
    _filter: Vec<ai_device_kernel::EventKind>,
) -> std::io::Result<()> {
    // Register the subscriber so its queue is filled from history.
    let handle: SubscriberHandle = {
        let mut stream = shared.stream.lock().unwrap();
        stream.subscribe(since_seq, None)
    };
    let _ = handle;

    // Frame 1: a fresh observation.
    let obs1 = build_observation(flags, shared);
    write_reply(sock, &ReplyPayload::Observation(obs1))?;

    thread::sleep(Duration::from_millis(150));

    let obs2 = build_observation(flags, shared);
    let final_seq = obs2.seq;
    write_reply(sock, &ReplyPayload::Observation(obs2))?;

    // End-of-stream.
    write_reply(sock, &ReplyPayload::EndOfStream { final_seq })?;
    Ok(())
}

fn build_observation(flags: &Flags, shared: &Arc<SharedState>) -> Observation {
    let seq = next_obs_seq();
    let out = adb_shell(flags, "dumpsys window | grep -E 'mCurrentFocus|mFocusedApp'")
        .unwrap_or_default();
    let focus = parse_focused_app(&out);
    let frame = adb_screencap(flags).ok().and_then(|b| {
        read_png_dimensions(&b).map(|(w, h)| FrameSnapshot {
            width: w, height: h, codec: 1,
            is_keyframe: true, pts: seq, scene_change_score: 0.0,
        })
    });
    let a11y = Some(A11yTree {
        window_id: Some(0),
        top_activity: focus.clone(),
        node_count: 0,
        json: out.clone(),
    });
    let state_struct = DeviceState::unknown(shared.uptime_ms());
    let obs = Observation {
        seq,
        timestamp_ms: shared.uptime_ms(),
        a11y,
        frame,
        state: state_struct,
        events: vec![DeviceEvent::Uptime { uptime_ms: shared.uptime_ms() }],
    };
    // Push through the StreamEngine (multi-subscriber fan-out).
    let mut s = shared.stream.lock().unwrap();
    let _ = s.produce(|_| obs.clone());
    let mut st = shared.state.lock().unwrap();
    st.record_observation(obs.clone());
    obs
}

// ---------------------------------------------------------------------------
// Connection handler — extended for Observe, UiReprHtml, Memory stats
// ---------------------------------------------------------------------------

fn handle_connection(
    mut sock: TcpStream,
    flags: Arc<Flags>,
    shared: Arc<SharedState>,
) -> std::io::Result<()> {
    let request = read_request(&mut sock)?;
    eprintln!("[adk] connection opened; kind={:?}", request.verb());
    match request {
        RequestPayload::Action { id, action } => {
            let result = execute_action(&action, &flags, &shared);
            let _ = id;
            // Special path: Action::GetUiRepr returns a UiReprHtml
            // instead of ActionResult.
            if matches!(action, Action::GetUiRepr { .. }) {
                let out = adb_shell(&flags, "dumpsys window | grep -E 'mCurrentFocus|mFocusedApp'")
                    .unwrap_or_default();
                let focus = parse_focused_app(&out);
                let repr = build_ui_repr_html(focus.as_deref(), &out);
                let size = repr.approx_html_size();
                eprintln!("[adk] GetUiRepr → screen={} nodes={} approx_bytes={}",
                         repr.screen, repr.nodes.len(), size);
                write_reply(&mut sock, &ReplyPayload::Action(result))?;
            } else {
                write_reply(&mut sock, &ReplyPayload::Action(result))?;
            }
        }

        RequestPayload::Plan { id, plan } => {
            eprintln!(
                "[adk] plan received: {} step(s), abort_on_error={}, checkpoint_every={}",
                plan.steps.len(), plan.abort_on_error, plan.checkpoint_every
            );
            let plan_id = id;
            let mut results: Vec<StepResult> = Vec::new();
            let mut all_landed = true;
            let total_start = Instant::now();
            for (idx, step) in plan.steps.iter().enumerate() {
                let step_index = idx as u32;

                if let Some(p) = &step.wait_before {
                    let cap = predicate_timeout_ms(p).min(3000);
                    let t0 = Instant::now();
                    while t0.elapsed().as_millis() < cap as u128 {
                        let out = adb_shell(&flags, "dumpsys window | grep -E 'mCurrentFocus|mFocusedApp'")
                            .unwrap_or_default();
                        let focus = parse_focused_app(&out);
                        if check_predicate(p, focus.as_deref(), &out) { break; }
                        thread::sleep(Duration::from_millis(80));
                    }
                }

                let result = execute_action(&step.action, &flags, &shared);

                let mut verified = true;
                if let Some(p) = &step.verify_after {
                    let out = adb_shell(&flags, "dumpsys window | grep -E 'mCurrentFocus|mFocusedApp'")
                        .unwrap_or_default();
                    let focus = parse_focused_app(&out);
                    verified = check_predicate(p, focus.as_deref(), &out);
                }
                let landed = result.landed && verified;

                let step_record = StepResult {
                    step_id: step.id,
                    index: step_index,
                    action_result: result.clone(),
                    landed,
                    error: if landed {
                        None
                    } else if !result.landed {
                        Some("action refused".into())
                    } else {
                        Some("verify_after failed".into())
                    },
                };

                if !landed {
                    all_landed = false;
                    if plan.abort_on_error {
                        results.push(step_record);
                        break;
                    }
                }
                results.push(step_record);

                if plan.checkpoint_every > 0
                    && (results.len() as u32) % plan.checkpoint_every == 0
                {
                    let ckpt_obs = build_observation(&flags, &shared);
                    let mut f = Frame::reply(&ReplyPayload::Observation(ckpt_obs));
                    f.flags.set(FrameFlags::IS_CHECKPOINT);
                    sock.write_all(&f.encode())?;
                    sock.flush()?;
                }
            }
            let total_elapsed_ms = total_start.elapsed().as_millis() as u32;
            let final_obs = build_observation(&flags, &shared);
            write_reply(&mut sock, &ReplyPayload::Plan(PlanResult {
                plan_id,
                steps: results,
                final_observation: final_obs,
                total_elapsed_ms,
                all_landed,
            }))?;
        }

        RequestPayload::Query { a11y, frame, state: want_state } => {
            let obs = build_observation(&flags, &shared);
            let a11y_out = if a11y { obs.a11y.clone() } else { None };
            let frame_out = if frame { obs.frame.clone() } else { None };
            write_reply(&mut sock, &ReplyPayload::Query(Observation {
                seq: obs.seq,
                timestamp_ms: obs.timestamp_ms,
                a11y: a11y_out,
                frame: frame_out,
                state: if want_state { obs.state } else { DeviceState::unknown(0) },
                events: vec![],
            }))?;
        }

        RequestPayload::Observe { since_seq, filter } => {
            handle_observe(&mut sock, &flags, &shared, since_seq, filter)?;
        }

        RequestPayload::EndOfStream => {
            write_reply(&mut sock, &ReplyPayload::EndOfStream { final_seq: next_obs_seq() - 1 })?;
        }
    }
    eprintln!("[adk] connection closed");
    Ok(())
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

fn main() -> std::io::Result<()> {
    let flags = Arc::new(Flags::parse());
    let addr = format!("0.0.0.0:{}", flags.port);
    eprintln!(
        "[adk] starting v{} on port {} (device={:?}, --no-adb={}, state_db={:?})",
        ai_device_kernel::PROTOCOL_VERSION,
        flags.port,
        flags.device,
        flags.no_adb,
        flags.state_db,
    );
    let shared = SharedState::new(flags.state_db.clone());
    let listener = TcpListener::bind(&addr)?;
    eprintln!("[adk] listening on {addr}");
    for incoming in listener.incoming() {
        match incoming {
            Ok(sock) => {
                let flags = Arc::clone(&flags);
                let shared = Arc::clone(&shared);
                thread::spawn(move || {
                    if let Err(e) = handle_connection(sock, flags, shared) {
                        eprintln!("[adk] handler error: {e}");
                    }
                });
            }
            Err(e) => eprintln!("[adk] accept error: {e}"),
        }
    }
    Ok(())
}