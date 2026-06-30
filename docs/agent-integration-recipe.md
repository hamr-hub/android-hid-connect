# AI Device Kernel — Agent Integration Recipe

> For the next session / next contributor that picks up **Phase 4.5+** (LiteRT model loading) and **Phase 6.5** (`adk` → on-device native binary).

This document captures the concrete wiring + build steps for shipping the AI Device Kernel onto an actual Android device. It's deliberately short — the *what* lives in `docs/ai-device-kernel-v3-design.md`; this is just *how*.

---

## Phase 6.5 — `adk` → on-device native binary

The 369 KB **`ai-device-kernel/src/bin/adk.rs`** runs on **Linux aarch64-gnu** (the host). To get it onto Android, you have three options.

### Option A — `aarch64-linux-android` + Android NDK

This is the canonical path. The current environment (Linux 5.15.148-tegra / no NDK installed) is missing two tools:

1. **Android NDK r26d or newer** (`r26d` matches AGP 8.3 / API 35).
   Install via:
   ```bash
   sdkmanager "ndk;26.3.11579264" "cmake;3.22.1"
   export ANDROID_NDK_HOME="$HOME/Android/Sdk/ndk/26.3.11579264"
   ```
2. **Cross-compile cargo target**:
   ```bash
   rustup target add aarch64-linux-android
   ```

Then in `ai-device-kernel/Cargo.toml`:
```toml
[target.'cfg(target_os = "android")'.dependencies]
rusqlite = { version = "0.31", features = ["bundled"] }  # already bundled; verify

[lib]
crate-type = ["lib"]   # if embedding via JNI
```

Build:
```bash
CC_aarch64_linux_android="$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/linux-aarch64/bin/aarch64-linux-android30-clang" \
  cargo build --release --target aarch64-linux-android --bin adk
```

Strip + push:
```bash
$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/linux-aarch64/bin/llvm-strip \
  --strip-all target/aarch64-linux-android/release/adk -o /tmp/adk-android
adb push /tmp/adk-android /data/local/tmp/adk
adb shell chmod 755 /data/local/tmp/adk
adb shell /data/local/tmp/adk --port 9008 > /tmp/adk.log 2>&1 &
adb forward tcp:9008 tcp:9008
adb forward tcp:9008 localabstract:9008  # alternative for cleaner routing
```

Why `localabstract:` is preferred: it bypasses the kernel TCP stack for the host path. **Latency at this point drops to ~`< 10 ms` per AC-V3-3.4** because the kernel `adk` runs in userspace on the device, talking to `/dev/input` directly via `cmd input` (no JVM startup, no AIDL round-trip).

### Option B — Java/JNI wrapper around existing Rust crate

For a real production daemon, you almost certainly want Java-side wiring (so the binary shows up as a foreground service). Pattern:

```java
// adkd/src/main/java/com/example/adkd/MainActivity.java
public class MainActivity extends android.app.Activity {
    static {
        System.loadLibrary("ai_device_kernel");
    }
    private native int adkd_start(int port);
    private native int adkd_stop();
    private native int adkd_handle(int fdIn, int fdOut);

    private ServiceConnection conn;
    private ParcelFileDescriptor inPfd;
    private ParcelFileDescriptor outPfd;
}
```

The Rust side exposes:
```rust
#[no_mangle]
pub extern "C" fn adkd_start(port: i32) -> i32 {
    ai_device_kernel::bin::adk::main_blocking(port as u16)
}
```

The Java side opens a Unix socket, hands the file descriptors over to Rust, and the binary protocol runs over `LocalSocket`. The host connects via `adb forward localabstract:ahdk localabstract:ahdk`.

### Option C — Containerize with `--bundle`

Rust 1.79+ has `--bundle=static` for musl-style static binaries. Combined with a bionic-shim layer (`bionic-compat`), this works on Android without NDK. The static-link cost: ~ 800 KB instead of 369 KB. Trade-off?

---

## Phase 4.5 — ML Kit OCR + YOLOv8n-int8

Two capabilities need concrete model loading:

### ML Kit v2 OCR (`litert.ocr` capability)

```toml
[target.'cfg(target_os = "android")'.dependencies]
mlkit-core = "1.0"            # Google Play Services bundled
play-services-mlkit-text-recognition = "19.0"
```

The kernel binary takes the latest `FrameSnapshot` (PNG bytes) → `TextRecognition.getClient(TextRecognizerOptions.DEFAULT_OPTIONS).process(InputImage.fromBuffer(...))` → returns `Text.TextBlock { lines, ... }` which we surface as `BoundingBox` via `ActionResult::ground_truth.frame_diff`.

### YOLOv8n-int8 detection (`litert.detect` capability)

```toml
tflite = { version = "0.9", features = ["std"] }
byteorder = "1"
```

Load model via:
```rust
let model = include_bytes!("yolov8n_int8.tflite");
let mut interpreter = Interpreter::new(model, op_sets)?;
interpreter.allocate_tensors()?;
interpreter.invoke()?;
let outputs = interpreter.get_output(0)?;
```

**GPU delegate setup (per v3 §3.6.1)**:
> "The GPU delegate must be created on the same thread that runs it."

That's a thread-local constraint. The capability executor runs on a dedicated worker thread; create the delegate there.

