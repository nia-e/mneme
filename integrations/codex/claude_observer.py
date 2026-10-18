"""Bounded Claude JSONL capture, not recall or a second recording worker.

The hook's prompt_id is a trusted host coordinate, NOT a transcript field. Exact
prompt bytes and an exact preceding raw anchor bind it to a physical transcript record.
Stop is provisional, not a completion or flush barrier. A trusted next prompt,
SessionEnd or legitimate resume can request one bounded observed snapshot. The
observer prefers a flushed stopped response, but may expose a valid complete-line
prefix after the caller's finite flush allowance. Source binding and observed
coverage never establish whole-turn finality or task success. Frozen snapshots
re-read only their original exact byte range; later appends cannot change them.
Synthetic fixtures are not evidence of installed Claude behavior.
"""
from __future__ import annotations

from dataclasses import asdict, dataclass
import os
from pathlib import Path
import re

from rollout_primitives import decode_json, digest, encoded, is_token, open_regular
from turn_observer import Limits

ADMISSION_SCHEMA = "mneme.claude-turn-admission.v1"
OBSERVATION_SCHEMA = "mneme.capture-turn-observation.v2"
SNAPSHOT_SCHEMA = "mneme.capture-source-snapshot.v1"
STOP_SCHEMA = "mneme.claude-stop-receipt.v1"
BOUNDARY_SCHEMA = "mneme.claude-boundary-receipt.v1"
SHA = re.compile(r"[0-9a-f]{64}\Z")


@dataclass(frozen=True)
class ClaudeAdmission:
    schema: str
    path: str
    sessions_root: str
    session_id: str
    turn_id: str
    expected_prompt_sha256: str
    device: int
    inode: int
    start_offset: int
    start_ordinal: int
    preceding_anchor: dict | None
    logical_preceding_anchor: dict | None
    prompt_anchor: dict | None
    fresh_first_turn: bool


class Deferred(ValueError):
    pass


def _paths(path, root):
    if not isinstance(path, (str, Path)) or not isinstance(root, (str, Path)):
        raise Deferred("invalid_locator")
    path, root = Path(path), Path(root)
    if not path.is_absolute() or not root.is_absolute() or ".." in path.parts or ".." in root.parts:
        raise Deferred("invalid_locator")
    if path.resolve(strict=True) != path or root.resolve(strict=True) != root or not path.is_relative_to(root):
        raise Deferred("source_outside_explicit_root")
    if "subagents" in path.relative_to(root).parts:
        raise Deferred("subagent_source")
    return path, root


def _work():
    return {"bytes_read": 0, "source_bytes_read": 0, "records_parsed": 0, "evidence_bytes": 0}


def _reference(row, line, ordinal, offset):
    reference = {"ordinal": ordinal, "line": ordinal + 1, "ordinal_scope": "source_window",
                 "byte_offset": offset, "line_bytes": len(line), "raw_line_sha256": digest(line)}
    if is_token(row.get("uuid")):
        reference["item_id"] = row["uuid"]
    return reference


