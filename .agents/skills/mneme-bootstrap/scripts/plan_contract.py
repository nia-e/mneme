#!/usr/bin/env python3
"""Deterministic construction primitives for mneme-bootstrap contracts."""

from __future__ import annotations

import hashlib
import json
import math
import unicodedata


SCHEMA_VERSION = 1
NAMESPACE = "repo-sync-v1"
EXTRACTOR_VERSION = 1
POLICY_VERSION = "repo-sync-v1-default"
BOOTSTRAP_EDGE_KINDS = {"associative", "supersedes", "derived-from"}
APPLY_BLOCKING_FINDINGS = {
    "conflict",
    "injection",
    "sensitive",
    "ambiguity",
    "ownership",
}

MODES = {"greenfield", "managed_refresh_proposal", "brownfield_proposal"}
HARD_MAXIMA = {
    "nodes": 20,
    "changed_nodes": 10,
    "edges": 40,
    "bytes_per_body": 16_384,
    "core_body_bytes": 2_048,
    "max_out_degree": 8,
    "max_in_degree": 12,
    "sources": 500,
    "sources_per_record": 16,
    "tags_per_node": 16,
    "summary_bytes": 1_024,
    "tag_bytes": 128,
    "findings": 100,
    "exclusions": 100_000,
    "brownfield_dispositions": 500,
    "plan_bytes": 2_097_152,
    "inventory_bytes": 33_554_432,
    "inventory_entries": 100_000,
    "inventory_scan_bytes": 1_073_741_824,
    "manifest_bytes": 2_097_152,
    "journal_bytes": 2_097_152,
    "source_bytes": 1_048_576,
}

FULL_NODE_ACTIONS = {
    "ingest",
    "ingest_proposal",
    "replace_proposal",
    "adoption_proposal",
}
PROPOSAL_NODE_ACTIONS = {
    "ingest_proposal",
    "replace_proposal",
    "retirement_proposal",
    "adoption_proposal",
}
FULL_EDGE_ACTIONS = {"link", "link_proposal", "replace_proposal"}
PROPOSAL_EDGE_ACTIONS = {"link_proposal", "replace_proposal", "retirement_proposal"}


def canonical_bytes(value: object) -> bytes:
    return json.dumps(
        value,
        ensure_ascii=True,
        allow_nan=False,
        sort_keys=True,
        separators=(",", ":"),
    ).encode("utf-8")


def canonical_sha256(value: object) -> str:
    return hashlib.sha256(canonical_bytes(value)).hexdigest()


def canonical_unit_number(value: object, context: str = "value") -> float:
    """Return the one canonical JSON representation for a finite unit number."""

    if not isinstance(value, (int, float)) or isinstance(value, bool):
        raise ValueError(f"{context} must be a finite number in [0,1]")
    try:
        number = float(value)
    except (OverflowError, ValueError) as error:
        raise ValueError(f"{context} must be a finite number in [0,1]") from error
    if not math.isfinite(number) or not 0 <= number <= 1:
        raise ValueError(f"{context} must be a finite number in [0,1]")
    # JSON distinguishes -0.0 from 0.0 even though the engine does not. Never let
    # that spelling create a different assertion/materialization hash.
    return 0.0 if number == 0 else number


def plan_hash(plan: dict) -> str:
    payload = dict(plan)
    payload.pop("plan_hash", None)
    return canonical_sha256(payload)


def normalize_text(value: str) -> str:
    """Canonicalize safe multiline text and reject terminal/control injection."""

    value = unicodedata.normalize("NFC", value)
    allowed_boundaries = {"\n", "\r", "\u0085", "\u2028", "\u2029"}
    for character in value:
        codepoint = ord(character)
        if character == "\t" or character in allowed_boundaries:
            continue
        if codepoint < 0x20 or 0x7F <= codepoint <= 0x9F:
            raise ValueError("text contains a forbidden control character")
    value = (
        value.replace("\r\n", "\n")
        .replace("\r", "\n")
        .replace("\u0085", "\n")
        .replace("\u2028", "\n")
        .replace("\u2029", "\n")
    )
    lines = [line.rstrip(" \t") for line in value.split("\n")]
    while lines and not lines[0]:
        lines.pop(0)
    while lines and not lines[-1]:
        lines.pop()
    return "\n".join(lines)


def has_forbidden_controls(value: str, *, multiline: bool = False) -> bool:
    allowed: set[str] = set()
    if multiline:
        allowed |= {"\t", "\n", "\r", "\u0085", "\u2028", "\u2029"}
    return any(
        character not in allowed
        and (ord(character) < 0x20 or 0x7F <= ord(character) <= 0x9F)
        for character in value
    )


def canonical_sources(sources: list[dict]) -> list[dict]:
    result = []
    for source in sources:
        result.append(
            {
                "path": source["path"],
                "blob": source["blob"],
                "sha256": source["sha256"],
                "span": source["span"],
            }
        )
    return sorted(
        result,
        key=lambda source: (
            source["path"],
            source["span"],
            source["blob"],
            source["sha256"],
        ),
    )


