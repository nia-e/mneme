#!/usr/bin/env python3
"""Deadline-bound project recall and optional guarded advisory notices.

The parent sends the prompt to a disposable child on stdin, never argv or a
file. The child contacts only an already-running explicit project MCP host.
"""

import hashlib
from contextlib import contextmanager
import json
from pathlib import Path
import re
import subprocess
import sys
import time

from mcp_client import McpClient, McpError
from recording_contract import AssociationTarget
from reader_contract import plan as reader_plan
from reader_contract import (EPISODE_FIELDS, EPISODE_OPTIONAL_FIELDS,
                             validate_episode_projection_fields, validate_recording_session)
from librarian_policy import LibrarianBudget
from touchstone_contract import validate_touchstone_view, validate_touchstone_retrieval
from service import ConnectConfig, MAX_CONFIG_BYTES, load_config
from target_policy import (policy_for, GLOBAL_PREFERENCE_TAG, GLOBAL_PREFERENCE_NAMESPACE,
                           GLOBAL_PREFERENCE_CUE)

MAX_PROMPT_BYTES = 8192
MAX_CARDS = 2
MAX_SUMMARY_BYTES = 700
MAX_SOURCE_BYTES = 256
MAX_CARDS_BYTES = 2600
MAX_CHILD_OUTPUT = 6144
NODE_ID_LEN = 26
NODE_ID_RE = re.compile(r"[0-7][0-9A-HJKMNP-TV-Z]{25}\Z")
MAX_READER_CARD_BYTES = 1536
MAX_READER_BYTES = 8192
MAX_READER_CONTROL_BYTES = 2048
MAX_READER_RESULT_BYTES = MAX_READER_BYTES + MAX_READER_CONTROL_BYTES
MAX_NATIVE_RESPONSE_BYTES = 32768
MAX_OVERLAP_SUMMARY_BYTES = 512


class _IdentityMismatch(ValueError):
    pass


def _result(outcome, cards=None, elapsed_ms=0, observation=None):
    result = {"outcome": outcome, "cards": cards or [], "elapsed_ms": elapsed_ms}
    if observation is not None:
        result["observation"] = observation
    return result


def _wire_value(value):
    # JSON wire equality distinguishes bools from integers, unlike Python's ==.
    return json.dumps(value, ensure_ascii=False, allow_nan=False,
                      sort_keys=True, separators=(",", ":"))


def _short(text, limit):
    # Keep model-facing cards single-line and UTF-8 bounded. The full original
    # summary and provenance, not this presentation prefix, bind fingerprint.
    text = " ".join("".join(char if ord(char) >= 32 and ord(char) != 127 else " "
                            for char in text).split())
    raw = text.encode("utf-8")
    if len(raw) <= limit:
        return text
    return raw[:limit].decode("utf-8", errors="ignore").rstrip()


def _source(provenance):
    if not isinstance(provenance, dict):
        return None
    kind = provenance.get("type")
    if kind == "external":
        source = provenance.get("source")
        if not isinstance(source, dict):
            return None
        fields = (source.get("namespace"), source.get("key"), source.get("reference"))
        if not all(isinstance(field, str) and field for field in fields):
            return None
        return _short("%s:%s @ %s" % fields, MAX_SOURCE_BYTES)
    if kind == "conversation":
        session, turn = provenance.get("session"), provenance.get("turn")
        if isinstance(session, str) and isinstance(turn, (str, int)):
            return _short("conversation:%s/%s" % (session, turn), MAX_SOURCE_BYTES)
    if kind == "web" and isinstance(provenance.get("url"), str):
        return _short("web:%s" % provenance["url"], MAX_SOURCE_BYTES)
    if kind == "derived" and isinstance(provenance.get("from"), list):
        return _short("derived:%s" % ",".join(map(str, provenance["from"])), MAX_SOURCE_BYTES)
    return None


def _expected_catalog(value, project_db, alias="project"):
    return (isinstance(value, list) and len(value) == 1
            and isinstance(value[0], dict)
            and value[0].get("db") == alias
            and value[0].get("name") == alias
            and value[0].get("state") == "open"
            and value[0].get("configured_path") == str(project_db))


# Ranking, lexical cue normalization and episode validity stay native; this is
# only the bounded candidate/readback presentation boundary.
def _candidate(entry, lane):
    if not isinstance(entry, dict) or not isinstance(entry.get("id"), str):
        raise ValueError("malformed recall card")
    identifier = entry["id"]
    if len(identifier) != NODE_ID_LEN:
        return None
    candidate = {"id": identifier, "kind": "episode" if lane == "episodes" else "semantic"}
    if "touchstone" in entry:
        if lane == "episodes":
            raise ValueError("episode cannot own touchstone")
        candidate["touchstone"] = validate_touchstone_view(entry["touchstone"])
    if lane == "episodes":
        if entry.get("kind") != "episode" or any(key not in entry for key in EPISODE_FIELDS):
            raise ValueError("malformed episodic recall card")
        candidate.update({key: entry[key] for key in EPISODE_FIELDS})
        for key in EPISODE_OPTIONAL_FIELDS:
            if key in entry:
                if not isinstance(entry[key], list) or not entry[key]:
                    raise ValueError("malformed episodic occurrence context")
                candidate[key] = entry[key]
        if candidate["edition_id"] != identifier:
            raise ValueError("episode edition identity mismatch")
        candidate.update(validate_episode_projection_fields(candidate))
    return candidate


def _candidates(context, *, reader=False, limit=None):
    if (not isinstance(context, dict) or context.get("schema") not in ("mneme.context.v6", "mneme.context.v7")
            or "probationary" in context
            or any(not isinstance(context.get(lane), list)
                   for lane in ("core", "primary", "expansions", "episodes"))
            or any(isinstance(context.get(key), dict) and "probationary" in context[key]
                   for key in ("omitted", "usage"))
            or (isinstance(context.get("retrieval"), dict)
                and isinstance(context["retrieval"].get("lanes"), dict)
                and "probationary" in context["retrieval"]["lanes"])):
        raise ValueError("unexpected recall context")
    if context["schema"] == "mneme.context.v6" and any(
            "touchstone" in entry for lane in ("core", "primary", "expansions", "episodes")
            for entry in context[lane] if isinstance(entry, dict)):
        raise ValueError("touchstone needs successor envelope")
    if context["schema"] == "mneme.context.v7":
        validate_touchstone_retrieval(context.get("touchstone_retrieval"))
    semantic, episodic = [], []
    seen = set()
    # Core is always-loaded, not a task-ranked hit lane. The ordinary hook
    # retains its two-card kind reservation; the reader samples every native
    # task lane before filling more from any one lane.
    lanes = ["primary"]
    if reader:
        lanes.append("expansions")
    lanes.append("episodes")
    by_lane = {}
    for lane in lanes:
        entries = context.get(lane)
        if not isinstance(entries, list):
            raise ValueError("malformed recall lane")
        if reader and _bytes(context) > MAX_NATIVE_RESPONSE_BYTES:
            raise ValueError("oversized recall envelope")
        target = episodic if lane == "episodes" else semantic
        lane_candidates = []
        for entry in entries:
            if not reader and len(target) == MAX_CARDS:
                break
            candidate = _candidate(entry, lane)
            if reader and candidate is not None and not NODE_ID_RE.fullmatch(candidate["id"]):
                raise ValueError("malformed native node id")
            if candidate is None or candidate["id"] in seen:
                continue
            target.append(candidate)
            lane_candidates.append(candidate)
            seen.add(candidate["id"])
        by_lane[lane] = lane_candidates
    if reader:
        # Graph and conditional entries also occupy primary. Only native
        # provenance classifies them; a lane label alone is not an origin oracle.
        observed = _observed_cards(context, semantic, reader=True)
        unknown_origin = (observed is not None and any(
            ("entry_kind" in row and row.get("entry_kind") != "conditional")
            or ("conditional_binding" in row and row.get("entry_kind") != "conditional")
            for row in context["observation"]["cards"]))
        if observed is None or unknown_origin:
            # No observation or an unknown origin is not evidence that every primary hit was direct.
            ordered = semantic + episodic
            return ordered if limit is None else ordered[:limit]
        rows = {row["node_id"]: row for row in observed["cards"]}
        non_direct = {identifier for identifier, row in rows.items()
                      if row.get("graph_path") or row.get("entry_kind") == "conditional"}
        non_direct.update(c["id"] for c in semantic if "touchstone" in c
                          and {"kind": "direct"} not in c["touchstone"]["origins"])
        direct = [c for c in semantic if c["id"] in rows and c["id"] not in non_direct]
        other = [c for c in semantic if c["id"] in non_direct or c["id"] not in rows]
        # Unknown origins never gain the direct reservation.
        ordered = direct[:1]
        groups = (other, direct[1:], episodic)
        ordered += [card for offset in range(max((len(g) for g in groups), default=0))
                    for group in groups for card in group[offset:offset + 1]]
        return ordered if limit is None else ordered[:limit]
    if semantic and episodic:
        return [semantic[0], episodic[0]]
    return (semantic or episodic)[:MAX_CARDS]


