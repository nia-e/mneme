#!/usr/bin/env python3
"""Recovery-journal construction and validation for greenfield bootstrap."""

from __future__ import annotations

import re

from plan_contract import (
    APPLY_BLOCKING_FINDINGS,
    NAMESPACE,
    SCHEMA_VERSION,
    expected_journal_operations,
)
from review_contract import ReviewError, validate_approval


HEX64 = re.compile(r"^[0-9a-f]{64}$")
ULID = re.compile(r"^[0-9A-HJKMNP-TV-Z]{26}$")
OP_STATES = {"planned", "started", "acked", "verified", "failed"}
JOURNAL_STATES = {"applying", "verified", "manifest_published", "failed"}


class JournalError(RuntimeError):
    pass


def require(condition: bool, message: str) -> None:
    if not condition:
        raise JournalError(message)


def build_initial_journal(plan: dict, approval: dict) -> dict:
    if plan.get("mode") != "greenfield":
        raise JournalError("only a validated greenfield plan may have an executable journal")
    blocking = [
        finding
        for finding in plan.get("adversarial_findings", [])
        if finding.get("kind") in APPLY_BLOCKING_FINDINGS
        and finding.get("status") == "unresolved"
    ]
    if blocking or not plan.get("adversarial_findings"):
        raise JournalError("missing review or unresolved safety/ownership findings block apply")
    try:
        approval = validate_approval(plan, approval)
    except ReviewError as error:
        raise JournalError(str(error)) from error
    operations = [
        {**operation, "state": "planned", "result": None, "error": None}
        for operation in expected_journal_operations(plan)
    ]
    return {
        "schema_version": SCHEMA_VERSION,
        "namespace": NAMESPACE,
        "plan_hash": plan["plan_hash"],
        "target": dict(plan["target"]),
        "base": dict(plan["base"]),
        "review_approval": approval,
        "state": "applying",
        "operations": operations,
        "published_manifest_hash": None,
        "error": None,
    }