def _read(path, limits, work, *, start_offset=0, tail=False, expected_identity=None, preceding_anchor=None,
          logical_preceding_anchor=None, end_offset=None):
    limits.validate()
    fd = open_regular(path)
    try:
        before = os.fstat(fd)
        if expected_identity is not None and (before.st_dev, before.st_ino) != expected_identity:
            raise Deferred("source_replaced")
        checked = set()
        for anchor in (preceding_anchor, logical_preceding_anchor):
            if anchor is None or anchor["byte_offset"] in checked:
                continue
            checked.add(anchor["byte_offset"])
            os.lseek(fd, anchor["byte_offset"], os.SEEK_SET)
            anchor_raw = os.read(fd, anchor["line_bytes"])
            work["bytes_read"] += len(anchor_raw)
            if len(anchor_raw) != anchor["line_bytes"] or digest(anchor_raw) != anchor["raw_line_sha256"]:
                raise Deferred("source_preceding_anchor_changed")
        if tail:
            start_offset = max(0, before.st_size - limits.tail_bytes)
        os.lseek(fd, start_offset, os.SEEK_SET)
        maximum = limits.tail_bytes if tail else limits.scan_bytes
        if end_offset is not None:
            if (tail or type(end_offset) is not int or not start_offset < end_offset <= before.st_size
                    or end_offset - start_offset > maximum):
                raise Deferred("snapshot_range_invalid")
            amount = end_offset - start_offset
        else:
            amount = maximum + 1
        raw = os.read(fd, amount)
        work["bytes_read"] += len(raw)
        work["source_bytes_read"] += len(raw)
        truncated = end_offset is None and len(raw) > maximum
        if start_offset > before.st_size or len(raw) != min(amount, before.st_size - start_offset):
            raise Deferred("source_read_incomplete")
        incomplete = bool(not truncated and raw and not raw.endswith(b"\n"))
        if truncated or incomplete:
            raw = raw[:maximum]
            raw = raw[:raw.rfind(b"\n") + 1]
        if tail and start_offset:
            # First tail fragment is not a physical line in this source window.
            newline = raw.find(b"\n")
            if newline < 0:
                raise Deferred("source_record_limit")
            start_offset += newline + 1
            raw = raw[newline + 1:]
        def records():
            offset = start_offset
            for ordinal, line in enumerate(raw.splitlines(keepends=True)):
                if len(line) > limits.line_bytes or ordinal >= limits.events:
                    raise Deferred("source_record_limit")
                row = decode_json(line)
                work["records_parsed"] += 1
                if not isinstance(row, dict):
                    raise Deferred("invalid_transcript_record")
                yield row, _reference(row, line, ordinal, offset)
                offset += len(line)
        after, named = os.fstat(fd), path.stat()
        if ((before.st_dev, before.st_ino) != (named.st_dev, named.st_ino)
                or (after.st_size != before.st_size if end_offset is None else after.st_size < end_offset)):
            raise Deferred("source_changed_during_read")
        return raw, records(), before, start_offset, truncated, incomplete
    finally:
        os.close(fd)


def _excluded(row):
    return any(row.get(key) for key in ("isSidechain", "agentId", "agent_id", "teamName"))


def _blocks(row):
    message = row.get("message")
    if not isinstance(message, dict) or message.get("role") != row.get("type"):
        raise Deferred("invalid_message")
    content = message.get("content")
    if isinstance(content, str):
        return [(0, {"type": "text", "text": content}, "/message/content")]
    if not isinstance(content, list) or any(not isinstance(block, dict) for block in content):
        raise Deferred("invalid_message_content")
    return [(i, block, f"/message/content/{i}") for i, block in enumerate(content)]


def _prompt(row):
    if (row.get("type") != "user" or _excluded(row) or row.get("isMeta")
            or row.get("isCompactSummary") or row.get("isSynthetic")
            or row.get("sourceToolAssistantUUID") or row.get("sourceAgentId")):
        return None
    blocks = _blocks(row)
    if len(blocks) != 1 or blocks[0][1].get("type") != "text":
        return None
    text = blocks[0][1].get("text")
    return text if isinstance(text, str) else None


def _prior_work(row):
    return (not _excluded(row) and (row.get("isCompactSummary") or row.get("subtype") == "compact_boundary"
            or row.get("type") in ("user", "assistant") and not row.get("isMeta")))