def _card(node, identifier, candidate=None):
    if not isinstance(node, dict) or node.get("id") != identifier:
        raise ValueError("get identity mismatch")
    status = node.get("status")
    if status == "archived":
        return None
    if status != "active":
        raise ValueError("unexpected node status")
    summary, provenance = node.get("summary"), node.get("provenance")
    if (not isinstance(summary, str) or not summary
            or node.get("summary_truncated") is not False
            or not isinstance(provenance, dict)):
        raise ValueError("incomplete get readback")
    source = _source(provenance)
    if not source:
        raise ValueError("missing node source")
    memory_kind = node.get("memory_kind", {"kind": "semantic"})
    if not isinstance(memory_kind, dict) or memory_kind.get("kind") not in ("semantic", "episode"):
        raise ValueError("unexpected memory kind")
    kind = memory_kind["kind"]
    if candidate is not None and kind != candidate["kind"]:
        raise ValueError("recall/get memory kind mismatch")
    facets = {"kind": kind}
    if candidate is not None and "touchstone" in candidate:
        if kind != "semantic" or not isinstance(node.get("touchstone"), dict):
            raise ValueError("touchstone readback mismatch")
        view = validate_touchstone_view(candidate["touchstone"])
        if node["touchstone"].get("subject") != view["subject"]:
            raise ValueError("touchstone subject readback mismatch")
        record = node["touchstone"]
        refs = record.get("references")
        if record.get("owner") != identifier or not isinstance(refs, list):
            raise ValueError("touchstone owner readback mismatch")
        by_id = {(ref.get("db_id"), ref.get("id")): ref for ref in refs if isinstance(ref, dict)}
        if len(by_id) != len(refs) or len(view["references"]) + view["references_omitted"] != len(refs):
            raise ValueError("touchstone reference readback mismatch")
        for ref in view["references"]:
            historical = by_id.get((ref["db_id"], ref["id"]))
            summary = historical.get("summary") if historical is not None else None
            if (not isinstance(summary, str) or not summary.startswith(ref["summary"]["text"])
                    or len(summary.encode()) != ref["summary"]["source_bytes"]):
                raise ValueError("touchstone historical summary readback mismatch")
        facets["touchstone"] = view
    elif "touchstone" in node:
        # Never demote an authored annotation to ordinary advice when its typed
        # view was omitted. Overlap acquisition separately keeps the owner flag.
        if candidate is not None:
            raise ValueError("touchstone view missing")
    if kind == "episode":
        episode = memory_kind.get("episode")
        if candidate is None or not isinstance(episode, dict):
            raise ValueError("incomplete episode readback")
        episode_fields = validate_episode_projection_fields(candidate)
        # Exact edition readback must agree with the native recall header. Do
        # not reconstruct episode rules or silently present it as semantic.
        for key in ("episode_id", "revision", "occurred", "recorded_at", "thread"):
            if key not in episode or _wire_value(episode[key]) != _wire_value(candidate[key]):
                raise ValueError("episode readback mismatch")
        if type(node.get("created")) is not int or node["created"] != candidate["edition_recorded_at"]:
            raise ValueError("episode edition timestamp mismatch")
        provenance_source = provenance.get("source")
        recording_session = (provenance_source.get("session")
                             if isinstance(provenance_source, dict) else None)
        if validate_recording_session(candidate["recording_session"]) != recording_session:
            raise ValueError("episode recording session readback mismatch")
        facets.update(episode_fields)
        for key in EPISODE_OPTIONAL_FIELDS:
            if (key in episode) != (key in candidate) or episode.get(key) != candidate.get(key):
                raise ValueError("episode occurrence context readback mismatch")
            if key in candidate:
                facets[key] = candidate[key]
    identity = {"id": identifier, "summary": summary,
                "provenance": provenance, **facets}
    if "touchstone" in node:
        # Current-resolution and navigation observations are query-dependent;
        # immutable GET content and authored snapshots alone bind this identity.
        identity.pop("touchstone", None)
        identity["touchstone"] = node["touchstone"]
    if kind == "episode":
        # This hashes the immutable account, not its query-dependent rendered
        # view. Generic exact GET does not refresh the editorial head. Origins
        # and current_edition_id remain recall-time observations only; neither
        # can manufacture a new account identity after links/head movement.
        identity = {"id": identifier, "summary": summary, "provenance": provenance,
                    "memory_kind": memory_kind, "created": node["created"]}
    fingerprint = hashlib.sha256(json.dumps(identity, sort_keys=True,
                                              separators=(",", ":"), ensure_ascii=False)
                                 .encode("utf-8")).hexdigest()
    return {"id": identifier, "summary": _short(summary, MAX_SUMMARY_BYTES),
            "status": status, "source": source, "fingerprint": fingerprint, **facets}


def _preference_node(node):
    """A tag match alone cannot expose other private user-store material."""
    provenance = node.get("provenance") if isinstance(node, dict) else None
    source = provenance.get("source") if isinstance(provenance, dict) else None
    return (isinstance(node, dict) and node.get("memory_kind", {"kind": "semantic"}) == {"kind": "semantic"}
            and "touchstone" not in node
            and isinstance(node.get("tags"), list) and GLOBAL_PREFERENCE_TAG in node["tags"]
            and isinstance(provenance, dict) and provenance.get("type") == "external"
            and isinstance(source, dict) and source.get("namespace") == GLOBAL_PREFERENCE_NAMESPACE)


def _observed_cards(context, delivered, *, reader=False, include_routing=True, include_entry=True):
    """Copy bounded native provenance only for cards the adapter actually kept."""
    observation = context.get("observation")
    if not isinstance(observation, dict) or observation.get("schema") != 1 or observation.get("learning") != "disabled":
        return None
    rows = observation.get("cards")
    if not isinstance(rows, list) or _bytes(observation) > MAX_NATIVE_RESPONSE_BYTES:
        return None
    by_id = {}
    navigation_only = {card["id"] for card in delivered if "touchstone" in card
                       and {"kind": "direct"} not in card["touchstone"]["origins"]}
    for row in rows:
        if not isinstance(row, dict):
            return None
        identifier, digest, lane = row.get("node_id"), row.get("card_sha256"), row.get("lane")
        if (not isinstance(identifier, str) or len(identifier) != NODE_ID_LEN
                or not isinstance(digest, str) or len(digest) != 64
                or any(c not in "0123456789abcdef" for c in digest)
                or lane not in ("primary", "expansions", "episodic", "episodes")
                or identifier in by_id):
            return None
        conditional = row.get("entry_kind") == "conditional" and identifier not in navigation_only
        unknown_origin = (("entry_kind" in row and not conditional)
                          or ("conditional_binding" in row and not conditional)
                          or identifier in navigation_only)
        path = None if conditional or unknown_origin else row.get("graph_path")
        if path is not None:
            if (not isinstance(path, list) or len(path) > 4
                    or any(not isinstance(hop, dict) or set(hop) !=
                           {"previous", "target", "from", "to", "kind", "anchor"} for hop in path)
                    or len(json.dumps(path, ensure_ascii=False).encode()) > 1200):
                return None
        by_id[identifier] = {"node_id": identifier, "card_sha256": digest,
                             "lane": lane, "graph_path": path}
        if conditional and include_entry:
            by_id[identifier]["entry_kind"] = "conditional"
            if (include_routing and row.get("graph_path") in (None, [])
                    and row.get("routing_binding") is None):
                try:
                    from routing_memory import validate_conditional_binding
                    by_id[identifier]["conditional_binding"] = validate_conditional_binding(
                        row.get("conditional_binding"), identifier, expected_db_id=context.get("db_id"))
                except (ValueError, TypeError, KeyError, UnicodeError, ImportError):
                    pass
        if include_routing and not conditional and not unknown_origin and path and row.get("routing_binding") is not None:
            try:
                from routing_memory import validate_observed_binding
                binding = validate_observed_binding(row["routing_binding"], path)
                if binding["route"]["target"] == identifier:
                    by_id[identifier]["routing_binding"] = binding
            except (ValueError, TypeError, KeyError, UnicodeError, ImportError):
                pass  # Native recall is still useful without optional learning metadata.
    return {"schema": 1, "learning": "disabled",
            "cards": [by_id[c["id"]] for c in delivered if c["id"] in by_id]}


