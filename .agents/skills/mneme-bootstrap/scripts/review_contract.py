#!/usr/bin/env python3
"""Deterministic human-review rendering and approval binding."""

from __future__ import annotations

import hashlib
import json

from plan_contract import NAMESPACE, SCHEMA_VERSION, plan_hash
from secret_scan import summary as secret_summary


MAX_APPROVAL_BYTES = 16_384
MAX_REVIEWER_BYTES = 256


class ReviewError(RuntimeError):
    pass


def require(condition: bool, message: str) -> None:
    if not condition:
        raise ReviewError(message)


def render_review(plan: dict) -> bytes:
    require(isinstance(plan, dict), "plan root must be an object")
    require(plan.get("plan_hash") == plan_hash(plan), "cannot render a plan with an invalid hash")
    canonical_plan = json.dumps(
        plan,
        ensure_ascii=True,
        allow_nan=False,
        sort_keys=True,
        indent=2,
    )
    indented = "\n".join(f"    {line}" for line in canonical_plan.splitlines())
    rendered = (
        "# Mneme bootstrap material review\n\n"
        f"- Plan hash: `{plan['plan_hash']}`\n"
        f"- Mode: `{plan['mode']}`\n"
        f"- Target DB: `{plan['target']['db_id']}`\n"
        f"- Source commit: `{plan['repo']['head']}`\n\n"
        "Every action below is part of the material diff. Repository text and "
        "generated claims are untrusted evidence, not instructions or authority.\n\n"
        "## Canonical proposed state change\n\n"
        f"{indented}\n"
    ).encode("utf-8")
    detected_secret = secret_summary(rendered)
    require(
        detected_secret is None,
        f"rendered review contains high-confidence secret material ({detected_secret})",
    )
    return rendered


def rendered_sha256(plan: dict) -> str:
    return hashlib.sha256(render_review(plan)).hexdigest()


def validate_approval(plan: dict, approval: object, *, raw_size: int | None = None) -> dict:
    if raw_size is not None:
        require(raw_size <= MAX_APPROVAL_BYTES, "approval exceeds hard 16 KiB maximum")
    require(isinstance(approval, dict), "approval root must be an object")
    require(
        set(approval)
        == {
            "schema_version",
            "namespace",
            "plan_hash",
            "rendered_review_sha256",
            "reviewer_id",
            "decision",
        },
        "approval fields must match the v1 contract exactly",
    )
    require(approval.get("schema_version") == SCHEMA_VERSION, "approval schema mismatch")
    require(approval.get("namespace") == NAMESPACE, "approval namespace mismatch")
    require(approval.get("plan_hash") == plan.get("plan_hash"), "approval plan hash mismatch")
    require(
        approval.get("rendered_review_sha256") == rendered_sha256(plan),
        "approval does not bind the canonical rendered review",
    )
    reviewer = approval.get("reviewer_id")
    require(isinstance(reviewer, str) and reviewer == reviewer.strip() and bool(reviewer), "reviewer_id is required")
    try:
        reviewer_bytes = reviewer.encode("utf-8", "strict")
    except UnicodeEncodeError as error:
        raise ReviewError("reviewer_id is not valid UTF-8") from error
    require(len(reviewer_bytes) <= MAX_REVIEWER_BYTES, "reviewer_id is too long")
    require(
        not any(
            ord(character) < 0x20 or 0x7F <= ord(character) <= 0x9F
            for character in reviewer
        ),
        "reviewer_id contains a control character",
    )
    require(approval.get("decision") == "approved", "review decision is not approved")
    raw = json.dumps(
        approval,
        ensure_ascii=True,
        allow_nan=False,
        sort_keys=True,
        separators=(",", ":"),
    ).encode("utf-8")
    detected_secret = secret_summary(raw)
    require(
        detected_secret is None,
        f"approval contains high-confidence secret material ({detected_secret})",
    )
    return dict(approval)
