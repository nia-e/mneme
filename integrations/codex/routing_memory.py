"""Bounded host annotations for contextual routing lessons.

An annotation records a fallible judgment, not causal proof or write authority.
It lives in the body of one ordinary source-bound note, not a second edge log.
Only native code interprets the opaque route fingerprints. This module does not
reimplement traversal, pool scores, call a provider, or mutate a store.
"""
from __future__ import annotations

import hashlib
import json
import re

SCHEMA = "mneme.routing-witness.v1"
NAMESPACE = "codex-routing.v1"
TAG = "routing-judgment"
MAX_HINT_BYTES = 16 * 1024
MAX_BINDING_BYTES = 1024
MAX_BODY_BYTES = 8192
MAX_NOTE_BYTES = 2048
MAX_CONDITIONS_BYTES = 512
MAX_RATIONALE_BYTES = 768
MAX_SHOWN_BYTES = 803
ULID = re.compile(r"[0-7][0-9A-HJKMNP-TV-Z]{25}\Z")
SHA = re.compile(r"[0-9a-f]{64}\Z")
TOKEN = re.compile(r"[A-Za-z0-9_.:-]{1,256}\Z")
ROUTE_IDS = ("previous", "target", "from", "to")
ROUTE_HASHES = ("previous_fingerprint", "target_fingerprint", "edge_fingerprint")
EVIDENCE_KINDS = {"memory_delivery", "user_statement", "assistant_assertion",
                  "tool_call", "tool_result"}


def encoded(value):
    return json.dumps(value, ensure_ascii=False, sort_keys=True,
                      separators=(",", ":"), allow_nan=False).encode("utf-8")


def _require(condition, reason):
    if not condition:
        raise ValueError(reason)


def _text(value, maximum):
    return isinstance(value, str) and bool(value.strip()) and len(value.encode()) <= maximum


def _matches(pattern, value):
    return isinstance(value, str) and pattern.fullmatch(value) is not None


def validate_binding(value, *, expected_db_id=None):
    """Copy the finite native wire shape; this does not verify current state."""
    _require(isinstance(value, dict) and set(value) == {"db_id", "route"}, "binding_shape")
    _require(_matches(ULID, value["db_id"])
             and (expected_db_id is None or value["db_id"] == expected_db_id), "binding_database")
    route = value["route"]
    _require(isinstance(route, dict) and set(route) == set(ROUTE_IDS + ROUTE_HASHES), "route_shape")
    _require(all(_matches(ULID, route[k]) for k in ROUTE_IDS)
             and all(_matches(SHA, route[k]) for k in ROUTE_HASHES), "route_identity")
    _require(route["previous"] != route["target"]
             and (route["previous"], route["target"]) in
             ((route["from"], route["to"]), (route["to"], route["from"])), "route_endpoints")
    raw = encoded(value)
    _require(len(raw) <= MAX_BINDING_BYTES, "binding_bytes")
    return json.loads(raw)


def validate_witness(value, *, expected_db_id=None):
    """Validate host-owned structure, never semantic truth or applicability."""
    fields = {"schema", "note", "binding", "sign", "conditions", "rationale",
              "shown_summary", "source_turn", "evidence", "corrects"}
    _require(isinstance(value, dict) and fields <= set(value) <= fields | {"entry_kind"}
             and value["schema"] == SCHEMA,
             "witness_shape")
    _require("entry_kind" not in value or value["entry_kind"] == "conditional", "witness_entry_kind")
    validate_binding(value["binding"], expected_db_id=expected_db_id)
    _require(value["sign"] in ("boost", "weaken"), "witness_sign")
    for key, limit in (("note", MAX_NOTE_BYTES), ("conditions", MAX_CONDITIONS_BYTES),
                       ("rationale", MAX_RATIONALE_BYTES), ("shown_summary", MAX_SHOWN_BYTES)):
        _require(_text(value[key], limit), "witness_" + key)
    source = value["source_turn"]
    _require(isinstance(source, dict) and set(source) == {"session", "turn"}
             and all(_matches(TOKEN, source[k]) for k in ("session", "turn")), "witness_source")
    evidence = value["evidence"]
    _require(isinstance(evidence, list) and 2 <= len(evidence) <= 4, "witness_evidence")
    for item in evidence:
        _require(isinstance(item, dict) and set(item) == {"kind", "reference"}
                 and item["kind"] in EVIDENCE_KINDS and _text(item["reference"], 512),
                 "witness_evidence")
    _require(any(item["kind"] == "memory_delivery" for item in evidence)
             and any(item["kind"] != "memory_delivery" for item in evidence), "witness_sources")
    _require(len({encoded(item) for item in evidence}) == len(evidence), "duplicate_evidence")
    correction = value["corrects"]
    if correction is not None:
        _require(isinstance(correction, dict) and set(correction) == {"node_id", "body_sha256"}
                 and _matches(ULID, correction["node_id"])
                 and _matches(SHA, correction["body_sha256"]), "witness_correction")
    raw = encoded(value)
    _require(len(raw) <= MAX_BODY_BYTES, "witness_bytes")
    return json.loads(raw)