def _bytes(value):
    return len(json.dumps(value, ensure_ascii=False, separators=(",", ":")).encode("utf-8"))


# Narrow wire projection of mneme-present::ContextEnvelope and core::tagged.
# Native owns policy validation; these allowlists admit data, not a second
# inference about completeness or consistency. No stamp, receipt or text enters.
_OMISSION_LANES = ("core", "primary", "expansion", "episodic")
_TAGGED_WORK_FIELDS = ("query_dimension", "raw_memberships", "unique_exact_ids",
                       "exact_hydrated_ids", "exact_vector_components", "fallback_hnsw_inspected",
                       "fallback_sample_inspected", "fallback_unique_ids",
                       "fallback_canonical_candidates_checked", "fallback_matching_candidates",
                       "fallback_hydrated_ids", "fallback_vector_components")
_ADAPTER_COUNTS = ("observed_unique_window", "readbacks_attempted", "work_budget_unread",
                   "readback_skipped", "byte_budget_omitted", "graph_readback_omitted",
                   "graph_byte_omitted", "returned")
_UNKNOWN_REASONS = ("absent", "malformed", "byte_cap", "diagnostic_byte_cap", "state_byte_cap")
_REFERENCE_COUNTS = ("anchors_total", "anchors_examined", "raw_edges_scanned", "raw_edge_limit",
                     "endpoint_reads", "endpoint_read_limit", "indexed_seeks", "edge_point_reads",
                     "body_anchor_point_reads", "missing", "non_episode", "cache_hits", "unread_anchors")


def _number(value, *, signed=False):
    if type(value) is not int or not (-(1 << 63) if signed else 0) <= value <= ((1 << 63) - 1 if signed else (1 << 64) - 1):
        raise ValueError("invalid coverage number")
    return value


def _flag(value):
    if type(value) is not bool:
        raise ValueError("invalid coverage flag")
    return value


def _seed_coverage(value):
    if value is None:
        return None
    strategy = value["strategy"]
    if strategy == "exact_cosine":
        return {"strategy": strategy}
    if strategy not in ("lifecycle_hnsw_and_hashed_tag_sample_postfilter",
                         "deterministic_hashed_tag_sample_postfilter"):
        raise ValueError("unknown coverage strategy")
    exceeded = value["exceeded_limit"]
    if exceeded not in ("raw_memberships", "unique_exact_ids", "exact_vector_components"):
        raise ValueError("unknown exact-work limit")
    physical = value["physical"]
    if not isinstance(physical, list) or not 1 <= len(physical) <= 3:
        raise ValueError("invalid physical coverage")
    rows = []
    for row in physical:
        status = row["status"]
        samples = row["tag_samples"]
        if status not in ("active", "archived") or not isinstance(samples, list) or not 1 <= len(samples) <= 32:
            raise ValueError("invalid physical coverage")
        rows.append({"status": status, "hnsw_quota": _number(row["hnsw_quota"]),
                     "hnsw_inspected": _number(row["hnsw_inspected"]),
                     "sample_pivot": _number(row["sample_pivot"], signed=True),
                     "tag_samples": [{key: _number(sample[key])
                                      for key in ("query_tag_index", "quota", "inspected")}
                                     for sample in samples]})
    return {"strategy": strategy, "exceeded_limit": exceeded, "physical": rows,
            **{key: _number(value[key]) for key in ("raw_memberships", "canonical_candidates_checked",
                                                   "matching_candidates")}}


def _native_discovery(context):
    """Project one native fragment, also reused to sanitize private receipts."""
    retrieval = context["retrieval"]
    mode = retrieval["mode"]
    if mode not in ("untagged", "tagged"):
        raise ValueError("unknown retrieval mode")
    work = retrieval["work"]
    episodic = context["episodic_retrieval"]
    state = episodic["state"]
    if state not in ("searched", "not_searched", "not_searched_tag_filter", "unavailable") or episodic["mode"] != "lexical":
        raise ValueError("unknown episodic coverage")
    episode = {"state": state, "mode": "lexical", "cue_normalized": _flag(episodic["cue_normalized"]),
               "cue_truncated": _flag(episodic["cue_truncated"])}
    if "unavailable_reason" in episodic:
        reason = episodic["unavailable_reason"]
        if reason not in ("adapter_unsupported", "store_not_upgraded"):
            raise ValueError("unknown episodic unavailable reason")
        episode["unavailable_reason"] = reason
    references = context["episode_reference_retrieval"]
    if not isinstance(references, dict) or references.get("state") not in ("searched", "not_searched"):
        raise ValueError("unknown episode reference coverage")
    reference_coverage = {"state": references["state"],
                          "further_tail_unknown": _flag(references["further_tail_unknown"])}
    for key in _REFERENCE_COUNTS:
        count = _number(references[key])
        if count > (1 << 32) - 1:
            raise ValueError("invalid reference work count")
        reference_coverage[key] = count
    if "stop_reason" in references:
        reason = references["stop_reason"]
        if reason not in ("budget", "deadline", "unsupported", "read_error"):
            raise ValueError("unknown reference stop reason")
        reference_coverage["stop_reason"] = reason
    omitted = {}
    for lane in _OMISSION_LANES:
        row = context["omitted"][lane]
        count = _number(row["bounded_window_budget"])
        if count > (1 << 32) - 1:
            raise ValueError("invalid omission count")
        omitted[lane] = {"bounded_window_budget": count,
                         "further_tail_unknown": _flag(row["further_tail_unknown"])}
    result = {"state": "observed", "partial": _flag(context["partial"]), "omitted": omitted,
            "retrieval": {"mode": mode, "partial": _flag(retrieval["partial"]),
                          "work": None if work is None else {key: _number(work[key]) for key in _TAGGED_WORK_FIELDS},
                          "lanes": {"primary": {"seed_coverage": _seed_coverage(retrieval["lanes"]["primary"]["seed_coverage"])}}},
            "episodic_retrieval": episode, "episode_reference_retrieval": reference_coverage}
    if context.get("schema") == "mneme.context.v7" or "touchstone_retrieval" in context:
        result["touchstone_retrieval"] = validate_touchstone_retrieval(context.get("touchstone_retrieval"))
    return result


def _unknown_discovery(reason="absent", adapter=None):
    return {"scope": "bounded_native_window", "continuation": "unavailable",
            "native": {"state": "unknown", "reason": reason}, "adapter": adapter}


def _bounded_discovery(value):
    """Copy only source-defined fields; metadata can never displace cards."""
    try:
        if not isinstance(value, dict):
            return _unknown_discovery()
        if value.get("scope") != "bounded_native_window" or value.get("continuation") != "unavailable":
            return _unknown_discovery("malformed")
        adapter = value.get("adapter")
        if adapter is not None:
            adapter = {key: _number(adapter[key]) for key in _ADAPTER_COUNTS}
            if any(count > MAX_NATIVE_RESPONSE_BYTES for count in adapter.values()):
                raise ValueError("invalid adapter counts")
        native = value["native"]
        if native.get("state") == "unknown":
            reason = native["reason"]
            if reason not in _UNKNOWN_REASONS:
                raise ValueError("unknown omission reason")
            return _unknown_discovery(reason, adapter)
        if native.get("state") != "observed":
            raise ValueError("invalid native coverage state")
        result = {"scope": "bounded_native_window", "continuation": "unavailable",
                  "native": _native_discovery(native), "adapter": adapter}
        # Include the field wrapper itself in the private control allowance.
        if _bytes({"discovery": result}) > MAX_READER_CONTROL_BYTES:
            return _unknown_discovery("byte_cap", adapter)
        return result
    except (ValueError, TypeError, KeyError, AttributeError, UnicodeError, RecursionError):
        return _unknown_discovery("malformed")


def _discovery(context, adapter):
    try:
        native = _native_discovery(context)
    except KeyError:
        native = {"state": "unknown", "reason": "absent"}
    except (ValueError, TypeError, AttributeError, UnicodeError, RecursionError):
        native = {"state": "unknown", "reason": "malformed"}
    return _bounded_discovery({"scope": "bounded_native_window", "continuation": "unavailable",
                               "native": native, "adapter": adapter})


