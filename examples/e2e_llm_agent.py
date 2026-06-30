#!/usr/bin/env python3
"""LLM-driven e2e agent for android-hid-connect.

Loop:
  1. Reset the device to a known state (press HOME, screencap).
  2. Build a function catalog describing every android-hid-connect
     intent (with JSON schema), the current screenshot (base64 PNG),
     and a task description. Ask the Claude API for the next action.
  3. Validate the JSON response, dispatch the action by invoking
     `cargo run --example e2e_full_intent -- dispatch <fn> <args>`.
  4. After each action, take a screenshot, feed it back into the
     loop until Claude emits `{"function": "done", ...}` or 20
     steps have been taken.
  5. Write a step-by-step report to /tmp/llm_agent_report.md.

Uses only stdlib (urllib, base64, json, subprocess, os, sys).

Usage:
  python3 examples/e2e_llm_agent.py
  python3 examples/e2e_llm_agent.py --max-steps 5 --task "press home"
"""

import argparse
import base64
import json
import os
import subprocess
import sys
import time
import urllib.request
import urllib.error
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
EXAMPLES = REPO_ROOT / "examples"
DISPATCH = [
    "cargo", "run", "--quiet", "--example", "e2e_full_intent",
    "--", "dispatch",
]
SCREENSHOT_DIR = Path("/tmp/llm_shots")
REPORT_PATH = Path("/tmp/llm_agent_report.md")
LOG_PATH = Path("/tmp/llm_agent.log")

# ---------- function catalog (JSON schema, name + params) ----------
FUNCTION_CATALOG = [
    {"name": "tap", "params": {"x": "int (0..1080)", "y": "int (0..2400)"},
     "description": "Single tap at absolute screen coordinate."},
    {"name": "double_tap", "params": {"x": "int", "y": "int"},
     "description": "Two quick taps at the same coordinate."},
    {"name": "long_press", "params": {"x": "int", "y": "int", "dur_ms": "int (default 150)"},
     "description": "Press and hold for dur_ms milliseconds."},
    {"name": "swipe", "params": {"x0": "int", "y0": "int", "x1": "int", "y1": "int",
                                  "dur_ms": "int (default 250)", "steps": "int (default 6)"},
     "description": "Linear swipe from (x0,y0) to (x1,y1)."},
    {"name": "type_text", "params": {"text": "string"},
     "description": "Inject text into the focused input via INJECT_TEXT."},
    {"name": "press_home", "params": {}, "description": "Press the Home key."},
    {"name": "press_back", "params": {}, "description": "Press the Back key."},
    {"name": "open_recents", "params": {}, "description": "Open app-switcher."},
    {"name": "volume_up", "params": {}, "description": "Volume up."},
    {"name": "volume_down", "params": {}, "description": "Volume down."},
    {"name": "volume_mute", "params": {}, "description": "Volume mute."},
    {"name": "launch_app", "params": {"package": "string (default com.android.settings)"},
     "description": "Launch an installed app by package name."},
    {"name": "tap_android_key", "params": {"key": "string (HOME|BACK|DPAD_UP|DPAD_DOWN|DPAD_LEFT|DPAD_RIGHT|ENTER)"},
     "description": "Press and release a typed Android key."},
    {"name": "show_notifications", "params": {},
     "description": "Expand the notification shade."},
    {"name": "show_quick_settings", "params": {},
     "description": "Expand quick-settings panel."},
    {"name": "collapse_panels", "params": {},
     "description": "Collapse notification + quick-settings panels."},
    {"name": "set_screen_power", "params": {"on": "bool (default true)"},
     "description": "Turn the screen on or off."},
    {"name": "set_clipboard", "params": {"text": "string", "paste": "bool (default false)"},
     "description": "Write to the device clipboard."},
    {"name": "scroll", "params": {"x": "int", "y": "int", "hscroll": "float", "vscroll": "float"},
     "description": "INJECT_SCROLL_EVENT. NOTE: not all lists respond (e.g. Settings list). Prefer `swipe` for list scrolling."},
    {"name": "open_hard_keyboard_settings", "params": {},
     "description": "Open the physical keyboard settings activity."},
    {"name": "rotate_device", "params": {},
     "description": "Rotate the display."},
    {"name": "set_torch", "params": {"on": "bool"},
     "description": "Toggle camera torch (skipped on devices without flash)."},
    {"name": "done", "params": {"summary": "string"},
     "description": "Mark the task as complete with a brief summary."},
]

CATALOG_TEXT = "\n".join(
    f"- {f['name']}({', '.join(f['params'].keys())}): {f['description']}"
    for f in FUNCTION_CATALOG
)


