#!/usr/bin/env python3
"""Finite Claude hook entrypoint. One shared recorder, no semantic recall.

Input is trusted only as a host invocation; hook receipts do not grant memory
owner authority. Malformed/oversized input never loads config or launches work.
"""
import argparse
import json
from pathlib import Path
import sys

from rollout_primitives import decode_json, is_token

MAX_INPUT = 128 * 1024
EVENTS = {"SessionStart", "UserPromptSubmit", "Stop", "StopFailure", "SessionEnd", "PreCompact"}


def read_event(stream):
    raw = stream.read(MAX_INPUT + 1)
    if not raw or len(raw) > MAX_INPUT:
        raise ValueError("hook_input_limit")
    from hooks import _check_wire_depth, HookInputError
    try:
        _check_wire_depth(raw)
    except HookInputError:
        raise ValueError("hook_input_depth") from None
    value = decode_json(raw)
    if (not isinstance(value, dict) or value.get("hook_event_name") not in EVENTS
            or not is_token(value.get("session_id"))):
        raise ValueError("invalid_claude_hook_event")
    for key in ("cwd", "transcript_path"):
        path = value.get(key)
        if (not isinstance(path, str) or not Path(path).is_absolute() or ".." in Path(path).parts
                or len(path.encode()) > 4096 or "\0" in path):
            raise ValueError("invalid_claude_hook_locator")
    event = value["hook_event_name"]
    if event in ("UserPromptSubmit", "Stop", "StopFailure") and not is_token(value.get("prompt_id")):
        raise ValueError("Claude hook lacks prompt_id; supported host receipt required")
    if event == "UserPromptSubmit" and (not isinstance(value.get("prompt"), str) or len(value["prompt"].encode()) > 8192):
        raise ValueError("invalid_claude_prompt")
    if event == "SessionStart" and value.get("source") not in ("startup", "resume", "clear", "compact", "fork"):
        raise ValueError("invalid_claude_session_start")
    if event == "Stop":
        from claude_observer import validate_stop_receipt
        validate_stop_receipt(value, session_id=value["session_id"], turn_id=value["prompt_id"])
    # agent_type alone is a legitimate root --agent session.
    return {**value, "source_host": "claude_code", "turn_id": value.get("prompt_id")}


def handle_event(event, config):
    if config.get("schema") != "mneme.claude-hooks.config.v1" or config.get("source_host") != "claude_code":
        raise ValueError("unsupported_claude_capture_config")
    if event.get("agent_id"):
        return {}
    from registry_runtime import selection
    selected = selection(config)
    if Path(event["cwd"]).resolve() != Path(selected.cwd).resolve():
        raise ValueError("Claude capture workspace changed; reprepare session")
    import recording_jobs
    import reader_worker
    session, turn, name = event["session_id"], event.get("turn_id"), event["hook_event_name"]
    if name == "SessionStart":
        if event["source"] == "resume":
            # Recover only unpaid stopped candidates. A failed boundary lock
            # must not be followed by cancellation of the candidate it guarded.
            if not recording_jobs.recover_stopped_turns(config, event):
                return {}
        elif event["source"] != "startup":
            recording_jobs.interrupt_open_turns(config, session)
        recording_jobs.session_start(config, {**event, "source": "resume" if event["source"] == "fork" else event["source"]})
        if event["source"] == "resume":
            reader_worker.background(config, event)
    elif name == "UserPromptSubmit":
        confirmed = recording_jobs.confirm_prompt_boundary(config, event)
        if not confirmed:
            # Do not retire a valid Stop candidate after a failed boundary lock.
            return {}
        capture_notice = recording_jobs.notice(config, event)
        # Rejected/replayed hook deliveries cannot rewind the shared foreground
        # lifecycle or cancel a newer turn's accounting fence.
        if capture_notice.get("outcome") != "admitted":
            if confirmed:
                reader_worker.background(config, event)
            return {}
        reader_notice = reader_worker.notice(config, event, substantive=False)
        if confirmed:
            reader_worker.background(config, event)
        if reader_notice.get("outcome") in ("skipped", "replay"):
            from touchstone_handoff import consume
            nomination = consume(config, event, 4096)
            if nomination.get("outcome") == "emitted" and isinstance(nomination.get("context"), str):
                return {"hookSpecificOutput": {"hookEventName": "UserPromptSubmit",
                                               "additionalContext": nomination["context"]}}
    elif name == "Stop":
        from claude_observer import validate_stop_receipt
        receipt = validate_stop_receipt(event, session_id=session, turn_id=turn)
        recording_jobs.record_stop(config, session, turn, receipt)
        # Peer Stop hooks may continue this prompt. No worker or provider yet.
    elif name == "StopFailure":
        recording_jobs.close_turn(config, session, turn, cancel=True)
        reader_worker.close_turn(config, session, turn)
    elif name == "SessionEnd":
        confirmed = recording_jobs.confirm_session_end(config, event)
        if not confirmed:
            return {}
        reader_worker.end_session(config, session)
        if confirmed:
            reader_worker.background(config, event)
    elif name == "PreCompact":
        recording_jobs.interrupt_open_turns(config, session)
    return {}


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", required=True, type=Path)
    args = parser.parse_args(argv)
    try:
        event = read_event(sys.stdin.buffer)
        if event.get("agent_id"):
            result = {}
        else:
            from claude_config import load_config
            from store_registry import native_selection_budget
            with native_selection_budget(3):
                config = load_config(args.config, cwd=event["cwd"])
                result = handle_event(event, config)
        print(json.dumps(result, ensure_ascii=False, separators=(",", ":")))
        return 0
    except (OSError, ValueError, TypeError, KeyError, RecursionError, UnicodeError):
        print('{"systemMessage":"Mneme capture deferred: invalid input or unavailable explicit adapter; no recall was scheduled."}')
        return 0


if __name__ == "__main__":
    raise SystemExit(main())