def _reader_card(card):
    """Fit only the display summary; never truncate typed identity or source."""
    while _bytes(card) > MAX_READER_CARD_BYTES:
        excess = _bytes(card) - MAX_READER_CARD_BYTES
        shorter = max(0, len(card["summary"].encode("utf-8")) - excess)
        card["summary"] = _short(card["summary"], shorter)
        if not card["summary"]:
            return None
    return card


@contextmanager
def _cleanup_deadline(client, deadline):
    """Do not let the last read's old allowance extend native-client cleanup."""
    try:
        yield
    finally:
        if deadline is not None:
            client.timeout = max(.001, min(deadline - time.monotonic(), 1.5))


def _collect(service_config_path, prompt, project_root, timeout, observe=False, *, reader=False,
             routing_hints=None, budget=None, read_plan=None, store_target=None, shared_work=None):
    budget = LibrarianBudget() if budget is None else budget
    if reader:
        if read_plan is None:
            read_plan, reason = reader_plan([{"role": "user", "text": prompt}], budget=budget)
            if read_plan is None:
                return _result("skipped")
        if read_plan["max_nodes"] == 0:
            return _result("empty")
    deadline = (shared_work["deadline"] if shared_work is not None else
                time.monotonic() + timeout if reader else None)
    decoded_bytes = shared_work["decoded_bytes"] if shared_work is not None else 0
    preference = getattr(store_target, "scope", None) == "global_preference"
    if preference and (not reader or prompt != GLOBAL_PREFERENCE_CUE or routing_hints is not None):
        raise ValueError("invalid global preference read")
    root = Path(project_root)
    target_snapshot = _config_snapshot(service_config_path) if store_target is not None else None
    target_policy, config = policy_for(service_config_path, root, store_target)
    if store_target is not None and _config_snapshot(service_config_path) != target_snapshot:
        raise _IdentityMismatch("service configuration changed")
    # A connect-only path identifies the owner's store; it is not a local file.
    # The exact single-project catalog below binds retrieval before any recall.
    token = None
    if config.token_env:
        import os
        token = os.environ.get(config.token_env)
        if not token:
            raise ValueError("configured token is unavailable")
    startup_seconds = min(timeout, 1.5) if deadline is None else deadline - time.monotonic()
    if startup_seconds <= 0:
        raise TimeoutError("reader deadline exceeded")
    # Native transport pins its ceiling on connect: background reads must start
    # with the whole remaining allowance, not a foreground per-RPC ceiling.
    with McpClient(config.url, token=token, timeout=startup_seconds) as client, _cleanup_deadline(client, deadline):
        def read(name, args):
            nonlocal decoded_bytes
            if reader and decoded_bytes >= budget.native_read_bytes:
                raise TimeoutError("reader decoded-read allowance exhausted")
            if deadline is not None:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise TimeoutError("reader deadline exceeded")
                client.timeout = remaining
            value = client.call_tool(name, args)
            if reader:
                decoded_bytes += _bytes(value)
                if shared_work is not None:
                    shared_work["decoded_bytes"] = decoded_bytes
            if deadline is not None and name not in ("get",) and time.monotonic() >= deadline:
                raise TimeoutError("reader deadline exceeded")
            return value

        catalog = read("databases", {})
        if not _expected_catalog(catalog, config.database_path, config.database_name):
            raise ValueError("unexpected project catalog")
        # Older catalogs can still supply recall, but only a canonical native
        # identity permits a delivery binding. Never retry a failed guard with
        # an unguarded get.
        db_id = catalog[0].get("db_id") if reader else None
        if not isinstance(db_id, str) or not NODE_ID_RE.fullmatch(db_id):
            db_id = None
        if store_target is not None:
            db_id = target_policy.catalog_identity(catalog)
        request = {"db": config.database_name, "text": prompt,
                   "k": read_plan["k"] if reader else 2,
                   "max_nodes": read_plan["max_nodes"] if reader else 4,
                   "depth": 2 if reader or observe else 0}
        if reader or observe:
            request["observe"] = True
        if preference:
            request.update(depth=0, tags=[GLOBAL_PREFERENCE_TAG])
        if routing_hints is not None:
            # Optional suggestions never widen an ordinary read into a write.
            # A malformed optional batch becomes the empty sampling opt-in.
            request["routing_hints"] = []
            try:
                from routing_memory import normalize_hints
                request["routing_hints"] = normalize_hints(routing_hints)
            except (ImportError, ValueError, TypeError, KeyError, UnicodeError):
                pass
        context = read("recall_context", request)
        have_observation = _observed_cards(context, [], reader=True) is not None
        if reader and not have_observation and routing_hints is None:
            raise ValueError("missing or malformed native observation")
        cards = []
        concern_endpoints = {}
        include_routing = True
        include_entry = True
        candidates = _candidates(context, reader=reader)
        if preference:
            candidates = [c for c in candidates if c["kind"] == "semantic" and "touchstone" not in c]
        if reader:
            adapter = dict.fromkeys(_ADAPTER_COUNTS, 0)
            adapter["observed_unique_window"] = len(candidates)
            graph_ids = {row["node_id"] for row in context.get("observation", {}).get("cards", [])
                         if row.get("graph_path") and "entry_kind" not in row
                         and "conditional_binding" not in row} if have_observation else set()
        for index, candidate in enumerate(candidates):
            identifier = candidate["id"]
            if reader and (decoded_bytes >= budget.native_read_bytes or time.monotonic() >= deadline):
                adapter["work_budget_unread"] += len(candidates) - index
                adapter["graph_readback_omitted"] += sum(c["id"] in graph_ids for c in candidates[index:])
                break
            if reader:
                adapter["readbacks_attempted"] += 1
            node = read("get", {"db": config.database_name, "id": identifier,
                                "body": False, "edges": False,
                                **({"expected_db_id": db_id} if db_id is not None else {})})
            if preference and not _preference_node(node):
                adapter["readback_skipped"] += 1
                continue
            card = _card(node, identifier, candidate)
            if card is None:
                if reader:
                    adapter["readback_skipped"] += 1
                    adapter["graph_readback_omitted"] += identifier in graph_ids
                continue
            if "touchstone" in card:
                validate_touchstone_view(card["touchstone"], expected_db_id=db_id)
            if preference:
                card.update(db_id=db_id, scope="global_preference")
            if reader:
                endpoint = None if preference else node.get("concern_endpoint")
                if (isinstance(endpoint, dict) and set(endpoint) == {"id", "meaning"}
                        and endpoint["id"] == identifier and isinstance(endpoint["meaning"], str)
                        and re.fullmatch(r"[0-9a-f]{64}", endpoint["meaning"])):
                    concern_endpoints[identifier] = dict(endpoint)
                card = _reader_card(card)
                if card is None:
                    adapter["byte_budget_omitted"] += 1
                    adapter["graph_byte_omitted"] += identifier in graph_ids
                    continue
                proposed = cards + [card]
                native = (_observed_cards(context, proposed, reader=True, include_routing=include_routing,
                                          include_entry=include_entry)
                          if have_observation else None)
                if have_observation and (native is None or len(native["cards"]) != len(proposed)):
                    raise ValueError("native observation/card mismatch")
                # Reserve a few bytes for the later measured elapsed_ms value.
                proposed_result = _result("ok", proposed, observation=native)
                if db_id is not None:
                    proposed_result["db_id"] = db_id
                if (_bytes(proposed_result) > MAX_READER_BYTES - 16 and include_routing and native is not None
                        and any("routing_binding" in row or "conditional_binding" in row for row in native["cards"])):
                    # Optional training metadata must lose the byte-budget contest
                    # before a card that ordinary recall could have returned.
                    stripped = _observed_cards(context, proposed, reader=True, include_routing=False,
                                               include_entry=include_entry)
                    proposed_result["observation"] = stripped
                    if _bytes(proposed_result) <= MAX_READER_BYTES - 16:
                        include_routing = False
                if (_bytes(proposed_result) > MAX_READER_BYTES - 16 and include_entry and native is not None
                        and any("entry_kind" in row for row in native["cards"])):
                    proposed_result["observation"] = _observed_cards(
                        context, proposed, reader=True, include_routing=False, include_entry=False)
                    if _bytes(proposed_result) <= MAX_READER_BYTES - 16:
                        include_routing = include_entry = False
                if _bytes(proposed_result) <= MAX_READER_BYTES - 16:
                    cards = proposed
                else:
                    adapter["byte_budget_omitted"] += 1
                    adapter["graph_byte_omitted"] += identifier in graph_ids
                continue
            # Episode identity/time fields are never truncated. If their extra
            # bytes squeeze the second card, shorten its displayed summary,
            # not the typed facet or the full-content fingerprint.
            while True:
                proposed = cards + [card]
                size = len(json.dumps(proposed, ensure_ascii=False, separators=(",", ":")).encode("utf-8"))
                if size <= MAX_CARDS_BYTES:
                    cards = proposed
                    break
                shorter = max(0, len(card["summary"].encode("utf-8")) - (size - MAX_CARDS_BYTES))
                card["summary"] = _short(card["summary"], shorter)
                if not card["summary"]:
                    break
            if len(cards) == MAX_CARDS:
                break
        native = (_observed_cards(context, cards, reader=reader, include_routing=include_routing,
                                  include_entry=include_entry)
                  if (reader or observe) and have_observation else None)
        result = _result("ok" if cards else "empty", cards, observation=native)
        if reader and db_id is not None:
            result["db_id"] = db_id
        if reader:
            adapter["returned"] = len(cards)
            result["discovery"] = _discovery(context, adapter)
            # The 2KiB reserve protects baseline content, not a separate relevance
            # ceiling. Private controls may borrow unused content headroom, never
            # displace baseline cards, and still fit the exact 10KiB result fuse.
            control = {"discovery": result["discovery"], "concern_endpoints": {},
                       "concern_rows": [], "concern_lookup": "unknown"}
            def fits_control(value):
                # Reserve elapsed_ms growth; charge wrappers and escaping exactly.
                return _bytes({**result, **value}) <= MAX_READER_RESULT_BYTES - 16
            for card in cards:
                endpoint = concern_endpoints.get(card["id"])
                if endpoint is not None:
                    trial = {**control, "concern_endpoints": {**control["concern_endpoints"], card["id"]: endpoint}}
                    if fits_control(trial):
                        control = trial
            if not preference and db_id is not None and hasattr(client, "concern_checked"):
                from turn_observer import validate_concern_row
                seen_cases = set()
                # Raw page work and retained unique rows are different resources:
                # duplicate endpoint pages still cost work, not duplicate storage.
                lookup_bytes = 0
                lookup_allowance = max(0, MAX_READER_RESULT_BYTES
                                       - _bytes({**result, **control}) - 16)
                try:
                    complete = True
                    for endpoint in control["concern_endpoints"]:
                        after = None
                        while True:
                            remaining = deadline - time.monotonic()
                            if remaining <= .05 or decoded_bytes >= budget.native_read_bytes:
                                complete = False
                                break
                            client.timeout = min(remaining / 2, .5)
                            response = client.concern_checked(config.database_name, {
                                "action": "list", "endpoint": endpoint,
                                **({"after": after} if after is not None else {})}, expected_db_id=db_id)
                            decoded_bytes += _bytes(response)
                            if shared_work is not None:
                                shared_work["decoded_bytes"] = decoded_bytes
                            if response.get("db") != config.database_name or response.get("db_id") != db_id or response.get("action") != "list":
                                raise ValueError("concern owner mismatch")
                            page = response["page"]
                            lookup_bytes += _bytes(page)
                            if lookup_bytes > lookup_allowance:
                                complete = False
                                break
                            for raw in page["items"]:
                                row = validate_concern_row(raw)
                                key = row["notice"]["binding"]["key"]
                                token = (key["kind"], key["lo"], key["hi"])
                                if token not in seen_cases and {key["lo"], key["hi"]} <= set(control["concern_endpoints"]):
                                    trial = {**control, "concern_rows": control["concern_rows"] + [row]}
                                    if not fits_control(trial):
                                        complete = False
                                        break
                                    control = trial
                                    seen_cases.add(token)
                            next_cursor = page["next"]
                            if not complete or next_cursor is None:
                                break
                            if next_cursor == after:
                                raise ValueError("concern cursor stalled")
                            after = next_cursor
                        if not complete:
                            break
                    if complete and len(control["concern_endpoints"]) == len(cards):
                        trial = {**control, "concern_lookup": "bounded_pages_complete"}
                        if fits_control(trial):
                            control = trial
                except (OSError, ValueError, TypeError, KeyError, McpError, TimeoutError):
                    pass  # Optional lookup cannot erase successful ordinary readback.
            result.update(control)
            work = {"decoded_bytes": decoded_bytes, "read_allowance_bytes": budget.native_read_bytes,
                    "read_allowance_exhausted": decoded_bytes >= budget.native_read_bytes}
            if _bytes({**result, "native_work": work}) <= MAX_READER_RESULT_BYTES - 16:
                result["native_work"] = work
        if store_target is not None:
            target_policy.catalog_identity(read("databases", {}), db_id)
            if _config_snapshot(service_config_path) != target_snapshot:
                raise _IdentityMismatch("service configuration changed")
        return result