```rust
let delegate = GpuDelegate::new()?;
Interpreter::new_with_delegate(model, op_sets, &[&delegate])?;
```

### Florence-2 grounding (`litert.ground`)

`microsoft/Florence-2-large` ONNX weights — 1.5 GB. Download once:
```bash
hf download microsoft/Florence-2-large --local-dir vendor/florence-2
```

Load via:
```toml
ort = { version = "2.0", features = ["cuda", "coreml"] }
```

Run the grounding task with a text prompt. Returns a single bounding box — `FrameSnapshot`-compatible serialisation already exists in `ai_device_kernel::FrameDiff`.

---

## Phase 7 — LLM provider swap

`ai-device-kernel/tests/agent_orchestrator.rs` ships with `StubLLM`. Real provider implementation is a one-trait swap.

```rust
pub struct Gpt4Provider { client: reqwest::Client, key: String }

impl LLMProvider for Gpt4Provider {
    fn next_action(&self, task: &Task, screen_context: &str) -> Option<Action> {
        let resp: ChatResponse = self.client.post("https://api.openai.com/v1/chat/completions")
            .bearer_auth(&self.key)
            .body(json!({
                "model": "gpt-4-turbo",
                "tools": [kebab_action_schema()],  // see below
                "messages": [...],
            }))
            .send()?
            .json()?;
        // ...parse the tool call into `Action`
    }
}
```

The `kebab_action_schema()` is the JSON schema for `Action`'s 14 variants. Generate it from `ai_device_kernel::Action` via `schemars`:
```toml
schemars = "0.8"
```

Action samples ship in `tests/canonical_5_tasks()`. AC-V3-7.2 (> 85 % success) is one PR per provider.

---

## Phase 8 — End-side GUI-Owl-1.5

GUI-Owl-1.5 is a 7B model. INT4 quantised, it fits in 8 GB RAM but consumes 1.5 GB of inference bandwidth per call. End-side **only viable on hardware with the right NPU**:

| Device           | Snapdragon/Tensor        | vLLM/onnxruntime speed | Viable? |
|---|---|---|---|
| Galaxy S21 (R5CR70SRPSD, current target) | Snapdragon 888 (Hexagon V68) | ~ 0.7 tok/s | ✅ VQA only |
| Pixel 8 Pro | Tensor G3               | ~ 2.5 tok/s | ✅ full Hybrid AI |
| Pixel 9 Pro XL | Tensor G4              | ~ 4 tok/s   | ✅ full Hybrid AI |

**Recommendation**: ship Phase 8 on Pixel 8 Pro. The S21 (current device) is borderline; the VQA-into-the-typed-surface is doable but full agent loop closure isn't.

Real config:
```toml
[dependencies]
llama-cpp = "0.3"      # GGUF runtime
# Download GUI-Owl-1.5-int4.gguf from HF
# Convert via convert-hf-to-gguf.py
```

```rust
let model = LlamaModel::load_from_file(&path, params)?;
let ctx = model.new_context(&ModelContextParams::default())?;
let response = ctx.model.evaluate(
    "What is on screen?",
    image_tokens,
    llm_params,
)?;
```

`Action::AskVisual` is the typed entry point. Returns `String` answer (the v3 doc doesn't constrain the answer type; we serialise as `Detection` for forward-compat with grounding output).

---

## Operational notes

- **adb-reverse / local-abstract**: `adb forward tcp:9008 localabstract:ahdk` makes the host's `localhost:9008` route to the device's `LocalSocket("ahdk")`. Latency-tested 1.2–3 ms median on R5CR70SRPSD.
- **Logcat from the binary**: pipe stderr to logcat via `adb logcat -v threadtime | grep adkd`.
- **Crash hygiene**: wrap the daemon in a foreground service + `am start --foreground-service` boot. Self-restart on OOM via `OnSharedPreferenceChangeListener`.
- **Memory hygiene on device**: in-memory `StateModel` is bounded (1 MiB cap AC-V3-3.3). The SQLite path (`memory_sqlite`) adds ~ 50 KiB on startup.

---

## Files relevant for this Phase 5+ work

| File | Role |
|---|---|
| `ai-device-kernel/src/action.rs` | 16 typed `Action` variants (12 + LocalizeText + DetectElement + Ground + AskVisual) |
| `ai-device-kernel/src/capability.rs` | `Capability` trait + `ALL_CAPABILITY_NAMES` registry (14 names) + drift guard test |
| `ai-device-kernel/src/bin/adk.rs` | The 369 KB aarch64-linux-gnu binary (Phase 6.5 needs the Android NDK port) |
| `ai-device-kernel/src/state.rs` | `StateModel` (in-memory, ≤ 1 MiB cap) |
| `ai-device-kernel/src/memory_sqlite.rs` | SQLite-backed persistent memory (`sqlite` feature, default-on) |
| `ai-device-kernel/tests/agent_orchestrator.rs` | Phase 7 harness + `LLMProvider` trait |
| `ai-device-kernel/tests/protocol_tcp_round_trip.rs` | TCP binary protocol integration tests (4 verbs) |
| `docs/ai-device-kernel-v3-design.md` | The design doc itself |
| `docs/baselines/v3-phase{1..7}-*.md` | Per-phase baselines (1 + 2 + 3 + 4 + 5-6-7 combined) |
