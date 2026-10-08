#!/usr/bin/env python3
"""Evaluation-only, cost-bounded Jev veto over at most two recalled cards.

No network is possible without ``live=True``, an explicit key file, a ledger,
and a provider. The default provider uses the older bounded HTTP primitive,
not its plan-selection policy. This module neither reads memory nor learns.
"""

from __future__ import annotations

import fcntl
import hashlib
import importlib.util
import json
import math
import os
from pathlib import Path
import re
import time
from typing import Callable


MODEL = "jev-1.13.0"
SCHEMA = "mneme.reflexive-reader.jev-ledger.v1"
MAX_CUE_BYTES = 800
MAX_SUMMARY_BYTES = 700
MAX_REQUEST_BYTES = 8192
MAX_RESPONSE_BYTES = 2_000_000
MAX_CALLS = 64
MAX_SPEND_MICRO_USD = 180_000
RESERVE_MICRO_USD = 2753  # ceil(65,536 tokens * $42 / billion)
VETO_THRESHOLD = 0.80
PROMPT_VERSION = "narrow-veto-v1"

QUESTION = ("Is this prior card CLEARLY unnecessary for the present task? "
            "Judge only whether including this card could add useful task context. "
            "Do not solve the task, propose actions, or generate new memory.")
TRUE = ("Clearly unnecessary: it is obviously unrelated; the current task already "
        "provides the same fact; or its scope is explicitly incompatible with the "
        "present task. A superficial shared word is not enough to make it useful.")
FALSE = ("Not clearly unnecessary: applicability is uncertain, or this could "
         "provide a relevant prior lesson, warning, contrast, or connection. "
         "When in doubt, keep it.")
ACK = re.compile(r"(?is)^\s*(?:ok(?:ay)?|yes|no|yep|nope|thanks?|thank you|got it|sounds good|cool|👍|\+1|continue|go ahead|please proceed)[.!\s]*$")
TRIVIAL = re.compile(r"(?is)^\s*(?:what(?:'s| is) the (?:time|date)(?: now| today)?\??|translate [^\n]{1,90}|(?:rewrite|rephrase|format) (?:this|the following)[: ]?[^\n]{0,90})\s*$")
WORK = re.compile(r"(?i)\b(?:implement|fix|debug|review|audit|design|research|investigate|test|refactor|compare|build|write|plan|analyze|analyse|remember|recall|decide|trace|verify|migrate)\b|[/\\][\w.-]+|\b(?:repo|module|PR|issue|bug|codebase|benchmark)\b")


def _encode(value: object) -> bytes:
    return json.dumps(value, ensure_ascii=False, allow_nan=False,
                      sort_keys=True, separators=(",", ":")).encode("utf-8")


def _substantive(cue: str) -> bool:
    text = cue.strip()
    if not text or ACK.fullmatch(text) or (len(text) <= 160 and TRIVIAL.fullmatch(text)):
        return False
    return bool(WORK.search(text) or len(text) >= 100 or "\n" in text)


def _receipt(ids: list[str], reason: str, **kwargs: object) -> dict:
    return {"selected_ids": ids, "reason": reason, "provider_attempt": False,
            "latency_ms": 0.0, "usage": None, "estimated_usd": 0.0,
            "cache_hit": False, "reserved_usd": 0.0, "probabilities": {}, **kwargs}


def _valid_cards(cards: list[dict]) -> bool:
    for card in cards:
        if not isinstance(card, dict):
            return False
        summary = card.get("summary")
        if not isinstance(summary, str) or not summary.strip() or len(summary.encode()) > MAX_SUMMARY_BYTES:
            return False
        for field, limit in (("source", 256), ("fingerprint", 128)):
            if field in card and (not isinstance(card[field], str) or len(card[field].encode()) > limit):
                return False
        native = card.get("native")
        if native is not None and not isinstance(native, dict):
            return False
    return True


def _load_ledger(path: Path) -> dict:
    if not path.exists():
        return {"schema": SCHEMA, "intents": []}
    with path.open("rb") as file:
        raw = file.read(512_001)
    if len(raw) > 512_000:
        raise ValueError("ledger too large")
    data = json.loads(raw, parse_constant=lambda _: (_ for _ in ()).throw(ValueError("nonfinite")))
    intents = data.get("intents") if isinstance(data, dict) and data.get("schema") == SCHEMA else None
    if (not isinstance(intents, list) or len(intents) > MAX_CALLS
            or any(not isinstance(i, dict) or not isinstance(i.get("identity"), str)
                   or i.get("reserved_micro_usd") != RESERVE_MICRO_USD
                   or i.get("status") not in ("reserved", "success", "error") for i in intents)
            or len({i["identity"] for i in intents}) != len(intents)):
        raise ValueError("invalid ledger")
    return data