def collect_reader(service_config_path: Path, cue: str, project_root: Path,
                   timeout: float | None = None, *, routing_hints=None, budget=None, read_plan=None,
                   store_target=None, shared_work=None) -> dict:
    """Read one effort-sized window in one whole-call deadline.

    This is for an already backgrounded integration worker, not the synchronous
    prompt hook. It uses the same native client and readback checks as `_collect`.
    An explicit optional preference read may share the worker-owned work ledger;
    failures stay charged and never restart another store's deadline or allowance.
    """
    started = time.monotonic()
    budget = LibrarianBudget() if budget is None else budget
    timeout = budget.native_seconds if timeout is None else timeout
    if (not isinstance(cue, str) or not cue.strip()
            or len(cue.encode("utf-8")) > MAX_PROMPT_BYTES
            or not isinstance(service_config_path, Path)
            or not isinstance(project_root, Path) or not project_root.is_absolute()
            or type(timeout) not in (int, float) or not 0 < timeout <= budget.native_seconds
            or shared_work is not None and (not isinstance(shared_work, dict)
                or set(shared_work) != {"deadline", "decoded_bytes"}
                or type(shared_work["deadline"]) not in (int, float)
                or not 0 < shared_work["deadline"] < float("inf")
                or type(shared_work["decoded_bytes"]) is not int or shared_work["decoded_bytes"] < 0)):
        result = _result("skipped", elapsed_ms=int((time.monotonic() - started) * 1000))
        result["discovery"] = _unknown_discovery()
        return result
    try:
        result = _collect(service_config_path, cue, project_root, timeout, reader=True,
                          budget=budget, read_plan=read_plan,
                          **({"shared_work": shared_work} if shared_work is not None else {}),
                          **({"store_target": store_target} if store_target is not None else {}),
                          **({"routing_hints": routing_hints} if routing_hints is not None else {}))
        # Required reads check the deadline themselves; an optional lookup miss
        # must not retroactively erase successfully collected ordinary cards.
    except TimeoutError:
        result = _result("timeout")
    except (OSError, ValueError, TypeError, KeyError, McpError):
        result = _result("timeout" if time.monotonic() - started >= timeout else "unavailable")
    result["elapsed_ms"] = int((time.monotonic() - started) * 1000)
    if shared_work is not None:
        result["native_work"] = {"decoded_bytes": shared_work["decoded_bytes"],
                                 "read_allowance_bytes": budget.native_read_bytes,
                                 "read_allowance_exhausted": shared_work["decoded_bytes"] >= budget.native_read_bytes}
    result["discovery"] = _bounded_discovery(result.get("discovery"))
    return result


def _config_snapshot(path):
    # Reuse the service parser for meaning; these bytes only fence changes to
    # its explicit configuration file during this background operation.
    with path.open("rb") as stream:
        raw = stream.read(MAX_CONFIG_BYTES + 1)
    if len(raw) > MAX_CONFIG_BYTES:
        raise ValueError("service configuration exceeds bound")
    return path.resolve(strict=True), hashlib.sha256(raw).digest()


def _project_config(path, root, store_target=None):
    snapshot = _config_snapshot(path)
    _, config = policy_for(path, root, store_target)
    if _config_snapshot(path) != snapshot:
        raise _IdentityMismatch("service configuration changed")
    token = None
    if config.token_env:
        import os
        token = os.environ.get(config.token_env)
        if not token:
            raise ValueError("configured token is unavailable")
    return config, snapshot, token


def _project_identity(catalog, config, expected=None, store_target=None):
    if store_target is not None:
        return store_target.catalog_identity(catalog, expected)
    if not _expected_catalog(catalog, config.database_path, config.database_name):
        raise _IdentityMismatch("unexpected project catalog")
    identifier = catalog[0].get("db_id")
    if not isinstance(identifier, str) or not NODE_ID_RE.fullmatch(identifier):
        raise ValueError("project catalog has no canonical database identity")
    if expected is not None and identifier != expected:
        raise _IdentityMismatch("project database identity changed")
    return identifier


