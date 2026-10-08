#!/usr/bin/env python3
"""Read-only qualification of an explicit emission against raw Codex rollout JSONL.

Contract / abstract cases before mechanics:
* A selected/emitted card is not yet host-admitted. Require an exact developer
  input_text slot tagged hooks.additional_context, with the same host session/turn.
* Identity is not content: hash the decoded displayed text's exact UTF-8 bytes,
  preserving newlines. Never reconstruct it from today's node/body or concatenate
  content slots. Undisplayed bodies, native route validity, epochs and model use
  are outside this helper's proof. Optional caller_binding is retained, not proved.
* Not-creditable does not mean invisible: cat(config) can start before admission
  and independently return the card's fact afterward. Keep that call/result in a
  separate noncreditable prior/inflight lane, never among later action targets.
* A prompt may already state the card's fact; a public plan may already choose
  the eventual action. Retain bounded actual source-turn user.text input, earlier
  public statements and tool observations, not an inferred inventory of knowledge.
  Missing/oversized prompts, absent results or prior-budget exhaustion degrade
  context coverage, not an independently established admission proof. Earlier
  session and other host inputs remain explicitly omitted: no novelty inference
  is justified when it depends on missing history.
* Later recorded custom/function calls and their same-turn results remain action
  targets, as do public assistant commentary/final_answer output_text messages.
  Never return private analysis/reasoning. No-tool answers remain observable.
  Code-mode wrappers are visible actions; do not invent chronology for inner calls.
* Task switch, compaction, cancellation, duplicate identity, missing metadata,
  incomplete JSONL or exhausted bounds leave qualification unknown. Preserve any
  already-established admission fact, but label the evidence incomplete.
* Later applicability facts (e.g. target R5 rather than R2) are separate judgment
  context. This helper neither judges helpfulness nor turns admission into learning
  authority. Cancellation, absence and truncation are never negative feedback.

Input: {schema: "mneme.contextual-emission.v1", session_id, turn_id,
        rendered_text, rendered_sha256, caller_binding?: object}.
Output: status qualified only for an unambiguous admission plus a closed, fully
scanned source turn with all retained later-call results. This proves no benefit.
Unknown results can retain accepted and partial actions; they are not trainable.
The v1 output extension is additive: prior_or_inflight_context has independent
coverage flags; existing fields/exit codes keep their meaning. The historical
ignored_pre_admission_results counter means excluded from later action targets,
not necessarily omitted from the new context lane.
task_prompt=observed means a tagged prompt candidate was retained, not that all
prompt/history context is complete; consumers must also inspect omissions.

Bounds: 8 MiB rollout, 512 KiB/line, 4096 events, 8 KiB packet, 32 action pairs,
32 public messages, 32 KiB/evidence item, 128 KiB total evidence, 160 KiB compact
CLI output. Prior context gets at most 16 KiB/16 items INSIDE the existing total,
with an 8 KiB prompt-text cap; optional context cannot displace admission evidence.
Smaller test limits are allowed.
Raw-line hashes include their terminating newline; ordinals are zero-based.
The CLI reads only its two explicit regular files and emits JSON on stdout.
No auth, network, provider, hook, Mneme/store or production-learning imports.
Usage: python3 tools/contextual_delivery_evidence.py --packet emission.json
       --rollout rollout.jsonl
Exit 0 means qualified evidence, 2 means unknown. For an unflushed active file,
retry only within the caller's bounded task window; otherwise skip qualification.
"""
from __future__ import annotations

import argparse
from dataclasses import asdict, dataclass
from pathlib import Path
import re
import sys
from typing import Any

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from integrations.codex.rollout_primitives import (  # noqa: E402
    CALL_FIELDS, META, OUTPUT_TYPES, TOKEN, decode_json as _json, digest,
    encoded as _encoded, is_token as _token, read_regular as _read_regular,
    record_ref as _ref,
)


from integrations.codex.turn_observer import hook_delivery_slot