def _write_ledger(path: Path, data: dict) -> None:
    tmp = path.with_name(path.name + f".{os.getpid()}.{time.monotonic_ns()}.tmp")
    try:
        with tmp.open("x", encoding="utf-8") as file:
            json.dump(data, file, ensure_ascii=False, allow_nan=False, sort_keys=True)
            file.write("\n")
            file.flush()
            os.fsync(file.fileno())
        os.replace(tmp, path)
        dir_fd = os.open(path.parent, os.O_RDONLY)
        try:
            os.fsync(dir_fd)
        finally:
            os.close(dir_fd)
    finally:
        tmp.unlink(missing_ok=True)


def _locked(path: Path, operation: Callable[[dict], tuple[dict, bool]]) -> dict:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.with_name(path.name + ".lock").open("a+b") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        ledger = _load_ledger(path)
        answer, changed = operation(ledger)
        if changed:
            _write_ledger(path, ledger)
        return answer


def _reserve(path: Path, identity: str, ids: list[str]) -> dict:
    def operation(ledger: dict) -> tuple[dict, bool]:
        intents = ledger["intents"]
        for intent in intents:
            if intent["identity"] == identity:
                return intent.copy(), False
        if (len(intents) >= MAX_CALLS or
                (len(intents) + 1) * RESERVE_MICRO_USD > MAX_SPEND_MICRO_USD):
            return {"status": "cap"}, False
        intent = {"identity": identity, "ids": ids, "status": "reserved",
                  "reserved_micro_usd": RESERVE_MICRO_USD}
        intents.append(intent)
        return {**intent, "_new": True}, True
    return _locked(path, operation)


def _finish(path: Path, identity: str, result: dict) -> None:
    def operation(ledger: dict) -> tuple[dict, bool]:
        for intent in ledger["intents"]:
            if intent["identity"] == identity and intent["status"] == "reserved":
                intent.update(result)
                return {}, True
        raise ValueError("missing reservation")
    _locked(path, operation)


def _validate_response(response: object, count: int) -> tuple[list[float], dict]:
    if not isinstance(response, dict) or response.get("model") != MODEL:
        raise ValueError("model mismatch")
    if len(_encode(response)) > MAX_RESPONSE_BYTES:
        raise ValueError("oversized response")
    answers, usage = response.get("answers"), response.get("usage")
    expected = {f"q{i}" for i in range(count)}
    if not isinstance(answers, dict) or set(answers) != expected or not isinstance(usage, dict):
        raise ValueError("response shape")
    probabilities = []
    for index in range(count):
        answer = answers[f"q{index}"]
        p = answer.get("noul") if isinstance(answer, dict) and answer.get("type") == "noul" else None
        if isinstance(p, bool) or not isinstance(p, (int, float)) or not math.isfinite(p) or not 0 <= p <= 1:
            raise ValueError("invalid noul")
        probabilities.append(float(p))
    tokens = [usage.get("input_tokens"), usage.get("output_tokens")]
    if (any(isinstance(t, bool) or not isinstance(t, int) or t < 0 for t in tokens)
            or tokens[0] > 65536 or tokens[1] > 1_000_000):
        raise ValueError("invalid usage")
    return probabilities, {"input_tokens": tokens[0], "output_tokens": tokens[1]}


def _default_provider(body: bytes, key: str) -> tuple[int | None, object | None, str | None, float]:
    path = Path(__file__).resolve().parent.parent / "tools/selection_http.py"
    spec = importlib.util.spec_from_file_location("_bounded_jev_http", path)
    if spec is None or spec.loader is None:
        raise ValueError("bounded HTTP client unavailable")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    status, response, error, elapsed, conn = module.call(None, body, key)
    if conn is not None:
        conn.close()
    return status, response, error, elapsed


def _read_key(path: Path) -> str:
    with path.open("rb") as file:
        raw = file.read(4097)
    key = raw.decode("ascii").strip()
    if (len(raw) > 4096 or not key or len(key) > 4096
            or any(not 33 <= ord(c) <= 126 for c in key)):
        raise ValueError("invalid key file")
    return key