def log(*a):
    line = " ".join(str(x) for x in a)
    print(line)
    with LOG_PATH.open("a", encoding="utf-8") as fh:
        fh.write(line + "\n")


def screencap(path: Path) -> bool:
    """Take a screenshot, write to path on the host. Returns True if file size > 0."""
    try:
        result = subprocess.run(
            ["adb", "exec-out", "screencap", "-p"],
            capture_output=True, timeout=10,
        )
        if result.returncode != 0:
            log(f"  screencap failed (rc={result.returncode})")
            return False
        if not result.stdout:
            log("  screencap empty stdout")
            return False
        path.write_bytes(result.stdout)
        return path.stat().st_size > 0
    except Exception as e:
        log(f"  screencap exception: {e}")
        return False


def device_focus() -> str:
    """Return the device's currently-focused window."""
    try:
        out = subprocess.check_output(
            ["adb", "shell", "dumpsys window | grep mCurrentFocus"],
            timeout=5, stderr=subprocess.STDOUT,
        )
        return out.decode("utf-8", "replace").strip()
    except Exception:
        return "<unknown>"


def ensure_scrcpy_running() -> bool:
    """scrcpy-server dies after each client disconnects (tunnel_forward=true).
    Restart it before each dispatch call so the next connect succeeds."""
    try:
        out = subprocess.check_output(
            ["adb", "shell", "ps -ef | grep com.genymobile.scrcpy.Server | grep -v grep || true"],
            timeout=5,
        )
        if b"com.genymobile.scrcpy.Server" in out:
            return True
    except Exception:
        pass
    log("  scrcpy-server not running; restarting")
    # Re-push the jar (it sometimes gets cleaned from /data/local/tmp).
    subprocess.run(["adb", "push", "/tmp/scrcpy-server",
                   "/data/local/tmp/scrcpy-server"],
                   capture_output=True, timeout=15)
    subprocess.run(
        ["adb", "shell",
         "CLASSPATH=/data/local/tmp/scrcpy-server nohup app_process / "
         "com.genymobile.scrcpy.Server 2.7 video=false audio=false control=true "
         "clipboard_autosync=false tunnel_forward=true send_dummy_byte=true "
         "send_device_meta=true > /data/local/tmp/scrcpy.log 2>&1 < /dev/null &"],
        timeout=5,
    )
    time.sleep(3.0)
    return True


def dispatch_action(name: str, args: dict) -> tuple[bool, str]:
    """Invoke `cargo run --example e2e_full_intent -- dispatch <name> <args>`."""
    ensure_scrcpy_running()
    argv = list(DISPATCH) + [name]
    if name == "tap":
        argv += [str(args.get("x", 540)), str(args.get("y", 1200))]
    elif name == "double_tap":
        argv += [str(args.get("x", 540)), str(args.get("y", 1200))]
    elif name == "long_press":
        argv += [str(args.get("x", 540)), str(args.get("y", 1200)),
                 str(args.get("dur_ms", 150))]
    elif name == "swipe":
        argv += [str(args.get("x0", 540)), str(args.get("y0", 1500)),
                 str(args.get("x1", 540)), str(args.get("y1", 800)),
                 str(args.get("dur_ms", 250)), str(args.get("steps", 6))]
    elif name == "type_text":
        argv += [str(args.get("text", "hello"))]
    elif name == "launch_app":
        argv += [str(args.get("package", "com.android.settings"))]
    elif name == "tap_android_key":
        argv += [str(args.get("key", "ENTER"))]
    elif name == "set_screen_power":
        argv += ["true" if args.get("on", True) else "false"]
    elif name == "set_clipboard":
        argv += [str(args.get("text", "e2e")),
                 "true" if args.get("paste", False) else "false"]
    elif name == "scroll":
        argv += [str(args.get("x", 540)), str(args.get("y", 1200)),
                 str(args.get("hscroll", 0)), str(args.get("vscroll", -2))]
    elif name == "set_torch":
        argv += ["true" if args.get("on", False) else "false"]
    elif name == "done":
        return True, ""

    try:
        proc = subprocess.run(argv, capture_output=True, timeout=20,
                              cwd=str(REPO_ROOT))
        ok = proc.returncode == 0
        out = proc.stdout.decode("utf-8", "replace").strip()
        err = proc.stderr.decode("utf-8", "replace").strip()
        if not ok:
            log(f"  dispatch {name} failed rc={proc.returncode}; stderr={err[:200]}")
        return ok, out or err
    except subprocess.TimeoutExpired:
        log(f"  dispatch {name} timed out after 20s")
        return False, "timeout"
    except Exception as e:
        log(f"  dispatch {name} exception: {e}")
        return False, str(e)


