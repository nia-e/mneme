"""Strict compact native touchstone views; no authorship or reference reads.

Historical summary coverage is deliberately weaker than full-content equality.
Keep the entire typed facet or omit the card under its existing byte budget.
"""
import json
import re
import hashlib

SCHEMA = "mneme.touchstone-view.v1"
ULID = re.compile(r"[0-7][0-9A-HJKMNP-TV-Z]{25}\Z")
SHA256 = re.compile(r"[0-9a-f]{64}\Z")


def _id(value):
    return isinstance(value, str) and ULID.fullmatch(value) is not None


def _count(value):
    return type(value) is int and 0 <= value < 1 << 32


def delivery_cache_fingerprint(card):
    """Current presented touchstone caveats participate in delivery dedup.

The immutable GET fingerprint remains separate for historical content proofs.
Ordinary-card cache behavior is intentionally unchanged.
"""
    if "touchstone" not in card:
        return card["fingerprint"]
    facet = validate_touchstone_view(card["touchstone"])
    identity = {"schema": "mneme.codex-touchstone-cache.v1", "fingerprint": card["fingerprint"],
                "id": card["id"], "summary": card["summary"], "source": card["source"],
                "status": card["status"], "kind": card["kind"], "touchstone": facet}
    return hashlib.sha256(json.dumps(identity, ensure_ascii=False, sort_keys=True,
            separators=(",", ":"), allow_nan=False).encode()).hexdigest()


def validate_touchstone_view(value, *, expected_db_id=None):
    """Copy exactly the known wire fields, rejecting loss and foreign scope."""
    try:
        if (not isinstance(value, dict) or set(value) != {
                "schema", "subject", "coverage", "references", "references_omitted", "origins"}
                or value["schema"] != SCHEMA or value["coverage"] != "summary_only"
                or not isinstance(value["subject"], str) or not value["subject"].strip()
                or len(value["subject"].encode()) > 256
                or not _count(value["references_omitted"])
                or not isinstance(value["references"], list)
                or not isinstance(value["origins"], list) or not value["origins"]):
            raise ValueError("invalid_touchstone_view")
        seen = set()
        for ref in value["references"]:
            if (not isinstance(ref, dict) or set(ref) != {
                    "db_id", "id", "snapshot_sha256", "summary", "resolution"}
                    or not _id(ref["db_id"]) or not _id(ref["id"])
                    or expected_db_id is not None and ref["db_id"] != expected_db_id
                    or not isinstance(ref["snapshot_sha256"], str)
                    or not SHA256.fullmatch(ref["snapshot_sha256"])
                    or ref["resolution"] not in
                       ("matches_snapshot", "changed_snapshot", "missing", "unavailable")):
                raise ValueError("invalid_touchstone_view")
            identity = ref["db_id"], ref["id"]
            if identity in seen:
                raise ValueError("invalid_touchstone_view")
            seen.add(identity)
            summary = ref["summary"]
            if (not isinstance(summary, dict) or set(summary) != {"text", "complete", "source_bytes"}
                    or not isinstance(summary["text"], str) or type(summary["complete"]) is not bool
                    or not _count(summary["source_bytes"])
                    or len(summary["text"].encode()) > summary["source_bytes"]
                    or summary["complete"] and len(summary["text"].encode()) != summary["source_bytes"]):
                raise ValueError("invalid_touchstone_view")
        seen = set()
        for origin in value["origins"]:
            if not isinstance(origin, dict) or not (
                    origin == {"kind": "direct"} or
                    set(origin) == {"kind", "anchor_id"} and origin["kind"] == "referrer"
                    and _id(origin["anchor_id"])):
                raise ValueError("invalid_touchstone_view")
            key = json.dumps(origin, sort_keys=True)
            if key in seen:
                raise ValueError("invalid_touchstone_view")
            seen.add(key)
        return json.loads(json.dumps(value, ensure_ascii=False, allow_nan=False))
    except (KeyError, TypeError, UnicodeError, RecursionError):
        raise ValueError("invalid_touchstone_view") from None


def validate_touchstone_retrieval(value):
    """Explicit operation-budget and frozen-anchor coverage, not relevance."""
    counts = ("read_limit", "catalog_reads", "record_reads", "referrer_page_reads",
              "target_reads", "anchors_total", "anchors_examined", "owners_discovered")
    if (not isinstance(value, dict) or set(value) != {*counts, "searched", "further_tail_unknown", "stop_reason"}
            or any(not _count(value[k]) for k in counts)
            or type(value["searched"]) is not bool or type(value["further_tail_unknown"]) is not bool
            or value["stop_reason"] not in (None, "budget", "deadline", "unsupported", "read_error")
            or sum(value[k] for k in ("catalog_reads", "record_reads", "referrer_page_reads", "target_reads")) > value["read_limit"]
            or value["anchors_examined"] > value["anchors_total"]
            or not value["searched"] and (any(value[k] for k in counts)
                or value["further_tail_unknown"] or value["stop_reason"] is not None)):
        raise ValueError("invalid_touchstone_retrieval")
    return dict(value)