SCHEMA = "mneme.contextual-delivery-evidence.v1"
PACKET_SCHEMA = "mneme.contextual-emission.v1"
MAX_PACKET_RECORD_BYTES = 16 * 1024
MAX_BINDING_BYTES = 4096
MAX_OUTPUT_BYTES = 160 * 1024
SHA256 = re.compile(r"[0-9a-f]{64}\Z")


@dataclass(frozen=True)
class Limits:
    scan_bytes: int = 8 * 1024 * 1024
    line_bytes: int = 512 * 1024
    events: int = 4096
    packet_bytes: int = 8192
    actions: int = 32
    public_outputs: int = 32
    item_evidence_bytes: int = 32 * 1024
    evidence_bytes: int = 128 * 1024
    prior_context_bytes: int = 16 * 1024
    prior_items: int = 16
    prompt_bytes: int = 8192

    def validate(self) -> None:
        for key, value in asdict(self).items():
            if type(value) is not int or not 1 <= value <= getattr(Limits(), key):
                raise ValueError("invalid_limits")


def _packet(value: Any, limits: Limits) -> dict:
    required = {"schema", "session_id", "turn_id", "rendered_text", "rendered_sha256"}
    if (not isinstance(value, dict) or not required <= set(value)
            or set(value) - required - {"caller_binding"}
            or value["schema"] != PACKET_SCHEMA
            or not _token(value["session_id"]) or not _token(value["turn_id"])):
        raise ValueError("invalid_packet")
    text, sha = value["rendered_text"], value["rendered_sha256"]
    if not isinstance(text, str) or not isinstance(sha, str) or not SHA256.fullmatch(sha):
        raise ValueError("invalid_packet")
    raw = text.encode("utf-8")
    if not raw or len(raw) > limits.packet_bytes:
        raise ValueError("packet_bytes_exceeded")
    if digest(raw) != sha:
        raise ValueError("packet_digest_mismatch")
    binding = value.get("caller_binding", {})
    if not isinstance(binding, dict) or len(_encoded(binding)) > MAX_BINDING_BYTES:
        raise ValueError("invalid_caller_binding")
    return value