def validate_journal(plan: dict, journal: object) -> dict:
    require(plan.get("mode") == "greenfield", "journals are executable only for greenfield plans")
    require(
        not any(
            isinstance(finding, dict)
            and finding.get("kind") in APPLY_BLOCKING_FINDINGS
            and finding.get("status") == "unresolved"
            for finding in plan.get("adversarial_findings", [])
        ),
        "unresolved safety/ownership findings block apply",
    )
    require(bool(plan.get("adversarial_findings")), "adversarial review evidence is required")
    require(isinstance(journal, dict), "journal root must be an object")
    expected_fields = {
        "schema_version",
        "namespace",
        "plan_hash",
        "target",
        "base",
        "review_approval",
        "state",
        "operations",
        "published_manifest_hash",
        "error",
    }
    require(set(journal) == expected_fields, "journal fields must match the v1 contract exactly")
    require(journal.get("schema_version") == SCHEMA_VERSION, "journal schema mismatch")
    require(journal.get("namespace") == NAMESPACE, "journal namespace mismatch")
    require(journal.get("plan_hash") == plan.get("plan_hash"), "journal plan_hash mismatch")
    require(journal.get("target") == plan.get("target"), "journal target mismatch")
    require(journal.get("base") == plan.get("base"), "journal base mismatch")
    try:
        validate_approval(plan, journal.get("review_approval"))
    except ReviewError as error:
        raise JournalError(f"journal review approval is invalid: {error}") from error
    state = journal.get("state")
    require(state in JOURNAL_STATES, "invalid journal state")
    operations = journal.get("operations")
    require(isinstance(operations, list), "journal operations must be an array")
    expected = expected_journal_operations(plan)
    require(len(operations) == len(expected), "journal operation count disagrees with plan")

    node_results: dict[str, tuple[str, str]] = {}
    operation_states: list[str] = []
    failed = 0
    for index, (operation, immutable) in enumerate(zip(operations, expected)):
        context = f"operations[{index}]"
        require(isinstance(operation, dict), f"{context}: operation must be an object")
        require(
            set(operation)
            == {"op_id", "kind", "key", "request_hash", "state", "result", "error"},
            f"{context}: fields must be exact",
        )
        for field in ("op_id", "kind", "key", "request_hash"):
            require(operation.get(field) == immutable[field], f"{context}: immutable {field} mismatch")
        op_state = operation.get("state")
        require(op_state in OP_STATES, f"{context}: invalid state")
        operation_states.append(op_state)
        result, error = operation.get("result"), operation.get("error")
        if op_state in {"planned", "started"}:
            require(result is None and error is None, f"{context}: unfinished operation cannot have result/error")
        elif op_state == "failed":
            failed += 1
            require(result is None, f"{context}: failed operation cannot have a result")
            require(isinstance(error, str) and bool(error.strip()), f"{context}: failed operation requires error")
        elif operation["kind"] == "node_ingest":
            require(
                isinstance(result, dict)
                and set(result) == {"node_id"}
                and isinstance(result.get("node_id"), str)
                and ULID.fullmatch(result["node_id"]) is not None,
                f"{context}: acknowledged node ingest requires exact node_id result",
            )
            require(error is None, f"{context}: successful operation cannot have error")
            require(result["node_id"] not in {item[1] for item in node_results.values()}, f"{context}: duplicate returned node_id")
            node_results[operation["key"]] = (op_state, result["node_id"])
        else:
            require(
                isinstance(result, dict)
                and set(result) == {"from_node_id", "to_node_id"}
                and all(
                    isinstance(result.get(field), str) and ULID.fullmatch(result[field]) is not None
                    for field in ("from_node_id", "to_node_id")
                ),
                f"{context}: acknowledged edge link requires physical endpoint ids",
            )
            require(error is None, f"{context}: successful operation cannot have error")

    plan_edges = {edge["key"]: edge for edge in plan["edges"]}
    for operation in operations:
        if operation["kind"] != "edge_link" or operation["state"] not in {"acked", "verified"}:
            continue
        edge = plan_edges[operation["key"]]
        require(edge["from_key"] in node_results and edge["to_key"] in node_results, f"edge {edge['key']!r} acknowledged before endpoint ingests")
        from_state, from_id = node_results[edge["from_key"]]
        to_state, to_id = node_results[edge["to_key"]]
        require(
            from_state == "verified" and to_state == "verified",
            f"edge {edge['key']!r} endpoints are not verified",
        )
        require(
            operation["result"] == {"from_node_id": from_id, "to_node_id": to_id},
            f"edge {edge['key']!r} result does not match node journal results",
        )

    verified_prefix = 0
    while (
        verified_prefix < len(operation_states)
        and operation_states[verified_prefix] == "verified"
    ):
        verified_prefix += 1
    tail = operation_states[verified_prefix:]
    if tail:
        boundary, remaining = tail[0], tail[1:]
        require(
            boundary in {"planned", "started", "acked", "failed"}
            and all(item == "planned" for item in remaining),
            "operations must be a verified prefix, at most one "
            "started/acked/failed boundary, then a planned suffix",
        )

    published_hash = journal.get("published_manifest_hash")
    top_error = journal.get("error")
    if state == "applying":
        require(failed == 0 and published_hash is None and top_error is None, "applying journal has terminal state")
    elif state == "verified":
        require(all(op["state"] == "verified" for op in operations), "verified journal has unfinished operations")
        require(published_hash is None and top_error is None, "verified journal cannot claim publication/error")
    elif state == "manifest_published":
        require(all(op["state"] == "verified" for op in operations), "published journal has unfinished operations")
        require(
            isinstance(published_hash, str) and HEX64.fullmatch(published_hash) is not None,
            "published journal requires manifest hash",
        )
        require(top_error is None, "published journal cannot have error")
    else:
        require(failed > 0 or (isinstance(top_error, str) and bool(top_error.strip())), "failed journal requires failure evidence")
        require(published_hash is None, "failed journal cannot claim publication")
    return {
        "valid": True,
        "contract_valid": True,
        "live_authority": False,
        "native_receipt_required": True,
        "plan_hash": plan["plan_hash"],
        "reviewer_id": journal["review_approval"]["reviewer_id"],
        "state": state,
        "operations": len(operations),
    }