def content_evidence(sources: list[dict]) -> list[dict]:
    """Path-independent evidence identity for rename-stable semantic content.

    Paths remain covered by the plan and manifest hashes. A validator must prove
    that a path change is the unique same-blob rename before accepting a no-op.
    """

    evidence = [
        {
            "blob": source["blob"],
            "sha256": source["sha256"],
            "span": source["span"],
        }
        for source in sources
    ]
    return sorted(
        evidence,
        key=lambda source: (source["span"], source["blob"], source["sha256"]),
    )


def node_content_hash(*, key: str, summary: str, claim: str, sources: list[dict]) -> str:
    return canonical_sha256(
        {
            "schema_version": SCHEMA_VERSION,
            "namespace": NAMESPACE,
            "key": key,
            "summary": normalize_text(summary),
            "claim": normalize_text(claim),
            "evidence": content_evidence(sources),
        }
    )


def materialization_hash(
    *,
    content_hash: str,
    body: str,
    tags: list[str],
    status: str,
    stability: float,
    confidence: float,
) -> str:
    return canonical_sha256(
        {
            "schema_version": SCHEMA_VERSION,
            "namespace": NAMESPACE,
            "content_hash": content_hash,
            "body_sha256": hashlib.sha256(body.encode("utf-8")).hexdigest(),
            "tags": sorted(set(tags)),
            "status": status,
            "stability": stability,
            "confidence": confidence,
        }
    )


def edge_assertion_hash(
    *,
    key: str,
    from_key: str,
    to_key: str,
    kind: str,
    weight: float,
    sources: list[dict],
) -> str:
    return canonical_sha256(
        {
            "schema_version": SCHEMA_VERSION,
            "namespace": NAMESPACE,
            "key": key,
            "from_key": from_key,
            "to_key": to_key,
            "kind": kind,
            "weight": weight,
            "evidence": content_evidence(sources),
        }
    )


def render_body(*, key: str, content_hash: str, source_commit: str, claim: str) -> str:
    normalized_claim = normalize_text(claim)
    header = (
        "--- mneme-repo-sync\n"
        f"namespace: {NAMESPACE}\n"
        f"key: {key}\n"
        f"content-sha256: {content_hash}\n"
        f"source-commit: {source_commit}\n"
        f"extractor-version: {EXTRACTOR_VERSION}\n"
        "source-state: git-committed\n"
        "content-trust: untrusted-evidence\n"
        "---\n"
    )
    return header + normalized_claim + ("\n" if normalized_claim else "")


def parse_body(body: str) -> tuple[dict[str, str], str]:
    # Builder output contains LF only. Splitting on LF (rather than splitlines)
    # keeps parsing identical to render_body for every Unicode input.
    lines = body.split("\n")
    if len(lines) < 10 or lines[0] != "--- mneme-repo-sync" or lines[8] != "---":
        raise ValueError("body must start with the exact nine-line recovery header")
    expected_names = (
        "namespace",
        "key",
        "content-sha256",
        "source-commit",
        "extractor-version",
        "source-state",
        "content-trust",
    )
    header: dict[str, str] = {}
    for line, name in zip(lines[1:8], expected_names):
        prefix = f"{name}: "
        if not line.startswith(prefix):
            raise ValueError(f"recovery header field {name!r} is missing or out of order")
        header[name] = line[len(prefix) :]
    claim = normalize_text("\n".join(lines[9:]))
    expected_body = render_body(
        key=header["key"],
        content_hash=header["content-sha256"],
        source_commit=header["source-commit"],
        claim=claim,
    )
    if body != expected_body:
        raise ValueError("body is not in canonical recovery-header form")
    return header, claim


def expected_post_manifest_delta(plan: dict) -> dict:
    node_actions = sorted(
        ({"key": node["key"], "action": node["action"]} for node in plan.get("nodes", [])),
        key=lambda item: (item["key"], item["action"]),
    )
    edge_actions = sorted(
        ({"key": edge["key"], "action": edge["action"]} for edge in plan.get("edges", [])),
        key=lambda item: (item["key"], item["action"]),
    )
    greenfield = plan.get("mode") == "greenfield"
    generation = plan.get("base", {}).get("manifest_generation", 0)
    return {
        "publish": greenfield,
        "next_generation": generation + 1 if greenfield else None,
        "node_actions": node_actions,
        "edge_actions": edge_actions,
    }


def expected_journal_operations(plan: dict) -> list[dict]:
    """Return immutable requests for the only executable mode: greenfield."""

    if plan.get("mode") != "greenfield":
        return []
    operations = []
    for node in plan.get("nodes", []):
        if node.get("action") == "ingest":
            operations.append(
                {
                    "op_id": f"node:ingest:{node['key']}",
                    "kind": "node_ingest",
                    "key": node["key"],
                    "request_hash": canonical_sha256(node),
                }
            )
    for edge in plan.get("edges", []):
        if edge.get("action") == "link":
            operations.append(
                {
                    "op_id": f"edge:link:{edge['key']}",
                    "kind": "edge_link",
                    "key": edge["key"],
                    "request_hash": canonical_sha256(edge),
                }
            )
    return operations