def qualify(packet: Any, raw_rollout: bytes, *, limits: Limits = Limits()) -> dict:
    """Qualify a bounded raw-rollout prefix; do not repair partial/malformed data."""
    result = {"schema": SCHEMA, "status": "unknown", "reason": "packet_not_found",
              "accepted": None, "actions": [], "public_outputs": [], "partial": False,
              "outcome_evidence": "absent",
              "ignored_pre_admission_results": 0,
              "limits": asdict(limits),
              "scan": {"bytes": 0, "events": 0, "closed_turn": False}}
    prior = {"items": [], "bytes": 0, "coverage": {
        "task_prompt": "missing", "prompt_candidates": 0,
        "source_turn_prefix": "unobserved", "prior_results": "none_observed",
        "missing_results": 0, "truncated": False, "omissions": {},
        "earlier_session_context": "omitted", "other_host_inputs": "omitted"}}
    result["prior_or_inflight_context"] = prior
    coverage = prior["coverage"]
    prior_call_ids: set[str] = set()
    outputs: set[str] = set()
    evidence_bytes = 0

    def finish():
        missing = len(prior_call_ids - outputs)
        coverage["missing_results"] = missing
        coverage["prior_results"] = ("missing" if missing else "partial"
            if any(k.startswith("prior_result_") or k == "prior_context_displaced"
                   for k in coverage["omissions"]) else "observed"
            if prior_call_ids else "none_observed")
        if coverage["source_turn_prefix"] != "unobserved" and coverage["omissions"]:
            coverage["source_turn_prefix"] = "incomplete"
        return result

    def stop(reason: str, *, partial: bool = True):
        result.update(status="unknown", reason=reason, partial=partial)
        return finish()

    def omit_prior(reason: str, *, truncated: bool = False):
        coverage["omissions"][reason] = coverage["omissions"].get(reason, 0) + 1
        coverage["truncated"] |= truncated

    def retain_prior(item: dict) -> bool:
        nonlocal evidence_bytes
        try:
            size = len(_encoded(item))
        except (ValueError, TypeError, UnicodeError, RecursionError):
            omit_prior("prior_item_unencodable")
            return False
        if (len(prior["items"]) >= limits.prior_items or size > limits.item_evidence_bytes
                or prior["bytes"] + size > limits.prior_context_bytes
                or evidence_bytes + size > limits.evidence_bytes):
            omit_prior("prior_item_budget", truncated=True)
            return False
        prior["items"].append(item)
        prior["bytes"] += size
        evidence_bytes += size
        return True

    def core_space(size: int) -> bool:
        nonlocal evidence_bytes
        if evidence_bytes + size > limits.evidence_bytes and prior["items"]:
            # Optional context must not displace admission/later action evidence.
            evidence_bytes -= prior["bytes"]
            prior["bytes"] = 0
            prior["items"].clear()
            for identifier in prior_call_ids:
                starts[identifier] = None
            if coverage["task_prompt"] == "observed":
                coverage["task_prompt"] = "omitted_budget"
            omit_prior("prior_context_displaced", truncated=True)
        return evidence_bytes + size <= limits.evidence_bytes

    try:
        limits.validate()
        packet = _packet(packet, limits)
    except (ValueError, UnicodeError, RecursionError, TypeError) as error:
        return stop(str(error) if str(error) in {
            "invalid_limits", "invalid_packet", "packet_bytes_exceeded",
            "packet_digest_mismatch", "invalid_caller_binding"} else "invalid_packet",
            partial=False)
    result["binding"] = {"session_id": packet["session_id"], "turn_id": packet["turn_id"],
                         "rendered_sha256": packet["rendered_sha256"],
                         "caller_binding": packet.get("caller_binding", {})}
    if not isinstance(raw_rollout, bytes):
        return stop("invalid_rollout", partial=False)
    overflow = len(raw_rollout) > limits.scan_bytes
    raw_rollout = raw_rollout[:limits.scan_bytes]
    result["scan"]["supplied_prefix_sha256"] = digest(raw_rollout)
    result["scan"]["supplied_prefix_bytes"] = len(raw_rollout)
    session_seen = False
    target_seen = False
    task_started = False
    active_turn = None
    starts: dict[str, dict | None] = {}
    start_types: dict[str, str] = {}
    seen_item_ids: set[str] = set()
    diagnostic = "packet_not_found"
    offset = 0

    for ordinal, raw_line in enumerate(raw_rollout.splitlines(keepends=True)):
        if ordinal >= limits.events:
            return stop("event_limit_exceeded")
        if len(raw_line) > limits.line_bytes:
            return stop("line_bytes_exceeded")
        if not raw_line.endswith(b"\n"):
            return stop("scan_bytes_exceeded" if overflow else "unflushed_record")
        result["scan"].update(bytes=offset + len(raw_line), events=ordinal + 1)
        try:
            row = _json(raw_line)
        except (ValueError, UnicodeError, RecursionError):
            return stop("malformed_record")
        if not isinstance(row, dict) or not isinstance(row.get("payload"), dict):
            return stop("malformed_record")
        kind, payload = row.get("type"), row["payload"]
        if not isinstance(kind, str):
            return stop("malformed_record")
        if kind in ("response_item", "event_msg") and not isinstance(payload.get("type"), str):
            return stop("malformed_record")
        reference = _ref(row, raw_line, ordinal, offset)
        offset += len(raw_line)
        if ordinal == 0 and kind != "session_meta":
            return stop("missing_session_metadata")
        if kind == "session_meta":
            if session_seen:
                return stop("repeated_session_metadata")
            if (payload.get("id") != packet["session_id"]
                    or payload.get("session_id", packet["session_id"]) != packet["session_id"]):
                return stop("session_mismatch")
            session_seen = True
            continue
        if kind == "compacted" or (kind == "event_msg" and payload.get("type") in
                                    ("context_compacted", "turn_aborted", "task_aborted")):
            if target_seen:
                return stop("source_task_interrupted")
        lifecycle = payload.get("type") if kind == "event_msg" else None
        if kind == "turn_context" or lifecycle == "task_started":
            turn = payload.get("turn_id")
            if not _token(turn):
                return stop("missing_turn_metadata")
            if target_seen and turn != packet["turn_id"]:
                return stop("turn_changed")
            active_turn = turn
            if turn == packet["turn_id"]:
                target_seen = True
                if lifecycle == "task_started":
                    if task_started:
                        return stop("repeated_task_start")
                    task_started = True
            continue
        if lifecycle in ("task_complete", "task_completed") and target_seen:
            if payload.get("turn_id") != packet["turn_id"]:
                return stop("turn_changed")
            result["scan"]["closed_turn"] = True
            result["boundary"] = reference
            if overflow:
                return stop("scan_bytes_exceeded")
            if not task_started:
                return stop("missing_task_start")
            if result["accepted"] is None:
                return stop(diagnostic, partial=False)
            if any(action["result"] is None for action in result["actions"]):
                return stop("action_result_missing")
            result.update(status="qualified", reason="host_admitted_source_turn_complete",
                          partial=False, outcome_evidence="observed" if result["actions"]
                          or result["public_outputs"] else "absent")
            return finish()
        if kind != "response_item":
            continue
        typ = payload.get("type")
        if not isinstance(typ, str):
            return stop("malformed_record")
        item_id = payload.get("id")
        if _token(item_id):
            if item_id in seen_item_ids:
                return stop("repeated_response_item_id")
            seen_item_ids.add(item_id)
        metadata = payload.get(META)
        host_turn = metadata.get("turn_id") if isinstance(metadata, dict) else None
        content = payload.get("content")
        index, slot_error = hook_delivery_slot(payload,
            rendered_text=packet["rendered_text"], turn_id=packet["turn_id"])
        if slot_error == "packet_not_developer_input":
            diagnostic = slot_error
        elif slot_error is not None:
            return stop(slot_error)
        if index is not None:
            if not target_seen or active_turn != packet["turn_id"]:
                return stop("packet_turn_mismatch")
            if result["accepted"] is not None:
                return stop("duplicate_admission")
            result["accepted"] = {"ref": reference, "content_index": index,
                                  "rendered_text": packet["rendered_text"],
                                  "rendered_sha256": packet["rendered_sha256"],
                                  "host_turn_id": host_turn,
                                  "kind": "hooks.additional_context"}
            accepted_bytes = len(_encoded(result["accepted"]))
            if not core_space(accepted_bytes):
                result["accepted"] = None
                return stop("evidence_bytes_exceeded")
            evidence_bytes += accepted_bytes
            if not task_started:
                coverage["source_turn_prefix"] = "incomplete"
                omit_prior("task_start_not_observed_before_admission")
                return stop("missing_task_start")
            coverage["source_turn_prefix"] = "complete_for_supported_kinds"
            continue
        if not target_seen or active_turn != packet["turn_id"]:
            continue
        if typ == "message" and payload.get("role") == "user":
            kinds = metadata.get("content_item_kinds") if isinstance(metadata, dict) else None
            if (host_turn != packet["turn_id"] or not isinstance(content, list)
                    or not isinstance(kinds, list) or len(content) != len(kinds)):
                omit_prior("user_input_metadata_unavailable")
                continue
            slots = [i for i, kind in enumerate(kinds) if kind == "user.text"]
            if not slots:
                continue  # Environment wrappers are not task-prompt evidence.
            if result["accepted"] is not None or starts or not task_started:
                omit_prior("later_user_input_not_retained")
                continue
            coverage["prompt_candidates"] += 1
            if (not _token(item_id) or any(not isinstance(content[i], dict)
                    or content[i].get("type") != "input_text"
                    or not isinstance(content[i].get("text"), str) for i in slots)):
                coverage["task_prompt"] = "partial"
                omit_prior("prompt_content_unavailable")
                continue
            try:
                prompt_size = sum(len(content[i]["text"].encode("utf-8")) for i in slots)
            except UnicodeError:
                prompt_size = limits.prompt_bytes + 1
            if prompt_size > limits.prompt_bytes:
                coverage["task_prompt"] = "too_large"
                omit_prior("prompt_bytes_exceeded", truncated=True)
                continue
            prompt = {"ref": reference, "kind": "task_prompt", "creditable": False,
                      "timing": "before_admission", "content": [
                          {"content_index": i, "text": content[i]["text"]} for i in slots]}
            kept = retain_prior(prompt)
            coverage["task_prompt"] = "observed" if kept else "omitted_budget"
            if coverage["prompt_candidates"] > 1 or len(slots) != len(content):
                coverage["task_prompt"] = "partial"
                omit_prior("prompt_context_partial")
            continue
        if (typ == "message" and payload.get("role") == "assistant"
                and payload.get("phase") in ("commentary", "final_answer")):
            is_prior = result["accepted"] is None
            if host_turn != packet["turn_id"]:
                if is_prior:
                    omit_prior("public_statement_turn_unavailable")
                    continue
                return stop("public_output_turn_metadata_mismatch")
            if (not _token(item_id) or not isinstance(content, list) or not content
                    or len(content) > 32
                    or any(not isinstance(slot, dict) or slot.get("type") != "output_text"
                           or not isinstance(slot.get("text"), str) for slot in content)):
                if is_prior:
                    omit_prior("public_statement_content_unavailable")
                    continue
                return stop("unsupported_public_output_content")
            if not is_prior and len(result["public_outputs"]) >= limits.public_outputs:
                return stop("public_output_limit_exceeded")
            public = {"ref": reference, "kind": "assistant_public_message",
                      "phase": payload["phase"],
                      "content": [{"content_index": i, "text": slot["text"]}
                                  for i, slot in enumerate(content)]}
            if is_prior:
                public.update(creditable=False, timing="before_admission")
                retain_prior(public)
                continue
            try:
                size = len(_encoded(public))
            except (ValueError, TypeError, UnicodeError, RecursionError):
                return stop("malformed_public_output")
            if size > limits.item_evidence_bytes or not core_space(size):
                return stop("evidence_bytes_exceeded")
            evidence_bytes += size
            result["public_outputs"].append(public)
            continue
        if typ not in CALL_FIELDS and typ not in OUTPUT_TYPES:
            if typ.endswith("_call"):
                if result["accepted"]:
                    return stop("unsupported_action_type")
                omit_prior("unsupported_prior_action")
            continue
        if host_turn != packet["turn_id"]:
            return stop("action_turn_metadata_mismatch")
        call_id = payload.get("call_id")
        item_id = payload.get("id")
        if not _token(call_id) or not _token(item_id):
            return stop("invalid_or_repeated_action_identity")
        if typ in CALL_FIELDS:
            if call_id in starts or call_id in outputs:
                return stop("duplicate_call_id")
            if not _token(payload.get("name")) or not isinstance(payload.get(CALL_FIELDS[typ]), str):
                return stop("malformed_action")
            start_types[call_id] = typ
            starts[call_id] = None
            if result["accepted"] is None:
                prior_call_ids.add(call_id)
                prior_call = {"ref": reference, "kind": "tool_call", "type": typ,
                              "call_id": call_id, "name": payload["name"],
                              "input": payload[CALL_FIELDS[typ]], "result": None,
                              "creditable": False, "timing": "started_before_admission"}
                if retain_prior(prior_call):
                    starts[call_id] = prior_call
                continue
            if len(result["actions"]) >= limits.actions:
                return stop("action_limit_exceeded")
            action = {"ref": reference, "type": typ, "call_id": call_id,
                      "name": payload["name"], "input": payload[CALL_FIELDS[typ]], "result": None}
            try:
                size = len(_encoded(action))
            except (ValueError, TypeError, UnicodeError, RecursionError):
                return stop("malformed_action")
            if size > limits.item_evidence_bytes or not core_space(size):
                return stop("evidence_bytes_exceeded")
            evidence_bytes += size
            starts[call_id] = action
            result["actions"].append(action)
        else:
            if call_id in outputs:
                return stop("duplicate_result")
            if call_id not in starts:
                return stop("result_without_start")
            if start_types[call_id] != OUTPUT_TYPES[typ] or "output" not in payload:
                return stop("result_type_mismatch")
            outputs.add(call_id)
            action = starts[call_id]
            if call_id in prior_call_ids:
                if result["accepted"] is not None:
                    result["ignored_pre_admission_results"] += 1
                if action is None:
                    omit_prior("prior_result_start_omitted")
                    continue
                observation = {"ref": reference, "output": payload["output"],
                               "timing": "after_admission" if result["accepted"] else "before_admission"}
                try:
                    size = len(_encoded(observation))
                    increment = len(_encoded({**action, "result": observation})) - len(_encoded(action))
                except (ValueError, TypeError, UnicodeError, RecursionError):
                    omit_prior("prior_result_unencodable")
                    continue
                if (size > limits.item_evidence_bytes or prior["bytes"] + increment > limits.prior_context_bytes
                        or evidence_bytes + increment > limits.evidence_bytes):
                    omit_prior("prior_result_budget", truncated=True)
                    continue
                action["result"] = observation
                prior["bytes"] += increment
                evidence_bytes += increment
                continue
            observation = {"ref": reference, "output": payload["output"]}
            try:
                size = len(_encoded(observation))
            except (ValueError, TypeError, UnicodeError, RecursionError):
                return stop("malformed_result")
            if size > limits.item_evidence_bytes or not core_space(size):
                return stop("evidence_bytes_exceeded")
            evidence_bytes += size
            action["result"] = observation
    if overflow:
        return stop("scan_bytes_exceeded")
    if not session_seen:
        return stop("missing_session_metadata")
    if not target_seen:
        return stop("source_turn_not_found")
    return stop("source_turn_not_closed" if result["accepted"] else diagnostic)