def _overlap_candidates(context, limit=None):
    candidates = _candidates(context, reader=True, limit=limit)
    # The historical hook skips wrong-length IDs. Acquisition must not turn
    # malformed discovery into an apparently successful empty overlap pool.
    lanes = ("primary", "expansions", "episodes")
    for lane in lanes:
        for entry in context.get(lane, []):
            if not NODE_ID_RE.fullmatch(entry["id"]):
                raise ValueError("malformed overlap node id")
    return candidates


def _background_project_read(service_config_path, project_root, timeout, *, cue=None,
                             expected_db_id=None, include_routing=False, budget=None, read_plan=None,
                             spent_native_bytes=0, store_target=None):
    started = time.monotonic()
    result = _result("unavailable")
    decoded_bytes = 0
    adapter = dict.fromkeys(_ADAPTER_COUNTS, 0)
    context = None
    policy = LibrarianBudget() if budget is None else budget
    try:
        budget = policy
        if (type(spent_native_bytes) is not int
                or not 0 <= spent_native_bytes <= budget.native_read_bytes):
            raise ValueError("invalid prior native read charge")
        decoded_bytes = spent_native_bytes
        seconds_limit = 2 if cue is None else budget.native_seconds
        if (not isinstance(service_config_path, Path)
                or not isinstance(project_root, Path) or not project_root.is_absolute()
                or type(timeout) not in (int, float) or not 0 < timeout <= seconds_limit):
            raise ValueError("invalid background project request")
        if cue is not None and (not isinstance(cue, str) or not cue.strip()
                or len(cue.encode("utf-8")) > MAX_PROMPT_BYTES
                or not isinstance(expected_db_id, str)
                or not NODE_ID_RE.fullmatch(expected_db_id)):
            raise ValueError("invalid overlap request")
        deadline = started + timeout
        # Prospective guard reserve, not a promise about backend response sizes
        # or scheduling. Every returned object is charged, including final guard.
        hydration_deadline = deadline - min(.2, timeout / 10)

        def remaining():
            budget = deadline - time.monotonic()
            if budget <= 0:
                raise TimeoutError("background project deadline exceeded")
            return budget

        config, snapshot, token = _project_config(service_config_path, project_root, store_target)
        with McpClient(config.url, token=token, timeout=remaining()) as client:
            def read(name, arguments, *, hydration=False):
                nonlocal decoded_bytes
                seconds = (hydration_deadline if hydration else deadline) - time.monotonic()
                if seconds <= 0:
                    raise TimeoutError("background project deadline exceeded")
                client.timeout = seconds
                value = client.call_tool(name, arguments)
                decoded_bytes += _bytes(value)
                remaining()
                return value

            try:
                catalog = read("databases", {})
                identifier = _project_identity(catalog, config, expected_db_id, store_target)
                catalog_bytes = _bytes(catalog)
                cards = []
                routing_candidates = set()
                if cue is not None:
                    if read_plan is None:
                        read_plan = budget.overlap_window(budget.recording_hint_bytes)
                    if (set(read_plan) != {"k", "max_nodes", "depth"}
                            or type(read_plan["max_nodes"]) is not int
                            or not 0 <= read_plan["max_nodes"] <= 256
                            or type(read_plan["k"]) is not int
                            or read_plan["k"] != min(64, read_plan["max_nodes"])
                            or type(read_plan["depth"]) is not int or read_plan["depth"] != 0):
                        raise ValueError("invalid overlap plan")
                    if decoded_bytes + catalog_bytes >= budget.native_read_bytes:
                        raise ValueError("no overlap discovery/final-guard room")
                    context = None
                    candidates = []
                    if read_plan["max_nodes"]:
                        context = read("recall_context", {"db": config.database_name, "text": cue,
                                       **read_plan}, hydration=True)
                        candidates = _overlap_candidates(context)
                        adapter["observed_unique_window"] = len(candidates)
                    # Minimum next get room and a final catalog sample. Actual
                    # readbacks can exceed the allowance; fail closed if so.
                    get_allowance = _bytes({"id": "0" * 26, "status": "active", "summary": "x",
                        "summary_truncated": False, "provenance": {}})
                    for index, candidate in enumerate(candidates):
                        if (decoded_bytes + get_allowance + catalog_bytes > budget.native_read_bytes
                                or time.monotonic() >= hydration_deadline):
                            adapter["work_budget_unread"] = len(candidates) - index
                            break
                        adapter["readbacks_attempted"] += 1
                        node = read("get", {"db": config.database_name, "id": candidate["id"],
                                    "body": False, "edges": False, "expected_db_id": identifier}, hydration=True)
                        get_allowance = max(get_allowance, _bytes(node))
                        if decoded_bytes + catalog_bytes > budget.native_read_bytes:
                            raise ValueError("no overlap final-guard room")
                        # McpClient owns native guard-support admission. Only
                        # this guarded readback, never discovery text, supplies
                        # model-facing overlap. No content-verification fallback.
                        card = _card(node, candidate["id"], candidate)
                        if card is None:
                            raise ValueError("overlap candidate is no longer current")
                        summary = _short(card["summary"], MAX_OVERLAP_SUMMARY_BYTES)
                        if not summary:
                            raise ValueError("overlap summary is empty")
                        overlap = {"id": card["id"], "summary": summary, "kind": card["kind"]}
                        if "touchstone" in card:
                            overlap["touchstone"] = validate_touchstone_view(
                                card["touchstone"], expected_db_id=identifier)
                        cards.append(overlap)
                        adapter["returned"] += 1
                        if (include_routing and card["kind"] == "semantic"
                                and "routing-judgment" in node.get("tags", [])
                                and node.get("provenance", {}).get("source", {}).get("namespace")
                                    == "codex-routing.v1"):
                            routing_candidates.add(card["id"])
                    if decoded_bytes + catalog_bytes > budget.native_read_bytes:
                        raise ValueError("no overlap final-guard room")
                    _project_identity(read("databases", {}), config, identifier, store_target)
                    if decoded_bytes > policy.native_read_bytes:
                        raise ValueError("overlap decoded-read allowance exceeded")
                    if routing_candidates:
                        # Summaries are already guarded. Optional history shares
                        # the remaining work but cannot erase them on exhaustion.
                        # Response sizes are unknown until decoded: permit one
                        # measured soft-allowance overshoot, then stop enrichment.
                        optional_deadline = min(hydration_deadline, time.monotonic() + min(.5, remaining() / 2))
                        for card in cards:
                            if card["id"] not in routing_candidates:
                                continue
                            optional_seconds = optional_deadline - time.monotonic()
                            try:
                                from routing_memory import decode_witness, MAX_BODY_BYTES
                                if optional_seconds <= 0 or decoded_bytes >= policy.native_read_bytes:
                                    break
                                client.timeout = min(optional_seconds, remaining() / 2)
                                node = client.call_tool("get", {"db": config.database_name, "id": card["id"],
                                                       "body": True, "max_body_bytes": MAX_BODY_BYTES,
                                                       "edges": False, "expected_db_id": identifier})
                                decoded_bytes += _bytes(node)
                                if decoded_bytes > policy.native_read_bytes:
                                    break
                                witness = decode_witness(node, expected_db_id=identifier)
                                if witness is not None:
                                    card["routing_witness"] = witness
                            except McpError as error:
                                if "expected_db_id mismatch" in str(error):
                                    raise _IdentityMismatch("optional routing owner changed") from error
                                break
                            except (ImportError, OSError, ValueError, TypeError, KeyError, TimeoutError):
                                break  # Keep ordinary overlap on optional miss.
                if _config_snapshot(service_config_path) != snapshot:
                    raise _IdentityMismatch("service configuration changed")
                remaining()
                result = _result("ok" if cue is None or cards else "empty", cards)
                result["db_id"] = identifier
            finally:
                # Cleanup uses the remaining phase budget, not the preceding
                # request's stale timeout; native client owns process cleanup.
                client.timeout = max(0.001, min(deadline - time.monotonic(), 1.5))
        remaining()
    except TimeoutError:
        result = _result("timeout")
    except _IdentityMismatch:
        result = _result("identity_mismatch")
    except (ImportError, OSError, ValueError, TypeError, KeyError, McpError) as error:
        if (type(timeout) in (int, float) and timeout > 0
                and time.monotonic() - started >= timeout):
            result = _result("timeout")
        elif isinstance(error, McpError) and "timed out" in str(error):
            result = _result("timeout")
        elif isinstance(error, McpError) and "expected_db_id mismatch" in str(error):
            result = _result("identity_mismatch")
        else:
            result = _result("unavailable")
    result["elapsed_ms"] = int((time.monotonic() - started) * 1000)
    if cue is not None:
        adapter["returned"] = len(result["cards"])
        result["discovery"] = _discovery(context, adapter) if context is not None else _unknown_discovery(adapter=adapter)
        result["native_work"] = {"decoded_bytes": decoded_bytes,
                                 "read_allowance_bytes": policy.native_read_bytes}
    else:
        result["native_work"] = {"decoded_bytes": decoded_bytes}
    return result