def admit_source_turn(path, *, sessions_root, session_id, turn_id, expected_prompt_sha256, limits=Limits()):
    work = _work()
    result = {"schema": ADMISSION_SCHEMA, "status": "deferred", "reason": None,
              "admission": None, "work": work}
    try:
        if not is_token(session_id) or not is_token(turn_id) or not isinstance(expected_prompt_sha256, str) or not SHA.fullmatch(expected_prompt_sha256):
            raise Deferred("invalid_source_identity")
        path, root = _paths(path, sessions_root)
        raw, records, st, window_start, truncated, incomplete = _read(path, limits, work, tail=True)
        rows = list(records)
        if truncated:
            raise Deferred("source_scan_limit")
        if incomplete:
            raise Deferred("source_incomplete_line")
        if st.st_size and not rows:
            raise Deferred("source_anchor_unavailable")
        candidates = [(row, ref) for row, ref in rows if _prompt(row) is not None
                      and digest(_prompt(row).encode()) == expected_prompt_sha256]
        # Repeated text cannot establish which invocation was submitted.
        if len(candidates) > 1:
            raise Deferred("ambiguous_source_prompt")
        start, anchor = st.st_size, None
        preceding = rows[-1][1] if rows else None
        main_rows = [(r, ref) for r, ref in rows if not _excluded(r) and not r.get("isMeta")
                     and r.get("type") in ("user", "assistant", "progress", "system", "attachment") and is_token(r.get("uuid"))]
        logical_preceding = main_rows[-1][1] if main_rows else None
        fresh = window_start == 0 and not any(_prior_work(row) for row, _ in rows)
        if candidates:
            row, ref = candidates[0]
            # Already emitted assistant content means this is not an open admission.
            if any(not _excluded(r) and r.get("type") in ("user", "assistant") for r, _ in rows[ref["ordinal"] + 1:]):
                raise Deferred("not_current_open_turn")
            if row.get("sessionId") != session_id:
                raise Deferred("source_session_mismatch")
            if "promptId" in row and row["promptId"] != turn_id:
                raise Deferred("source_prompt_id_mismatch")
            start = ref["byte_offset"]
            anchor = {**ref, "ordinal": 0, "line": 1}
            preceding = rows[ref["ordinal"] - 1][1] if ref["ordinal"] else None
            prior_main = [r for r in main_rows if r[1]["byte_offset"] < start]
            logical_preceding = prior_main[-1][1] if prior_main else None
            fresh = window_start == 0 and not any(_prior_work(r) for r, _ in rows[:ref["ordinal"]])
        admission = ClaudeAdmission(ADMISSION_SCHEMA, str(path), str(root), session_id, turn_id,
                                   expected_prompt_sha256, st.st_dev, st.st_ino, start, 0,
                                   preceding, logical_preceding, anchor, fresh)
        result.update(status="admitted", reason="source_turn_admitted", admission=admission)
    except Deferred as error:
        result["reason"] = str(error)
    except (ValueError, TypeError, OSError, UnicodeError, RecursionError):
        result["reason"] = "invalid_source_or_locator"
    return result


def _validate_anchor(anchor):
    fields = {"ordinal", "line", "ordinal_scope", "byte_offset", "line_bytes", "raw_line_sha256"}
    if (not isinstance(anchor, dict) or not fields <= set(anchor) or set(anchor) - fields - {"item_id"}
            or any(type(anchor[key]) is not int or anchor[key] < 0 for key in ("ordinal", "line", "byte_offset", "line_bytes"))
            or anchor["ordinal_scope"] != "source_window" or anchor["line"] != anchor["ordinal"] + 1
            or not 1 <= anchor["line_bytes"] <= Limits().line_bytes
            or not isinstance(anchor["raw_line_sha256"], str) or not SHA.fullmatch(anchor["raw_line_sha256"])
            or "item_id" in anchor and not is_token(anchor["item_id"])):
        raise ValueError("invalid_claude_raw_anchor")


def restore_admission(value):
    if not isinstance(value, dict) or set(value) != set(ClaudeAdmission.__dataclass_fields__):
        raise ValueError("invalid_claude_admission")
    admission = ClaudeAdmission(**value)
    if (admission.schema != ADMISSION_SCHEMA or not is_token(admission.session_id) or not is_token(admission.turn_id)
            or not isinstance(admission.expected_prompt_sha256, str) or not SHA.fullmatch(admission.expected_prompt_sha256)
            or any(type(getattr(admission, key)) is not int or getattr(admission, key) < 0
                   for key in ("device", "inode", "start_offset", "start_ordinal"))
            or admission.start_ordinal != 0 or type(admission.fresh_first_turn) is not bool):
        raise ValueError("invalid_claude_admission")
    if admission.prompt_anchor is not None:
        _validate_anchor(admission.prompt_anchor)
        if admission.prompt_anchor["ordinal"] != 0 or admission.prompt_anchor["byte_offset"] != admission.start_offset:
            raise ValueError("invalid_claude_prompt_anchor")
    if admission.preceding_anchor is not None:
        _validate_anchor(admission.preceding_anchor)
        if admission.preceding_anchor["byte_offset"] + admission.preceding_anchor["line_bytes"] != admission.start_offset:
            raise ValueError("invalid_claude_preceding_anchor")
    if admission.logical_preceding_anchor is not None:
        _validate_anchor(admission.logical_preceding_anchor)
        if (admission.logical_preceding_anchor["byte_offset"] + admission.logical_preceding_anchor["line_bytes"] > admission.start_offset
                or "item_id" not in admission.logical_preceding_anchor):
            raise ValueError("invalid_claude_logical_preceding_anchor")
    return admission