def qualify_files(packet_path: Path, rollout_path: Path) -> dict:
    try:
        packet_raw = _read_regular(packet_path, MAX_PACKET_RECORD_BYTES)
        if len(packet_raw) > MAX_PACKET_RECORD_BYTES:
            raise ValueError("packet_record_bytes_exceeded")
        packet = _json(packet_raw)
        raw = _read_regular(rollout_path, Limits().scan_bytes)
    except (OSError, ValueError, UnicodeError, RecursionError):
        return {"schema": SCHEMA, "status": "unknown", "reason": "input_file_unavailable_or_invalid",
                "accepted": None, "actions": [], "partial": True}
    result = qualify(packet, raw)
    result["source"] = {"rollout": str(rollout_path), "emission": str(packet_path),
                        "emission_record_sha256": digest(packet_raw)}
    return result


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--packet", type=Path, required=True,
                        help="explicit emitted-packet JSON; no discovery or regeneration")
    parser.add_argument("--rollout", type=Path, required=True,
                        help="explicit raw Codex rollout JSONL, not CLI/app-server event JSON")
    args = parser.parse_args(argv)
    result = qualify_files(args.packet, args.rollout)
    raw = _encoded(result)
    if len(raw) > MAX_OUTPUT_BYTES:
        result = {"schema": SCHEMA, "status": "unknown", "reason": "output_bytes_exceeded",
                  "accepted": None, "actions": [], "public_outputs": [], "partial": True}
        raw = _encoded(result)
    print(raw.decode("utf-8"))
    return 0 if result["status"] == "qualified" else 2


if __name__ == "__main__":
    sys.exit(main())
