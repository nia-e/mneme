"""Synthetic Codex transcript builders and an owned temporary source-turn fixture.

No TestCase lives here: callers register cleanup and assertions with their own test.
"""
import hashlib
import json
from pathlib import Path
import tempfile

import turn_observer as observer


SESSION = "synthetic-session"
TURN = "synthetic-turn"
PROMPT = "Inspect the synthetic config, then explain the result."
META = "internal_chat_message_metadata_passthrough"


def sha(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def row(kind, payload):
    return {"type": kind, "payload": payload}


def header(session=SESSION):
    return row("session_meta", {"id": session, "session_id": session})


def event(kind, turn=TURN):
    return row("event_msg", {"type": kind, "turn_id": turn})


def start(turn=TURN):
    return event("task_started", turn)


def context(turn=TURN):
    return row("turn_context", {"turn_id": turn})


def user(text=PROMPT, *, identifier="prompt", turn=TURN, kind="user.text"):
    return row("response_item", {
        "type": "message", "id": identifier, "role": "user",
        "content": [{"type": "input_text", "text": text}],
        META: {"turn_id": turn, "content_item_kinds": [kind]},
    })


def assistant(text="The synthetic config was inspected.", *, identifier="answer",
              turn=TURN, phase="final_answer"):
    return row("response_item", {
        "type": "message", "id": identifier, "role": "assistant", "phase": phase,
        "content": [{"type": "output_text", "text": text}],
        META: {"turn_id": turn, "content_item_kinds": ["unknown"]},
    })


def call(identifier="read", *, turn=TURN, typ="custom_tool_call", text="inspect()"):
    field = "input" if typ == "custom_tool_call" else "arguments"
    return row("response_item", {
        "type": typ, "id": "call-" + identifier, "call_id": identifier,
        "name": "exec", field: text, META: {"turn_id": turn},
    })


def output(identifier="read", *, turn=TURN, typ="custom_tool_call_output", text="setting=5"):
    return row("response_item", {
        "type": typ, "id": "result-" + identifier, "call_id": identifier,
        "output": text, META: {"turn_id": turn},
    })


def opening(prompt=PROMPT):
    return [header(), start(), context(), user(prompt)]


def complete_turn():
    return opening() + [call(), output(), assistant(), event("task_complete")]


def delivery_packet(text='Memory: use UTC for q7.'):
    return refresh_delivery_packet({"schema": observer.DELIVERY_SCHEMA, "session_id": SESSION, "turn_id": TURN,
            "rendered_text": text, "rendered_sha256": sha(text.encode()),
            "displayed": [{"db_id": "0" * 26, "node_id": "1" * 26, "kind": "semantic",
                           "shown_summary": "Use UTC for q7.", "full_get_fingerprint": "a" * 64}], "concerns": []})


def refresh_delivery_packet(packet):
    """Build current fixtures from their exact final displayed JSON, not prose claims."""
    prefix = packet["rendered_text"].split(observer.DISPLAY_PREFIX, 1)[0]
    views = []
    for card in packet["displayed"]:
        view = {**card.get("displayed_view", {}), "id": card["node_id"],
                "summary": card["shown_summary"], "kind": card["kind"],
                "status": "active", "source": "fixture"}
        card["displayed_view"] = view
        card["displayed_view_sha256"] = sha(observer.encoded(view))
        views.append(view)
    suffix = (observer.DISPLAY_PREFIX + json.dumps(views, ensure_ascii=False, separators=(",", ":"))
              + "".join(" " + observer.render_delivery_concern(case) for case in packet["concerns"]))
    prefix = prefix.encode()[:max(0, observer.MAX_DELIVERY_TEXT_BYTES - len(suffix.encode()))].decode("utf-8", "ignore")
    packet["rendered_text"] = prefix + suffix
    packet["rendered_sha256"] = sha(packet["rendered_text"].encode())
    return packet


def delivered(packet, *, identifier="memory", tag="hooks.additional_context", turn=TURN, role="developer"):
    return row("response_item", {"type": "message", "id": identifier, "role": role,
               "content": [{"type": "input_text", "text": packet["rendered_text"]}],
               META: {"turn_id": turn, "content_item_kinds": [tag]}})


def encoded(rows):
    return b"".join(json.dumps(value, ensure_ascii=False).encode("utf-8") + b"\n"
                    for value in rows)


class SourceTurnFixture:
    def __init__(self, checks):
        self.checks = checks
        temporary = tempfile.TemporaryDirectory()
        checks.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.sessions = self.root / "sessions"
        self.sessions.mkdir()
        self.path = self.sessions / "synthetic.jsonl"

    def write(self, rows):
        self.path.write_bytes(encoded(rows))

    def admit(self, *, prompt=PROMPT, limits=None, path=None, session=SESSION, turn=TURN):
        args = {"sessions_root": self.sessions, "session_id": session,
                "turn_id": turn, "expected_prompt_sha256": sha(prompt.encode("utf-8"))}
        if limits is not None:
            args["limits"] = limits
        return observer.admit_source_turn(path or self.path, **args)

    def admission(self, **kwargs):
        result = self.admit(**kwargs)
        self.checks.assertEqual(result["status"], "admitted", result)
        return result["admission"]

    def observe(self, pin=None, *, limits=None, delivery=None):
        if pin is None:
            pin = self.admission()
        return observer.observe_source_turn(pin, delivery=delivery,
                                           **({"limits": limits} if limits else {}))