def call_claude(screenshot_b64: str, task: str, history: list[dict]) -> dict | None:
    """Call the Claude API and return a parsed action dict, or None on failure."""
    base = os.environ.get("ANTHROPIC_BASE_URL", "").rstrip("/")
    token = (os.environ.get("ANTHROPIC_AUTH_TOKEN")
             or os.environ.get("ANTHROPIC_API_KEY")
             or "")
    model = (os.environ.get("ANTHROPIC_MODEL")
             or os.environ.get("ANTHROPIC_DEFAULT_HAIKU_MODEL")
             or "claude-haiku-4-5")
    if not base or not token:
        log("  missing ANTHROPIC_BASE_URL or ANTHROPIC_AUTH_TOKEN")
        return None

    system = (
        "You are an agent that controls an Android device (R5CR70SRPSD, "
        "SM-G9910, 1080x2400, Android 11) using a Rust UHID control plane. "
        "You receive a screenshot of the current device state plus a task, "
        "and must respond with ONE action to take next. Output a single JSON "
        "object with this shape:\n"
        '{"function": "<name>", "args": {<params>}, "reasoning": "<short>"}\n'
        "When the task is fully complete, output:\n"
        '{"function": "done", "args": {"summary": "<one-line summary>"}}\n\n'
        "Available functions:\n" + CATALOG_TEXT + "\n\n"
        "Tips:\n"
        "- Use absolute coordinates (0..1080 x 0..2400).\n"
        "- The status bar is around y=0..120. The nav bar is around y=2280..2400.\n"
        "- Apps launch with `launch_app`. To open Settings, "
        "launch_app('com.android.settings').\n"
        "- System apps include `com.android.settings`, "
        "`com.android.camera`, `com.android.dialer`, "
        "`com.android.contacts`, `com.android.music`.\n"
        "- For scrolling within a list, PREFER `swipe(x0=540,y0=1800,x1=540,y1=800)` "
        "rather than `scroll`. `scroll` (inject_scroll_event) does not work on all lists.\n"
        "- After swiping, take another screenshot before deciding the next action.\n"
    )

    user_text = (
        f"Current task: {task}\n\n"
        f"Screenshot attached (PNG, base64-encoded, length {len(screenshot_b64)} chars).\n"
        "Decide the next single action. Output ONLY the JSON object.\n"
    )
    if history:
        user_text += "\nPrevious actions you took:\n"
        for h in history[-5:]:
            user_text += (
                f"- step {h['step']}: {h['function']}({h.get('args', {})}) "
                f"-> {'OK' if h.get('ok') else 'FAIL'}: {h.get('note', '')}\n"
            )

    body = {
        "model": model,
        "max_tokens": 400,
        "system": system,
        "messages": [
            {
                "role": "user",
                "content": [
                    {"type": "text", "text": user_text},
                    {"type": "image",
                     "source": {"type": "base64", "media_type": "image/png",
                                "data": screenshot_b64}},
                ],
            }
        ],
    }

    url = f"{base}/v1/messages"
    req = urllib.request.Request(
        url,
        data=json.dumps(body).encode("utf-8"),
        headers={
            "content-type": "application/json",
            "x-api-key": token,
            "anthropic-version": "2023-06-01",
            "authorization": f"Bearer {token}",
        },
        method="POST",
    )

    try:
        with urllib.request.urlopen(req, timeout=30) as resp:
            data = json.loads(resp.read().decode("utf-8"))
    except urllib.error.HTTPError as e:
        body = e.read().decode("utf-8", "replace")[:500]
        log(f"  claude HTTP {e.code}: {body}")
        return None
    except Exception as e:
        log(f"  claude call exception: {e}")
        return None

    # Extract text from the response
    try:
        text = ""
        for block in data.get("content", []):
            if block.get("type") == "text":
                text += block.get("text", "")
        if not text:
            log(f"  claude response empty content: {json.dumps(data)[:300]}")
            return None
        # Strip optional markdown fences
        text = text.strip()
        if text.startswith("```"):
            text = text.strip("`").strip()
            if text.startswith("json"):
                text = text[4:].strip()
            if text.endswith("```"):
                text = text[:-3].strip()
        # Find the first { ... } block
        start = text.find("{")
        end = text.rfind("}")
        if start == -1 or end == -1 or end <= start:
            log(f"  claude response missing JSON: {text[:200]}")
            return None
        action = json.loads(text[start:end + 1])
        if "function" not in action:
            log(f"  claude response missing 'function' key: {text[:200]}")
            return None
        return action
    except json.JSONDecodeError as e:
        log(f"  claude response not valid JSON: {e}; raw={text[:200]}")
        return None


