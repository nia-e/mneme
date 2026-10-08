#!/usr/bin/env python3
"""Finite, stdlib-only TUI input smoke for Linux/macOS; always synthetic --demo.

Checks responsiveness under mouse reports, effective xterm tracking mode, and
terminal restoration. Fixed demo click coordinates deliberately do not prove hit
selection; rendered-buffer tests and separate visual QA cover that contract.
Never opens stores, changes configuration, builds, installs, or starts services.
"""
import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import pty
import re
import select
import signal
import struct
import subprocess
import termios
import time

CSI = re.compile(rb"\x1b\[([0-?]*)([ -/]*)([@-~])")
MAX_OUTPUT = 4 * 1024 * 1024
TRACKING_MODES = {9, 1000, 1001, 1002, 1003}
MOUSE_MODES = TRACKING_MODES | {1006, 1015}


def mouse_state(output):
    """Tracking modes are mutually exclusive; independent booleans are wrong."""
    active = None
    enabled = set()
    for parameters, _, command in CSI.findall(output):
        if not parameters.startswith(b"?") or command not in (b"h", b"l"):
            continue
        for parameter in parameters[1:].split(b";"):
            mode = int(parameter)
            if mode not in MOUSE_MODES:
                continue
            if command == b"h":
                if mode in TRACKING_MODES:
                    enabled.difference_update(TRACKING_MODES)
                    active = mode
                enabled.add(mode)
            else:
                enabled.discard(mode)
                if active == mode:
                    active = None
    return {"tracking": active, "enabled_modes": sorted(enabled)}


def private_mode(output, wanted):
    """Last recognized transition, not mere historical presence."""
    state = None
    for parameters, _, command in CSI.findall(output):
        if parameters.startswith(b"?") and command in (b"h", b"l"):
            if str(wanted).encode() in parameters[1:].split(b";"):
                state = command == b"h"
    return state


def reports(count, wheel=False):
    return b"".join(
        f"\x1b[<{64 + i % 2 if wheel else 35};{2 + i % 73};{3 + i % 25}M".encode()
        for i in range(count)
    )


# Fixed initial 120x36 demo marker: node 08. Resize tests intentionally do not
# assert that the old cell still picks the same memory after geometry changes.
CLICK = b"\x1b[<0;15;13M\x1b[<0;15;13m"


def run(binary, env, name, payload, exit_key, resize=False, expected_tracking=1002):
    master, slave = pty.openpty()
    child = None
    original = termios.tcgetattr(slave)
    output = bytearray()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 36, 120, 0, 0))
    os.set_blocking(master, False)

    def drain(seconds):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            if select.select([master], [], [], .01)[0]:
                try:
                    block = os.read(master, 65536)
                except OSError:
                    break
                output.extend(block)
                if len(output) > MAX_OUTPUT:
                    raise RuntimeError("TUI output exceeded 4 MiB")
            if child.poll() is not None:
                break

    try:
        child = subprocess.Popen([str(binary), "tui", "--demo"], stdin=slave,
                                 stdout=slave, stderr=slave, env=env)
        ready_deadline = time.monotonic() + 8
        while time.monotonic() < ready_deadline:
            drain(.04)
            plain = CSI.sub(b"", bytes(output)).decode("utf-8", errors="replace")
            if b"\x1b[?1006h" in output and "s scenes" in plain and "?  q" in plain:
                break
            if child.poll() is not None:
                raise RuntimeError("demo exited before its first ready frame")
        else:
            raise RuntimeError("demo did not become ready within 8 seconds")
        ready_state = mouse_state(bytes(output))
        if resize:
            fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 48, 180, 0, 0))
            child.send_signal(signal.SIGWINCH)
        incoming = payload + exit_key
        started, sent = time.monotonic(), 0
        while child.poll() is None and time.monotonic() - started < 2:
            if sent < len(incoming):
                try:
                    sent += os.write(master, incoming[sent:])
                except BlockingIOError:
                    pass
            drain(.015)
        latency = time.monotonic() - started
        met_deadline = child.poll() is not None
        fallback = None
        if not met_deadline:
            fallback = "SIGTERM"
            child.terminate()
            cleanup_deadline = time.monotonic() + 4
            while child.poll() is None and time.monotonic() < cleanup_deadline:
                drain(.04)
            if child.poll() is None:
                fallback = "SIGKILL"
                child.kill()
        child.wait(timeout=2)
        drain(.04)
        final = bytes(output)
        cleanup = {
            "termios": termios.tcgetattr(slave) == original,
            "alternate_screen": b"\x1b[?1049h" in final and private_mode(final, 1049) is False,
            "cursor": private_mode(final, 25) is True,
            "mouse": mouse_state(final) == {"tracking": None, "enabled_modes": []},
        }
        result = {
            "name": name, "exit": "Ctrl-C" if exit_key == b"\x03" else "q",
            "exit_within_2_seconds": met_deadline,
            "latency_seconds": round(latency, 4), "returncode": child.returncode,
            "input_bytes_sent": sent, "input_bytes_total": len(incoming),
            "ready_mouse_state": ready_state, "final_mouse_state": mouse_state(final),
            "cleanup": cleanup, "cleanup_fallback": fallback,
        }
        result["passed"] = (met_deadline and child.returncode == 0
                            and sent == len(incoming) and all(cleanup.values())
                            and ready_state["tracking"] == expected_tracking
                            and 1006 in ready_state["enabled_modes"])
        return result
    finally:
        if child is not None and child.poll() is None:
            child.kill()
            child.wait(timeout=2)
        os.close(master)
        os.close(slave)



DRAG = b"\x1b[<0;40;15M" + b"".join(
    f"\x1b[<32;{40 + i % 15};{15 + i % 5}M".encode() for i in range(500)
) + b"\x1b[<0;54;19m"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--receipt", type=Path, required=True)
    parser.add_argument("--expect-tracking", type=int, choices=(1000, 1002), default=1002,
                        help="1002 for drag-capable builds; 1000 only for the earlier click-only baseline")
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    env = os.environ.copy()
    env.pop("MNEME_DB", None)
    env.pop("NO_COLOR", None)
    env["TERM"] = "xterm-256color"
    cases = [("click-q", CLICK, b"q", False),
             ("motion50-click-q", reports(50) + CLICK, b"q", False),
             ("motion500-click-q", reports(500) + CLICK, b"q", False),
             ("motion500-click-ctrlc", reports(500) + CLICK, b"\x03", False),
             ("left-drag500-release-q", DRAG, b"q", False),
             ("wheel300-click-q", reports(300, wheel=True) + CLICK, b"q", False),
             ("repeat200-click-q", CLICK * 200, b"q", False),
             ("motion500-resize-q", reports(500) + CLICK, b"q", True)]
    receipt = {"binary": str(binary), "sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
               "synthetic_only": True, "size": [120, 36],
               "expected_tracking": args.expect_tracking, "cases": []}
    for name, payload, key, resize in cases:
        receipt["cases"].append(run(binary, env, name, payload, key, resize, args.expect_tracking))
    receipt["passed"] = all(case["passed"] for case in receipt["cases"])
    args.receipt.parent.mkdir(parents=True, exist_ok=True)
    args.receipt.write_text(json.dumps(receipt, indent=2) + "\n")
    print(json.dumps({"passed": receipt["passed"], "receipt": str(args.receipt),
                      "sha256": receipt["sha256"]}))
    return 0 if receipt["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