def pin(admission):
    return {key: getattr(admission, key) for key in ("schema", "path", "sessions_root", "session_id", "device", "inode")}


def source_start(admission):
    return None  # Nominations bind exact evidence refs; a preflush hook has no raw start record.


def snapshot_end(admission, observation):
    return admission.start_offset + observation["work"]["source_bytes_read"]


def first_turn(admission):
    return admission.fresh_first_turn


def observation_byte_ceiling(admission, limits=Limits()):
    restore_admission(asdict(admission))
    limits.validate()
    return limits.scan_bytes + 1 + sum(a["line_bytes"] for a in (admission.preceding_anchor, admission.logical_preceding_anchor) if a is not None)


def validate_stop_receipt(receipt, *, session_id, turn_id):
    fields = {"schema", "session_id", "turn_id", "transcript_path", "last_assistant_sha256"}
    if isinstance(receipt, dict) and receipt.get("schema") == STOP_SCHEMA:
        if (set(receipt) != fields or receipt["session_id"] != session_id or receipt["turn_id"] != turn_id
                or not isinstance(receipt["last_assistant_sha256"], str) or not SHA.fullmatch(receipt["last_assistant_sha256"])):
            raise ValueError("invalid_stop_receipt")
        result = dict(receipt)
    else:
        if (not isinstance(receipt, dict) or receipt.get("hook_event_name") != "Stop"
                or receipt.get("agent_id") or receipt.get("session_id") != session_id
                or receipt.get("prompt_id", receipt.get("turn_id")) != turn_id
                or not isinstance(receipt.get("last_assistant_message"), str)
                or not 1 <= len(receipt["last_assistant_message"].encode()) <= 32768
                or type(receipt.get("stop_hook_active")) is not bool):
            raise ValueError("invalid_stop_receipt")
        result = {"schema": STOP_SCHEMA, "session_id": session_id, "turn_id": turn_id,
                  "transcript_path": receipt.get("transcript_path"),
                  "last_assistant_sha256": digest(receipt["last_assistant_message"].encode())}
    path = result["transcript_path"]
    if not isinstance(path, str) or not Path(path).is_absolute() or ".." in Path(path).parts or len(path.encode()) > 4096:
        raise ValueError("invalid_stop_locator")
    return result


def validate_boundary_receipt(receipt, *, session_id, turn_id):
    fields = {"schema", "session_id", "turn_id", "kind", "next_turn_id", "prompt_sha256", "transcript_path"}
    if (not isinstance(receipt, dict) or set(receipt) != fields or receipt.get("schema") != BOUNDARY_SCHEMA
            or receipt["session_id"] != session_id or receipt["turn_id"] != turn_id
            or receipt["kind"] not in ("next_prompt", "session_end", "resume")):
        raise ValueError("invalid_boundary_receipt")
    if receipt["kind"] == "next_prompt":
        if (not is_token(receipt["next_turn_id"]) or receipt["next_turn_id"] == turn_id
                or not isinstance(receipt["prompt_sha256"], str) or not SHA.fullmatch(receipt["prompt_sha256"])):
            raise ValueError("invalid_next_prompt_boundary")
    elif receipt["next_turn_id"] is not None or receipt["prompt_sha256"] is not None:
        raise ValueError("invalid_session_end_boundary")
    path = receipt["transcript_path"]
    if not isinstance(path, str) or not Path(path).is_absolute() or ".." in Path(path).parts or len(path.encode()) > 4096:
        raise ValueError("invalid_boundary_locator")
    return dict(receipt)