def match_focus_after(focus_before: str, focus_after: str, action_name: str) -> str:
    """Crude match: did the device focus change in a way consistent with action?"""
    if focus_before == focus_after:
        # No focus change — only matches no-op actions
        if action_name in ("show_notifications", "show_quick_settings", "set_screen_power",
                           "rotate_device", "set_clipboard", "scroll", "type_text",
                           "set_torch", "tap_android_key", "volume_up", "volume_down",
                           "volume_mute"):
            return "ok_no_focus_change_expected"
        return "ok_unknown"
    return "ok_focus_changed"


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--max-steps", type=int, default=20)
    parser.add_argument("--task", type=str,
                        default="Open Settings, navigate to About Phone, return to Home")
    parser.add_argument("--start-clean", action="store_true", default=True)
    args = parser.parse_args()

    SCREENSHOT_DIR.mkdir(exist_ok=True)
    if LOG_PATH.exists():
        LOG_PATH.unlink()

    log(f"# android-hid-connect LLM agent")
    log(f"# task: {args.task}")
    log(f"# max-steps: {args.max_steps}")

    # 1. Reset device.
    if args.start_clean:
        log("# step 0: reset to home")
        dispatch_action("press_home", {})
        time.sleep(0.5)

    history = []
    focus_history = []
    last_action = None

    for step in range(1, args.max_steps + 1):
        shot = SCREENSHOT_DIR / f"llm_step{step}.png"
        if not screencap(shot):
            log(f"# step {step}: screencap failed; aborting")
            break
        b64 = base64.b64encode(shot.read_bytes()).decode("ascii")
        focus_before = device_focus()
        log(f"# step {step}: focus-before={focus_before}")
        log(f"# step {step}: screenshot={shot} ({len(b64)} b64 chars)")

        action = call_claude(b64, args.task, history)
        if action is None:
            log(f"# step {step}: claude call failed; retry next step")
            time.sleep(1.0)
            continue

        name = action.get("function", "")
        a_args = action.get("args", {})
        reasoning = action.get("reasoning", "")
        log(f"# step {step}: action={name}({a_args}) reasoning={reasoning!r}")

        if name == "done":
            summary = a_args.get("summary", "")
            log(f"# step {step}: done ({summary})")
            focus_history.append((step, focus_before, device_focus(), name, a_args, True, summary, str(shot)))
            last_action = ("done", summary)
            break

        ok, note = dispatch_action(name, a_args)
        time.sleep(0.6)
        focus_after = device_focus()
        match = match_focus_after(focus_before, focus_after, name)
        log(f"# step {step}: dispatch ok={ok} focus-after={focus_after} match={match}")

        history.append({
            "step": step, "function": name, "args": a_args,
            "ok": ok, "note": note, "shot": str(shot),
            "focus_before": focus_before, "focus_after": focus_after,
        })
        focus_history.append((step, focus_before, focus_after, name, a_args, ok, note, str(shot)))
        last_action = (name, note)

        # Detect a stuck loop: same action repeated 5x in a row
        if len(history) >= 5:
            recent = [h["function"] for h in history[-5:]]
            if len(set(recent)) == 1:
                log(f"# step {step}: stuck loop on {recent[0]}; aborting")
                break

    # 4. End state.
    final_focus = device_focus()
    end_shot = SCREENSHOT_DIR / "llm_step_final.png"
    screencap(end_shot)
    log(f"# final focus: {final_focus}")
    log(f"# final shot: {end_shot}")

    # 5. Write report.
    with REPORT_PATH.open("w", encoding="utf-8") as fh:
        fh.write(f"# LLM agent e2e report\n\n")
        fh.write(f"**Task**: {args.task}\n\n")
        fh.write(f"**Steps taken**: {len(focus_history)}\n\n")
        fh.write(f"**Final device focus**: `{final_focus}`\n\n")
        fh.write("## Step-by-step\n\n")
        fh.write("| # | action | reasoning | focus_before | focus_after | ok | screencap |\n")
        fh.write("|---|--------|-----------|--------------|-------------|----|-----------|\n")
        for row in focus_history:
            step, fb, fa, name, a_args, ok, note, shot = row
            reasoning = ""
            for h in history:
                if h.get("step") == step:
                    reasoning = h.get("note", "")
                    break
            fh.write(f"| {step} | `{name}({json.dumps(a_args, ensure_ascii=False)})` | "
                     f"{reasoning[:80]} | `{fb}` | `{fa}` | "
                     f"{'OK' if ok else 'FAIL'} | `{shot}` |\n")
        if last_action and last_action[0] == "done":
            fh.write(f"\n**Agent self-report**: done — {last_action[1]}\n")
        else:
            fh.write(f"\n**Agent self-report**: did NOT emit `done` "
                     f"(last action: {last_action})\n")

    log(f"# report written to {REPORT_PATH}")
    return 0


if __name__ == "__main__":
    sys.exit(main())