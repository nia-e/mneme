#!/usr/bin/env python3
"""Passive Pi inbox metadata for primary Codex SessionStart hooks only.

The explicit workshop root supplies the existing validated event queue. This
reader never claims events, tracks reads, starts a service, or writes state.
"""

import argparse
import json
from pathlib import Path
import sys

# Even importing the queue reader must not create sibling bytecode caches.
sys.dont_write_bytecode = True

import wake


MAX_STDIN = 64 * 1024
MAX_CONTEXT_BYTES = 600
SOURCES = {"startup", "resume", "clear", "compact"}
UNAVAILABLE = (
    "Pi inbox summary unavailable; no queue state was changed. "
    "Use the configured Signal read_history tool to inspect recent conversation "
    "before replying; do not assume the inbox is empty."
)


def _eligible(event):
    return (isinstance(event, dict) and event.get("hook_event_name") == "SessionStart"
            and isinstance(event.get("source"), str) and event["source"] in SOURCES
            and event.get("agent_id") in (None, "")
            and event.get("agent_type") in (None, ""))


def _context(text):
    if len(text.encode("utf-8")) > MAX_CONTEXT_BYTES:
        raise ValueError("inbox context exceeds size limit")
    return {"hookSpecificOutput": {"hookEventName": "SessionStart", "additionalContext": text}}


def handle_event(event, root):
    if not _eligible(event):
        return {}
    try:
        counts = wake.inbox_summary(root)
        if not any(counts.values()):
            return {}
        return _context(
            f"Pi inbox: pending Signal bursts: {counts['pending_signal_bursts']}; "
            f"pending private-mailbox messages: {counts['pending_mailbox_messages']}. "
            f"Unfinished Signal bursts: {counts['unfinished_signal_bursts']}; "
            f"unfinished private-mailbox messages: {counts['unfinished_mailbox_messages']}. "
            "Signal bursts are not individual text-message counts. "
            "Use the configured Signal read_history tool for recent conversation; "
            "inspect history before replying to avoid duplicates. "
            "This cue does not deliver, claim, or acknowledge anything.")
    except (OSError, ValueError, KeyError, TypeError, RuntimeError):
        # No durable error marker, raw event payload, path, or exception leaks.
        return _context(UNAVAILABLE)


def _reject_constant(_token):
    raise ValueError("non-finite JSON number")


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", required=True, type=Path,
                        help="existing absolute canonical Pi workshop directory")
    args = parser.parse_args(argv)
    try:
        raw = sys.stdin.buffer.read(MAX_STDIN + 1)
        if len(raw) > MAX_STDIN:
            raise ValueError("event input exceeds size limit")
        event = json.loads(raw, object_pairs_hook=wake._pairs, parse_constant=_reject_constant)
        if not isinstance(event, dict):
            raise ValueError("event input must be an object")
        result = handle_event(event, args.root)
    except (OSError, ValueError, KeyError, TypeError, RuntimeError):
        result = _context(UNAVAILABLE)
    print(json.dumps(result, ensure_ascii=False, separators=(",", ":"), allow_nan=False))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
