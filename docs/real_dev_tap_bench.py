#!/usr/bin/env python3
"""Real-device adk tap latency benchmark — Phase 6.5 native path's
*real* predecessor.

Measures the end-to-end latency of `Action::Tap` round-trips
through the v3 binary protocol on R5CR70SRPSD, where the `adk`
binary runs on the **host** and dispatches via `adb shell input tap`.
This is the *current* production path on this build host until
Phase 6.5 ships the on-device binary.

Reports p50/p95/p99/p99.9 statistics over N samples to
characterise the full request → adb → input service → reply
chain. The Phase 6.5 native binary targets **< 10 ms p50**; this
benchmark numbers the savings.

Usage:
    /mnt/.../target/release/adk --device <serial> --port 19008 &
    python3 real_dev_tap_bench.py --host 127.0.0.1 --port 19008 \
        --n 50 --x 540 --y 1200
"""

import argparse
import socket
import statistics
import struct
import sys
import time


def varint(n):
    out = bytearray()
    while True:
        b = n & 0x7F
        n >>= 7
        if n == 0:
            return bytes(out + bytes([b]))
        out.append(b | 0x80)


def zigzag_varint_i32(n):
    if n >= 0:
        return varint(n << 1)
    return varint(((-n) << 1) - 1)


def varint_u32(n):
    return varint(n)


def read_frame(sock):
    hdr = b""
    while len(hdr) < 2:
        chunk = sock.recv(2 - len(hdr))
        if not chunk:
            raise EOFError("closed mid-header")
        hdr += chunk
    verb, flags = hdr[0], hdr[1]
    length = 0
    shift = 0
    while True:
        b = sock.recv(1)
        v = b[0]
        length |= (v & 0x7F) << shift
        if v & 0x80 == 0:
            break
        shift += 7
    body = b""
    while len(body) < length:
        chunk = sock.recv(length - len(body))
        if not chunk:
            break
        body += chunk
    return verb, flags, body


def write_frame(sock, verb, flags, payload):
    sock.sendall(bytes([verb, flags]) + varint(len(payload)) + payload)


def build_tap_payload(x, y, deadline_ms=500):
    """postcard for RequestPayload::Action { id: ActionId(0), action: Tap{...} }.

    RequestPayload variant tag = 0 (Action first variant).
    The ActionTap enum uses custom Serialize which calls
    `serialize_u64` for u64 ActionId (varint-encoded by postcard
    1.1.x default), then the Action variant tag + zigzag-varint
    i32 + zigzag-varint i32 + varint u32.
    """
    out = bytearray()
    out.append(0)              # RequestPayload::Action
    out += varint(0)           # ActionId(0)
    out.append(0)              # Action::Tap variant 0
    out += zigzag_varint_i32(x)
    out += zigzag_varint_i32(y)
    out += varint_u32(deadline_ms)
    return bytes(out)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--port", type=int, default=19008)
    ap.add_argument("--n", type=int, default=50)
    ap.add_argument("--x", type=int, default=540)
    ap.add_argument("--y", type=int, default=1200)
    ap.add_argument("--deadline-ms", type=int, default=500)
    args = ap.parse_args()

    payload = build_tap_payload(args.x, args.y, args.deadline_ms)
    verb = 0x01  # Action
    flags = 0x00

    samples_ms = []
    last_status = ""
    for i in range(args.n):
        sock = socket.create_connection(
            (args.host, args.port), timeout=5.0
        )
        sock.settimeout(5.0)
        try:
            t0 = time.monotonic_ns()
            write_frame(sock, verb, flags, payload)
            rv, _rf, body = read_frame(sock)
            t1 = time.monotonic_ns()
            elapsed = (t1 - t0) / 1_000_000.0
            samples_ms.append(elapsed)
            last_status = f"verb=0x{rv:02x} body={len(body)}B"
        finally:
            sock.close()
        if (i + 1) % 10 == 0:
            print(
                f"  [{i+1:3d}/{args.n}] last={elapsed:.1f} ms ({last_status})",
                flush=True,
            )

    s = sorted(samples_ms)
    p50 = s[len(s) // 2]
    p95 = s[int(len(s) * 0.95)]
    p99 = s[int(len(s) * 0.99)] if len(s) >= 100 else float("nan")
    p999 = (
        s[int(len(s) * 0.999)] if len(s) >= 1000 else float("nan")
    )
    print()
    print(f"Real-device tap round-trip on {args.host}:{args.port}:")
    print(f"  runs      = {len(s)}")
    print(f"  min       = {min(s):.2f} ms")
    print(f"  mean      = {statistics.mean(s):.2f} ms")
    print(f"  median    = {statistics.median(s):.2f} ms")
    print(f"  p50       = {p50:.2f} ms")
    print(f"  p95       = {p95:.2f} ms")
    if not (p99 != p99):  # not NaN
        print(f"  p99       = {p99:.2f} ms")
    if not (p999 != p999):
        print(f"  p99.9     = {p999:.2f} ms")
    print(f"  max       = {max(s):.2f} ms")
    print(f"  stdev     = {statistics.stdev(s):.2f} ms")
    print()
    print(f"  Phase 6.5 native binary target p50 < 10 ms (kernel-internal: 0.78 ms).")
    print(f"  Current host binary path adds ~adb shell + input service overhead.")
    print(f"  Expected savings on device-side: ~ {(p50 - 0.78):.0f} ms p50.")


if __name__ == "__main__":
    main()