def validate_observed_binding(value, path, *, expected_db_id=None):
    """A target with several incoming routes cannot borrow another hop's binding."""
    binding = validate_binding(value, expected_db_id=expected_db_id)
    _require(isinstance(path, list) and 1 <= len(path) <= 4 and isinstance(path[-1], dict),
             "binding_path")
    _require(all(binding["route"][key] == path[-1].get(key) for key in ROUTE_IDS),
             "binding_path_mismatch")
    return binding


def validate_conditional_binding(value, target_id, *, expected_db_id=None):
    """Bind a recommendation to its exact target, never claim an observed hop."""
    binding = validate_binding(value, expected_db_id=expected_db_id)
    _require(binding["route"]["target"] == target_id, "conditional_target_mismatch")
    return binding


def encode_witness(*, note, binding, sign, conditions, rationale, shown_summary,
                   session, turn, evidence, corrects=None, entry_kind=None):
    """Host formats one captured lesson; assessor does not supply native IDs."""
    value = {"schema": SCHEMA, "note": note, "binding": binding, "sign": sign,
             "conditions": conditions, "rationale": rationale, "shown_summary": shown_summary,
             "source_turn": {"session": session, "turn": turn},
             "evidence": evidence, "corrects": corrects}
    if entry_kind is not None:
        value["entry_kind"] = entry_kind
    return encoded(validate_witness(value)).decode("utf-8")


def _unique_object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError("duplicate_json_key")
        result[key] = value
    return result


def decode_witness(node, *, expected_db_id):
    """Read a complete guarded `get`; ordinary prose is never signed evidence.

    The namespace/tag identify the producer convention, not an authenticity
    certificate. Missing, edited, oversized or partial annotations are optional
    learning misses. The ordinary note remains available for normal recall.
    """
    try:
        _require(isinstance(node, dict) and _matches(ULID, node.get("id"))
                 and _matches(ULID, expected_db_id) and node.get("db_id") == expected_db_id,
                 "witness_readback_identity")
        _require(node.get("status") == "active"
                 and node.get("memory_kind") == {"kind": "semantic"}
                 and isinstance(node.get("tags"), list) and TAG in node["tags"], "witness_readback_kind")
        provenance = node.get("provenance", {})
        source = provenance.get("source", {})
        _require(provenance.get("type") == "external" and source.get("namespace") == NAMESPACE,
                 "witness_producer")
        body = node.get("body")
        _require(_text(body, MAX_BODY_BYTES), "witness_readback_body")
        span = node.get("body_range", {})
        _require(span.get("source_start") == 0 and span.get("source_end") == len(body.encode())
                 and span.get("has_more") is False and span.get("next_offset") is None,
                 "witness_partial_body")
        value = validate_witness(json.loads(body, object_pairs_hook=_unique_object),
                                 expected_db_id=expected_db_id)
        origin = value["source_turn"]
        _require(source.get("session") == origin["session"]
                 and source.get("reference") == f"codex://{origin['session']}/{origin['turn']}",
                 "witness_source_mismatch")
        return {"node_id": node["id"], "body_sha256": hashlib.sha256(body.encode()).hexdigest(),
                "witness": value}
    except (ValueError, TypeError, KeyError, AttributeError, UnicodeError, RecursionError):
        return None


def hint(binding, sign):
    """One fixed signed suggestion; no count, confidence or accumulating weight."""
    _require(sign in ("boost", "weaken"), "hint_sign")
    return {**validate_binding(binding), "sign": sign}


def normalize_hints(values):
    """Validate the whole canonical wire array; never retain a signed prefix."""
    _require(isinstance(values, list), "hints_shape")
    normalized = []
    for value in values:
        _require(isinstance(value, dict) and set(value) == {"db_id", "route", "sign"}, "hint_shape")
        normalized.append(hint({"db_id": value["db_id"], "route": value["route"]}, value["sign"]))
        # Complete rejection still follows; early overflow avoids more parsing.
        _require(len(encoded(normalized)) <= MAX_HINT_BYTES, "hints_bytes")
    return normalized