def select(cue: str, cards: list[dict], ledger_path: Path | None = None, *,
           live: bool = False, key_file: Path | None = None,
           provider: Callable[[bytes, str], tuple[int | None, object | None, str | None, float]] | None = None,
           seen: tuple[tuple[str, str], ...] = ()) -> dict:
    """Return only original IDs. Errors and uncertainty retain admitted context.

    ``seen`` is explicit previously delivered (id, fingerprint) pairs. Omitted
    fingerprints are not assumed to match. No caller may use this as an authoring
    or reflection judgment; it is only a read-time selection experiment.
    """
    if not isinstance(cards, list) or len(cards) > 2 or any(
            not isinstance(c, dict) or not isinstance(c.get("id"), str)
            or not 0 < len(c["id"].encode()) <= 128 for c in cards):
        raise ValueError("expected at most two bounded identified cards")
    ids = [c["id"] for c in cards]
    if len(ids) != len(set(ids)):
        raise ValueError("duplicate card IDs")
    if not cards:
        return _receipt([], "empty")
    if not isinstance(cue, str) or len(cue.encode()) > MAX_CUE_BYTES or not _valid_cards(cards):
        return _receipt(ids, "invalid_input")
    if not _substantive(cue):
        return _receipt(ids, "nonsubstantive")
    try:
        seen_pairs = set(seen)
    except (TypeError, ValueError):
        return _receipt(ids, "invalid_seen")
    remaining = [c for c in cards if not (c.get("fingerprint") is not None
                 and (c["id"], c["fingerprint"]) in seen_pairs)]
    remaining_ids = [c["id"] for c in remaining]
    if not remaining:
        return _receipt([], "already_delivered")
    if any((c.get("native") or {}).get("graph_path") for c in cards):
        return _receipt(remaining_ids, "graph_path_bypass")
    if not live:
        return _receipt(remaining_ids, "offline")
    if ledger_path is None or key_file is None:
        return _receipt(remaining_ids, "live_not_configured")

    request = {"model": MODEL, "state": {"task": cue}, "questions": {
        f"q{i}": {"type": "noul", "instructions": {"card": c["summary"], "question": QUESTION},
                 "criteria": {"true": TRUE, "false": FALSE}}
        for i, c in enumerate(remaining)}}
    body = _encode(request)
    if len(body) > MAX_REQUEST_BYTES:
        return _receipt(remaining_ids, "request_cap")
    identity = hashlib.sha256(_encode([PROMPT_VERSION, VETO_THRESHOLD, request, [
        [c["id"], c.get("source"), c.get("fingerprint")] for c in remaining]])).hexdigest()
    try:
        intent = _reserve(Path(ledger_path), identity, remaining_ids)
    except (OSError, ValueError, TypeError):
        return _receipt(remaining_ids, "ledger_error")
    if intent["status"] == "cap":
        return _receipt(remaining_ids, "budget_cap")
    if intent["status"] == "success":
        if (not isinstance(intent.get("selected_ids"), list)
                or any(i not in remaining_ids for i in intent["selected_ids"])
                or len(intent["selected_ids"]) != len(set(intent["selected_ids"]))
                or not isinstance(intent.get("probabilities"), dict)
                or not isinstance(intent.get("usage"), dict)
                or not isinstance(intent.get("estimated_usd"), (int, float))):
            return _receipt(remaining_ids, "ledger_error")
        return _receipt(intent["selected_ids"], "cache", cache_hit=True,
                        usage=intent["usage"], estimated_usd=intent["estimated_usd"],
                        probabilities=intent["probabilities"])
    if not intent.get("_new", False):
        return _receipt(remaining_ids, "prior_attempt")

    start = time.perf_counter()
    status = None
    response = None
    error = "provider_error"
    elapsed = 0.0
    invoked = False
    try:
        key = _read_key(Path(key_file))
        invoked = True
        status, response, error, elapsed = (provider or _default_provider)(body, key)
        if (isinstance(elapsed, bool) or not isinstance(elapsed, (int, float))
                or not math.isfinite(elapsed) or elapsed < 0):
            raise ValueError("invalid provider elapsed time")
        if error is None and status == 200:
            probabilities, usage = _validate_response(response, len(remaining))
            selected = [c["id"] for c, p in zip(remaining, probabilities) if p < VETO_THRESHOLD]
            estimates = usage["input_tokens"] * 42 / 1_000_000_000
            result = {"status": "success", "selected_ids": selected,
                      "probabilities": dict(zip(remaining_ids, probabilities)),
                      "usage": usage, "estimated_usd": estimates}
            reason = "veto" if len(selected) < len(remaining) else "keep"
        else:
            result = {"status": "error"}
            reason = "provider_error"
    except Exception:
        result = {"status": "error"}
        reason = "provider_error"
    try:
        _finish(Path(ledger_path), identity, result)
    except (OSError, ValueError, TypeError):
        return _receipt(remaining_ids, "ledger_error", provider_attempt=invoked,
                        latency_ms=round((time.perf_counter() - start) * 1000, 3),
                        estimated_usd=None,
                        reserved_usd=RESERVE_MICRO_USD / 1_000_000)
    return _receipt(result.get("selected_ids", remaining_ids), reason, provider_attempt=invoked,
                    latency_ms=round(elapsed or (time.perf_counter() - start) * 1000, 3),
                    usage=result.get("usage"), estimated_usd=result.get("estimated_usd"),
                    probabilities=result.get("probabilities", {}),
                    reserved_usd=RESERVE_MICRO_USD / 1_000_000)