def resolve_project_identity(service_config_path: Path, project_root: Path, *, timeout: float = 2,
                             store_target=None) -> dict:
    """Background-only initial identity sample from the native named catalog."""
    return _background_project_read(service_config_path, project_root, timeout,
                                   **({"store_target": store_target} if store_target is not None else {}))


_ROUTING_HYDRATION_COUNTS = ("observed_unique_window", "readbacks_attempted", "rejected",
                             "returned", "work_budget_unread")


def _bounded_routing_discovery(value):
    """Content-free routing work receipt; a bounded window is never corpus coverage."""
    try:
        if not isinstance(value, dict):
            raise ValueError("missing routing discovery")
        hydration = {key: _number(value["hydration"][key]) for key in _ROUTING_HYDRATION_COUNTS}
        work = value["native_work"]
        result = {"discovery": _bounded_discovery(value.get("discovery")), "hydration": hydration,
                  "native_work": {"decoded_bytes": _number(work["decoded_bytes"]),
                                  "read_allowance_bytes": _number(work["read_allowance_bytes"]),
                                  "read_allowance_exhausted": _flag(work["read_allowance_exhausted"])}}
        if _bytes(result) > MAX_READER_CONTROL_BYTES:
            raise ValueError("routing diagnostic overflow")
        return result
    except (ValueError, TypeError, KeyError, AttributeError, UnicodeError, RecursionError):
        return {"discovery": _unknown_discovery(), "hydration": None, "native_work": None}


def collect_routing_witnesses(service_config_path: Path, cue: str, project_root: Path, *,
                              current, budget, discovery_plan=None, timeout=None, store_target=None) -> dict:
    """One effort-budgeted tagged depth-zero window with complete guarded bodies.

    Resource pressure may return a verified prefix, with unread history unknown.
    Final catalog/config failure discards that prefix. No body pagination, global
    fallback, extra retrieval or relationship between witness and hint counts.
    """
    from routing_memory import MAX_BODY_BYTES, TAG, decode_witness
    from routing_contract import discovery_plan as plan_routing
    started = time.monotonic()
    counts = {key: 0 for key in _ROUTING_HYDRATION_COUNTS}
    decoded_bytes, exhausted = 0, False
    native_discovery = _unknown_discovery()
    result = {"outcome": "unavailable", "witnesses": []}
    try:
        need_plan, reason = plan_routing(current, budget=budget)
        if need_plan is None:
            return {**result, "outcome": "skipped", "reason": reason,
                    "routing_discovery": _bounded_routing_discovery(None)}
        if discovery_plan is None:
            discovery_plan = need_plan
        if discovery_plan != need_plan:
            raise ValueError("routing discovery plan mismatch")
        seconds = budget.native_seconds if timeout is None else timeout
        if (not isinstance(service_config_path, Path) or not isinstance(project_root, Path)
                or not project_root.is_absolute() or not isinstance(cue, str) or not cue.strip()
                or len(cue.encode("utf-8")) > MAX_PROMPT_BYTES
                or type(seconds) not in (int, float) or not 0 < seconds <= budget.native_seconds):
            raise ValueError("invalid routing discovery request")
        if discovery_plan["max_nodes"] == 0:
            return {**result, "outcome": "empty", "routing_discovery": _bounded_routing_discovery(None)}
        deadline = started + seconds
        # Reserve the final identity/config sample before optional hydration.
        # These are prospective room checks, not wire-size or scheduling promises.
        final_seconds = min(.2, seconds / 10)
        hydration_deadline = deadline - final_seconds
        config, snapshot, token = _project_config(service_config_path, project_root, store_target)
        startup_seconds = deadline - time.monotonic()
        if startup_seconds <= 0:
            raise TimeoutError("routing phase deadline exceeded")
        with McpClient(config.url, token=token, timeout=startup_seconds) as client:
            def read(name, arguments, *, hydration=False):
                nonlocal decoded_bytes
                remaining = (hydration_deadline if hydration else deadline) - time.monotonic()
                if remaining <= 0:
                    raise TimeoutError("routing phase deadline exceeded")
                client.timeout = remaining
                value = client.call_tool(name, arguments)
                decoded_bytes += _bytes(value)
                return value
            try:
                catalog = read("databases", {})
                identifier = _project_identity(catalog, config, store_target=store_target)
                catalog_allowance = _bytes(catalog)
                if decoded_bytes + catalog_allowance >= budget.native_read_bytes:
                    exhausted = True
                    raise ValueError("initial routing catalog leaves no discovery/final-guard room")
                context = read("recall_context", {"db": config.database_name, "text": cue,
                    "k": discovery_plan["k"], "max_nodes": discovery_plan["max_nodes"],
                    "depth": 0, "tags": [TAG]}, hydration=True)
                native_discovery = _discovery(context, None)
                candidates = _overlap_candidates(context, discovery_plan["max_nodes"])
                counts["observed_unique_window"] = len(candidates)
                cards = []
                # MAX_BODY_BYTES bounds content only; conservative envelope room
                # covers ordinary summary/provenance fields without claiming a
                # strict RPC response cap. Charge the actual returned object.
                get_allowance = MAX_BODY_BYTES + 4096
                for index, candidate in enumerate(candidates):
                    if (decoded_bytes + get_allowance + catalog_allowance > budget.native_read_bytes
                            or time.monotonic() >= hydration_deadline):
                        exhausted = True
                        counts["work_budget_unread"] = len(candidates) - index
                        break
                    counts["readbacks_attempted"] += 1
                    try:
                        node = read("get", {"db": config.database_name, "id": candidate["id"], "body": True,
                            "edges": False, "expected_db_id": identifier,
                            "max_body_bytes": MAX_BODY_BYTES}, hydration=True)
                    except TimeoutError:
                        exhausted = True
                        counts["work_budget_unread"] = len(candidates) - index
                        break
                    if _card(node, candidate["id"], candidate) is None:
                        raise ValueError("routing candidate no longer current")
                    witness = decode_witness(node, expected_db_id=identifier)
                    if witness is None:
                        counts["rejected"] += 1
                    else:
                        cards.append(witness)
                        counts["returned"] += 1
                _project_identity(read("databases", {}), config, identifier, store_target)
                if _config_snapshot(service_config_path) != snapshot:
                    raise _IdentityMismatch("service configuration changed")
                if time.monotonic() >= deadline:
                    raise TimeoutError("final routing guard deadline exceeded")
                if decoded_bytes > budget.native_read_bytes:
                    exhausted = True
                    raise ValueError("final routing guard exceeded decoded allowance")
                result = {"outcome": "ok" if cards else "empty", "witnesses": cards, "db_id": identifier}
            finally:
                client.timeout = max(.001, min(deadline - time.monotonic(), 1.5))
    except _IdentityMismatch:
        result["outcome"] = "identity_mismatch"
    except TimeoutError:
        result["outcome"] = "timeout"
    except (ImportError, OSError, ValueError, TypeError, KeyError, McpError) as error:
        if isinstance(error, McpError) and "expected_db_id mismatch" in str(error):
            result["outcome"] = "identity_mismatch"
        elif isinstance(error, McpError) and "timed out" in str(error):
            result["outcome"] = "timeout"
    counts["returned"] = len(result["witnesses"])
    result["elapsed_ms"] = int((time.monotonic() - started) * 1000)
    result["routing_discovery"] = _bounded_routing_discovery({"discovery": native_discovery,
        "hydration": counts, "native_work": {"decoded_bytes": decoded_bytes,
            "read_allowance_bytes": budget.native_read_bytes, "read_allowance_exhausted": exhausted}})
    return result