def validate_snapshot(value):
    """Checked persisted read horizon; neither finality nor write authority."""
    fields = {"schema", "trigger", "observed_through", "observed_sha256", "latest_stop",
              "trailing_partial_line", "unmatched_calls"}
    if (not isinstance(value, dict) or set(value) != fields or value.get("schema") != SNAPSHOT_SCHEMA
            or value.get("trigger") not in ("next_prompt", "session_end", "resume")
            or not isinstance(value.get("observed_sha256"), str) or not SHA.fullmatch(value["observed_sha256"])
            or value.get("latest_stop") not in ("matched", "not_observed")
            or type(value.get("trailing_partial_line")) is not bool
            or type(value.get("unmatched_calls")) is not int or not 0 <= value["unmatched_calls"] <= Limits().events):
        raise ValueError("invalid_source_snapshot")
    _validate_anchor(value["observed_through"])
    return decode_json(encoded(value))


def observe_source_turn(admission, *, limits=Limits(), delivery=None, stop_receipt=None, boundary_receipt=None,
                        allow_partial=False, snapshot=None):
    work = _work()
    coverage = {"source_turn": "incomplete", "prompt": "pending", "earlier_session_context": "omitted",
                "other_host_inputs": "omitted", "private_reasoning": "excluded", "sensitivity_review": "not_performed",
                "missing_results": 0, "omissions": {}, "public_evidence": None, "memory_delivery": "not_recorded",
                "prompt_authority": "unverified"}
    result = {"schema": OBSERVATION_SCHEMA, "source_host": "claude_code", "status": "deferred", "reason": None,
              "evidence": [], "coverage": coverage, "work": work, "boundary": None}
    calls, outputs = set(), set()
    seen_ids = set()
    try:
        if type(allow_partial) is not bool:
            raise Deferred("invalid_snapshot_policy")
        restore_admission(asdict(admission))
        path, _ = _paths(admission.path, admission.sessions_root)
        frozen = None if snapshot is None else validate_snapshot(snapshot)
        end_offset = (None if frozen is None else frozen["observed_through"]["byte_offset"]
                      + frozen["observed_through"]["line_bytes"])
        raw, rows, st, _, truncated, incomplete = _read(path, limits, work, start_offset=admission.start_offset,
                                   expected_identity=(admission.device, admission.inode),
                                   preceding_anchor=admission.preceding_anchor, logical_preceding_anchor=admission.logical_preceding_anchor,
                                   end_offset=end_offset)
        if frozen is not None and digest(raw) != frozen["observed_sha256"]:
            raise Deferred("source_snapshot_changed")
        receipt = None if stop_receipt is None else validate_stop_receipt(stop_receipt, session_id=admission.session_id, turn_id=admission.turn_id)
        if receipt and receipt["transcript_path"] != admission.path:
            raise Deferred("stop_source_mismatch")
        ending = None if boundary_receipt is None else validate_boundary_receipt(boundary_receipt, session_id=admission.session_id, turn_id=admission.turn_id)
        if ending and ending["transcript_path"] != admission.path:
            raise Deferred("boundary_source_mismatch")
        if frozen is not None and (ending is None or ending["kind"] != frozen["trigger"]):
            raise Deferred("source_snapshot_boundary_changed")
        evidence, prompt_seen, final_text, final_ref = [], False, None, None
        last_ref = None
        previous = (admission.logical_preceding_anchor or {}).get("item_id")
        prefix_closed = False
        if previous is not None:
            seen_ids.add(previous)
        def append(item, field, ref):
            item.update(evidence_index=len(evidence), source_field=field, ref=ref)
            if len(encoded(item)) > limits.item_bytes or len(evidence) >= limits.items:
                raise Deferred("public_evidence_limit")
            evidence.append(item)
            if len(encoded(evidence)) > limits.evidence_bytes:
                raise Deferred("public_evidence_limit")
        for row, ref in rows:
            preceding_ref, last_ref = last_ref, ref
            if ref["byte_offset"] < admission.start_offset:
                raise Deferred("source_anchor_changed")
            kind = row.get("type")
            if _excluded(row):
                coverage["omissions"]["collaboration_input"] = coverage["omissions"].get("collaboration_input", 0) + 1
                continue
            if kind not in ("user", "assistant", "progress", "system", "attachment"):
                # Recognized metadata outside the conversation chain carries no evidence.
                if kind in ("file-history-snapshot", "queue-operation", "last-prompt", "custom-title", "summary"):
                    continue
                raise Deferred("unknown_transcript_record")
            if row.get("sessionId") != admission.session_id:
                raise Deferred("source_session_mismatch")
            if row.get("isCompactSummary") or row.get("subtype") == "compact_boundary":
                raise Deferred("source_compacted")
            if not is_token(row.get("uuid")) or row.get("uuid") in seen_ids:
                raise Deferred("invalid_chain_identity")
            if prompt_seen and kind == "user" and _prompt(row) is not None:
                if receipt is None:
                    raise Deferred("source_turn_interrupted")
                if ending is None:
                    raise Deferred("source_boundary_pending")
                if (ending["kind"] != "next_prompt" or row.get("parentUuid") != previous
                        or digest(_prompt(row).encode()) != ending["prompt_sha256"]
                        or "promptId" in row and row["promptId"] != ending["next_turn_id"]):
                    raise Deferred("source_boundary_prompt_mismatch")
                prefix_closed = True
                last_ref = preceding_ref
                break
            if not prompt_seen:
                if admission.prompt_anchor is not None and ref != admission.prompt_anchor:
                    raise Deferred("source_prompt_anchor_changed")
                text = _prompt(row)
                if text is None:
                    # Previous-turn writes may lag the next prompt hook too. Only
                    # source-bound continuation records may precede the exact new
                    # prompt, never a different root text input.
                    prior_result = kind == "user" and all(b.get("type") == "tool_result" for _, b, _ in _blocks(row))
                    if (admission.prompt_anchor is not None or previous is None
                            or row.get("parentUuid") != previous
                            or kind not in ("assistant", "progress", "system", "attachment") and not prior_result):
                        raise Deferred("source_prompt_missing")
                    previous = row["uuid"]
                    seen_ids.add(previous)
                    coverage["omissions"]["other_host_message"] = coverage["omissions"].get("other_host_message", 0) + 1
                    continue
                if admission.prompt_anchor is None and previous is not None and row.get("parentUuid") != previous:
                    raise Deferred("source_branch_changed")
                if len(text.encode()) > limits.prompt_bytes or digest(text.encode()) != admission.expected_prompt_sha256:
                    raise Deferred("source_prompt_mismatch")
                if "promptId" in row and row["promptId"] != admission.turn_id:
                    raise Deferred("source_prompt_id_mismatch")
                prompt_seen = True
                coverage["prompt"] = "verified"
            elif row.get("parentUuid") != previous:
                raise Deferred("source_branch_changed")
            previous = row["uuid"]
            seen_ids.add(previous)
            if kind not in ("user", "assistant"):
                continue
            if row.get("isMeta") or row.get("isSynthetic"):
                raise Deferred("source_host_input")
            blocks = _blocks(row)
            if kind == "user" and evidence and any(b.get("type") != "tool_result" for _, b, _ in blocks):
                raise Deferred("source_turn_interrupted")
            text_blocks = []
            for index, block, pointer in blocks:
                typ = block.get("type")
                if typ in ("thinking", "redacted_thinking"):
                    continue
                if typ == "text":
                    text = block.get("text")
                    if not isinstance(text, str):
                        raise Deferred("invalid_text_block")
                    field = pointer if isinstance(row["message"]["content"], str) else pointer + "/text"
                    append({"kind": "user_statement" if kind == "user" else "assistant_assertion",
                            **({"phase": "commentary"} if kind == "assistant" else {}),
                            "content": [{"content_index": index, "text": text}]}, field, ref)
                    text_blocks.append(text)
                elif typ == "tool_use" and kind == "assistant":
                    call = block.get("id")
                    if not is_token(call) or call in calls or not is_token(block.get("name")) or not isinstance(block.get("input"), dict):
                        raise Deferred("invalid_tool_call")
                    calls.add(call)
                    append({"kind": "tool_call", "type": "tool_use", "call_id": call,
                            "name": block["name"], "input": block["input"]}, pointer, ref)
                elif typ == "tool_result" and kind == "user":
                    call = block.get("tool_use_id")
                    if not is_token(call) or call not in calls or call in outputs or "content" not in block:
                        raise Deferred("unmatched_tool_result")
                    output = block["content"]
                    if (not isinstance(output, (str, list)) or isinstance(output, list) and any(
                            not isinstance(b, dict) or b.get("type") != "text" or not isinstance(b.get("text"), str)
                            for b in output)):
                        raise Deferred("unsupported_tool_result_content")
                    outputs.add(call)
                    append({"kind": "tool_result", "type": "tool_result", "call_id": call,
                            "output": output}, pointer + "/content", ref)
                else:
                    raise Deferred("unsupported_content_block")
            final_text = "\n".join(text_blocks) if kind == "assistant" and text_blocks else None
            final_ref = ref if final_text else None
        if truncated and not prefix_closed:
            raise Deferred("source_scan_limit")
        if incomplete and not prefix_closed and not allow_partial and frozen is None:
            raise Deferred("source_incomplete_line")
        if not prompt_seen:
            raise Deferred("source_prompt_pending")
        if receipt is None:
            raise Deferred("source_turn_not_closed")
        if ending is None:
            raise Deferred("source_boundary_pending")
        matched = bool(final_text and digest(final_text.encode()) == receipt["last_assistant_sha256"])
        if frozen is None and not allow_partial:
            if ending["kind"] == "next_prompt" and not prefix_closed:
                raise Deferred("source_boundary_pending")
            if not matched:
                raise Deferred("final_transcript_pending_or_mismatch")
            if calls != outputs:
                raise Deferred("missing_tool_result")
        observed = len(evidence)
        dangling = calls - outputs
        evidence = [item for item in evidence if not (item["kind"] == "tool_call" and item["call_id"] in dangling)]
        omitted = observed - len(evidence)
        if omitted:
            coverage["omissions"]["incomplete_tool_exchange"] = omitted
        if not evidence or last_ref is None:
            raise Deferred("source_prompt_pending")
        endpoint = last_ref["byte_offset"] + last_ref["line_bytes"] - admission.start_offset
        selected_snapshot = validate_snapshot({"schema": SNAPSHOT_SCHEMA, "trigger": ending["kind"],
            "observed_through": last_ref, "observed_sha256": digest(raw[:endpoint]),
            "latest_stop": "matched" if matched else "not_observed",
            "trailing_partial_line": frozen["trailing_partial_line"] if frozen is not None else bool(incomplete and not prefix_closed),
            "unmatched_calls": len(dangling)})
        if frozen is not None and selected_snapshot != frozen:
            raise Deferred("source_snapshot_changed")
        for item in evidence:
            if matched and item["kind"] == "assistant_assertion" and item["ref"] == final_ref:
                item["phase"] = "final_answer"
        coverage.update(source_turn="observed_prefix", turn_finality="unproven", remaining_transcript="unknown",
                        snapshot=selected_snapshot, public_evidence={"mode": "selected_subset" if omitted else "all",
                            "observed_records": observed, "selected_records": len(evidence), "omitted_records": omitted})
        result.update(status="observed", reason="source_snapshot_ready", evidence=evidence, boundary=last_ref)
        work["evidence_bytes"] = len(encoded(evidence))
    except Deferred as error:
        result["reason"] = str(error)
    except (ValueError, TypeError, KeyError, OSError, UnicodeError, RecursionError):
        result["reason"] = "input_file_or_evidence_invalid"
    return result