def collect_overlap(service_config_path: Path, cue: str, project_root: Path, *,
                    expected_db_id: str, timeout=None, include_routing: bool = False,
                    budget=None, read_plan=None, spent_native_bytes=0, store_target=None) -> dict:
    """Background-only bounded hints, never completeness or novelty evidence.

    One unpinned, resource-sized discovery window nominates IDs. Each summary comes
    exclusively from native identity-guarded readback, then the original named
    project catalog and service configuration are checked again. Required-read or
    identity failures discard the pool; optional history exhaustion preserves the
    guarded summaries and reports actual decoded work, including any overshoot.
    """
    if cue is None:
        return _result("unavailable")
    budget = LibrarianBudget() if budget is None else budget
    timeout = budget.native_seconds if timeout is None else timeout
    return _background_project_read(service_config_path, project_root, timeout,
                                    cue=cue, expected_db_id=expected_db_id, include_routing=include_routing,
                                    budget=budget, read_plan=read_plan, spent_native_bytes=spent_native_bytes,
                                    **({"store_target": store_target} if store_target is not None else {}))


def check_association_target(service_config_path: Path, project_root: Path,
                             target: AssociationTarget, *, expected_db_id: str,
                             timeout: float, store_target=None) -> str:
    """One guarded get of the retained semantic hint before capture is frozen.

    Fresh overlap compares the bounded assessor summary. Historical delivery
    compares the full native-get fingerprint, not the truncated display. Neither
    is a body-version CAS; a later edit or deletion can still make capture fail.
    """
    if (not isinstance(target, AssociationTarget) or target.kind != "semantic"
            or not isinstance(target.native_id, str) or not NODE_ID_RE.fullmatch(target.native_id)
            or not isinstance(target.summary, str)
            or not target.summary or len(target.summary.encode("utf-8")) > (MAX_OVERLAP_SUMMARY_BYTES if target.origin == "overlap" else 803)
            or target.origin not in ("overlap", "delivery")
            or (target.origin == "delivery" and (target.db_id != expected_db_id
                or not isinstance(target.full_get_fingerprint, str)
                or not re.fullmatch(r"[0-9a-f]{64}", target.full_get_fingerprint)))
            or not isinstance(expected_db_id, str) or not NODE_ID_RE.fullmatch(expected_db_id)
            or not isinstance(service_config_path, Path) or not isinstance(project_root, Path)
            or not project_root.is_absolute() or type(timeout) not in (int, float)
            or not 0 < timeout <= 2):
        return "unavailable"
    started = time.monotonic()

    def remaining():
        value = started + timeout - time.monotonic()
        if value <= 0:
            raise TimeoutError("association read deadline")
        return min(value, 1.5)

    try:
        config, snapshot, token = _project_config(service_config_path, project_root, store_target)
        with McpClient(config.url, token=token, timeout=remaining()) as client:
            try:
                client.timeout = remaining()
                _project_identity(client.call_tool("databases", {}), config, expected_db_id, store_target)
                client.timeout = remaining()
                node = client.call_tool("get", {"db": config.database_name, "id": target.native_id,
                                                "body": False, "edges": False,
                                                "expected_db_id": expected_db_id})
                remaining()
                # This is an identity comparison, not a delivered card. No
                # recall-time reference status/origin is invented by exact GET.
                card = _card(node, target.native_id)
                if card is None:
                    return "stale"
                if target.origin == "delivery":
                    same = card["fingerprint"] == target.full_get_fingerprint
                else:
                    same = _short(card["summary"], MAX_OVERLAP_SUMMARY_BYTES) == target.summary
                if not same:
                    return "stale"
                if _config_snapshot(service_config_path) != snapshot:
                    return "unavailable"
                remaining()
                return "kept"
            finally:
                client.timeout = max(0.001, min(started + timeout - time.monotonic(), 1.5))
    except (OSError, ValueError, TypeError, KeyError, McpError, TimeoutError):
        return "unavailable"

def recall_cards(service_config_path: Path, prompt: str, project_root: Path,
                 timeout: float = 1.5, observe: bool = False) -> dict:
    """Return at most two read-only source-labelled cards within a wall deadline."""
    started = time.monotonic()
    if (not isinstance(prompt, str) or not prompt.strip()
            or len(prompt.encode("utf-8")) > MAX_PROMPT_BYTES
            or not isinstance(service_config_path, Path)
            or not isinstance(project_root, Path)
            or not project_root.is_absolute()
            or not 0 < timeout <= 5 or type(observe) is not bool):
        return _result("skipped", elapsed_ms=int((time.monotonic() - started) * 1000))
    payload = json.dumps({"service_config_path": str(service_config_path),
                          "project_root": str(project_root), "prompt": prompt,
                          "timeout": timeout, "observe": observe}, ensure_ascii=False).encode("utf-8")
    try:
        completed = subprocess.run([sys.executable, str(Path(__file__).resolve()), "--collect"],
                                   input=payload, capture_output=True, timeout=timeout,
                                   check=False)
    except subprocess.TimeoutExpired:
        return _result("timeout", elapsed_ms=int((time.monotonic() - started) * 1000))
    except OSError:
        return _result("unavailable", elapsed_ms=int((time.monotonic() - started) * 1000))
    elapsed_ms = int((time.monotonic() - started) * 1000)
    if completed.returncode != 0 or len(completed.stdout) > MAX_CHILD_OUTPUT:
        return _result("unavailable", elapsed_ms=elapsed_ms)
    try:
        value = json.loads(completed.stdout)
        if (not isinstance(value, dict) or value.get("outcome") not in ("ok", "empty", "unavailable")
                or not isinstance(value.get("cards"), list) or len(value["cards"]) > MAX_CARDS):
            raise ValueError("malformed collector result")
    except (ValueError, UnicodeDecodeError):
        return _result("unavailable", elapsed_ms=elapsed_ms)
    return _result(value["outcome"], value["cards"], elapsed_ms,
                   value.get("observation") if observe else None)


def _child_main():
    try:
        raw = sys.stdin.buffer.read(12_000 + 1)
        if len(raw) > 12_000:
            raise ValueError("collector input too large")
        value = json.loads(raw)
        result = _collect(value["service_config_path"], value["prompt"],
                          value["project_root"], value["timeout"], value.get("observe", False))
    except (OSError, ValueError, KeyError, TypeError, McpError):
        result = _result("unavailable")
    sys.stdout.write(json.dumps(result, ensure_ascii=False, separators=(",", ":")))


if __name__ == "__main__":
    if sys.argv[1:] != ["--collect"]:
        raise SystemExit(2)
    _child_main()


def register_reader_concerns(service_config_path, project_root, db_id, nominations,
                             endpoints, *, timeout=1.5, allowed=None, store_target=None):
    """Optional guarded notices; native failure keeps a warning without write authority."""
    from turn_observer import validate_concern_row
    warnings = [{"shown_text": c["caveat"] + " Missing fact: " + c["missing_fact"],
                 "displayed_endpoint_ids": [c["left_id"], c["right_id"]], "expected_row": None}
                for c in nominations]
    if (not warnings or not isinstance(db_id, str) or not NODE_ID_RE.fullmatch(db_id)
            or (allowed is not None and not allowed())):
        return warnings
    deadline = time.monotonic() + timeout
    try:
        config, snapshot, token = _project_config(service_config_path, project_root, store_target)
        with McpClient(config.url, token=token, timeout=min(timeout, 1.5)) as client:
            if store_target is not None:
                store_target.catalog_identity(client.call_tool("databases", {}), db_id)
            for nomination, warning in zip(nominations, warnings):
                ids = sorted(warning["displayed_endpoint_ids"])
                observed = [endpoints.get(i) for i in ids]
                if any(not isinstance(e, dict) or e.get("id") != i for e, i in zip(observed, ids)):
                    continue
                if (time.monotonic() >= deadline or _config_snapshot(service_config_path) != snapshot
                        or (allowed is not None and not allowed())):
                    break
                binding = {"key": {"kind": nomination["kind"], "lo": ids[0], "hi": ids[1]},
                           "endpoints": observed}
                client.timeout = min(deadline - time.monotonic(), 1.5)
                response = client.concern_checked(config.database_name, {"action": "notice", "notice": {
                    "binding": binding, "concern": nomination["caveat"], "missing_fact": nomination["missing_fact"]}},
                    expected_db_id=db_id)
                if response.get("db") != config.database_name or response.get("db_id") != db_id or response.get("action") != "notice":
                    raise ValueError("concern owner mismatch")
                if (_config_snapshot(service_config_path) != snapshot
                        or (allowed is not None and not allowed())):
                    break
                outcome = response["outcome"]
                if outcome["status"] in ("applied", "unchanged"):
                    row = validate_concern_row(outcome["row"])
                    if row["notice"]["binding"] == binding:
                        warning["expected_row"] = row
    except (AttributeError, OSError, ValueError, TypeError, KeyError, McpError, TimeoutError):
        pass
    return warnings
